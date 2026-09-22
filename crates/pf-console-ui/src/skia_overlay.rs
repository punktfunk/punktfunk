//! Skia overlay on the presenter's device: `DirectContext` over the shared
//! Vulkan handles, a two-slot offscreen ring (one frame in flight; the
//! presenter may still sample the previous image), and damage-driven redraws.
//!
//! Two personas on one `Overlay`: the console shell (home, library, settings,
//! pairing — redrawn every frame, at 30 Hz once idle) and stream chrome (stats OSD,
//! capture hint, auto-fading start banner).

use crate::console::{Console, ConsoleEntry, ConsoleHandles, FrameCost};
use crate::shell::{ConsoleOptions, Shell};
use crate::theme::{fill, match_first_family, Fonts};
use anyhow::{anyhow, Context as _, Result};
use ash::vk as avk;
use ash::vk::Handle as _;
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_presenter::overlay::{
    FrameCtx, HudLine, Overlay, OverlayAction, OverlayFrame, PointerInput, RingCommand, RingInput,
    Role, SessionPhase, SharedDevice,
};
use skia_safe::gpu::vk as skvk;
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Font, FontMgr, Point, RRect, Rect, Surface};
use std::time::{Duration, Instant};

/// An idle console redraws at most this often. 30 Hz keeps the aurora moving.
const IDLE_FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// Long enough to read the leave/stats chords; the last `BANNER_FADE_S` fade out.
const BANNER_S: f64 = 6.0;
const BANNER_FADE_S: f64 = 0.6;

/// Offscreen target. Skia owns the image (freed with the surface); we own the view.
struct Slot {
    surface: Surface,
    image: avk::Image,
    view: avk::ImageView,
    width: u32,
    height: u32,
}

impl Slot {
    fn frame(&self) -> OverlayFrame {
        OverlayFrame {
            image: self.image,
            view: self.view,
            width: self.width,
            height: self.height,
        }
    }
}

/// Damage key for the current ring slot — re-render only when this changes.
#[derive(PartialEq, Clone, Default)]
struct Drawn {
    width: u32,
    height: u32,
    stats: Option<Vec<HudLine>>,
    hint: Option<String>,
    /// Chip text. Countdown ticks once a minute, so a still chip is free per frame.
    access: Option<String>,
    /// Access toast; occupies the hint pill's slot while up.
    notice: Option<String>,
    mic_muted: bool,
    /// Standing mute badge: the sentence, or empty while the stream is audible.
    audio_mute: String,
    /// UI scale in percent. Text is identical across monitors, so scale must live in the key.
    scale_pct: u16,
    /// Banner alpha, quantized: a fade step redraws, a steady alpha does not.
    banner_step: u8,
    /// Resize spinner phase, quantized. Nonzero while in flight (forces per-frame redraw); `0` idle.
    resize_step: u16,
    /// Ring draw state (`0` = closed). Opening animates every frame; settled is free.
    ring: u64,
}

/// Chrome metrics in px at 100 % (96 dpi). Multiply by `FrameCtx::scale`: the overlay
/// is 1:1 in physical pixels, so an unscaled 14 px OSD is half-size on a 200 % panel.
mod base {
    /// OSD/hint size, and the size the shared `Font` is built at — scale is a multiplier on this.
    pub const FONT_PX: f32 = 14.0;
    pub const OSD_MARGIN: f32 = 12.0;
    pub const OSD_PAD_X: f32 = 10.0;
    pub const OSD_PAD_Y: f32 = 8.0;
    pub const OSD_RADIUS: f32 = 8.0;
    pub const PILL_PAD_X: f32 = 14.0;
    pub const PILL_PAD_Y: f32 = 8.0;
    pub const PILL_BOTTOM: f32 = 24.0;
}

pub struct SkiaOverlay {
    /// Set in `init`. After a failed init the run loop drops us, so mid-session this is `Some`.
    gpu: Option<Gpu>,
    slots: [Option<Slot>; 2],
    /// Slot of the last returned frame; the next render takes the other.
    current: usize,
    drawn: Drawn,
    /// Stats OSD face (system monospace; the console uses Geist).
    font: Option<Font>,
    fonts: Option<Fonts>,
    /// `--browse` shell. `None` for a plain `--connect` session.
    shell: Option<Shell>,
    /// Stream start; drives the start banner.
    streaming_since: Option<Instant>,
    banner_text: Option<String>,
    /// Resize-scrim start; spinner phase. `None` = `FrameCtx::resizing` was false last frame.
    resizing_since: Option<Instant>,
    /// In-stream quick-action ring (`design/touch-client-overlay.md`).
    ring: crate::ring::Ring,
    ring_drawn_at: Option<Instant>,
    /// The ring's finger, read the same way as the shell's.
    ring_touch: crate::pointer::Touch,
    /// Scale the ring last drew at; its touch slop and drag ticks grow with it.
    ring_k: f64,
    /// Last console render, for the idle rate. `None` once stream chrome took the slots.
    console_at: Option<Instant>,
    console_cost: FrameCost,
}

