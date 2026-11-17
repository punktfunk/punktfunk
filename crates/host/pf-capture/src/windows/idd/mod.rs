//! Host-side IDD-push capture: the driver encodes into a sealed AU section, so this side owns
//! no pixels, only the session's display (mode, HDR, cursor channel) around the [`RemoteSource`]
//! that delivers and supervises the driver's frames. Both sides `use` [`pf_driver_proto`], so
//! layout drift is a compile error. The `(pure)` files build on every target, from `windows/`.
//!
//! - `open_plan.rs`: every step an open takes, decided before any of it runs (pure).
//! - `open.rs`: the plan run against the display, the WUDFHost broker and the remote source.
//! - `channel.rs`: the WUDFHost handle broker and [`IddTransport`], the source's driver edge.
//! - `capturer.rs`: `try_consume`, one tick of the capture loop, and the `Capturer` surface.
//! - `tick.rs`: the tick's order around the remote source's phases (pure).
//! - `depth_pin.rs`: which depth the display holds; when a change re-opens the encoder (pure).
//! - `display.rs`: mode, colour depth and the encoder's output format, over `DisplayColour`.
//! - `descriptor.rs`: the off-thread sampler of the target's HDR, wide colour and mode.
//! - `cursor.rs`: the hardware-cursor channel's section, delivered to the driver.
//! - `cursor_model.rs`: the cursor model run against the driver and the two shape sources.
//! - `cursor_poll.rs`: the GDI poller: the pointer's position, visibility and fallback shape.
//! - `health.rs`: this source's end of the recovery ladder: its inputs, its local rungs.
//! - `stall.rs`: the log lines for a hole the stall watch found, with coinciding OS events.
//! - `probes.rs`: the stall-attribution probes: GPU engine, DWM, composition wait, KMD, CPU.
//! - `dxgkrnl_etw.rs`: the real-time DxgKrnl and DXGI ETW watch a stall report cites.

use crate::knobs::Knobs;
use crate::windows::cursor_state::CursorModel;
use crate::windows::cursor_witness::CursorWitness;
use crate::windows::depth_pin::{DepthPin, DisplayDescriptor};
use crate::windows::remote::section::{MappedSection, SECTION_MAP_RW};
use crate::windows::remote::source::RemoteSource;
use crate::{CapturedFrame, Capturer, PixelFormat};
use anyhow::{bail, Context, Result};
use pf_frame::dxgi::WinCaptureTarget;
use pf_win_display::open_wudfhost;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, POINT};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SET_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

/// A session found SDR wide colour refused on this host's virtual display. Later handshakes
/// then stop offering a 10-bit SDR source without the widening opt-in.
static WCG_REFUSED: AtomicBool = AtomicBool::new(false);

/// A 10-bit SDR session can compose in SDR wide colour: Windows 11 24H2, and no refusal on
/// this host yet. Before the first session tries, the OS build alone answers.
pub(crate) fn wcg_available() -> bool {
    static BUILD: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    !WCG_REFUSED.load(Ordering::Relaxed)
        && *BUILD.get_or_init(pf_win_display::os_build) >= pf_win_display::WCG_MIN_BUILD
}

/// `Some` while [`Knobs::idd_diag`] runs this capturer's full diagnostics: the probes, the ETW
/// session and the encoder's access-unit dump, which goes in this directory.
pub(super) fn diag_dir() -> Option<&'static std::path::Path> {
    Knobs::process().idd_diag.as_deref()
}

/// [`Knobs::idd_flow`]: the DxgKrnl ETW session alone. For a field host: [`diag_dir`] also
/// dumps every access unit to disk.
pub(super) fn flow_watch() -> bool {
    Knobs::process().idd_flow
}

mod capturer;
mod channel;
mod cursor;
mod cursor_model;
mod cursor_poll;
mod open;
use cursor_model::deliver_cursor_channel;
mod descriptor;
mod display;
mod dxgkrnl_etw;
mod health;
mod probes;
mod stall;
use crate::windows::tick::Tick;
use channel::{ChannelBroker, IddTransport};
use descriptor::DescriptorPoller;

