//! Skia overlay on the presenter's device: `DirectContext` over the shared
//! Vulkan handles, a two-slot offscreen ring (one frame in flight; the
//! presenter may still sample the previous image), and damage-driven redraws.
//!
//! Two personas on one `Overlay`: the console shell (home, library, settings,
//! pairing — redrawn every frame, at 30 Hz once idle) and stream chrome (stats OSD,
//! capture hint, auto-fading start banner).

use crate::console::{Console, ConsoleEntry, ConsoleHandles, FrameCost};
use crate::shell::{ConsoleOptions, Shell};
use crate::stream_chrome::{chrome_px, chrome_w, draw_hint_pill, draw_hud, Hud};
use crate::theme::{fill, Fonts, W};
use anyhow::{anyhow, Context as _, Result};
use ash::vk as avk;
use ash::vk::Handle as _;
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::trust::HudCorner;
use pf_presenter::overlay::{
    FrameCtx, HudLine, Overlay, OverlayAction, OverlayFrame, PointerInput, RingCommand, RingInput,
    SessionPhase, SharedDevice,
};
use skia_safe::gpu::vk as skvk;
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{Canvas, Color4f, Rect, Surface};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// An idle console redraws at most this often. 30 Hz keeps the aurora moving.
const IDLE_FRAME: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// Long enough to read the exit hint; the last `BANNER_FADE_S` fade out.
const BANNER_S: f64 = 6.0;
const BANNER_FADE_S: f64 = 0.6;

/// Offscreen target. Skia owns the image (freed with the surface); we own the view.
struct Slot {
    surface: Surface,
    image: avk::Image,
    view: avk::ImageView,
    format: avk::Format,
    width: u32,
    height: u32,
    ten_bit: bool,
    /// What the last render drew, as [`OverlayFrame::rects`]: `None` until a HUD-only render.
    rects: Option<Arc<[avk::Rect2D]>>,
}

impl Slot {
    fn frame(&self) -> OverlayFrame {
        OverlayFrame {
            image: self.image,
            view: self.view,
            format: self.format,
            width: self.width,
            height: self.height,
            rects: self.rects.clone(),
        }
    }
}