struct Gpu {
    device: ash::Device,
    queue_family_index: u32,
    context: DirectContext,
    /// Shared queue lock (`SharedDevice::queue_lock`). Skia submits on the presenter's
    /// graphics queue, which FFmpeg's decode prep also uses from the pump thread —
    /// every `flush*`/`submit` below holds this.
    queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    // Loader + instance dispatch must outlive DirectContext (fn pointers live in libvulkan).
    _entry: ash::Entry,
    _instance: ash::Instance,
}

impl SkiaOverlay {
    #[allow(clippy::new_without_default)]
    pub fn new() -> SkiaOverlay {
        SkiaOverlay {
            gpu: None,
            slots: [None, None],
            current: 0,
            drawn: Drawn::default(),
            font: None,
            fonts: None,
            shell: None,
            streaming_since: None,
            banner_text: None,
            ring: crate::ring::Ring::new(),
            ring_drawn_at: None,
            ring_touch: crate::pointer::Touch::default(),
            ring_k: 1.0,
            resizing_since: None,
            console_at: None,
            console_cost: FrameCost::default(),
        }
    }

    /// `--browse` overlay: full shell between streams, stream chrome during them.
    /// Returns the binary's handles (models + command bus).
    pub fn console(
        opts: ConsoleOptions,
        entry: ConsoleEntry,
    ) -> Result<(SkiaOverlay, ConsoleHandles)> {
        let handles = ConsoleHandles::new();
        let console = Console::new(opts, entry, &handles)?;
        let mut o = SkiaOverlay::new();
        let (shell, fonts) = console.into_parts();
        o.shell = Some(shell);
        o.fonts = Some(fonts);
        Ok((o, handles))
    }

    fn console_visible(&self) -> bool {
        self.shell.as_ref().is_some_and(|s| !s.in_stream)
    }
}

impl Drop for SkiaOverlay {
    /// The run loop quiesces the queue before drop. Views + Skia surfaces (which
    /// free their VkImages) can then go. Field order drops slots before DirectContext.
    fn drop(&mut self) {
        if let Some(gpu) = &mut self.gpu {
            for slot in self.slots.iter_mut().flat_map(Option::take) {
                // SAFETY: the view belongs to this slot and the overlay is being dropped, so no
                // further recording can reference it; the flush/submit + queue guard below is what
                // retires any work that still could.
                unsafe { gpu.device.destroy_image_view(slot.view, None) };
                drop(slot.surface);
            }
            let _q = gpu.queue_lock.guard(); // queue external sync vs FFmpeg's pump
            gpu.context.flush_and_submit();
        }
    }
}

