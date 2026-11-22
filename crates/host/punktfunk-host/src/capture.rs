//! Frame-capture facade over `pf-capture`, at the historical `crate::capture::*` paths.
//!
//! The Linux opens derive [`pf_capture::ZeroCopyPolicy`] here from the session's
//! [`crate::encode::Resolved`], so capture never reaches back into encode. The Windows opens
//! live in `crate::windows::idd`, beside the display manager whose control device they send
//! through.

use anyhow::Result;

pub use pf_frame::{CapturedFrame, OutputFormat};
// Named only by the GameStream media path and the Linux modifier plumbing below.
// Off both, a native-only Windows build would trip -D warnings.
#[cfg(any(target_os = "linux", feature = "gamestream"))]
pub use pf_frame::PixelFormat;
// `capturer_supports_hdr` is not re-exported: on Linux that name is the platform
// floor and would silently miss the gamescope arm. Use [`capturer_supports_hdr_for`].
#[cfg(feature = "gamestream")]
pub use pf_capture::FastSyntheticCapturer;
pub use pf_capture::{capturer_supports_444, Capturer, SyntheticCapturer};
#[cfg(target_os = "windows")]
pub use pf_capture::{gpu_pref, synthetic_nv12};

/// What the session's encoder proved it can import, per capture fourcc. SDR
/// asks the packed-RGB pair, HDR the packed 10-bit pair; empty verdicts carry
/// no entry, so an unproved fourcc stays LINEAR-only. `None` (portal/diagnostic
/// opens) advertises no encoder-proved tiled modifiers.
#[cfg(target_os = "linux")]
fn encoder_modifiers(
    encoder: &crate::encode::Resolved,
    codec: Option<crate::encode::Codec>,
    bit_depth: u8,
    hdr: bool,
) -> Vec<(u32, Vec<u64>)> {
    codec.map_or_else(Vec::new, |codec| {
        let formats = if hdr {
            [PixelFormat::X2Bgr10, PixelFormat::X2Rgb10]
        } else {
            [PixelFormat::Bgrx, PixelFormat::Bgra]
        };
        formats
            .into_iter()
            .filter_map(|fmt| {
                let fourcc = pf_frame::drm_fourcc(fmt)?;
                let mods = encoder.capture_modifiers(codec, fourcc, bit_depth, hdr);
                (!mods.is_empty()).then_some((fourcc, mods))
            })
            .collect()
    })
}

/// Ask the box's fused-convert verdict and packed-RGB modifiers on a helper thread, so a first
/// session's probe runs while its display comes up. The session's own ask waits on that probe's
/// lock instead of starting a second. Once per process, and only where [`zero_copy_policy`]
/// would probe: direct-SDK NVENC off the VA plane (`hdr_cuda_ok`) with the raw lane on.
#[cfg(target_os = "linux")]
pub(crate) fn prewarm_fused_convert(encoder: &std::sync::Arc<crate::encode::Resolved>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !encoder.hdr_cuda_ok()
        || !pf_zerocopy::nvenc_raw_enabled()
        || STARTED.swap(true, Ordering::Relaxed)
    {
        return;
    }
    let encoder = encoder.clone();
    let spawned = std::thread::Builder::new()
        .name("pf-fused-probe".into())
        .spawn(move || {
            if encoder.nvenc_raw_dmabuf_ok() {
                for fourcc in [pf_zerocopy::drm::XR24, pf_zerocopy::drm::AR24] {
                    pf_zerocopy::fused_convert_modifiers(fourcc);
                }
            }
        });
    if let Err(e) = spawned {
        tracing::debug!(error = %e, "fused convert probe thread did not start");
    }
}

/// Encode route and per-fourcc import verdicts for one Linux capture session, read off the
/// session's `encoder` so pf-capture never reaches back into encode.
#[cfg(target_os = "linux")]
fn zero_copy_policy(
    encoder: &crate::encode::Resolved,
    pyrowave_session: bool,
    native_nv12_session: bool,
    codec: Option<crate::encode::Codec>,
    bit_depth: u8,
    hdr: bool,
) -> pf_capture::ZeroCopyPolicy {
    let backend_is_vaapi = encoder.zero_copy_is_vaapi();
    // Raw-dmabuf passthrough serves PyroWave on any vendor: the wavelet encoder
    // imports the dmabuf on its own Vulkan device.
    #[cfg(not(feature = "pyrowave"))]
    let pyrowave_session = {
        let _ = pyrowave_session;
        false
    };
    let modifier_codec = if pyrowave_session {
        Some(crate::encode::Codec::PyroWave)
    } else {
        codec
    };
    let encoder_modifiers = if backend_is_vaapi || pyrowave_session {
        encoder_modifiers(encoder, modifier_codec, bit_depth, hdr)
    } else {
        Vec::new()
    };
    pf_capture::ZeroCopyPolicy {
        backend_is_vaapi,
        backend_is_gpu: encoder.is_gpu(),
        pyrowave_session,
        native_nv12_session,
        // Only the direct-SDK NVENC backend takes a packed 10-bit PQ CUDA payload.
        // Without it HDR capture stays on the CPU path.
        hdr_cuda_ok: encoder.hdr_cuda_ok(),
        nvenc_raw_dmabuf: encoder.nvenc_raw_dmabuf_ok(),
        gamescope_tiled: false,
        encoder_modifiers,
        planar_modifiers: None,
    }
}

