//! Presenter↔console-UI overlay contract: shared Vulkan handles, per-frame
//! scene facts, and the `Overlay` trait the run loop drives.
//!
//! The presenter composites at most one sampled premultiplied RGBA quad per
//! frame. The overlay (pf-console-ui, Skia) fills offscreen images on its own
//! damage-driven schedule. No Skia type crosses this crate. `frame()` returning
//! `None` records no quad.
//!
//! Pin: `Option<Box<dyn Overlay>>` from the session binary; `None` is the
//! Skia-free build.

use ash::vk;
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::SlotId;
use pf_client_core::shortcuts::{Action, Client, Device, Shortcuts};
use punktfunk_core::config::GamepadPref;

/// Presenter's Vulkan handles. Overlay allocations use this device and queue.
/// Valid for the presenter's lifetime; the run loop drops the overlay first.
pub struct SharedDevice {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
    /// Decode submits to `queue` from the pump thread. Overlay flush/submit
    /// holds this via [`pf_client_core::video::QueueLock::guard`].
    pub queue_lock: std::sync::Arc<pf_client_core::video::QueueLock>,
    /// Overlay function-table cap: min of [`crate::vk::INSTANCE_API_VERSION`]
    /// and the loader. Do not probe `vkEnumerateInstanceVersion` — extra
    /// `vkGetDeviceProcAddr` entry points above that are null.
    pub api_version: u32,
    /// This device decodes AV1 in hardware ([`pf_client_core::video::av1_advertised`]) —
    /// the same answer that gates the `CODEC_AV1` advertisement, so a settings UI on this
    /// overlay says what the Hello asks.
    pub av1_decode: bool,
}

/// Per-frame overlay scene. The run loop fills this from session state.
pub struct FrameCtx<'a> {
    /// Swapchain size in pixels. Overlay renders 1:1.
    pub width: u32,
    pub height: u32,
    /// The swapchain is 10-bit; the overlay draws in 10 too, or its gradients band.
    pub ten_bit: bool,
    /// Window display scale (`1.0` at 96 dpi / 100 %) × `PUNKTFUNK_OSD_SCALE`.
    /// Overlay chrome is physical pixels; multiply every metric by this. The
    /// run loop (`overlay_scale`) clamps it finite and > 0.
    pub scale: f32,
    /// Stats overlay lines, painted by role. `None` = overlay off.
    pub stats: Option<&'a [HudLine]>,
    /// The stats panel's corner, and its size on top of [`Self::scale`].
    pub stats_corner: punktfunk_core::hud::HudCorner,
    pub stats_scale: f32,
    /// Show the exit hint when a stream starts; the overlay owns its timing.
    pub exit_hint: bool,
    pub hint: Option<&'a str>,
    /// Access chip. `None` for a full-control permanent session.
    pub access: Option<&'a str>,
    /// Transient access toast. Occupies the hint slot while up; the run loop owns timing.
    pub notice: Option<&'a str>,
    /// The microphone just became muted (`true`) or open again
    /// ([`punktfunk_core::client::mic_mute_notice`]), by the player or by a saved mute at the
    /// session's start; `None` otherwise. Independent of the stats tier, so it still shows with
    /// chrome off.
    pub mic_notice: Option<bool>,
    /// The stream's mute sentence ([`punktfunk_core::client::audio_mute_notice`]); `None`
    /// while it is audible, or once a local mute has been read. A host mute stands.
    /// Independent of the stats tier.
    pub audio_mute: Option<&'a str>,
    /// Mid-stream Match-window resize in flight. Draw a full-screen scrim and spinner until
    /// the new-resolution frame arrives.
    pub resizing: bool,
    pub pad: Option<&'a str>,
    /// Resolved pad kind for button glyphs (PlayStation shapes vs ABXY).
    pub pad_pref: Option<GamepadPref>,
    pub pads: &'a [pf_client_core::gamepad::PadInfo],
    /// Quick-action ring facts (`design/touch-client-overlay.md`). `None` outside a stream.
    pub ring: Option<&'a RingFacts>,
    /// The shortcuts of this launch, for the hints that name them ([`leave_line`]).
    pub shortcuts: &'a Shortcuts,
    /// The window's display as a stream request: what a Native connect asks for.
    pub native: punktfunk_core::Mode,
}

