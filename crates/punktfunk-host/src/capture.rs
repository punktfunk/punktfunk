//! Frame-capture facade over `pf-capture`.
//!
//! Re-exports the shared frame types and capturer traits at the historical
//! `crate::capture::*` paths. Host-only entry points — [`open_portal_monitor`],
//! [`capture_virtual_output`] — resolve [`pf_capture::ZeroCopyPolicy`] and, on
//! Windows, the driver-IOCTL senders so the capturer never reaches back into
//! encode or vdisplay.

use anyhow::Result;

pub use pf_frame::{CapturedFrame, OutputFormat};
// Named only by the GameStream media path and the Linux pyrowave plumbing below.
// Off both, a native-only Windows build would trip -D warnings.
#[cfg(any(target_os = "linux", feature = "gamestream"))]
pub use pf_frame::PixelFormat;
// `capturer_supports_hdr` is not re-exported: on Linux that name is the platform
// floor and would silently miss the gamescope arm. Use [`capturer_supports_hdr_for`].
#[cfg(feature = "gamestream")]
pub use pf_capture::FastSyntheticCapturer;
pub use pf_capture::{capturer_supports_444, Capturer, SyntheticCapturer};
#[cfg(target_os = "windows")]
pub use pf_capture::{dxgi, synthetic_nv12};