/// Live capturer for a client-sized monitor via the xdg ScreenCast portal.
/// Pass `want_hdr` only when the session negotiated HDR and the mirrored monitor
/// is in HDR mode ([`pf_capture::gnome_hdr_monitor_active`]). Pass
/// `want_metadata_cursor` only when the encode backend composites
/// `CapturedFrame::cursor`; otherwise the portal embeds the pointer and no
/// backend × cursor-mode pair streams cursorless. `full_range` is the range the encoder
/// signals. `encoder` is the session's resolution.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    want_hdr: bool,
    want_metadata_cursor: bool,
    full_range: bool,
    encoder: &crate::encode::Resolved,
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
        full_range,
        zero_copy_policy(encoder, false, false, None, 8, want_hdr),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor(
    _want_hdr: bool,
    _want_metadata_cursor: bool,
    _full_range: bool,
    _encoder: &crate::encode::Resolved,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

/// One [`capture_virtual_output`] request, shared by every platform
/// implementation so the facade takes two arguments. `codec` and `encoder` are the session's
/// codec and encode resolution — Linux reads them for the zero-copy policy and the gamescope
/// tiled offer, other platforms ignore them; `encoder` is owned because the direct-capture
/// policy runs lazily. `kwin`/`gamescope` carry PipeWire producer contracts node ids and
/// remote fds cannot reveal. `stream_hz` is the wire rate, which a display created at a
/// multiple of it cannot tell the capture. `screen` is the session's screen the output shows,
/// `0` the first: absolute input for it aims at this head. `gamescope_cursor` reads the
/// pointer from the output's nested Xwaylands (Linux; ignored elsewhere). `mirrored` is a
/// session that streams a host monitor, whose capture worker encodes for it (Windows).
/// `framing` is the encoder's: a macOS mirror's stream scales the head to its fitted size.
pub(crate) struct VirtualCaptureRequest {
    pub output: OutputFormat,
    pub codec: Option<crate::encode::Codec>,
    pub encoder: std::sync::Arc<crate::encode::Resolved>,
    pub capture: crate::session_plan::CaptureBackend,
    pub kwin: bool,
    pub gamescope: bool,
    pub stream_hz: u32,
    pub screen: u8,
    pub gamescope_cursor: bool,
    pub mirrored: bool,
    #[cfg(target_os = "macos")]
    pub framing: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
}

/// A live output's metadata without its keepalive: what [`capture_virtual_output`] needs to
/// attach a second time, once the old capturer hands the keepalive back.
#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(crate) struct OutputLease {
    node_id: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    refresh_mhz: Option<u32>,
    ownership: pf_vdisplay::DisplayOwnership,
    pool_gen: Option<u64>,
    output_name: Option<String>,
    input_output: Option<String>,
    seat: Option<String>,
    pid: Option<u32>,
}

#[cfg(target_os = "linux")]
impl OutputLease {
    /// `None` on the portal path: its remote fd cannot be re-derived from the metadata.
    pub(crate) fn of(vout: &crate::vdisplay::VirtualOutput) -> Option<OutputLease> {
        vout.remote_fd.is_none().then(|| OutputLease {
            node_id: vout.node_id,
            preferred_mode: vout.preferred_mode,
            refresh_mhz: vout.refresh_mhz,
            ownership: vout.ownership,
            pool_gen: vout.pool_gen,
            output_name: vout.output_name.clone(),
            input_output: vout.input_output.clone(),
            seat: vout.seat.clone(),
            pid: vout.pid,
        })
    }

    /// The gamescope instance this output belongs to; `None` for the box's own session.
    pub(crate) fn seat(&self) -> Option<&str> {
        self.seat.as_deref()
    }

