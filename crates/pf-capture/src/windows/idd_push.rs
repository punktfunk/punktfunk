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
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{w, PCWSTR, PWSTR};
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
    CreateEventW, GetCurrentProcess, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject,
    PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

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

/// `PUNKTFUNK_IDD_DIAG` — the one gate for this capturer's diagnostics: the micro-probe engine,
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

/// File mapping + mapped view. Drop unmaps, then [`OwnedHandle`] closes.
/// Borrowers hold the pointer, so declare this before whatever borrows it.
struct MappedSection {
    handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

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

/// Image path is `%SystemRoot%\System32\WUDFHost.exe` before duplicating
/// handles into `process`. `what` names the channel in the error.
///
/// Path only — not our UMDF host, and not authorization. Callers judge
/// sufficiency (`design/idd-push-security.md`). A token/session check
/// false-negatives: genuine host and spawned copy are both session 0
/// LocalService.
///
/// # Safety
/// `process` must carry `PROCESS_QUERY_LIMITED_INFORMATION`.
pub unsafe fn verify_is_wudfhost(process: HANDLE, wudf_pid: u32, what: &str) -> Result<()> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `process` carries QUERY_LIMITED; `buf`/`len` are a valid out-buffer.
    // On success `len` is the UTF-16 unit count written (no NUL).
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .with_context(|| format!("QueryFullProcessImageNameW on the {what} pid"))?;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let got = path.to_ascii_lowercase().replace('/', "\\");
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let expected = format!("{}\\system32\\wudfhost.exe", sysroot.to_ascii_lowercase());
    if got != expected {
        bail!(
            "{what} pid {wudf_pid} is not the system WUDFHost (image={path:?}, expected \
             {expected:?}) — refusing to duplicate the channel's handles into it (spoofed driver / \
             wrong devnode?)"
        );
    }
    Ok(())
}

/// Open `pid` as a handle-duplication target and prove it is the system WUDFHost.
///
/// The mask and the check travel together: `DUP_HANDLE` to place section handles,
/// `QUERY_LIMITED_INFORMATION` for the image-path proof, `SYNCHRONIZE` so the
/// retained handle doubles as the incumbent-liveness probe. `what` names the
/// channel in the error. Brokers diverge after this point — what they duplicate
/// and with which rights is theirs.
pub fn open_wudfhost(pid: u32, what: &str) -> Result<OwnedHandle> {
    if pid == 0 {
        bail!("no WUDFHost pid for the {what} sections");
    }
    // SAFETY: `pid` is a copy. The handle (`?`-checked) is owned solely here and moved into
    // `OwnedHandle` (single owner, closes on drop); `verify_is_wudfhost` borrows it for the
    // synchronous check and forms no lasting alias.
    unsafe {
        let h = OpenProcess(
            PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
        .with_context(|| format!("OpenProcess(PROCESS_DUP_HANDLE) on the {what} pid"))?;
        let process = OwnedHandle::from_raw_handle(h.0 as _);
        verify_is_wudfhost(HANDLE(process.as_raw_handle()), pid, what)?;
        Ok(process)
    }
}

// The frame-delivery endpoint: `try_consume` and the `Capturer` surface.
#[path = "idd_push/capturer.rs"]
mod capturer;
#[path = "idd_push/channel.rs"]
mod channel;
// Construction: adapter, HDR, cursor opt-in.
#[path = "idd_push/open.rs"]
mod open;
// Synthetic DWM compose kick — the first-frame lever on an idle desktop.
#[path = "idd_push/compose_kick.rs"]
mod compose_kick;
use compose_kick::kick_dwm_compose;
#[path = "idd_push/cursor.rs"]
mod cursor;
#[path = "idd_push/cursor_model.rs"]
mod cursor_model;
#[path = "idd_push/cursor_poll.rs"]
mod cursor_poll;
use cursor_model::deliver_cursor_channel;
#[path = "idd_push/descriptor.rs"]
mod descriptor;
#[path = "idd_push/display.rs"]
mod display;
// Stall reporting: the driver-clock verdict, plus DxgKrnl ETW and micro-probes as evidence.
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
// In-driver encode: the AU section, `SET_ENCODE`, and the `Encoder` proxy over `ENCODE_CTL`.
#[path = "idd_push/driver_encode.rs"]
pub(crate) mod driver_encode;
use channel::ChannelBroker;
use descriptor::{DescriptorPoller, DisplayDescriptor};
use stall::{StallEvidence, StallWatch};

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
    /// 10-bit SDR: the driver expands BGRA 8→10 into [`PixelFormat::Rgb10a2Sdr`]. Display
    /// colour is never touched — `want_hdr` stays false.
    ten_bit_sdr: bool,
    /// Live `advanced_color_enabled`. A change re-opens the driver's encoder.
    display_hdr: bool,
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
    /// Mid-session [`kick_dwm_compose`] (recovery window only).
    last_kick: Instant,
    /// Multi-hundred-ms DWM holes during active flow; warns when they turn metronomic.
    stall_watch: StallWatch,
    /// The stalest drain heartbeat (µs) seen since the last fresh frame.
    max_hb_age_us: u64,
    /// Damage witness for [`stall::StallEvidence::cursor_moved_px`] and the recovery
    /// classifier. user32 only, never the display-config lock; the rule itself is
    /// [`crate::cursor_witness`].
    cursor: CursorWitness,
    /// Micro-probe singleton; `None` unless [`diag_dir`] is on. A missing window reads as
    /// never-stalled, so a report never invents a leg.
    probes: Option<Arc<probes::ProbeEngine>>,
    /// DxgKrnl ETW; `None` unless [`diag_dir`] is on, or the session refused to start.
    etw: Option<Arc<dxgkrnl_etw::EtwWatch>>,
    /// `PowerRequestDisplayRequired` for this capturer's life: DWM composes nothing
    /// once the console goes dark. Waking an already-off display is the HID kick.
    _display_wake: Option<pf_frame::session_tuning::DisplayWakeRequest>,
    _keepalive: Box<dyn Send>,
}
// SAFETY: `!Send` only through the cursor section's mapped-view pointer. Created, used and
// dropped on the capture thread, and the driver's writes into that section arrive through its
// own seqlock. `Send` moves ownership with no concurrent access; we do not claim `Sync`.
unsafe impl Send for IddPushCapturer {}

