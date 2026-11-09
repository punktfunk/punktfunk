//! The IDD-push display manager's touch points for a session: the connector-slot guard held
//! across `create`, the topology re-assert generation the stream loop follows, the capture
//! open over the manager's control device (a worker's for a display this host did not create),
//! and the remote encoder. `main.rs` carries the no-op twin of the guard, the generation and
//! the encoder for every other OS, so a session path reads the same on both; `crate::capture`
//! re-exports the opens at its own paths.

use crate::capture::{Capturer, OutputFormat, VirtualCaptureRequest};
use anyhow::{Context, Result};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const REJECTED_SLOT: &str = "pf-vdisplay refused this process's connector slot; see the log for the PUNKTFUNK_SEAT_DISPLAY_SLOT or reservation problem";

/// Reserve this client's connector slot before `create`, so a prior session on the same slot
/// is the only one preempted. A rejected slot means this host would drive a connector that
/// belongs to another one; refusing the session beats attaching to someone else's display.
/// `None` for every other capture backend. GameStream passes no identity and takes the
/// anonymous slot, which under a seats reservation resolves to this host's own connector.
pub(crate) fn setup_guard(
    capture: crate::session_plan::CaptureBackend,
    identity: Option<[u8; 32]>,
    size: (u32, u32),
    stop: &Arc<AtomicBool>,
) -> Result<Option<std::sync::MutexGuard<'static, ()>>> {
    if capture != crate::session_plan::CaptureBackend::IddPush {
        return Ok(None);
    }
    crate::vdisplay::manager::vdm()
        .begin_idd_setup(identity, size, stop.clone())
        .context(REJECTED_SLOT)
        .map(Some)
}

/// The v5 IddCx hardware-cursor channel is up: the client may draw the pointer itself.
pub(crate) fn hw_cursor_capable() -> bool {
    crate::vdisplay::manager::hw_cursor_capable()
}

/// The manager's topology re-assert generation. A stream loop that sees it move re-attaches.
pub(crate) fn topology_reassert_gen() -> u64 {
    crate::vdisplay::manager::topology_reassert_gen()
}

/// Windows `crate::capture::capture_virtual_output`: IDD-push on a display this host created,
/// a capture worker on one it did not.
pub(crate) fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    let VirtualCaptureRequest {
        output: want,
        // Linux-only (encoder-proved tiled offer); IDD-push negotiates no dmabuf modifiers.
        codec: _codec,
        encoder: _encoder,
        capture: _capture,
        // Linux-only (`SPA_META_Cursor`, pool depth). IDD-push has no such meta; hide is
        // CURSOR_SUPPRESSED.
        kwin: _kwin,
        gamescope: _gamescope,
        // The driver paces its own encodes (`pf_frame::pace`).
        stream_hz,
        screen,
        // gamescope is Linux-only.
        gamescope_cursor: _gamescope_cursor,
        mirrored,
    } = request;
    // A monitor this host did not create is captured by a worker; the driver has no part in it.
    if vout.ownership == crate::vdisplay::DisplayOwnership::External {
        return capture_external_output(vout, want, stream_hz, screen, mirrored);
    }
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay target not yet an active display path (activation failed — see the \
             virtual-display warnings above)"
        )
    })?;
    // Aim absolute input at THIS display: a physical monitor can share the desktop (Extend, or
    // an Exclusive isolate degraded to the keep-physicals fallback).
    crate::inject::aim_at_target(
        screen,
        pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id),
        vout.preferred_mode,
    );
    let pref = vout.preferred_mode;
    let keep = vout.keepalive;
    // Resolve the pf-vdisplay control device once and wrap its cursor IOCTLs for the
    // IDD-push capturer. This is the one host reach into `crate::vdisplay` the capturer
    // would otherwise make.
    let control = crate::vdisplay::manager::control_device_handle().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay control device not open (monitor not created via the manager?)"
        )
    })?;
    // Each closure clones the `Arc<ControlDevice>`, so the handle stays open for the closure's
    // life and closes when the manager retires it and the last session drops. An open control
    // handle vetoes the wake-from-sleep PnP cycle.

    // Presence of this closure opts the session into v5 cursor-channel delivery (capturer
    // creates CursorShm; driver declares the IddCx hardware cursor). An already-excluded target
    // gets one too — it is the shape source the pool's blend needs. Whether a CLIENT draws the
    // pointer is `want.hw_cursor`, passed separately: the channel alone does not say.
    let control_cursor = control.clone();
    let want_channel = want.hw_cursor || target.cursor_excluded;
    let cursor_sender: Option<pf_capture::CursorChannelSender> = want_channel.then(|| {
        std::sync::Arc::new(
            move |req: &pf_driver_proto::control::SetCursorChannelRequest| {
                crate::vdisplay::driver::send_cursor_channel(&control_cursor, req)
            },
        ) as pf_capture::CursorChannelSender
    });
    // The cursor render model (`IOCTL_SET_CURSOR_FORWARD`): `false` = the client draws no
    // pointer (capture model, UAC/Winlogon), so the driver blends the excluded one into its
    // frames. Built for every session: a channel-less reuse can still have a live cursor
    // worker from an earlier session. Never-declared targets answer NOT_FOUND, which the
    // capturer logs and ignores.
    let target_id = target.target_id;
    let control_forward = control.clone();
    let cursor_forward: Option<pf_capture::CursorForwardSender> = Some({
        std::sync::Arc::new(move |enable: bool| {
            let req = pf_driver_proto::control::SetCursorForwardRequest {
                target_id,
                enable: enable as u32,
            };
            crate::vdisplay::driver::send_cursor_forward(&control_forward, &req)
        }) as pf_capture::CursorForwardSender
    });
    // The driver's encoder opens and is driven over the same handle (`RemoteEncoder::open`).
    let control_open = control.clone();
    let set_encode: pf_capture::SetEncodeSender =
        std::sync::Arc::new(move |req: &pf_driver_proto::encode::SetEncodeRequest| {
            crate::vdisplay::driver::send_set_encode(&control_open, req)
        });
    let encode_ctl: pf_capture::EncodeCtlSender =
        std::sync::Arc::new(move |req: &pf_driver_proto::encode::EncodeCtlRequest| {
            crate::vdisplay::driver::send_encode_ctl(&control, req)
        });
    let opts = pf_capture::IddPushOpts {
        want_hdr: want.hdr,
        ten_bit_sdr: want.ten_bit_sdr,
        want_444: want.chroma_444,
        pyrowave: want.pyrowave,
        cursor_sender,
        cursor_forward,
        forwards_to_client: want.hw_cursor,
        set_encode,
        encode_ctl,
    };
    pf_capture::open_idd_push(target, pref, keep, opts)
        .map_err(|(e, _keep)| e.context("IDD-push capture open (no fallback)"))
}