    /// The output as a fresh capture sees it. Never a birth-size gate: the output already
    /// sits at its mode.
    pub(crate) fn into_output(self, keepalive: Box<dyn Send>) -> crate::vdisplay::VirtualOutput {
        crate::vdisplay::VirtualOutput {
            node_id: self.node_id,
            remote_fd: None,
            preferred_mode: self.preferred_mode,
            refresh_mhz: self.refresh_mhz,
            keepalive,
            ownership: self.ownership,
            reused_gen: None,
            pool_gen: self.pool_gen,
            expect_exact_dims: false,
            output_name: self.output_name,
            input_output: self.input_output,
            seat: self.seat,
            pid: self.pid,
        }
    }
}

/// Capturer from an already-created [`crate::vdisplay::VirtualOutput`], which it owns. A
/// named output is offered to direct capture first; `pf_capture::open_virtual_output` picks.
/// Direct capture probes its consumer. On raw-dmabuf passthrough, PipeWire offers the
/// encoder-proved tiled modifiers to level-21 gamescope, to any other producer's VAAPI encode,
/// and to non-gamescope PyroWave; older gamescope keeps its LINEAR offer.
/// The YUV it asks for or converts takes the session's range.
#[cfg(target_os = "linux")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    let VirtualCaptureRequest {
        output: want,
        codec,
        encoder,
        capture: _capture,
        kwin,
        gamescope,
        stream_hz,
        screen,
        gamescope_cursor,
        // The portal path has no capture worker.
        mirrored: _mirrored,
    } = request;
    // `want.hdr` was resolved by [`capturer_supports_hdr_for`] before the Welcome.
    // Aim absolute input at THIS head: EXTEND backends sit beside the operator's
    // screens. `None` (Mutter/gamescope) CLEARS a stale name, e.g. after a Game-Mode
    // switch Hyprland → gamescope has removed `PF-…`.
    crate::inject::aim_at_head(
        screen,
        vout.output_name.clone().or(vout.input_output.clone()),
        vout.preferred_mode,
    );
    // The encoder modifier probe keys on bit depth: HDR and 10-bit SDR both ride
    // the packed 10-bit fourccs.
    let bit_depth = if want.hdr || want.ten_bit_sdr { 10 } else { 8 };
    let gamescope_tiled = gamescope && pf_vdisplay::gamescope_tiled_capture(None);
    let planar_tiled = gamescope && pf_vdisplay::gamescope_tiled_planar_capture(None);
    // NVENC copies a producer's planar frame; an older gamescope composites it through system
    // RAM, which costs more than the conversion it saves. VAAPI keeps its own rule.
    let nv12_native = want.nv12_native
        && (!gamescope
            || encoder.zero_copy_is_vaapi()
            || pf_vdisplay::gamescope_planar_capture(None));
    // A compositor on the encoder's GPU allocates what the encoder proved it imports. Left
    // on LINEAR it packs rows a VA driver reads at a rounded pitch.
    let modifier_codec = if gamescope {
        gamescope_tiled.then_some(codec).flatten()
    } else if want.pyrowave || encoder.zero_copy_is_vaapi() {
        codec
    } else {
        None
    };
    // Direct capture's modifier lists come from the session codec, PipeWire's from
    // `modifier_codec`.
    let (pyrowave, hdr) = (want.pyrowave, want.hdr);
    let direct_encoder = encoder.clone();
    let direct = vout
        .output_name
        .clone()
        .map(|output_name| pf_capture::DirectOutput {
            output_name,
            policy: Box::new(move || {
                zero_copy_policy(
                    &direct_encoder,
                    pyrowave,
                    nv12_native,
                    codec,
                    bit_depth,
                    hdr,
                )
            }),
        });
    let seat = vout.seat.clone();
    let gamescope_cursor = gamescope_cursor.then(|| {
        std::sync::Arc::new(move || pf_vdisplay::gamescope_xwayland_cursor_targets(seat.as_deref()))
            as pf_capture::GamescopeCursorTargets
    });
    let producer = if kwin {
        pf_capture::Producer::Kwin
    } else if gamescope {
        pf_capture::Producer::Gamescope
    } else {
        pf_capture::Producer::Other
    };
    pf_capture::open_virtual_output(
        vout.remote_fd,
        vout.node_id,
        vout.preferred_mode,
        vout.keepalive,
        pf_capture::VirtualOutputOpts {
            allow_zerocopy: want.gpu,
            want_444: want.chroma_444,
            want_hdr: want.hdr,
            ten_bit_sdr: want.ten_bit_sdr,
            sdr10_native: want.sdr10_native,
            studio_range: !want.full_range,
            expect_exact_dims: vout.expect_exact_dims,
            producer,
            stream_hz,
            gamescope_cursor,
            direct,
            // gamescope has one stream for every session on its screen.
            share: gamescope && crate::vdisplay::policy::prefs().get().any_join(),
            ..Default::default()
        },
        // No route here. A wrong "foreign" only keeps the LINEAR offer.
        move || {
            // What the encoder reads a producer's NV12/P010 from. An older gamescope lists
            // LINEAR alone, so its offer keeps what is LINEAR.
            let planar = codec.filter(|_| nv12_native).map(|codec| {
                let mut offer = encoder.planar_capture_modifiers(codec, bit_depth, hdr);
                offer.retain(|&m| m == 0 || planar_tiled);
                offer
            });
            pf_capture::ZeroCopyPolicy {
                gamescope_tiled,
                planar_modifiers: planar,
                ..zero_copy_policy(
                    &encoder,
                    pyrowave,
                    nv12_native,
                    modifier_codec,
                    bit_depth,
                    hdr,
                )
            }
        },
    )
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
/// macOS: the capture composes PQ from any display, so the display decides: a virtual one
/// must take an HDR mode, a mirrored one is captured as it stands.
/// Linux + gamescope: host knob, `packaging/gamescope` 10-bit BT.2020/PQ, a
/// spawned (not attached-foreign) sub-mode, and no earlier virtual-output HDR
/// downgrade latched. Hyprland and wlroots: direct capture of a head lit in
/// BT.2020 PQ ([`direct_capture_hdr`], which asks `encoder`). Anything else on Linux is 8-bit;
/// GNOME 50+ portal HDR is the GameStream plane (`gamestream::launch_hdr_capable`).
pub fn capturer_supports_hdr_for(
    compositor: Option<crate::vdisplay::Compositor>,
    gamescope_route: Option<&crate::vdisplay::GamescopeRoute>,
    encoder: &crate::encode::Resolved,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        use crate::vdisplay::Compositor;
        match compositor {
            Some(Compositor::Gamescope) => {
                return pf_host_config::config().gamescope_hdr
                    && pf_vdisplay::gamescope_hdr_available(gamescope_route)
                    && !pf_capture::hdr_capture_failed(pf_capture::HdrSource::VirtualOutput);
            }
            Some(c @ (Compositor::Hyprland | Compositor::Wlroots)) => {
                return direct_capture_hdr(encoder, c);
            }
            _ => {}
        }
    }
    let _ = (compositor, gamescope_route, encoder);
    #[cfg(target_os = "macos")]
    if !pf_vdisplay::hdr_displays() {
        return false;
    }
    pf_capture::capturer_supports_hdr()
}