/// Encode-backend facts for a Linux capture session. Resolved here so pf-capture
/// never reaches `crate::encode` (that would recreate the capture→encode cycle).
#[cfg(target_os = "linux")]
fn zero_copy_policy(
    pyrowave_session: bool,
    native_nv12_session: bool,
) -> pf_capture::ZeroCopyPolicy {
    let backend_is_vaapi = crate::encode::linux_zero_copy_is_vaapi();
    // Raw-dmabuf passthrough serves PyroWave on any vendor: the wavelet encoder
    // imports the dmabuf on its own Vulkan device. The `PUNKTFUNK_ENCODER=pyrowave`
    // lab lever also flips `backend_is_vaapi`.
    #[cfg(feature = "pyrowave")]
    let pyrowave_session =
        pyrowave_session || pf_host_config::config().encoder_pref.as_str() == "pyrowave";
    #[cfg(not(feature = "pyrowave"))]
    let pyrowave_session = {
        let _ = pyrowave_session;
        false
    };
    #[cfg(feature = "pyrowave")]
    let pyrowave_modifiers = if pyrowave_session {
        // BGRx is the capture path's canonical packed-RGB; `drm_fourcc(Bgrx)` is always `Some`.
        pf_frame::drm_fourcc(PixelFormat::Bgrx)
            .map(crate::encode::pyrowave_capture_modifiers)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    #[cfg(not(feature = "pyrowave"))]
    let pyrowave_modifiers = Vec::new();
    pf_capture::ZeroCopyPolicy {
        backend_is_vaapi,
        backend_is_gpu: crate::encode::resolved_backend_is_gpu(),
        pyrowave_session,
        pyrowave_modifiers,
        native_nv12_session,
        // Only the direct-SDK NVENC backend takes a packed 10-bit PQ CUDA payload.
        // Without it HDR capture stays on the CPU path.
        hdr_cuda_ok: pf_encode::linux_hdr_cuda_ok(),
        nvenc_raw_dmabuf: pf_encode::linux_nvenc_raw_dmabuf_ok(),
    }
}

/// Live capturer for a client-sized monitor via the xdg ScreenCast portal.
/// Pass `want_hdr` only when the session negotiated HDR and the mirrored monitor
/// is in HDR mode ([`pf_capture::gnome_hdr_monitor_active`]). Pass
/// `want_metadata_cursor` only when the encode backend composites
/// `CapturedFrame::cursor`; otherwise the portal embeds the pointer and no
/// backend × cursor-mode pair streams cursorless.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    want_hdr: bool,
    want_metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    // RemoteDesktop-capable desktops (KWin/GNOME) inherit that grant headlessly.
    // wlroots/Sway has no RemoteDesktop portal, so use a plain ScreenCast session.
    let anchored = crate::inject::default_backend() == crate::inject::Backend::Libei;
    // Monitor mirrors never carry the native PyroWave plane (GameStream protocol).
    // Native NV12 stays off: this path does not resolve the codec, and GNOME/KWin
    // do not produce NV12 anyway.
    pf_capture::open_portal_monitor(
        anchored,
        want_hdr,
        want_metadata_cursor,
        zero_copy_policy(false, false),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor(
    _want_hdr: bool,
    _want_metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

/// Streamed head's mode for the pointer warp (`pf_inject::set_stream_extent`), in pixels.
/// A display mode never exceeds `u16`; anything that does is not one, so it publishes nothing.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn head_extent(mode: Option<(u32, u32, u32)>) -> Option<(u16, u16)> {
    let (w, h, _) = mode?;
    Some((u16::try_from(w).ok()?, u16::try_from(h).ok()?))
}

/// Capturer from an already-created [`crate::vdisplay::VirtualOutput`].
/// The compositor flags carry PipeWire producer contracts that node ids and
/// remote fds cannot reveal. The capturer owns the output keepalive.
#[cfg(target_os = "linux")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
    // The output's compositor is KWin, derived from the backend that created
    // `vout` (a pooled display only ever matches its own backend). KWin rewrites
    // `SPA_META_Cursor` on every buffer (id-0 is an authoritative hide), serves a pool
    // of `KWIN_POOL_MIN..=KWIN_POOL_MAX`, and paces delivery on a millisecond-rounded
    // timer unless offered no `maxFramerate` ceiling.
    kwin: bool,
    // Gamescope omits cursor metadata and exports LINEAR-only dmabufs.
    gamescope: bool,
) -> Result<Box<dyn Capturer>> {
    // Portal negotiates its own pixel format, so `want.gpu` gates GPU zero-copy
    // (this path is always the portal; `CaptureBackend` is Windows-only dispatch)
    // and `want.chroma_444` selects planar-YUV444 GPU convert. `gpu = false`
    // forces CPU mmap so the encoder gets CPU-resident RGB for YUV444P.

    // `want.hdr` offers 10-bit PQ/BT.2020. Handshake already resolved it through
    // [`capturer_supports_hdr_for`]; only gamescope off `pipewire-hdr` is HDR.

    // Aim absolute input at THIS head: EXTEND backends sit beside the operator's
    // screens. `None` (Mutter/gamescope) CLEARS a stale name, e.g. after a Game-Mode
    // switch Hyprland → gamescope has removed `PF-…`.
    crate::inject::set_stream_output(vout.output_name.clone().or(vout.input_output.clone()));
    crate::inject::set_stream_extent(head_extent(vout.preferred_mode));
    // Direct capture first where the compositor has it: the portal's re-request timer
    // halves the rate above ~140 Hz. GPU consumers only — this delivers dmabufs, and a
    // software encoder wants the portal's CPU pixels. Any failure falls through.
    if let (Some(name), true) = (
        vout.output_name.clone(),
        want.gpu && pf_capture::direct_capture(),
    ) {
        // `keepalive` must move exactly once: rebuild it for the portal on failure.
        match pf_capture::open_direct_output(
            name.clone(),
            Box::new(()),
            zero_copy_policy(want.pyrowave, want.nv12_native),
        ) {
            Ok(c) => {
                tracing::info!(output = %name, "capturing the compositor output directly");
                // The keepalive still has to outlive the capturer; hand it over now that
                // the session is known good.
                return Ok(Box::new(KeptAlive {
                    inner: c,
                    _keepalive: vout.keepalive,
                }));
            }
            Err(e) => tracing::info!(
                output = %name,
                reason = %format!("{e:#}"),
                "no direct capture on this compositor — using the ScreenCast portal"
            ),
        }
    }
    pf_capture::open_virtual_output(
        vout.remote_fd,
        vout.node_id,
        vout.preferred_mode,
        vout.keepalive,
        want.gpu,
        want.chroma_444,
        want.hdr,
        want.ten_bit_sdr,
        zero_copy_policy(want.pyrowave, want.nv12_native),
        vout.expect_exact_dims,
        kwin,
        gamescope,
        if kwin {
            pf_capture::KWIN_POOL_MIN
        } else {
            pf_capture::POOL_MIN
        },
        kwin.then_some(pf_capture::KWIN_POOL_MAX),
        kwin && pf_capture::unpaced_capture(),
    )
}

/// Keeps the compositor's output alive for a capturer that did not take it.
///
/// `pf-capture` owns the keepalive on the portal path; the direct path is opened before
/// the keepalive can be committed, so it rides here instead. Every trait call forwards.
#[cfg(target_os = "linux")]
struct KeptAlive {
    inner: Box<dyn Capturer>,
    /// Dropped after `inner`, releasing the output only once capture has stopped.
    _keepalive: Box<dyn Send>,
}

#[cfg(target_os = "linux")]
impl Capturer for KeptAlive {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        self.inner.next_frame()
    }
    fn next_frame_within(&mut self, b: std::time::Duration) -> Result<CapturedFrame> {
        self.inner.next_frame_within(b)
    }
    fn next_frame_within_provisional(&mut self, b: std::time::Duration) -> Result<CapturedFrame> {
        self.inner.next_frame_within_provisional(b)
    }
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.inner.try_latest()
    }
    fn supports_arrival_wait(&self) -> bool {
        self.inner.supports_arrival_wait()
    }
    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        self.inner.wait_arrival(deadline)
    }
    fn set_active(&mut self, active: bool) {
        self.inner.set_active(active)
    }
    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        self.inner.cursor()
    }
    fn attach_gamescope_cursor(&mut self, t: pf_capture::GamescopeCursorTargets) {
        self.inner.attach_gamescope_cursor(t)
    }
    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        self.inner.hdr_meta()
    }
    fn pipeline_depth(&self) -> usize {
        self.inner.pipeline_depth()
    }
}