/// Capture a monitor this session does not own: start a capture worker as the signed-in user
/// and open the monitor in it. The absolute-input aim moves to that monitor, as the IDD-push
/// open does for its own.
///
/// A `mirrored` session's worker gets the GPU scheduling class a session's encoder gets. A
/// viewer of another session's display keeps the default, below the driver that encodes for
/// its owner.
fn capture_external_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    stream_hz: u32,
    screen: u8,
    mirrored: bool,
) -> Result<Box<dyn Capturer>> {
    let target = vout
        .win_capture
        .clone()
        .context("the mirrored monitor carries no capture target")?;
    let key = pf_win_display::win_display::CcdTargetKey::new(target.adapter_luid, target.target_id);
    let hdr = pf_win_display::display_events::snapshot_or_query()
        .target(key)
        .and_then(|t| t.hdr)
        .unwrap_or(false);
    crate::inject::aim_at_target(screen, key, vout.preferred_mode);
    let worker = crate::windows::capture_worker::spawn().context("start the capture worker")?;
    let source = pf_capture::WgcSource {
        gdi_name: target.gdi_name,
        target_id: target.target_id,
        hdr,
    };
    pf_capture::open_wgc(
        worker,
        source,
        want.hdr,
        stream_hz,
        mirrored,
        vout.keepalive,
    )
    .context("WGC capture open (no fallback)")
}