/// Direct capture's HDR answer. A pinned head must already be lit in BT.2020 PQ (its
/// `wp_color_management_v1` description); a Hyprland headless output is created in HDR
/// by its backend. Needs an encoder that reads packed 10-bit RGB off a dmabuf, and no
/// virtual-output latch from a rule or offer that did not take.
///
/// The compositor must advertise the globals an HDR capture binds: the portal a failed
/// open falls back to is SDR, and the client already renders by the Welcome's colour.
#[cfg(target_os = "linux")]
fn direct_capture_hdr(
    encoder: &crate::encode::Resolved,
    compositor: crate::vdisplay::Compositor,
) -> bool {
    if !pf_capture::direct_capture()
        || pf_capture::hdr_capture_failed(pf_capture::HdrSource::VirtualOutput)
    {
        return false;
    }
    let ingests_rgb10 =
        encoder.zero_copy_is_vaapi() || encoder.hdr_cuda_ok() || encoder.nvenc_raw_dmabuf_ok();
    if !ingests_rgb10 || !pf_capture::direct_hdr_globals() {
        return false;
    }
    match pf_vdisplay::capture_monitor() {
        Some(head) => pf_capture::output_hdr(&head) == Some(true),
        None => compositor == crate::vdisplay::Compositor::Hyprland,
    }
}

/// Does the source composite 10-bit SDR itself? Our gamescope from `+pfhdr26`, which offers
/// its 10-bit formats under BT.709, and Windows composing the virtual display in SDR wide
/// colour (FP16). Such a session needs no widening opt-in.
pub fn capturer_delivers_sdr10_for(
    compositor: Option<crate::vdisplay::Compositor>,
    gamescope_route: Option<&crate::vdisplay::GamescopeRoute>,
) -> bool {
    match compositor {
        Some(crate::vdisplay::Compositor::Gamescope) => {
            pf_vdisplay::gamescope_sdr10_capture(gamescope_route)
        }
        Some(crate::vdisplay::Compositor::Windows) => pf_capture::capturer_delivers_sdr10(),
        _ => false,
    }
}