/// Overlay image ready to composite: RGBA, premultiplied, already in
/// `SHADER_READ_ONLY_OPTIMAL`. A stale `width`×`height` during resize
/// stretches for one frame.
pub struct OverlayFrame {
    pub image: vk::Image,
    pub view: vk::ImageView,
    /// The image's format; the native Wayland lane copies it into a buffer of the same.
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    /// Every texel with any alpha lies in one of these, in the image's pixels. `None`: the
    /// image may hold ink anywhere.
    pub rects: Option<std::sync::Arc<[vk::Rect2D]>>,
}

impl OverlayFrame {
    /// The render area and scissors that blend this image over a swapchain of `extent`,
    /// stretched to fill it. Each rect grows by a texel, the linear sampler's reach, then
    /// scales and rounds out. `rects: None` is the whole extent; `None` is nothing to blend.
    pub fn scissors(&self, extent: vk::Extent2D) -> Option<(vk::Rect2D, Vec<vk::Rect2D>)> {
        let full = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        let Some(rects) = &self.rects else {
            return Some((full, vec![full]));
        };
        // [lo, lo + len) of `from` texels, widened by one, as a span of `to` pixels.
        let span = |lo: i32, len: u32, from: u32, to: u32| {
            let (from, to) = (u64::from(from.max(1)), u64::from(to));
            let start = (i64::from(lo) - 1).max(0) as u64;
            let end = (i64::from(lo) + i64::from(len) + 1).max(0) as u64;
            (
                (start * to / from).min(to) as u32,
                (end * to).div_ceil(from).min(to) as u32,
            )
        };
        let rect = |(x0, y0, x1, y1): (u32, u32, u32, u32)| vk::Rect2D {
            offset: vk::Offset2D {
                x: x0 as i32,
                y: y0 as i32,
            },
            extent: vk::Extent2D {
                width: x1 - x0,
                height: y1 - y0,
            },
        };
        let mut area = None;
        let mut scissors = Vec::with_capacity(rects.len());
        for r in rects.iter() {
            let (x0, x1) = span(r.offset.x, r.extent.width, self.width, extent.width);
            let (y0, y1) = span(r.offset.y, r.extent.height, self.height, extent.height);
            if x1 > x0 && y1 > y0 {
                scissors.push(rect((x0, y0, x1, y1)));
                let a = area.get_or_insert((x0, y0, x1, y1));
                *a = (a.0.min(x0), a.1.min(y0), a.2.max(x1), a.3.max(y1));
            }
        }
        Some((rect(area?), scissors))
    }
}

// Shared with the Android GL host; lives in `pf_client_core::console`
// (pf-console-ui sits above this crate). Re-exported so the run loop and the
// console keep the `pf_presenter::overlay::…` path.
pub use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};
pub use pf_client_core::ring::{RingCommand, RingFacts, RingInput};
pub use punktfunk_core::hud::{HudLine, Role};

/// How to leave a stream from `device`, in one line: its first End stream trigger, "Hold L1 +
/// R1 + Start + Select to leave". `None` when nothing on `device` ends the stream.
pub fn leave_line(shortcuts: &Shortcuts, device: Device) -> Option<String> {
    let end = Action::Slot(SlotId::EndStream);
    let b = shortcuts
        .bindings
        .iter()
        .find(|b| b.action == end && b.trigger.device() == device)?;
    let keys = b.trigger.chord(Client::Desktop);
    Some(match b.trigger.hold() {
        true => pf_l10n::client::notice_exit_hold(keys),
        false => pf_l10n::client::notice_exit_keys(keys),
    })
}

/// Console-UI side. The session binary passes `Option<Box<dyn Overlay>>`;
/// `None` is the Skia-free build.
pub trait Overlay {
    fn init(&mut self, shared: &SharedDevice) -> anyhow::Result<()>;

    /// Before capture. `true` consumes the event; do not forward.
    fn handle_event(&mut self, event: &sdl3::event::Event) -> bool;

    /// Browse-mode gamepad menu nav. The run loop drains the service channel.
    fn handle_menu(&mut self, _event: MenuEvent) -> Option<MenuPulse> {
        None
    }

    /// Pointer in swapchain pixels, before capture. `true` consumes; do not
    /// forward. Separate from [`Self::handle_event`]: window→pixel belongs to
    /// the run loop (it holds the window); the overlay never sees display scale.
    fn handle_pointer(&mut self, _input: PointerInput) -> bool {
        false
    }

    /// Drain one action raised by handled input. Once per loop iteration.
    fn take_action(&mut self) -> Option<OverlayAction> {
        None
    }

    /// Browse-mode session lifecycle. OSD/HUD ignore this.
    fn session_phase(&mut self, _phase: SessionPhase) {}

    /// The host no longer has the profile the connect named. Comes just before that
    /// connect's [`SessionPhase::Failed`].
    fn profile_gone(&mut self) {}