#[cfg(test)]
mod tests {
    use super::stall::Stall;
    use super::*;

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

    /// Feed [`StallWatch`] at `offsets_ms`; metronome is non-damage-idle, as `report` feeds it.
    fn watch_run(offsets_ms: &[u64]) -> Vec<Option<(Stall, Option<Duration>)>> {
        let base = Instant::now();
        let mut w = StallWatch::new();
        offsets_ms
            .iter()
            .map(|ms| {
                let at = base + Duration::from_millis(*ms);
                w.note_fresh(at, None).map(|s| {
                    let period = w.cycle(at, false);
                    (s, period)
                })
            })
            .collect()
    }

    fn flow(out: &mut Vec<u64>, start_ms: u64, frames: u64) {
        out.extend((0..frames).map(|i| start_ms + i * 16));
    }

    #[test]
    fn stall_detected_after_active_flow() {
        // 20 frames of 60 fps, then a 300 ms hole — the resuming frame is a stall.
        let mut t = Vec::new();
        flow(&mut t, 0, 20); // last frame at 304 ms
        t.push(604);
        let out = watch_run(&t);
        assert!(out[..20].iter().all(Option::is_none));
        let (stall, period) = out[20].as_ref().expect("hole after active flow is a stall");
        assert_eq!(stall.gap.as_millis(), 300);
        assert!(period.is_none(), "one stall is not a cycle");
    }

    #[test]
    fn idle_desktop_gaps_are_not_stalls() {
        // ~530 ms caret blink: activity gate never opens.
        let t: Vec<u64> = (0..12).map(|i| i * 530).chain([20_000]).collect();
        assert!(watch_run(&t).iter().all(Option::is_none));
    }

    #[test]
    fn thirty_fps_content_still_qualifies_as_active() {
        // 33 ms cadence: 8 pre-gap frames span 231 ms ≤ ACTIVE_SPAN.
        let mut t: Vec<u64> = (0..10).map(|i| i * 33).collect(); // last at 297 ms
        t.push(497);
        let out = watch_run(&t);
        assert!(out[10].is_some(), "30 fps flow must pass the activity gate");
    }

    /// First degraded-stretch summary, checked after every frame like the capture loop.
    /// Every frame reports the same 40 ms present→arrival, so the folded tally is
    /// assertable without modelling which frames land inside the stretch.
    fn watch_recovery(offsets_ms: &[u64]) -> (StallWatch, Option<super::stall::Recovery>) {
        let base = Instant::now();
        let mut w = StallWatch::new();
        let mut recovery = None;
        for ms in offsets_ms {
            w.note_fresh(base + Duration::from_millis(*ms), Some(40));
            if let Some(r) = w.take_recovery() {
                recovery.get_or_insert(r);
            }
        }
        (w, recovery)
    }

