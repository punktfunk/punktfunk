//! Host-side IDD-push capture: the driver encodes, so this side owns no pixels.
//!
//! The driver's swap-chain worker passes each composed frame through its encode
//! pool into a sealed AU section ([`driver_encode`]); this capturer owns the
//! session's display identity — mode, HDR, cursor channel, health — and reports
//! the driver's frame cadence as pixel-less [`CapturedFrame`]s so the stream loop
//! keeps its geometry and pacing. Driver:
//! `packaging/windows/drivers/pf-vdisplay/src/encode/`. Layout and status codes
//! live in [`pf_driver_proto`] — both sides `use` it, so drift is a compile error.

use super::dxgi::WinCaptureTarget;
use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use crate::cursor_witness::CursorWitness;
use anyhow::{bail, Context, Result};
use pf_win_display::open_wudfhost;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    DuplicateHandle, LocalFree, DUPLICATE_CLOSE_SOURCE, DUPLICATE_HANDLE_OPTIONS,
    DUPLICATE_SAME_ACCESS, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, POINT, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, WaitForSingleObject, PROCESS_SET_INFORMATION,
};
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

/// Map-only on the driver's section duplicate. No OWNER / `WRITE_DAC` / DELETE.
const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;
/// Driver only `SetEvent`s; host keeps `SYNCHRONIZE` on its own handle.
const EVENT_MODIFY_STATE: u32 = 0x0002;

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// `PUNKTFUNK_IDD_DIAG` — the gate for this capturer's full diagnostics: the micro-probe engine,
/// the DxgKrnl ETW session and the access-unit dump. All three are off in a normal session,
/// because standing fence/scanline/DWM traffic and an ETW session alter the very path a
/// disturbance report describes.
///
/// `1` puts the dump beside the driver log (`%SystemRoot%\Temp`); any other non-empty value is
/// the directory it goes in. Read once per process, so an env edited mid-session is stale.
pub(super) fn diag_dir() -> Option<&'static std::path::Path> {
    static DIAG: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    DIAG.get_or_init(|| {
        let raw = std::env::var("PUNKTFUNK_IDD_DIAG").ok()?;
        match raw.trim() {
            "" | "0" | "off" | "false" => None,
            "1" | "on" | "true" => {
                let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
                Some(std::path::PathBuf::from(root).join("Temp"))
            }
            path => Some(std::path::PathBuf::from(path)),
        }
    })
    .as_deref()
}

/// `PUNKTFUNK_IDD_FLOW` — the DxgKrnl ETW session alone, printing a present-flow line every ten
/// seconds. For a field host: [`diag_dir`] also dumps every access unit to disk. Read once per
/// process.
pub(super) fn flow_watch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("PUNKTFUNK_IDD_FLOW")
            .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "off" | "false"))
    })
}