/// Open a remote-encoder source's encoder (the driver's, or a capture worker's) at the depth the
/// display composes now, and record it under the name of what opened. The heap is sized from
/// the opening rate; ABR climbs past twice it eat the burst margin. `client_hdr` replaces the
/// capturer's HDR baseline, as the stream loop does, so the first IDR carries the client's panel.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_driver_encoder(
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn Capturer,
    size: (u32, u32),
    fps: u32,
    bitrate_bps: u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<Box<dyn crate::encode::Encoder>> {
    use crate::session_plan::EncoderBackend;
    use pf_driver_proto::encode::backend as be;
    // The plan's backend is the session's resolution; a rebuild reopens on it, never on the
    // Encoder row as it stands now.
    let backend = match plan.encoder {
        EncoderBackend::Nvenc => Some(be::NVENC),
        EncoderBackend::Amf => Some(be::AMF),
        EncoderBackend::Qsv => Some(be::QSV),
        EncoderBackend::MediaFoundation => Some(be::MEDIA_FOUNDATION),
        EncoderBackend::Software | EncoderBackend::PlatformAuto => None,
    };
    // The capturer reports HDR metadata exactly while the display composes FP16.
    let colour = capturer.colour();
    let (hdr, bit_depth) =
        crate::session_plan::live_depth(plan.hdr, colour.hdr_meta.is_some(), bit_depth);
    let params = pf_capture::DriverEncodeParams::for_session(pf_capture::SessionFacts {
        codec: plan.codec,
        chroma: plan.chroma,
        backend,
        hdr,
        bit_depth,
        full_range: plan.full_range(hdr, bit_depth),
        colour,
        client_hdr,
        size,
        fps,
        bitrate_bps,
        slices: plan.slices,
        wire_chunk: plan.wire_chunk,
        wire_seq_base,
    })?;
    let remote = capturer.remote_encoder().ok_or_else(|| {
        anyhow::anyhow!("driver encode: the capture source runs no remote encoder")
    })?;
    let enc = remote.open(&params)?;
    let label = remote.site().label(enc.telemetry().map(|t| t.backend));
    Ok(crate::encode::track_session(enc, label))
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Live IDD-push ring: a session-shaped capturer must deliver Source frames.
    /// Elevated console session; host service stopped.
    #[test]
    #[ignore = "live: needs the pf-vdisplay driver, a console session, the host service stopped"]
    fn live_idd_push_ring_delivers_source_frames() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,pf_capture=debug"))
            .with_test_writer()
            .try_init();
        let mut vd = crate::vdisplay::open(crate::vdisplay::Compositor::Windows)
            .expect("open the pf-vdisplay backend");
        let vout = vd
            .create(punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create a virtual display");
        let want = OutputFormat {
            gpu: true,
            hdr: false,
            ten_bit_sdr: false,
            sdr10_native: false,
            chroma_444: false,
            pyrowave: false,
            nv12_native: false,
            hw_cursor: false,
            full_range: true,
        };
        let mut cap = capture_virtual_output(
            vout,
            VirtualCaptureRequest {
                output: want,
                codec: None,
                encoder: crate::encode::resolve(),
                capture: crate::session_plan::CaptureBackend::IddPush,
                kwin: false,
                gamescope: false,
                stream_hz: 60,
                screen: 0,
                gamescope_cursor: false,
                mirrored: false,
            },
        )
        .expect("open the IDD-push capturer");
        // A static desktop composes nothing: walk the pointer so every tick has a new image.
        let (mut source, mut regen, mut repeat, mut errors) = (0u32, 0u32, 0u32, 0u32);
        // `PF_LIVE_RING_SECS` stretches the run (default 12 s) so slower behaviours — the
        // exclusive watchdog's breaker on a relight fight — have time to show in the log.
        let secs: u64 = std::env::var("PF_LIVE_RING_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12);
        let start = Instant::now();
        let mut x = 100i32;
        while start.elapsed() < Duration::from_secs(secs) {
            x = 100 + ((x - 100 + 7) % 600);
            // SAFETY: plain FFI; the pointer is parked on the operator's desktop for the test.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, 100 + x / 3);
            }
            if let Some(feed) = cap.feed() {
                feed.wait_arrival(Instant::now() + Duration::from_millis(40));
            }
            match cap.try_latest() {
                Ok(Some(f)) => match f.provenance.origin {
                    pf_frame::FrameOrigin::Source => source += 1,
                    _ => regen += 1,
                },
                Ok(None) => {
                    repeat += 1;
                    std::thread::sleep(Duration::from_millis(8));
                }
                Err(e) => {
                    errors += 1;
                    assert!(errors <= 3, "the ring keeps failing: {e:#}");
                }
            }
        }
        assert!(
            source >= 60,
            "expected a steady stream of NEW source frames from the ring, got {source} \
             (regen={regen} repeat={repeat} errors={errors} in {:?})",
            start.elapsed()
        );
        drop(cap);
        drop(vd);
    }

    /// Freeze `pid` for `hold` by debugger attach (every thread stops at the attach event and
    /// stays stopped until the SAME thread detaches), on a helper thread. `attached` flips once
    /// the freeze took; kill-on-exit is off so a test panic never takes the debuggee down.
    fn freeze_for(
        pid: u32,
        hold: Duration,
        attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        use windows::Win32::System::Diagnostics::Debug::{
            DebugActiveProcess, DebugActiveProcessStop, DebugSetProcessKillOnExit,
        };
        std::thread::spawn(move || {
            // SAFETY: plain debug-API FFI on a pid the caller owns for the test's life.
            unsafe {
                let _ = DebugSetProcessKillOnExit(false);
                if DebugActiveProcess(pid).is_err() {
                    return;
                }
                attached.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(hold);
                let _ = DebugActiveProcessStop(pid);
            }
        })
    }

    /// Live WUDFHost freeze: the same capturer must recover or end with a typed fault.
    /// Default hold 18 s (`PF_LIVE_FREEZE_SECS`). Elevated console; host service stopped.
    #[test]
    #[ignore = "live: freezes the pf-vdisplay WUDFHost for ~18 s; elevated console session, host service stopped"]
    fn live_frozen_driver_host_ends_the_plane_with_a_typed_fault() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,pf_capture=debug"))
            .with_test_writer()
            .try_init();
        let hold = Duration::from_secs(
            std::env::var("PF_LIVE_FREEZE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(18),
        );
        let mut vd = crate::vdisplay::open(crate::vdisplay::Compositor::Windows)
            .expect("open the pf-vdisplay backend");
        let vout = vd
            .create(punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            })
            .expect("create a virtual display");
        let pid = vout
            .win_capture
            .as_ref()
            .expect("the virtual display carries its capture target")
            .wudf_pid;
        assert_ne!(pid, 0, "the driver reported no WUDFHost pid");
        let want = OutputFormat {
            gpu: true,
            hdr: false,
            ten_bit_sdr: false,
            sdr10_native: false,
            chroma_444: false,
            pyrowave: false,
            nv12_native: false,
            hw_cursor: false,
            full_range: true,
        };
        let mut cap = capture_virtual_output(
            vout,
            VirtualCaptureRequest {
                output: want,
                codec: None,
                encoder: crate::encode::resolve(),
                capture: crate::session_plan::CaptureBackend::IddPush,
                kwin: false,
                gamescope: false,
                stream_hz: 60,
                screen: 0,
                gamescope_cursor: false,
                mirrored: false,
            },
        )
        .expect("open the IDD-push capturer");
        let mut x = 100i32;
        let mut step = |cap: &mut Box<dyn Capturer>| -> Result<bool> {
            x = 100 + ((x - 100 + 7) % 600);
            // SAFETY: plain FFI; the pointer is parked on the operator's desktop for the test.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, 100 + x / 3);
            }
            if let Some(feed) = cap.feed() {
                feed.wait_arrival(Instant::now() + Duration::from_millis(40));
            }
            match cap.try_latest()? {
                Some(f) => Ok(f.provenance.origin == pf_frame::FrameOrigin::Source),
                None => {
                    std::thread::sleep(Duration::from_millis(8));
                    Ok(false)
                }
            }
        };
        // Warm: the ring must be delivering before anything is broken.
        let warm_until = Instant::now() + Duration::from_secs(3);
        let mut warm = 0u32;
        while Instant::now() < warm_until {
            if step(&mut cap).expect("capture before the fault") {
                warm += 1;
            }
        }
        assert!(warm >= 10, "no steady source before the fault (got {warm})");
        // Attaching to WUDFHost (LocalService) needs it even from an elevated console.
        pf_frame::privilege::enable("SeDebugPrivilege").expect("enable SeDebugPrivilege");
        let attached = Arc::new(AtomicBool::new(false));
        let freezer = freeze_for(pid, hold, attached.clone());
        let t_freeze = Instant::now();
        while !attached.load(Ordering::SeqCst) && t_freeze.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            attached.load(Ordering::SeqCst),
            "debugger attach to WUDFHost pid {pid} did not take"
        );
        let (mut during, mut after, mut outage, mut error) = (0u32, 0u32, None, None);
        let mut first_after: Option<Duration> = None;
        while t_freeze.elapsed() < hold + Duration::from_secs(60) {
            match step(&mut cap) {
                Ok(true) if t_freeze.elapsed() < hold => during += 1,
                Ok(true) => {
                    after += 1;
                    first_after.get_or_insert(t_freeze.elapsed());
                }
                Ok(false) => {}
                Err(e) => {
                    error = Some(format!("{e:#}"));
                    break;
                }
            }
            let recovered = cap.events().into_iter().find_map(|e| match e {
                pf_capture::SourceEvent::RecoveredOutage(o) => Some(o),
                _ => None,
            });
            if let Some(o) = recovered {
                outage = Some(o);
                break;
            }
        }
        let ended = t_freeze.elapsed();
        let _ = freezer.join();
        // An in-flight slot or two can still drain after the attach; more means the worker ran.
        assert!(
            during <= 2,
            "a frozen worker cannot keep publishing: {during} source frames during the freeze \
             (after={after} first_after={first_after:?} outage={outage:?} error={error:?} \
             ended_after={ended:?})"
        );
        match (outage, &error) {
            (Some(outage), _) => {
                assert!(
                    outage >= Duration::from_secs(15),
                    "a recovered outage must span the stall floor, got {outage:?}"
                );
                assert!(after >= 3, "real source frames must resume after the thaw");
            }
            (None, Some(e)) => {
                // Death watch ("WUDFHost … exited") or `SourceStalled` ("no source frame for Ns").
                assert!(
                    e.contains("WUDFHost") || e.contains("no source frame"),
                    "the plane must end with a typed driver/source fault, got: {e}"
                );
            }
            (None, None) => panic!(
                "neither a recovery nor a typed end within {:?} — the stale plane is exactly \
                 what must never happen",
                hold + Duration::from_secs(60)
            ),
        }
        drop(cap);
        drop(vd);
    }
}