    #[test]
    fn a_degraded_stretch_summarizes_on_recovery() {
        // ~2 fps phase (10×500 ms holes) after active flow: one summary for the stretch.
        let mut t = Vec::new();
        flow(&mut t, 0, 20); // last frame at 304 ms
        t.extend((1..=10).map(|i| 304 + i * 500)); // 804..5304: ten 500 ms holes
        t.extend((1..=12).map(|i| 5304 + i * 16)); // sustained flow is back
        let (_, r) = watch_recovery(&t);
        let r = r.expect("a multi-hole degraded stretch summarizes at recovery");
        assert_eq!(r.holes, 10);
        assert_eq!(r.hole_time.as_millis(), 5000);
        assert_eq!(r.worst.as_millis(), 500);
        assert_eq!(r.degraded.as_millis(), 5000);
        // Every stamped frame reported 40 ms, at least one per hole.
        assert_eq!(r.arrival_ms().as_deref(), Some("40/40/40"));
        assert!(r.arrival_n >= r.holes, "n={}", r.arrival_n);
    }

    #[test]
    fn a_single_stall_never_summarizes() {
        // One hole in healthy flow: its stall line covers it; a one-hole stretch must not summarize.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.push(604); // the lone 300 ms hole
        t.extend((1..=12).map(|i| 604 + i * 16));
        let (_, r) = watch_recovery(&t);
        assert!(
            r.is_none(),
            "single stall must not produce a stretch summary"
        );
    }

    #[test]
    fn a_reset_cut_stretch_still_summarizes() {
        // A reset clears flow history mid-stretch; holes before it must still surface.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.extend((1..=3).map(|i| 304 + i * 500));
        let (mut w, r) = watch_recovery(&t);
        assert!(r.is_none(), "stretch still open — no summary yet");
        w.reset();
        let r = w
            .take_recovery()
            .expect("reset closes and summarizes the open stretch");
        assert_eq!(r.holes, 3);
        assert_eq!(r.hole_time.as_millis(), 1500);
    }

    #[test]
    fn a_content_stop_closes_the_stretch_without_folding_the_pause_in() {
        // Two degraded holes, then a 20 s pause. Summary covers the stretch only.
        let mut t = Vec::new();
        flow(&mut t, 0, 20);
        t.extend([804, 1304, 21_304]);
        let (_, r) = watch_recovery(&t);
        let r = r.expect("the content stop closes the stretch");
        assert_eq!(r.holes, 2);
        assert_eq!(r.hole_time.as_millis(), 1000);
        assert_eq!(r.degraded.as_millis(), 1000);
    }