impl Overlay for SkiaOverlay {
    fn init(&mut self, shared: &SharedDevice) -> Result<()> {
        // Skia resolves Vulkan entry points through us (same ash dispatch). The
        // DirectContext bakes the table in `make_vulkan`; this closure dies with `init`.
        let entry = shared.entry.clone();
        let instance = shared.instance.clone();
        let get_proc = move |of: skvk::GetProcOf| -> *const std::ffi::c_void {
            // SAFETY: Skia calls this loader with raw instance/device handles it received from the
            // `BackendContext` below — i.e. the very ones owned by `shared`, still live for the
            // overlay's lifetime — and `from_raw` only rewraps them for the ash entry points. Each
            // name is a NUL-terminated C string from Skia, borrowed for the call.
            unsafe {
                match of {
                    skvk::GetProcOf::Instance(raw_instance, name) => entry
                        .get_instance_proc_addr(avk::Instance::from_raw(raw_instance as _), name)
                        .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void),
                    skvk::GetProcOf::Device(raw_device, name) => {
                        (instance.fp_v1_0().get_device_proc_addr)(
                            avk::Device::from_raw(raw_device as _),
                            name,
                        )
                        .map_or(std::ptr::null(), |f| f as *const std::ffi::c_void)
                    }
                }
            }
        };
        let backend_builder = skvk::BackendContext::new_builder(
            shared.instance.handle().as_raw() as _,
            shared.physical_device.as_raw() as _,
            shared.device.handle().as_raw() as _,
            (
                shared.queue.as_raw() as _,
                shared.queue_family_index as usize,
            ),
            &get_proc,
            // Must be the presenter's declared version, never `None`.
            // `None` leaves Skia's `fMaxAPIVersion` at 0, so Skia calls
            // `vkEnumerateInstanceVersion()` (the loader's ceiling, not ours)
            // and validates a newer table against a 1.3 instance.
            Some(skvk::Version::from(shared.api_version)),
        );
        // SAFETY: the instance/physical-device/device handles come from `shared`, which owns them
        // and outlives this backend context, and `get_proc` above resolves through those same
        // handles. Skia stores them but does not take ownership — teardown stays ours.
        let backend = unsafe { backend_builder.build() };
        let mut context = gpu::direct_contexts::make_vulkan(&backend, None)
            .ok_or_else(|| anyhow!("Skia DirectContext over the shared device"))?;
        context.set_resource_cache_limit(
            self.shell
                .as_ref()
                .map_or(crate::shell::DEFAULT_GPU_CACHE_BYTES, |s| s.gpu_cache_bytes),
        );
        // The console is built before the presenter, so the codec row starts optimistic.
        // This is the first moment the real device can answer, and it runs before any frame.
        if let Some(shell) = &mut self.shell {
            shell.av1_ok = shared.av1_decode;
        }

        let typeface = match_first_family(
            &FontMgr::new(),
            &["monospace", "Consolas", "Cascadia Mono", "Courier New"],
            skia_safe::FontStyle::normal(),
        )
        .context("no monospace typeface (fontconfig alias or system family)")?;
        self.font = Some(Font::new(typeface, base::FONT_PX));
        if self.fonts.is_none() {
            self.fonts = Some(crate::theme::build_fonts()?);
        }