/// Damage key for the current ring slot — re-render only when this changes.
#[derive(PartialEq, Clone, Default)]
struct Drawn {
    width: u32,
    height: u32,
    stats: Option<Vec<HudLine>>,
    /// The stats panel's corner (`HudCorner::ALL` index) and size in percent.
    stats_corner: u8,
    stats_pct: u16,
    hint: Option<String>,
    /// Chip text. Countdown ticks once a minute, so a still chip is free per frame.
    access: Option<String>,
    /// Access toast; occupies the hint pill's slot while up.
    notice: Option<String>,
    /// The microphone's notice: muted, live, or none showing.
    mic_notice: Option<bool>,
    /// The speakers' mute badge: the sentence, or empty while none shows.
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

pub struct SkiaOverlay {
    /// Set in `init`. After a failed init the run loop drops us, so mid-session this is `Some`.
    gpu: Option<Gpu>,
    slots: [Option<Slot>; 2],
    /// Slot of the last returned frame; the next render takes the other.
    current: usize,
    drawn: Drawn,
    /// The console's faces, which the stream chrome draws in too.
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
            shell.device.av1_ok = shared.av1_decode;
        }

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
                repeat,
                ..
            } = event
            {
                use sdl3::keyboard::Mod;
                if self.ring.open()
                    && !keymod
                        .intersects(Mod::LCTRLMOD | Mod::RCTRLMOD | Mod::LALTMOD | Mod::RALTMOD)
                {
                    if let Some(key) = key_of(*sc) {
                        return self.ring.key(key, *repeat);
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
            sdl3::event::Event::TextEditing { text, .. } => {
                shell.preedit(text);
                true
            }
            _ => false,
        }
    }

    fn handle_menu(&mut self, event: MenuEvent) -> Option<MenuPulse> {
        if !self.console_visible() && self.ring.open() {
            let pulse = self.ring.menu(event);
            return match &self.shell {
                Some(s) => s.felt(pulse),
                None => pulse,
            };
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
        // The ring takes no pans or long presses, so its finger needs no clock.
        self.ring_touch
            .feed(input, self.ring_k, 0.0, |p| ring.pointer(p))
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

    fn text_input_secure(&self) -> bool {
        self.text_input_active()
            && (self.shell.as_ref())
                .and_then(Shell::edit_field)
                .is_some_and(|f| f.secure)
    }

    fn text_input_area(&self) -> Option<(sdl3::rect::Rect, i32)> {
        if !self.text_input_active() {
            return None;
        }
        let (field, caret) = self.shell.as_ref()?.ui.caret.get()?;
        let (w, h) = (field.width().round(), field.height().round());
        let at = sdl3::rect::Rect::new(field.left as i32, field.top as i32, w as u32, h as u32);
        Some((at, caret.round() as i32))
    }

    fn session_phase(&mut self, phase: SessionPhase) {
        // The exit hint's clock is the overlay's, in a plain `--connect` session too; the
        // phase itself is the console shell's.
        match &phase {
            SessionPhase::Streaming => self.streaming_since = Some(Instant::now()),
            SessionPhase::Ended(_) | SessionPhase::Reconnecting(_) => self.streaming_since = None,
            SessionPhase::Connecting | SessionPhase::AwaitingApproval | SessionPhase::Failed(_) => {
            }
        }
        // An open ring is the ended stream's. Left up, it masks the next stream's pad.
        if matches!(phase, SessionPhase::Ended(_) | SessionPhase::Failed(_)) {
            self.ring.input(RingInput::Cancel);
        }
        if let Some(shell) = &mut self.shell {
            shell.session_phase(phase);
        }
    }

    fn profile_gone(&mut self) {
        if let Some(shell) = &mut self.shell {
            shell.profile_gone();
        }
    }

    fn profile_locked(&mut self) {
        if let Some(shell) = &mut self.shell {
            shell.profile_locked();
        }
    }

    fn frame(&mut self, ctx: &FrameCtx) -> Result<Option<OverlayFrame>> {
        // The window may have moved to another display since the last frame.
        if let Some(shell) = &mut self.shell {
            shell.device.native_mode = Some(ctx.native);
        }
        // Full-screen and opaque; the aurora animates every frame. Idle, the slot on glass
        // is handed back until `IDLE_FRAME` passes, and the presenter skips its present.
        if self.console_visible() {
            let idle = self.shell.as_ref().is_some_and(Shell::idle)
                && self.console_at.is_some_and(|t| t.elapsed() < IDLE_FRAME);
            if let Some(slot) = self.slots[self.current].as_ref().filter(|s| {
                idle && (s.width, s.height, s.ten_bit) == (ctx.width, ctx.height, ctx.ten_bit)
            }) {
                return Ok(Some(slot.frame()));
            }
            let drew = Instant::now();
            let next = 1 - self.current;
            self.ensure_slot(next, ctx.width, ctx.height, ctx.ten_bit)?;
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
            slot.rects = None; // opaque, window-wide
            let budget = crate::shell::gpu_cache_budget(shell.gpu_cache_bytes, shell.last_k);
            set_cache_limit(&mut gpu.context, budget);
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
        // The console's covers stay cached through a stream only up to the unscaled budget.
        let base = (self.shell.as_ref())
            .map_or(crate::shell::DEFAULT_GPU_CACHE_BYTES, |s| s.gpu_cache_bytes);
        set_cache_limit(&mut self.gpu.as_mut().expect("init ran").context, base);

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
            if let Some(shell) = &mut self.shell {
                shell.send_cmd(cmd);
            }
        }
        let ring_key = self.ring.damage();
        if ctx.stats.is_none()
            && ctx.hint.is_none()
            && ctx.access.is_none()
            && ctx.notice.is_none()
            && ctx.mic_notice.is_none()
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
            stats_corner: HudCorner::ALL
                .iter()
                .position(|c| *c == ctx.stats_corner)
                .unwrap_or(0) as u8,
            stats_pct: (ctx.stats_scale * 100.0).round() as u16,
            hint: ctx.hint.map(str::to_owned),
            access: ctx.access.map(str::to_owned),
            notice: ctx.notice.map(str::to_owned),
            mic_notice: ctx.mic_notice,
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
        self.ensure_slot(next, ctx.width, ctx.height, ctx.ten_bit)?;
        let gpu = self.gpu.as_mut().expect("init ran");
        let slot = self.slots[next].as_mut().expect("just ensured");

        let canvas = slot.surface.canvas();
        canvas.clear(Color4f::new(0.0, 0.0, 0.0, 0.0));
        // Size the face per drawer, don't scale the canvas: Skia hints at the
        // requested size. A magnified 14 px bitmap is mush.
        let fonts = self.fonts.as_ref().expect("init ran");
        // The stream's chrome draws in the look the console last drew; a plain connect in
        // the default one.
        let fallback = crate::theme::Frame::default();
        let ui = self.shell.as_ref().map_or(&fallback, |s| &s.ui);
        // Scrim under OSD/hint so those stay legible.
        if let Some(phase) = resize_phase {
            draw_resize_scrim(canvas, ui, fonts, ctx.width, ctx.height, phase, scale);
        }
        let hud = Hud {
            stats: want.stats.as_deref(),
            stats_corner: ctx.stats_corner,
            stats_scale: ctx.stats_scale,
            mic_notice: want.mic_notice,
            audio_mute: &want.audio_mute,
            access: want.access.as_deref(),
            // Access toast outranks the capture hint for its few seconds.
            pill: want.notice.as_deref().or(want.hint.as_deref()),
        };
        let hud_rects = draw_hud(canvas, fonts, &hud, (ctx.width, ctx.height), scale);
        // The exit hint, fading out, where no other pill stands.
        let banner = hud.pill.is_none() && banner_step > 0;
        if let Some(text) = self.banner_text.as_ref().filter(|_| banner) {
            let alpha = banner_alpha as f32;
            draw_hint_pill(canvas, fonts, text, ctx.width, ctx.height, alpha, scale);
        }

        // Ring on top. Needs console fonts, which `--connect` never loads.
        if ring_key != 0 {
            let dt = ring_dt(&mut self.ring_drawn_at);
            if let Some(fonts) = self.fonts.as_ref() {
                self.ring
                    .render(canvas, ui, ctx.width, ctx.height, scale, fonts, dt);
                self.ring_k = f64::from(scale);
            }
        }
        // The scrim, the banner and the ring are blended over the whole window, the HUD
        // only where it drew.
        let hud_only = resize_phase.is_none() && !banner && ring_key == 0;
        slot.rects = hud_only.then(|| hud_rects.iter().map(vk_rect).collect());

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
    /// Exit-hint alpha 1→0 across the fade tail; 0 when the player turned the hint off.
    /// The words refresh while visible so a pad hot-plug swaps them.
    fn banner_alpha(&mut self, ctx: &FrameCtx) -> f64 {
        let Some(since) = self.streaming_since.filter(|_| ctx.exit_hint) else {
            self.banner_text = None;
            return 0.0;
        };
        let age = since.elapsed().as_secs_f64();
        if age >= BANNER_S {
            self.streaming_since = None;
            self.banner_text = None;
            return 0.0;
        }
        self.banner_text = exit_hint(ctx.shortcuts, ctx.pad.is_some());
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

    /// `ten_bit` follows the swapchain: a deep slot under an 8-bit blit gains nothing. F16,
    /// not 10/10/10/2: the overlay blends over video, and two bits of alpha step its fades.
    /// Unbudgeted: the slot lives as long as the surface, and in the budget it would only
    /// crowd out the covers (two 4K F16 slots are 133 MB).
    fn ensure_slot(&mut self, i: usize, width: u32, height: u32, ten_bit: bool) -> Result<()> {
        if self.slots[i]
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height && s.ten_bit == ten_bit)
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
        let info = skia_safe::ImageInfo::new(
            (width.max(1) as i32, height.max(1) as i32),
            if ten_bit {
                skia_safe::ColorType::RGBAF16
            } else {
                skia_safe::ColorType::N32
            },
            skia_safe::AlphaType::Premul,
            None,
        );
        let mut surface = gpu::surfaces::render_target(
            &mut gpu.context,
            gpu::Budgeted::No,
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
            format: avk::Format::from_raw(image_info.format as i32),
            width,
            height,
            ten_bit,
            rects: None,
        });
        Ok(())
    }
}

/// Skia's cache ceiling to `bytes`; a lower one purges down to it at once.
fn set_cache_limit(context: &mut DirectContext, bytes: usize) {
    if context.resource_cache_limit() != bytes {
        context.set_resource_cache_limit(bytes);
    }
}

/// A drawn rect as [`OverlayFrame::rects`] holds it.
fn vk_rect(r: &skia_safe::IRect) -> avk::Rect2D {
    avk::Rect2D {
        offset: avk::Offset2D {
            x: r.left,
            y: r.top,
        },
        extent: avk::Extent2D {
            width: r.width().max(0) as u32,
            height: r.height().max(0) as u32,
        },
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

/// How to leave a stream, in one line: the pad's End stream when a pad is forwarded, else the
/// keyboard's. `None` when neither ends it.
pub(crate) fn exit_hint(
    shortcuts: &pf_client_core::shortcuts::Shortcuts,
    pad: bool,
) -> Option<String> {
    use pf_client_core::shortcuts::Device;
    use pf_presenter::overlay::leave_line;
    pad.then(|| leave_line(shortcuts, Device::Pad))
        .flatten()
        .or_else(|| leave_line(shortcuts, Device::Keyboard))
}

/// Mid-stream resize cover: full-screen scrim + spinner + "Resizing…". The overlay
/// cannot sample the video to blur it, so an opaque scrim hides the stretched
/// in-between frame instead.
fn draw_resize_scrim(
    canvas: &Canvas,
    ui: &crate::theme::Frame,
    fonts: &Fonts,
    width: u32,
    height: u32,
    phase: f64,
    scale: f32,
) {
    let (wf, hf) = (width as f32, height as f32);
    canvas.draw_rect(
        Rect::from_wh(wf, hf),
        &fill(Color4f::new(0.0, 0.0, 0.0, 0.55)),
    );
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let r = (f64::from(width.min(height)) * 0.045).clamp(16.0, 44.0);
    crate::theme::spinner(canvas, ui, cx, cy - r, r, phase);
    let (_, metrics) = fonts.font(W::Medium, chrome_px(scale)).metrics();
    let label = pf_l10n::client::notice_resizing();
    let tw = chrome_w(fonts, label, scale);
    fonts.draw(
        canvas,
        label,
        f64::from((wf - tw) / 2.0),
        cy + r * 0.9 - f64::from(metrics.ascent),
        W::Medium,
        chrome_px(scale),
        Color4f::new(1.0, 1.0, 1.0, 0.9),
    );
}