    #[test]
    fn metronomic_stalls_self_diagnose() {
        // ~300 ms DWM holes every 4 s in 60 fps flow. 5 cycles → 4 stalls; the 4th is the period.
        let mut t = Vec::new();
        for cycle in 0..5u64 {
            // ~3.7 s of flow, then the hole to the next cycle.
            flow(&mut t, cycle * 4_000, 232); // last frame at cycle*4000 + 3696
        }
        let out = watch_run(&t);
        let stalls: Vec<&(Stall, Option<Duration>)> = out.iter().flatten().collect();
        assert_eq!(stalls.len(), 4, "each cycle boundary is one stall");
        assert!(stalls[..3].iter().all(|(_, period)| period.is_none()));
        let period = stalls[3]
            .1
            .expect("the 4th evenly-spaced event completes the metronome streak");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.3,
            "period={period:?}"
        );
    }

    /// Same four evenly-spaced stalls as [`metronomic_stalls_self_diagnose`], one
    /// damage-idle: a hand/input pause is not display-disturbance evidence.
    #[test]
    fn damage_idle_stalls_do_not_feed_the_metronome() {
        let base = Instant::now();
        let mut w = StallWatch::new();
        let mut periods = Vec::new();
        for cycle in 0..5u64 {
            let mut t = Vec::new();
            flow(&mut t, cycle * 4_000, 232);
            for ms in t {
                let at = base + Duration::from_millis(ms);
                if let Some(_stall) = w.note_fresh(at, None) {
                    // 2nd stall is damage-idle (cursor still on a dwm-only desktop).
                    let damage_idle = periods.len() == 1;
                    periods.push(w.cycle(at, damage_idle));
                }
            }
        }
        assert_eq!(periods.len(), 4);
        assert!(
            periods.iter().all(Option::is_none),
            "a skipped beat must break the streak: {periods:?}"
        );
    }

    #[test]
    fn reset_swallows_the_restart_gap() {
        // Restart, then resume 800 ms later: not a stall; detection re-arms after.
        let base = Instant::now();
        let at = |ms: u64| base + Duration::from_millis(ms);
        let mut w = StallWatch::new();
        for i in 0..20u64 {
            assert!(w.note_fresh(at(i * 16), None).is_none());
        }
        w.reset();
        assert!(
            w.note_fresh(at(1_104), None).is_none(),
            "restart gap swallowed"
        );
        for i in 1..20u64 {
            assert!(w.note_fresh(at(1_104 + i * 16), None).is_none());
        }
        assert!(
            w.note_fresh(at(1_104 + 19 * 16 + 300), None).is_some(),
            "detection re-armed after the reset"
        );
    }

    /// Third stall in 60 s warns; quiet through 300 s re-warn spacing; re-arms after age-out.
    #[test]
    fn stall_rate_warn_window_and_rewarn() {
        let base = Instant::now();
        let at = |s: u64| base + Duration::from_secs(s);
        let mut w = StallWatch::new();
        assert_eq!(w.note_for_rate_warn(at(0)), None);
        assert_eq!(w.note_for_rate_warn(at(10)), None);
        assert_eq!(
            w.note_for_rate_warn(at(20)),
            Some(3),
            "third stall in 60 s warns"
        );
        assert_eq!(
            w.note_for_rate_warn(at(30)),
            None,
            "inside the re-warn spacing the arm stays quiet"
        );
        // Past the spacing: old entries aged out, so RATE_MIN_STALLS again then re-warns.
        assert_eq!(w.note_for_rate_warn(at(400)), None);
        assert_eq!(w.note_for_rate_warn(at(401)), None);
        assert_eq!(
            w.note_for_rate_warn(at(402)),
            Some(3),
            "re-warns after the spacing"
        );
    }

    /// [`stall::attribute`] verdict table: the drain heartbeat, then the cursor witness.
    #[test]
    fn stall_attribution_verdicts() {
        use super::stall::{attribute, StallVerdict};
        let verdict = |gap_ms: u64, hb_age_ms: Option<u64>, moved: Option<u32>| {
            attribute(
                Duration::from_millis(gap_ms),
                &StallEvidence {
                    max_heartbeat_age_ms: hb_age_ms,
                    probes: None,
                    etw: None,
                    etw_counts: None,
                    cursor_moved_px: moved,
                },
            )
        };
        // No encoder open yet: no heartbeat, no verdict.
        assert_eq!(verdict(300, None, None), StallVerdict::NoTelemetry);
        // Heartbeat silent for most of the hole → worker starved.
        assert_eq!(verdict(600, Some(400), None), StallVerdict::WorkerStalled);
        // ≤16 ms heartbeat; 200 ms silence on a 300 ms gap is under max(gap/2, 250 ms).
        assert_eq!(verdict(300, Some(200), None), StallVerdict::ComposeSilence);
        assert_eq!(
            verdict(300, Some(20), Some(312)),
            StallVerdict::ComposeSilence
        );
        // Long holes scale the bar: 900 ms silence on a 3 s gap is not half.
        assert_eq!(
            verdict(3_000, Some(900), None),
            StallVerdict::ComposeSilence
        );
        assert_eq!(
            verdict(3_000, Some(1_600), None),
            StallVerdict::WorkerStalled
        );
        // The cursor never moved through the hole: nothing was dirty.
        assert_eq!(verdict(600, Some(16), Some(0)), StallVerdict::DamageIdle);
        // A starved worker is never demoted by a still cursor.
        assert_eq!(
            verdict(600, Some(400), Some(0)),
            StallVerdict::WorkerStalled
        );
    }

    /// With the ETW leg on (`PUNKTFUNK_IDD_DIAG`), a game presenting through the hole keeps
    /// compose-silence even under a still cursor; dwm-only flow still demotes.
    #[test]
    fn a_present_witness_blocks_the_damage_idle_demotion() {
        use super::dxgkrnl_etw::EtwWindowCounts;
        use super::stall::{attribute, StallVerdict};
        let verdict = |dwm_only: bool| {
            attribute(
                Duration::from_millis(600),
                &StallEvidence {
                    max_heartbeat_age_ms: Some(16),
                    probes: None,
                    etw: None,
                    etw_counts: Some(EtwWindowCounts {
                        presents: 40,
                        queue_adds: 0,
                        present_history: true,
                        queue_history: true,
                        flow_dwm_only: dwm_only,
                    }),
                    cursor_moved_px: Some(0),
                },
            )
        };
        assert_eq!(verdict(false), StallVerdict::ComposeSilence);
        assert_eq!(verdict(true), StallVerdict::DamageIdle);
    }
}