/// Can the native-plane source this session will drive deliver 10-bit PQ/BT.2020?
/// Capture half of the punktfunk/1 bit-depth gate (`native::handshake`).
///
/// Must be truthful before spawn: `bit_depth` is decided before the display
/// exists, and PQ frames to an 8-bit encoder are a hard error (`pf-encode`
/// Linux encoder). `pf_capture::capturer_supports_hdr()` cannot answer this on
/// Linux — it depends on the resolved compositor and the installed gamescope.
///
/// Windows: IDD-push enables advanced colour, so the platform answer.
/// Linux + gamescope: host knob, `packaging/gamescope` 10-bit BT.2020/PQ, a
/// spawned (not attached-foreign) sub-mode, and no earlier virtual-output HDR
/// downgrade latched. Anything else on Linux is 8-bit; GNOME 50+ portal HDR is
/// the GameStream plane (`gamestream::host_hdr_capable` + live monitor probe).
pub fn capturer_supports_hdr_for(compositor: Option<crate::vdisplay::Compositor>) -> bool {
    #[cfg(target_os = "linux")]
    {
        if compositor == Some(crate::vdisplay::Compositor::Gamescope) {
            return pf_host_config::config().gamescope_hdr
                && pf_vdisplay::gamescope_hdr_available()
                && !pf_capture::hdr_capture_failed(pf_capture::HdrSource::VirtualOutput);
        }
    }
    let _ = compositor;
    pf_capture::capturer_supports_hdr()
}