// The Windows opens live with the display manager's other session touch points.
#[cfg(target_os = "windows")]
pub(crate) use crate::windows::idd::{capture_virtual_output, open_driver_encoder};

/// macOS: ScreenCaptureKit on the display the output stands for: a virtual display at its own
/// mode, unscaled, or a mirrored head scaled by the stream to the encoder's fitted size
/// ([`capture_size`]). Absolute input for screen `screen` aims at that display. The client
/// draws the pointer only when the session forwards it.
#[cfg(target_os = "macos")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    use anyhow::Context as _;
    let VirtualCaptureRequest {
        output,
        // Encode resolution and PipeWire producers are other platforms' concerns.
        codec: _codec,
        encoder: _encoder,
        capture: _capture,
        kwin: _kwin,
        gamescope: _gamescope,
        stream_hz,
        screen,
        gamescope_cursor: _gamescope_cursor,
        mirrored,
        framing,
    } = request;
    let display_id = vout
        .cg_display
        .context("the output names no display to capture")?;
    let (w, h, _) = vout.preferred_mode.context("the display reports no mode")?;
    let (width, height) = capture_size(mirrored, framing, (w, h));
    if screen == 0 {
        crate::inject::set_stream_display(Some(display_id));
    } else {
        crate::inject::set_screen_display(screen, Some(display_id));
    }
    pf_capture::open_display(pf_capture::DisplayCaptureOpts {
        display_id,
        width,
        height,
        stream_hz,
        metadata_cursor: output.hw_cursor,
        output,
        keepalive: vout.keepalive,
    })
}

/// The size a macOS stream delivers for a display whose own mode is `mode`: a mirrored head
/// fitted whole to the encoder's view, so the encoder takes it unscaled; anything else, a
/// virtual display among them, at the mode. Scaling here costs nothing on the encode path.
#[cfg(any(target_os = "macos", test))]
fn capture_size(
    mirrored: bool,
    framing: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    mode: (u32, u32),
) -> (u32, u32) {
    use punktfunk_core::video_fit::{Reframe, VideoFit};
    match framing {
        Some((VideoFit::Fit, view)) if mirrored => Reframe::plan(VideoFit::Fit, mode, view).out,
        _ => mode,
    }
}

#[cfg(test)]
mod tests {
    use super::capture_size;
    use punktfunk_core::video_fit::{Reframe, VideoFit};

    /// A mirror of a 1080p head is captured at the picture the encoder sends, the head whole
    /// and unstretched, so the encoder frames nothing more; a view wider or taller than the
    /// head leaves it at its own size for the client's bars.
    #[test]
    fn a_mirror_is_captured_at_the_encoders_size() {
        let head = (1920, 1080);
        for (view, want) in [
            ((1280, 720), (1280, 720)),
            ((960, 540), (960, 540)),
            ((1440, 1080), (1440, 810)),
            ((1920, 1200), (1920, 1080)),
        ] {
            let got = capture_size(true, Some((VideoFit::Fit, view)), head);
            assert_eq!(got, want, "view {view:?}");
            assert_eq!(
                got.0 * head.1,
                got.1 * head.0,
                "{got:?} keeps the head's shape"
            );
            let framed = Reframe::plan(VideoFit::Fit, got, view);
            assert!(
                framed.is_full(),
                "{view:?}: the encoder scales nothing more"
            );
        }
        let wide = capture_size(true, Some((VideoFit::Fit, (1920, 1080))), (5120, 1440));
        assert_eq!(wide, (1920, 540));
    }

    /// A virtual display, and a framing the stream can't take, open at the display's mode.
    #[test]
    fn a_virtual_display_opens_at_its_mode() {
        let mode = (2560, 1440);
        for framing in [None, Some((VideoFit::Fit, (1280, 720)))] {
            assert_eq!(capture_size(false, framing, mode), mode);
        }
        let crop = Some((VideoFit::Crop, (1280, 720)));
        assert_eq!(capture_size(true, crop, mode), mode);
        assert_eq!(capture_size(true, None, mode), mode);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    request: VirtualCaptureRequest,
) -> Result<Box<dyn Capturer>> {
    let VirtualCaptureRequest {
        output: _want,
        codec: _codec,
        encoder: _encoder,
        capture: _capture,
        kwin: _kwin,
        gamescope: _gamescope,
        stream_hz: _stream_hz,
        screen: _screen,
        gamescope_cursor: _gamescope_cursor,
        mirrored: _mirrored,
    } = request;
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}