        self.gpu = Some(Gpu {
            device: shared.device.clone(),
            queue_family_index: shared.queue_family_index,
            context,
            queue_lock: shared.queue_lock.clone(),
            _entry: shared.entry.clone(),
            _instance: shared.instance.clone(),
        });
        tracing::info!("Skia console UI on the presenter's device");
        Ok(())
    }

    fn handle_event(&mut self, event: &sdl3::event::Event) -> bool {
        // Console keys/text only while visible. Chord-modified keys stay the
        // run loop's. During a stream, only the open ring consumes keys.
        if !self.console_visible() {
            if let sdl3::event::Event::KeyDown {
                scancode: Some(sc),
                keymod,
                ..
            } = event
            {
                use sdl3::keyboard::Mod;
                if self.ring.open()
                    && !keymod
                        .intersects(Mod::LCTRLMOD | Mod::RCTRLMOD | Mod::LALTMOD | Mod::RALTMOD)
                {
                    if let Some(key) = key_of(*sc) {
                        return self.ring.key(key);
                    }
                }
            }
            return false;
        }
        let Some(shell) = &mut self.shell else {
            return false;
        };
        match event {
            sdl3::event::Event::KeyDown {
                scancode: Some(sc),
                keymod,
                repeat,
                ..
            } => {
                use sdl3::keyboard::Mod;
                if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD | Mod::LALTMOD | Mod::RALTMOD) {
                    return false;
                }
                let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                let Some(key) = key_of(*sc) else {
                    return false;
                };
                shell.key(key, shift, *repeat)
            }
            sdl3::event::Event::TextInput { text, .. } => {
                shell.text_input(text);
                true
            }
            _ => false,
        }
    }

    fn handle_menu(&mut self, event: MenuEvent) -> Option<MenuPulse> {
        if !self.console_visible() && self.ring.open() {
            return self.ring.menu(event);
        }
        if self.console_visible() {
            self.shell.as_mut().and_then(|s| {
                // menu_rx is pad-only (keyboard goes through `key`); this is the pad source.
                s.note_input_source(crate::console::InputSource::Pad);
                s.handle_menu(event)
            })
        } else {
            None
        }
    }

    /// The console takes the pointer while visible, the open ring otherwise. The side
    /// that skips an event drops its finger, so no gesture outlives a switch.
    fn handle_pointer(&mut self, input: PointerInput) -> bool {
        if self.console_visible() {
            self.ring_touch.reset();
            return self.shell.as_mut().is_some_and(|s| s.pointer_input(input));
        }
        if let Some(shell) = &mut self.shell {
            shell.touch.reset();
        }
        if !self.ring.open() {
            self.ring_touch.reset();
            return false;
        }
        let ring = &mut self.ring;
        self.ring_touch
            .feed(input, self.ring_k, |p| ring.pointer(p))
    }

    fn take_action(&mut self) -> Option<OverlayAction> {
        self.shell.as_mut().and_then(|s| s.take_action())
    }

    fn ring_input(&mut self, input: RingInput) {
        self.ring.input(input);
    }

    fn ring_open(&self) -> bool {
        self.ring.open()
    }

    fn holds_stream(&self) -> bool {
        self.shell.as_ref().is_some_and(Shell::holds_stream)
    }

    fn take_ring_command(&mut self) -> Option<RingCommand> {
        self.ring.take_command()
    }

    fn text_input_active(&self) -> bool {
        self.console_visible() && self.shell.as_ref().is_some_and(Shell::editing)
    }

    fn session_phase(&mut self, phase: SessionPhase) {
        let Some(shell) = &mut self.shell else { return };
        // Banner clock is the overlay's; everything else is the shell's.
        match &phase {
            SessionPhase::Streaming => self.streaming_since = Some(Instant::now()),
            SessionPhase::Ended(_) | SessionPhase::Reconnecting(_) => self.streaming_since = None,
            SessionPhase::Connecting | SessionPhase::Failed(_) => {}
        }
        shell.session_phase(phase);
    }

    fn frame(&mut self, ctx: &FrameCtx) -> Result<Option<OverlayFrame>> {
        // Full-screen and opaque; the aurora animates every frame. Idle, the slot on glass
        // is handed back until `IDLE_FRAME` passes, and the presenter skips its present.
        if self.console_visible() {
            let idle = self.shell.as_ref().is_some_and(Shell::idle)
                && self.console_at.is_some_and(|t| t.elapsed() < IDLE_FRAME);
            if let Some(slot) = self.slots[self.current]
                .as_ref()
                .filter(|s| idle && (s.width, s.height) == (ctx.width, ctx.height))
            {
                return Ok(Some(slot.frame()));
            }
            let drew = Instant::now();
            let next = 1 - self.current;
            self.ensure_slot(next, ctx.width, ctx.height)?;
            let Self {
                gpu,
                slots,
                shell,
                fonts,
                ..
            } = self;
            let gpu = gpu.as_mut().expect("init ran");
            let slot = slots[next].as_mut().expect("just ensured");
            let shell = shell.as_mut().expect("console_visible");
            let fonts = fonts.as_ref().expect("init ran");
            shell.render_in(
                slot.surface.canvas(),
                &crate::console::Viewport::plain(ctx.width, ctx.height),
                fonts,
                ctx.pad,
                ctx.pad_pref,
                ctx.pads,
            );
            {
                // Queue external sync vs FFmpeg's pump-thread submits (same queue).
                let _q = gpu.queue_lock.guard();
                gpu.context.flush_surface_with_texture_state(
                    &mut slot.surface,
                    &gpu::FlushInfo::default(),
                    Some(&skvk::mutable_texture_states::new_vulkan(
                        skvk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        gpu.queue_family_index,
                    )),
                );
                gpu.context.submit(None);
            }
            self.current = next;
            self.drawn = Drawn::default(); // stream chrome re-renders when it returns
            let now = Instant::now();
            self.console_at = Some(now);
            if let Some(r) = self.console_cost.add(now - drew, now) {
                tracing::debug!(
                    width = ctx.width,
                    height = ctx.height,
                    frames = r.frames,
                    mean_ms = %format_args!("{:.1}", r.mean_ms),
                    peak_ms = %format_args!("{:.1}", r.peak_ms),
                    "console frame cost"
                );
            }
            return Ok(Some(
                self.slots[next].as_ref().expect("just rendered").frame(),
            ));
        }
        // A cost window never spans a stream: it would report minutes for a few frames.
        self.console_at = None;
        self.console_cost = FrameCost::default();

        let banner_alpha = self.banner_alpha(ctx);
        let banner_step = (banner_alpha * 32.0).round() as u8;
        let resize_phase = self.resize_phase(ctx);
        // 120 steps/s: a ~16 ms frame hits a new step, so the spinner passes
        // the damage gate. `+ 1` keeps phase 0 nonzero so the first frame draws.
        let resize_step = resize_phase.map_or(0, |p| (p * 120.0) as u16 + 1);
        if let Some(facts) = ctx.ring {
            self.ring.set_facts(facts);
        }
        self.ring.tick();
        // Host actions ride the console command bus (same as the power rows).
        for cmd in self.ring.take_cmds() {
            if let Some(shell) = &self.shell {
                shell.send_cmd(cmd);
            }
        }
        let ring_key = self.ring.damage();
        if ctx.stats.is_none()
            && ctx.hint.is_none()
            && ctx.access.is_none()
            && ctx.notice.is_none()
            && !ctx.mic_muted
            && ctx.audio_mute.is_none()
            && banner_step == 0
            && resize_step == 0
            && ring_key == 0
        {
            self.drawn = Drawn::default(); // forget content so re-show re-renders
            return Ok(None);
        }
        // 1 %: fine enough that no real display scale rounds into another, coarse
        // enough that float noise on the same monitor cannot churn the damage gate.
        let scale = ctx.scale.clamp(0.5, 4.0);
        let want = Drawn {
            width: ctx.width,
            height: ctx.height,
            stats: ctx.stats.map(<[HudLine]>::to_vec),
            hint: ctx.hint.map(str::to_owned),
            access: ctx.access.map(str::to_owned),
            notice: ctx.notice.map(str::to_owned),
            mic_muted: ctx.mic_muted,
            audio_mute: ctx.audio_mute.unwrap_or_default().to_owned(),
            scale_pct: (scale * 100.0).round() as u16,
            banner_step,
            resize_step,
            ring: ring_key,
        };
        if want == self.drawn {
            return Ok(self.slots[self.current].as_ref().map(Slot::frame));
        }

        // Other slot: the presenter may still be sampling this one (one frame in flight).
        let next = 1 - self.current;
        self.ensure_slot(next, ctx.width, ctx.height)?;
        let gpu = self.gpu.as_mut().expect("init ran");
        let slot = self.slots[next].as_mut().expect("just ensured");

        let canvas = slot.surface.canvas();
        canvas.clear(Color4f::new(0.0, 0.0, 0.0, 0.0));
        // Size the face per drawer, don't scale the canvas: Skia hints at the
        // requested size. A magnified 14 px bitmap is mush.
        let font = self.font.as_ref().expect("init ran");
        // Scrim under OSD/hint so those stay legible.
        if let Some(phase) = resize_phase {
            draw_resize_scrim(canvas, font, ctx.width, ctx.height, phase, scale);
        }
        if let Some(stats) = &want.stats {
            draw_osd_panel(canvas, font, stats, ctx.width, scale);
        }
        // Top-right, stacked in a fixed order: never collides with the stats panel or the
        // bottom pill, even at stats Off.
        let mut row = 0;
        if want.mic_muted {
            draw_badge(canvas, font, "Microphone muted", ctx.width, row, scale);
            row += 1;
        }
        if !want.audio_mute.is_empty() {
            draw_badge(canvas, font, &want.audio_mute, ctx.width, row, scale);
            row += 1;
        }
        // Same corner as the badges (must survive stats Off); stacks under them.
        if let Some(access) = &want.access {
            draw_access_chip(canvas, font, access, ctx.width, row, scale);
        }
        // Access toast outranks the capture hint for its few seconds.
        if let Some(notice) = &want.notice {
            draw_hint_pill(canvas, font, notice, ctx.width, ctx.height, 1.0, scale);
        } else if let Some(hint) = &want.hint {
            draw_hint_pill(canvas, font, hint, ctx.width, ctx.height, 1.0, scale);
        } else if banner_step > 0 {
            // Leave/stats shortcuts, fading so they are discoverable without the OSD.
            if let Some(text) = &self.banner_text {
                draw_hint_pill(
                    canvas,
                    font,
                    text,
                    ctx.width,
                    ctx.height,
                    banner_alpha as f32,
                    scale,
                );
            }
        }

        // Ring on top. Needs console fonts, which `--connect` never loads.
        if ring_key != 0 {
            let dt = ring_dt(&mut self.ring_drawn_at);
            if let Some(fonts) = self.fonts.as_ref() {
                self.ring
                    .render(canvas, ctx.width, ctx.height, scale, fonts, dt);
                self.ring_k = f64::from(scale);
            }
        }

        // Flush on the shared queue, ending SHADER_READ_ONLY on our family
        // (the layout the composite samples). Lock: vs FFmpeg's pump submits.
        {
            let _q = gpu.queue_lock.guard();
            gpu.context.flush_surface_with_texture_state(
                &mut slot.surface,
                &gpu::FlushInfo::default(),
                Some(&skvk::mutable_texture_states::new_vulkan(
                    skvk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    gpu.queue_family_index,
                )),
            );
            gpu.context.submit(None);
        }

        self.current = next;
        self.drawn = want;
        Ok(Some(
            self.slots[next].as_ref().expect("just rendered").frame(),
        ))
    }
}