#[cfg(target_os = "windows")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
    // Linux-only (`SPA_META_Cursor`, pool depth). IDD-push has no such meta; hide is
    // CURSOR_SUPPRESSED.
    _kwin: bool,
    _gamescope: bool,
) -> Result<Box<dyn Capturer>> {
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay target not yet an active display path (activation failed — see the \
             virtual-display warnings above)"
        )
    })?;
    // Aim the injectors' absolute mapping (pen/touch/abs-mouse) at THIS display: the wire
    // normalizes over the streamed frame, and mapping it over the whole virtual desktop is wrong
    // the moment a physical monitor shares the desktop (Extend topology, or an Exclusive isolate
    // degraded to the keep-physicals fallback) — the pen-offset field bug.
    crate::inject::set_stream_target(Some(pf_win_display::win_display::CcdTargetKey::new(
        target.adapter_luid,
        target.target_id,
    )));
    crate::inject::set_stream_extent(head_extent(vout.preferred_mode));
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
    // Each closure clones the `Arc<OwnedHandle>`, so the handle stays open for the closure's
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
                // SAFETY: the captured `control_cursor` Arc keeps the control handle open across
                // this call (`send_cursor_channel`'s precondition).
                unsafe {
                    crate::vdisplay::driver::send_cursor_channel(
                        windows::Win32::Foundation::HANDLE(
                            std::os::windows::io::AsRawHandle::as_raw_handle(&*control_cursor),
                        ),
                        req,
                    )
                }
            },
        ) as pf_capture::CursorChannelSender
    });
    // The cursor render model (`IOCTL_SET_CURSOR_FORWARD`): `false` = the client draws no
    // pointer (capture model, UAC/Winlogon), so the driver blends the excluded one into its
    // frames. Built for every session: a channel-less reuse can still have a live cursor
    // worker from an earlier session. Never-declared targets answer NOT_FOUND, which the
    // capturer logs and ignores.
    let target_id = target.target_id;
    let cursor_forward: Option<pf_capture::CursorForwardSender> = Some({
        std::sync::Arc::new(move |enable: bool| {
            let req = pf_driver_proto::control::SetCursorForwardRequest {
                target_id,
                enable: enable as u32,
            };
            // SAFETY: the captured `control` Arc keeps the control handle open across this call
            // (`send_cursor_forward`'s precondition).
            unsafe {
                crate::vdisplay::driver::send_cursor_forward(
                    windows::Win32::Foundation::HANDLE(
                        std::os::windows::io::AsRawHandle::as_raw_handle(&*control),
                    ),
                    &req,
                )
            }
        }) as pf_capture::CursorForwardSender
    });
    pf_capture::open_idd_push(
        target,
        pref,
        want.hdr,
        want.ten_bit_sdr,
        want.chroma_444,
        want.pyrowave,
        keep,
        cursor_sender,
        cursor_forward,
        want.hw_cursor,
    )
    .map_err(|(e, _keep)| e.context("IDD-push capture open (no fallback)"))
}

/// Open the in-driver encoder for an IDD-push session: the plan as the driver numbers it, the
/// resolved Windows backend ahead of any fallback rung, the two IOCTL senders over the
/// manager's control handle, and the `pf_gpu` session record. The heap is sized from the
/// opening rate; ABR climbs past twice it eat the burst margin. `client_hdr` replaces the
/// capturer's HDR baseline, as the stream loop does, so the first IDR carries the client's panel.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub fn open_driver_encoder(
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn Capturer,
    size: (u32, u32),
    fps: u32,
    bitrate_bps: u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<Box<dyn crate::encode::Encoder>> {
    use crate::encode::{Codec, WindowsBackend};
    let endpoint = capturer
        .driver_endpoint()
        .ok_or_else(|| anyhow::anyhow!("driver encode: the capture source is not IDD-push"))?;
    let control = crate::vdisplay::manager::control_device_handle().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay control device not open (monitor not created via the manager?)"
        )
    })?;
    let control_open = control.clone();
    let set_encode: pf_capture::SetEncodeSender =
        std::sync::Arc::new(move |req: &pf_driver_proto::encode::SetEncodeRequest| {
            // SAFETY: the captured Arc keeps the control handle open across this call
            // (`send_set_encode`'s precondition).
            unsafe {
                crate::vdisplay::driver::send_set_encode(
                    windows::Win32::Foundation::HANDLE(
                        std::os::windows::io::AsRawHandle::as_raw_handle(&*control_open),
                    ),
                    req,
                )
            }
        });
    let encode_ctl: pf_capture::EncodeCtlSender =
        std::sync::Arc::new(move |req: &pf_driver_proto::encode::EncodeCtlRequest| {
            // SAFETY: the captured Arc keeps the control handle open across this call
            // (`send_encode_ctl`'s precondition).
            unsafe {
                crate::vdisplay::driver::send_encode_ctl(
                    windows::Win32::Foundation::HANDLE(
                        std::os::windows::io::AsRawHandle::as_raw_handle(&*control),
                    ),
                    req,
                )
            }
        });
    use pf_driver_proto::encode::{backend as be, codec as cc};
    let backend = match plan.codec {
        Codec::PyroWave => be::PYROWAVE,
        _ => match crate::encode::windows_resolved_backend() {
            WindowsBackend::Nvenc => be::NVENC,
            WindowsBackend::Amf => be::AMF,
            WindowsBackend::Qsv => be::QSV,
            WindowsBackend::MediaFoundation => be::MEDIA_FOUNDATION,
            WindowsBackend::Software => anyhow::bail!(
                "driver encode: the resolved backend is software, which the driver cannot run"
            ),
        },
    };
    // Media Foundation is the second rung for an H.26x session a missing `amfrt64.dll` or a
    // declined native open would otherwise end, but only inside its 8-bit 4:2:0 ceiling: the
    // plan is already negotiated here, so a wider one would open and then refuse every frame.
    let mf_fits = !plan.hdr && !plan.chroma.is_444() && bit_depth <= 8;
    let fallback = u32::from(!matches!(backend, be::PYROWAVE | be::MEDIA_FOUNDATION) && mf_fits)
        * be::MEDIA_FOUNDATION;
    let params = pf_capture::DriverEncodeParams {
        codec: match plan.codec {
            Codec::H264 => cc::H264,
            Codec::H265 => cc::HEVC,
            Codec::Av1 => cc::AV1,
            Codec::PyroWave => cc::PYROWAVE,
        },
        chroma: u32::from(plan.chroma.is_444()),
        bit_depth: u32::from(bit_depth),
        width: size.0,
        height: size.1,
        fps,
        bitrate_kbps: (bitrate_bps / 1000).min(u64::from(u32::MAX)) as u32,
        hdr: plan.hdr,
        hdr_meta: capturer.hdr_meta().map(|m| client_hdr.unwrap_or(m)),
        wire_chunk_bytes: plan.wire_chunk.unwrap_or(0) as u32,
        backends: [backend, fallback, 0, 0],
        wire_seq_base,
    };
    let enc = pf_capture::open_driver_encoder(endpoint, &params, set_encode, encode_ctl)?;
    // The driver walks the preference list, so the record names what actually opened —
    // reading back the request's first choice would hide every fallback.
    let label = match enc.telemetry().map(|t| t.backend) {
        Some("nvenc") => "driver-nvenc",
        Some("amf") => "driver-amf",
        Some("qsv") => "driver-qsv",
        Some("pyrowave") => "driver-pyrowave",
        Some("mf") => "driver-mf",
        _ => "driver",
    };
    Ok(crate::encode::track_session(enc, label))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    _want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
    _kwin: bool,
    _gamescope: bool,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}