/// File mapping + mapped view. Drop unmaps, then [`OwnedHandle`] closes.
/// Borrowers hold the pointer, so declare this before whatever borrows it.
struct MappedSection {
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

// SAFETY: `!Send` only through the view pointer. The mapping is process-wide, so its one
// owner may use, unmap and close it from any thread; the driver's concurrent writes arrive
// through the section's atomics. Not `Sync`.
unsafe impl Send for MappedSection {}

impl MappedSection {
    /// View base; valid only while this section lives.
    fn ptr<T>(&self) -> *mut T {
        self.view.Value as *mut T
    }
}

impl Drop for MappedSection {
    fn drop(&mut self) {
        // SAFETY: `view` is the live `MapViewOfFile` mapping; unmap before `handle` closes.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

// The frame-delivery endpoint: `try_consume` and the `Capturer` surface.
#[path = "idd_push/capturer.rs"]
mod capturer;
#[path = "idd_push/channel.rs"]
mod channel;
// Construction: adapter, HDR, cursor opt-in.
#[path = "idd_push/cursor.rs"]
mod cursor;
#[path = "idd_push/cursor_model.rs"]
mod cursor_model;
#[path = "idd_push/cursor_poll.rs"]
mod cursor_poll;
#[path = "idd_push/open.rs"]
mod open;
use cursor_model::deliver_cursor_channel;
#[path = "idd_push/descriptor.rs"]
mod descriptor;
#[path = "idd_push/display.rs"]
mod display;
// Stall reporting over `crate::stall_model`, plus DxgKrnl ETW and micro-probes as evidence.
#[path = "idd_push/dxgkrnl_etw.rs"]
mod dxgkrnl_etw;
#[path = "idd_push/probes.rs"]
mod probes;
#[path = "idd_push/stall.rs"]
mod stall;
// Live health classification + the staged-recovery ladder (immunity plan WP12/WP13).
#[path = "idd_push/recovery.rs"]
mod recovery;
// The capturer end of that ladder: the classifier inputs and the rungs it runs here.
#[path = "idd_push/health.rs"]
mod health;
// Full GPU clocks on NVIDIA for as long as the driver's NVENC session is open.
#[path = "idd_push/clock_boost.rs"]
mod clock_boost;
// In-driver encode: the AU section, `SET_ENCODE`, and the `Encoder` proxy over `ENCODE_CTL`.
#[path = "idd_push/driver_encode.rs"]
pub(crate) mod driver_encode;
use crate::stall_model::{StallEvidence, StallWatch};
use channel::ChannelBroker;
use descriptor::{DescriptorPoller, DisplayDescriptor};

/// The session's virtual display: its mode, its cursor channel, and its health.
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
    /// Monotonic count of NEW source images the driver reported (`FrameOrigin::Source` only).
    source_seq: u64,
    /// The driver's own source counter at the last delivery — the freshness test.
    driver_source_seq: u64,
    /// Geometry and format of the last delivery. A mode or depth change must reach the stream
    /// loop even over a desktop composing nothing: it rebuilds the encoder off the frame, and
    /// the in-place resize waits for one at the new size with the encoder untouched.
    delivered: Option<(u32, u32, PixelFormat)>,
    /// Health classifier + staged-recovery ladder over this capturer's clocks (WP12/WP13).
    recovery: recovery::Supervisor,
    /// A closed episode's measured outage, until the stream loop takes it (WP14).
    recovered_outage: Option<Duration>,
    /// A rung the stream loop's actuator runs, until it takes it (`take_pending_stage`).
    pending_stage: Option<pf_frame::recovery::Stage>,
    /// A typed end the ladder decided off the capture path; `try_consume` returns it next.
    pending_fault: Option<anyhow::Error>,
    /// The session encoder's clocks from the loop's last `observe_encoder` — the only view
    /// this side has of the driver's frame and access-unit progress.
    encoder: Option<pf_frame::health::EncoderTelemetry>,
    /// Handle-duplication into WUDFHost, and the driver-death probe.
    broker: ChannelBroker,
    /// Hardware-cursor shm (`Some` = delivered). The driver publishes into it and blends
    /// from it; this side reads it for the client's own pointer.
    cursor_shared: Option<cursor::CursorShared>,
    /// GDI overlay source while alive — full-fidelity shapes IddCx never delivers.
    cursor_poll: Option<cursor_poll::CursorPoller>,
    /// `IOCTL_SET_CURSOR_FORWARD`. A declared IddCx hardware cursor blocks the
    /// OS software-cursor path; [`Self::poll_secure_desktop`] stands it down at UAC/Winlogon.
    cursor_forward: Option<crate::CursorForwardSender>,
    /// Kept so a re-arrived monitor can be handed the cursor channel again. The driver's worker
    /// does not survive a re-arrival, and the composite render model has no other shape source.
    cursor_sender: Option<crate::CursorChannelSender>,
    /// Poller reports a secure input desktop and the declare is stood down.
    secure_active: bool,
    /// The client draws no pointer, so the driver blends the excluded one into what it encodes.
    composite_cursor: bool,
    /// No cursor channel, but the target still has an earlier session's hardware-cursor
    /// declare (`WinCaptureTarget::cursor_excluded`). Pins `composite_cursor` on.
    composite_forced: bool,
    /// [`Self::live_cursor`] fell back to shm. Independent serial namespaces — never unlatch.
    cursor_shm_latched: bool,
    /// HDR cursor match to desktop SDR white (vs 80 nits). 2.5 ≈ Windows default; stamped into
    /// the cursor section because session 0 cannot query it.
    sdr_white_scale: f32,
    /// The scale has been logged once; later lines only on change.
    sdr_white_logged: bool,
    width: u32,
    height: u32,
    /// Handshake advertised `VIDEO_CAP_HDR` (not merely 10-bit). Pins composition
    /// so an SDR client never gets in-band PQ.
    want_hdr: bool,
    /// 10-bit SDR: composed in SDR wide colour where Windows allows it (`want_wcg`), else the
    /// driver's encoder widens BGRA itself. `want_hdr` stays false either way.
    ten_bit_sdr: bool,
    /// Wide colour took at open, so the session keeps it pinned on; off pins it off.
    want_wcg: bool,
    /// Live `advanced_color_enabled`. A change re-opens the driver's encoder.
    display_hdr: bool,
    /// Live SDR wide colour: the driver reads FP16 for an SDR stream. A change re-opens the
    /// driver's encoder.
    display_wcg: bool,
    /// One-shot: the display refused the negotiated depth (poller is ~4 Hz).
    hdr_pin_warned: bool,
    /// Failed pin attempts. Past [`Self::HDR_PIN_EAGER`] retry every
    /// [`Self::HDR_PIN_RETRY_EVERY`]th sample — CCD write+query takes the session-global lock.
    hdr_pin_failures: u32,
    /// Full-chroma 4:4:4.
    want_444: bool,
    /// Wavelet session (`design/pyrowave-windows-host-zerocopy.md`).
    pyrowave: bool,
    /// Off-thread CCD snapshot; the capture loop never runs those queries inline.
    desc_poller: DescriptorPoller,
    /// Last consumed poller sequence (0 = none yet).
    desc_seq: u64,
    /// Two-strikes debounce: act only when a second consecutive sample agrees,
    /// so a topology re-probe blip never re-opens the encoder.
    pending_desc: Option<DisplayDescriptor>,
    /// The topology generation `pending_desc` was sampled under ([`poll_display_hdr`]).
    pending_desc_gen: u64,
    /// A presentation restart is in flight; if no frame resumes past the window,
    /// `try_consume` drops the session (recover-or-drop, no DDA).
    recovering_since: Option<Instant>,
    /// Last fresh driver frame. A dead WUDFHost and an idle desktop both stop
    /// advancing the driver's source counter.
    last_fresh: Instant,
    /// Last drain-worker progress: frames the pool TOOK plus frames it DROPPED, and when that
    /// total last moved. Takes alone freeze while the worker is healthy, so this is the clock
    /// the classifier reads ([`Self::recovery_tick`]).
    drain_seq: u64,
    last_drain: Instant,
    /// One 0 ms wait per second, and only while stale.
    last_liveness: Instant,
    /// Multi-hundred-ms DWM holes during active flow; warns when they turn metronomic.
    stall_watch: StallWatch,
    /// The stalest drain heartbeat (µs) seen since the last fresh frame.
    max_hb_age_us: u64,
    /// Damage witness for [`StallEvidence::cursor_moved_px`] and the recovery
    /// classifier. user32 only, never the display-config lock; the rule itself is
    /// [`crate::cursor_witness`].
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