/// Seconds since the ring last drew (spring clock). Cap so a paused loop does not snap.
fn ring_dt(drawn_at: &mut Option<Instant>) -> f64 {
    let now = Instant::now();
    let dt = drawn_at
        .map_or(1.0 / 60.0, |t| now.duration_since(t).as_secs_f64())
        .min(0.1);
    *drawn_at = Some(now);
    dt
}

impl SkiaOverlay {
    /// Banner alpha 1→0 across the fade tail. Refresh the words while visible
    /// so a pad hot-plug updates the leave hint.
    fn banner_alpha(&mut self, ctx: &FrameCtx) -> f64 {
        let Some(since) = self.streaming_since else {
            self.banner_text = None;
            return 0.0;
        };
        let age = since.elapsed().as_secs_f64();
        if age >= BANNER_S {
            self.streaming_since = None;
            self.banner_text = None;
            return 0.0;
        }
        // The dial's opener leads: it is the one shortcut that reaches every other action, so a
        // reader who remembers only this line still has stats, mic and disconnect.
        self.banner_text = Some(if ctx.pad.is_some() {
            "Select + A quick actions · Hold L1 + R1 + Start + Select to leave · \
             Ctrl+Alt+Shift+S stats"
                .to_string()
        } else {
            "Ctrl+Alt+Shift+O quick actions · Ctrl+Alt+Shift+Q releases input · \
             Ctrl+Alt+Shift+D disconnects · Ctrl+Alt+Shift+S stats"
                .to_string()
        });
        ((BANNER_S - age) / BANNER_FADE_S).min(1.0)
    }