/// The session's virtual display: its mode, its cursor channel, and the remote source that
/// delivers and supervises it.
///
/// Frames carry no pixels — the driver encodes them — so a delivery is only the
/// news that the driver's pool took a new composed frame, which is what the
/// stream loop needs to pace and what the classifier needs as source progress.
pub struct IddPushCapturer {
    /// Driver-protocol target id (encoder open, cursor channel, logs). CCD path selection goes
    /// through `ccd` — a bare id is only unique per adapter.
    target_id: u32,
    /// Complete CCD identity (adapter LUID + target id) for every display-global helper.
    ccd: pf_win_display::win_display::CcdTargetKey,
    /// The delivery gate, the supervisor and the encoder's telemetry, over the driver's
    /// transport. The in-place resize re-opens the driver's encoder at the new size, then waits
    /// for a delivery its pool took.
    remote: RemoteSource<IddTransport>,
    /// Handle duplication into WUDFHost for the cursor channel; the transport shares it.
    broker: Arc<ChannelBroker>,
    /// Hardware-cursor shm (`Some` = delivered). The driver publishes into it and blends
    /// from it; this side reads it for the client's own pointer.
    cursor_shared: Option<cursor::CursorShared>,
    /// GDI pointer position and visibility while alive, and the shape for a cursor the
    /// section holds none of.
    cursor_poll: Option<cursor_poll::CursorPoller>,
    /// Whether the client's last shape came from the section; the first and every change
    /// are logged.
    shape_from_section: Option<bool>,
    /// `IOCTL_SET_CURSOR_FORWARD`. A declared IddCx hardware cursor blocks the
    /// OS software-cursor path; [`Self::poll_secure_desktop`] stands it down at UAC/Winlogon.
    cursor_forward: Option<crate::CursorForwardSender>,
    /// Kept so a re-arrived monitor can be handed the cursor channel again. The driver's worker
    /// does not survive a re-arrival, and the composite render model has no other shape source.
    cursor_sender: Option<crate::CursorChannelSender>,
    /// Who draws the pointer, whether the secure desktop has the declare stood down, and
    /// which source the client's shape is read from.
    cursor_model: CursorModel,
    /// HDR cursor match to desktop SDR white (vs 80 nits). 2.5 ≈ Windows default; stamped into
    /// the cursor section because session 0 cannot query it.
    sdr_white_scale: f32,
    /// The scale has been logged once; later lines only on change.
    sdr_white_logged: bool,
    width: u32,
    height: u32,
    /// The negotiated depth, what the display composes, and the debounce and retry over the
    /// poller's samples. A change of what it composes re-opens the driver's encoder.
    depth: DepthPin,
    /// Every advanced-colour, wide-colour, mode, secure-desktop and snapshot read; the
    /// descriptor poller shares it.
    colour: Arc<dyn pf_win_display::colour::DisplayColour>,
    /// Full-chroma 4:4:4.
    want_444: bool,
    /// Off-thread CCD snapshot; the capture loop never runs those queries inline.
    desc_poller: DescriptorPoller,
    /// What the tick keeps between the remote source's phases: the recover-or-drop window, the
    /// heartbeat maximum and the stall watch.
    tick: Tick,
    /// Damage witness for the stall report and the recovery classifier. user32 only, never the
    /// display-config lock; the rule itself is [`crate::windows::cursor_witness`].
    cursor: CursorWitness,
    /// Micro-probe singleton; `None` unless [`diag_dir`] is on. A missing window reads as
    /// never-stalled, so a report never invents a leg.
    probes: Option<Arc<probes::ProbeEngine>>,
    /// DxgKrnl ETW; `None` unless [`diag_dir`] or [`flow_watch`] is on, or the session refused
    /// to start.
    etw: Option<Arc<dxgkrnl_etw::EtwWatch>>,
    /// `PowerRequestDisplayRequired` for this capturer's life: DWM composes nothing
    /// once the console goes dark. It only keeps a lit display on; the monitor create wakes one.
    _display_wake: Option<pf_frame::session_tuning::DisplayWakeRequest>,
    _keepalive: Box<dyn Send>,
}

#[cfg(test)]
mod tests {
    /// The `CcdTargetKey` packing must equal `pf_frame::dxgi::pack_luid` — the capture target's
    /// `adapter_luid` (packed by pf-frame) is what pf-capture builds its CCD keys from, so a
    /// divergence would make every display-global helper miss its own target's paths. This crate
    /// is the lowest one that depends on both, which is why the assertion lives here.
    #[test]
    fn ccd_key_packing_matches_pf_frame_pack_luid() {
        for (low, high) in [
            (0u32, 0i32),
            (0xdead_beef, -2),
            (7, 0x7fff_ffff),
            (u32::MAX, -1),
        ] {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: low,
                HighPart: high,
            };
            assert_eq!(
                pf_win_display::win_display::CcdTargetKey::from_luid_parts(low, high, 1)
                    .adapter_luid,
                pf_frame::dxgi::pack_luid(luid),
                "packing diverged for LUID {high:#x}:{low:#x}"
            );
        }
    }
}