#[cfg(all(test, target_os = "windows"))]
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
            chroma_444: false,
            pyrowave: false,
            nv12_native: false,
            hw_cursor: false,
        };
        let mut cap = capture_virtual_output(
            vout,
            want,
            crate::session_plan::CaptureBackend::IddPush,
            false,
            false,
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
            if cap.supports_arrival_wait() {
                cap.wait_arrival(Instant::now() + Duration::from_millis(40));
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

    /// `SeDebugPrivilege` on our token: attaching to a service process (WUDFHost runs as
    /// LocalService) needs it even from an elevated console session.
    fn enable_debug_privilege() -> bool {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
        use windows::Win32::Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_DEBUG_NAME,
            SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: plain token FFI on our own process; the handle is closed here.
        unsafe {
            let mut tok = HANDLE::default();
            if OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut tok,
            )
            .is_err()
            {
                return false;
            }
            let mut luid = LUID::default();
            let ok = LookupPrivilegeValueW(PCWSTR::null(), SE_DEBUG_NAME, &mut luid).is_ok() && {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                AdjustTokenPrivileges(tok, false, Some(&tp), 0, None, None).is_ok()
            };
            let _ = CloseHandle(tok);
            ok
        }
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
            chroma_444: false,
            pyrowave: false,
            nv12_native: false,
            hw_cursor: false,
        };
        let mut cap = capture_virtual_output(
            vout,
            want,
            crate::session_plan::CaptureBackend::IddPush,
            false,
            false,
        )
        .expect("open the IDD-push capturer");
        let mut x = 100i32;
        let mut step = |cap: &mut Box<dyn Capturer>| -> Result<bool> {
            x = 100 + ((x - 100 + 7) % 600);
            // SAFETY: plain FFI; the pointer is parked on the operator's desktop for the test.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetCursorPos(x, 100 + x / 3);
            }
            if cap.supports_arrival_wait() {
                cap.wait_arrival(Instant::now() + Duration::from_millis(40));
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
        assert!(
            enable_debug_privilege(),
            "SeDebugPrivilege could not be enabled"
        );
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
            if let Some(o) = cap.take_recovered_outage() {
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