    /// Spinner phase (seconds since the scrim came up), or `None` if idle.
    /// Latch on the first `resizing` frame; clear when the flag drops so the
    /// next resize starts at zero.
    fn resize_phase(&mut self, ctx: &FrameCtx) -> Option<f64> {
        if !ctx.resizing {
            self.resizing_since = None;
            return None;
        }
        Some(
            self.resizing_since
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs_f64(),
        )
    }

    fn ensure_slot(&mut self, i: usize, width: u32, height: u32) -> Result<()> {
        if self.slots[i]
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height)
        {
            return Ok(());
        }
        let gpu = self.gpu.as_mut().expect("init ran");
        if let Some(old) = self.slots[i].take() {
            // SAFETY: the view belongs to the slot being replaced. Sampling of this
            // slot ended two presents ago (ring alternates; presenter waits its fence
            // before each record), so the GPU is done with it.
            unsafe { gpu.device.destroy_image_view(old.view, None) };
        }
        let info =
            skia_safe::ImageInfo::new_n32_premul((width.max(1) as i32, height.max(1) as i32), None);
        let mut surface = gpu::surfaces::render_target(
            &mut gpu.context,
            gpu::Budgeted::Yes,
            &info,
            None,
            SurfaceOrigin::TopLeft,
            None,
            false,
            None,
        )
        .context("Skia render-target surface")?;
        let texture = gpu::surfaces::get_backend_texture(
            &mut surface,
            skia_safe::surface::BackendHandleAccess::FlushRead,
        )
        .context("surface backend texture")?;
        let image_info = texture
            .vulkan_image_info()
            .context("backend texture is not Vulkan")?;
        let image = avk::Image::from_raw(*image_info.image() as u64);
        // SAFETY: a create call on the live device `gpu` owns, over a builder that is a local
        // outliving the call; `image` is the VkImage Skia just reported for this backend texture,
        // which the surface keeps alive. The returned view is owned by the slot stored below.
        let view = unsafe {
            gpu.device.create_image_view(
                &avk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(avk::ImageViewType::TYPE_2D)
                    .format(avk::Format::from_raw(image_info.format as i32))
                    .subresource_range(
                        avk::ImageSubresourceRange::default()
                            .aspect_mask(avk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .context("overlay image view")?;
        self.slots[i] = Some(Slot {
            surface,
            image,
            view,
            width,
            height,
        });
        Ok(())
    }
}

/// SDL scancode → [`Key`](crate::input::Key). `None` = unused; the run loop keeps it.
fn key_of(sc: sdl3::keyboard::Scancode) -> Option<crate::input::Key> {
    use crate::input::Key as K;
    use sdl3::keyboard::Scancode as S;
    Some(match sc {
        S::Left => K::Left,
        S::Right => K::Right,
        S::Up => K::Up,
        S::Down => K::Down,
        S::Return | S::KpEnter => K::Return,
        S::Space => K::Space,
        S::Escape => K::Escape,
        S::Backspace => K::Backspace,
        S::PageUp => K::PageUp,
        S::PageDown => K::PageDown,
        S::Tab => K::Tab,
        S::Y => K::Y,
        S::X => K::X,
        _ => return None,
    })
}

/// Chrome face at `scale`. `with_size` fails only on a nonsensical size (caller clamps);
/// the unscaled face is still better than no text.
fn chrome_font(font: &Font, scale: f32) -> Font {
    font.with_size(base::FONT_PX * scale)
        .unwrap_or_else(|| font.clone())
}

/// Shrink `scale` until `width_at_scale` (linear in scale) fits `budget`. DPI-up
/// is only an improvement while the result still fits: the capture hint is ~150
/// chars and already fills a 1280 px window at 100 %.
fn fit_scale(scale: f32, width_at_scale: f32, budget: f32) -> f32 {
    if width_at_scale > budget && width_at_scale > 0.0 {
        (scale * budget / width_at_scale).max(0.1)
    } else {
        scale
    }
}

/// Stats OSD: translucent rounded panel, top-left, one line each, painted by role.
fn draw_osd_panel(canvas: &Canvas, base_font: &Font, lines: &[HudLine], width: u32, scale: f32) {
    // Width is linear in scale; measure once, then fit so Detailed-tier lines stay in-window.
    let width_at = |s: f32| {
        let font = chrome_font(base_font, s);
        let widest = lines
            .iter()
            .map(|l| font.measure_str(&l.text, None).0)
            .fold(0.0f32, f32::max);
        widest + 2.0 * (base::OSD_PAD_X + base::OSD_MARGIN) * s
    };
    let scale = fit_scale(scale, width_at(scale), width as f32);
    let font = chrome_font(base_font, scale);

    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent + metrics.leading;
    let widest = lines
        .iter()
        .map(|l| font.measure_str(&l.text, None).0)
        .fold(0.0f32, f32::max);
    let (pad_x, pad_y) = (base::OSD_PAD_X * scale, base::OSD_PAD_Y * scale);
    let (x, y) = (base::OSD_MARGIN * scale, base::OSD_MARGIN * scale);
    let panel = Rect::from_xywh(
        x,
        y,
        widest + 2.0 * pad_x,
        line_h * lines.len() as f32 + 2.0 * pad_y,
    );
    let radius = base::OSD_RADIUS * scale;
    canvas.draw_rrect(
        RRect::new_rect_xy(panel, radius, radius),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62)),
    );
    for (i, line) in lines.iter().enumerate() {
        canvas.draw_str(
            &line.text,
            Point::new(x + pad_x, y + pad_y - metrics.ascent + line_h * i as f32),
            &font,
            &fill(role_color(line.role)),
        );
    }
}

/// Headline bright, breakdowns softer, asides dimmer, warnings in status red.
fn role_color(role: Role) -> Color4f {
    match role {
        Role::Primary => Color4f::new(1.0, 1.0, 1.0, 0.92),
        Role::Detail => Color4f::new(1.0, 1.0, 1.0, 0.78),
        Role::Muted => Color4f::new(1.0, 1.0, 1.0, 0.6),
        Role::Warn => crate::theme::ERROR,
    }
}

/// Standing badge (error-colour dot + words), top-right at `row`. Drawn from state, not
/// from the stats text, so it survives stats Off. Words: the runtime monospace may not ship
/// a mute glyph. Persistent, because what it reports does not go away on its own.
fn draw_badge(canvas: &Canvas, base_font: &Font, label: &str, width: u32, row: usize, scale: f32) {
    // Short; it fits any stream window, so take the display scale as-is.
    let font = &chrome_font(base_font, scale);
    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent;
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let dot_r = 4.0 * scale;
    let dot_gap = 8.0 * scale;
    let text_w = font.measure_str(label, None).0;
    let w = text_w + 2.0 * dot_r + dot_gap + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let margin = base::OSD_MARGIN * scale;
    let x = width as f32 - w - margin;
    let y = margin + row as f32 * (h + 8.0 * scale);
    canvas.draw_rrect(
        RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62)),
    );
    canvas.draw_circle(
        Point::new(x + pad_x + dot_r, y + h / 2.0),
        dot_r,
        &fill(crate::theme::ERROR),
    );
    canvas.draw_str(
        label,
        Point::new(
            x + pad_x + 2.0 * dot_r + dot_gap,
            y + pad_y - metrics.ascent,
        ),
        font,
        &fill(Color4f::new(1.0, 1.0, 1.0, 0.92)),
    );
}