    /// The host wants the profile unlocked, or refused the unlock the connect sent. Comes just
    /// before that connect's [`SessionPhase::Failed`], whose message is the host's sentence.
    fn profile_locked(&mut self) {}

    /// A text field is being edited. The run loop starts and stops SDL text
    /// input (IME / `Event::TextInput`) from this.
    fn text_input_active(&self) -> bool {
        false
    }

    /// The field being edited is a password: the run loop starts SDL text input as a hidden
    /// password, or none where SDL would hand it to a keyboard that can't hide it.
    fn text_input_secure(&self) -> bool {
        false
    }

    /// The field being edited in swapchain pixels, and its caret's offset from the field's
    /// left. The run loop opens the input method's candidate window under it.
    fn text_input_area(&self) -> Option<(sdl3::rect::Rect, i32)> {
        None
    }

    /// Once per presenter iteration. Re-render (flush + `SHADER_READ_ONLY`) only
    /// on content or size change. The returned image stays untouched until the
    /// next `frame()` — one frame in flight; a ring of two is enough.
    fn frame(&mut self, ctx: &FrameCtx) -> anyhow::Result<Option<OverlayFrame>>;

    fn ring_input(&mut self, _input: RingInput) {}

    /// Ring is up: pointer and keys belong to it; touch fingers stop feeding
    /// the gesture engine.
    fn ring_open(&self) -> bool {
        false
    }

    /// The console is drawn over a live stream (a launch hold) and takes the pad
    /// as menu events, exactly as the ring does.
    fn holds_stream(&self) -> bool {
        false
    }

    /// A title this device's own host started is up and the console has let go: the loop
    /// asks for no frame, presents nothing, closes the pads and lets the screen saver run.
    fn playing_here(&self) -> bool {
        false
    }

    /// Once per pass in place of [`Self::frame`] while [`Self::playing_here`]: the console
    /// keeps up with the host and draws nothing.
    fn tick(&mut self) {}

    /// Drain one ring command. Each iteration while streaming.
    fn take_ring_command(&mut self) -> Option<RingCommand> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect((x, y, width, height): (i32, i32, u32, u32)) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D { width, height },
        }
    }

    fn tuple(r: vk::Rect2D) -> (i32, i32, u32, u32) {
        (r.offset.x, r.offset.y, r.extent.width, r.extent.height)
    }

    /// `(area, scissors)` for a `width`×`height` image holding `rects`, over a 1080p swapchain.
    #[allow(clippy::type_complexity)]
    fn blend(
        (width, height): (u32, u32),
        rects: Option<&[(i32, i32, u32, u32)]>,
    ) -> Option<((i32, i32, u32, u32), Vec<(i32, i32, u32, u32)>)> {
        let frame = OverlayFrame {
            image: vk::Image::null(),
            view: vk::ImageView::null(),
            format: vk::Format::R16G16B16A16_SFLOAT,
            width,
            height,
            rects: rects.map(|r| r.iter().copied().map(rect).collect()),
        };
        let (area, scissors) = frame.scissors(vk::Extent2D {
            width: 1920,
            height: 1080,
        })?;
        Some((tuple(area), scissors.into_iter().map(tuple).collect()))
    }

    /// Each rect grows by the sampler's texel, clamps to the swapchain and scales out when
    /// the image is stale-sized. No rects blends nothing; unbounded ink blends everything.
    #[test]
    fn scissors_hold_each_rect_and_the_sampler_reach() {
        let hud = [(100, 50, 200, 80), (1700, 1000, 220, 80)];
        let (area, scissors) = blend((1920, 1080), Some(&hud)).unwrap();
        assert_eq!(scissors, [(99, 49, 202, 82), (1699, 999, 221, 81)]);
        assert_eq!(area, (99, 49, 1821, 1031));
        let corner = [(-5, -5, 10, 10)];
        assert_eq!(
            blend((1920, 1080), Some(&corner)).unwrap().1,
            [(0, 0, 6, 6)]
        );
        let small = [(10, 10, 20, 20)];
        assert_eq!(
            blend((960, 540), Some(&small)).unwrap().1,
            [(18, 18, 44, 44)]
        );
        assert_eq!(
            blend((1280, 720), Some(&small)).unwrap().1,
            [(13, 13, 34, 34)]
        );
        assert_eq!(blend((1920, 1080), Some(&[])), None);
        let full = (0, 0, 1920, 1080);
        assert_eq!(blend((1280, 720), None), Some((full, vec![full])));
    }
}