/// Access chip: preset label + countdown, top-right, under whatever badges hold the
/// corner. Standing, like them: must stay readable at every stats tier including Off.
/// Omitted for a full-control permanent session (`None` from the run loop).
fn draw_access_chip(
    canvas: &Canvas,
    base_font: &Font,
    text: &str,
    width: u32,
    rows_above: usize,
    scale: f32,
) {
    let font = &chrome_font(base_font, scale);
    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent;
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let text_w = font.measure_str(text, None).0;
    let w = text_w + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let margin = base::OSD_MARGIN * scale;
    // One row per badge already in the corner (same height formula; a badge's dot fits
    // inside the shared line height).
    let y = margin + rows_above as f32 * (h + 8.0 * scale);
    let x = width as f32 - w - margin;
    canvas.draw_rrect(
        RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62)),
    );
    canvas.draw_str(
        text,
        Point::new(x + pad_x, y + pad_y - metrics.ascent),
        font,
        &fill(Color4f::new(1.0, 1.0, 1.0, 0.92)),
    );
}

/// Mid-stream resize cover: full-screen scrim + spinner + "Resizing…". The overlay
/// cannot sample the video to blur it, so an opaque scrim hides the stretched
/// in-between frame instead.
fn draw_resize_scrim(
    canvas: &Canvas,
    base_font: &Font,
    width: u32,
    height: u32,
    phase: f64,
    scale: f32,
) {
    let font = &chrome_font(base_font, scale);
    let (wf, hf) = (width as f32, height as f32);
    canvas.draw_rect(
        Rect::from_wh(wf, hf),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.55)),
    );
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let r = (f64::from(width.min(height)) * 0.045).clamp(16.0, 44.0);
    crate::theme::spinner(canvas, cx, cy - r, r, phase);
    let (_, metrics) = font.metrics();
    let label = "Resizing\u{2026}";
    let tw = font.measure_str(label, None).0;
    canvas.draw_str(
        label,
        Point::new((wf - tw) / 2.0, (cy + r * 0.9) as f32 - metrics.ascent),
        font,
        &fill(Color4f::new(1.0, 1.0, 1.0, 0.9)),
    );
}

/// Capture hint / start banner: centered pill near the bottom. `scale` is the
/// display UI scale (size already rides in `font`).
fn draw_hint_pill(
    canvas: &Canvas,
    base_font: &Font,
    text: &str,
    width: u32,
    height: u32,
    alpha: f32,
    scale: f32,
) {
    // Capture hint already fills most of a 1280 px window at 100 %; fit to the
    // window (4 % gutter) so 2× scale does not overrun both edges.
    let pill_w =
        |s: f32| chrome_font(base_font, s).measure_str(text, None).0 + 2.0 * base::PILL_PAD_X * s;
    let scale = fit_scale(scale, pill_w(scale), width as f32 * 0.96);
    let font = &chrome_font(base_font, scale);

    let (_, metrics) = font.metrics();
    let line_h = metrics.descent - metrics.ascent;
    let text_w = font.measure_str(text, None).0;
    let (pad_x, pad_y) = (base::PILL_PAD_X * scale, base::PILL_PAD_Y * scale);
    let w = text_w + 2.0 * pad_x;
    let h = line_h + 2.0 * pad_y;
    let x = (width as f32 - w) / 2.0;
    let y = height as f32 - h - base::PILL_BOTTOM * scale;
    canvas.draw_rrect(
        RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.62 * alpha)),
    );
    canvas.draw_str(
        text,
        Point::new(x + pad_x, y + pad_y - metrics.ascent),
        font,
        &fill(Color4f::new(1.0, 1.0, 1.0, 0.92 * alpha)),
    );
}
