//! Per-session capture, topology, and encoder decision, resolved once from
//! [`HostConfig`](crate::config), the session's encode resolution, and handshake-negotiated
//! depth, HDR, chroma, and codec.
//!
//! Capture and topology callers read this artifact instead of re-deriving from config.
//! Every encode answer here reads the session's `Resolved`, which the handshake took and every
//! rebuild reuses, so an Encoder-row change applies at the next session.
//! [`SessionPlan::output_format`] is the one-way edge into capture so the capturer never
//! probes the encode backend again.
//!
//! Platform-neutral so it threads through `virtual_stream` / `build_pipeline`. Linux
//! resolves to portal + single-process; Windows is IDD-push + single-process.
//!
//! See `design/windows-host-rewrite.md`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Linux: xdg ScreenCast portal → PipeWire. The only Linux capture path.
    Portal,
    /// Windows: IDD direct-push from the pf-vdisplay driver's shared ring. The
    /// host runs as SYSTEM in the interactive console session, so it captures
    /// the secure desktop too.
    IddPush,
    /// Windows: a monitor the host did not create, captured with Windows Graphics Capture
    /// by a capture worker that runs as the signed-in user. It does not show the secure
    /// desktop.
    Wgc,
}

impl CaptureBackend {
    /// The capture of a session that streams a monitor (`mirrored`) or a display of its own.
    /// Shared by [`SessionPlan::mirroring`] and the standalone callers (GameStream / spike).
    pub fn resolve(mirrored: bool) -> Self {
        if !cfg!(target_os = "windows") {
            CaptureBackend::Portal
        } else if mirrored {
            CaptureBackend::Wgc
        } else {
            CaptureBackend::IddPush
        }
    }

    /// The session's encoder runs outside this process, behind an AU section: in the driver,
    /// or in the capture worker. The host then opens a proxy, never a backend.
    pub fn encodes_remotely(self) -> bool {
        matches!(self, CaptureBackend::IddPush | CaptureBackend::Wgc)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTopology {
    /// One process captures and encodes: Linux portal, or Windows IDD-push in the
    /// host's SYSTEM process in the interactive console session.
    SingleProcess,
    /// Windows: a capture worker in the user's session captures and encodes; the host reads
    /// its access units.
    CaptureWorker,
}

/// The backend the session resolved: logged, and the one a Windows driver encoder opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Linux: NVENC vs VAAPI is auto-detected inside `encode::open_video` (not modeled here).
    #[cfg_attr(windows, allow(dead_code, reason = "Windows always names a backend"))]
    PlatformAuto,
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows only"))]
    Nvenc,
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows only"))]
    Amf,
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows only"))]
    Qsv,
    /// Windows: any vendor's Media Foundation MFT.
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows only"))]
    MediaFoundation,
    Software,
}

impl EncoderBackend {
    /// `PlatformAuto` (Linux NVENC/VAAPI) is always GPU; only `Software` takes CPU staging.
    pub fn is_gpu(self) -> bool {
        !matches!(self, EncoderBackend::Software)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionPlan {
    pub capture: CaptureBackend,
    pub topology: SessionTopology,
    pub encoder: EncoderBackend,
    /// 8, or 10 = HEVC Main10 / 10-bit AV1. 10 does not imply HDR — `hdr` carries that.
    pub bit_depth: u8,
    /// Handshake HDR verdict, handed to the capturer (not derived from depth).
    /// Windows IDD-push enables advanced colour; Linux offers 10-bit PQ/BT.2020.
    /// Set only where `capture::capturer_supports_hdr_for` said yes — Linux means
    /// a gamescope output off our `pipewire-hdr` build; other compositors are 8-bit.
    pub hdr: bool,
    /// 4:2:0, or 4:4:4 when client, host, and GPU all support it. `Yuv420` on every backend that declined.
    pub chroma: crate::encode::ChromaFormat,
    /// HEVC by default; H.264 for a GPU-less software host (`resolve_codec` over advertised ∩ host capability).
    pub codec: crate::encode::Codec,
    /// Datagram-aligned encoder chunking: `Some(shard_payload)` on PyroWave, applied
    /// to every encoder this plan opens so AUs stay shard-aligned across rebuilds.
    /// `None` for H.26x.
    pub wire_chunk: Option<usize>,
    /// PyroWave's bits per pixel for this session: every pin a rebuild or mode switch
    /// resolves uses it. The host's row `pyrowave_bpp` until the native handshake sets the
    /// client's.
    pub pyrowave_bpp: f64,
    /// Encoder composites cursor bitmaps. Set only via [`cursor_blend_for`]: Linux
    /// when the encoder is the compositing stage; Windows always `false` (IDD
    /// composites the pointer). Encoders whose fast path cannot blend stay off
    /// those shapes — see [`Self::output_format`] and `Resolved::cursor_blend_capable`.
    pub cursor_blend: bool,
    /// Client draws the pointer locally, so `cursor_blend` is off and (on Windows)
    /// the capturer sets the driver's hardware cursor via [`OutputFormat::hw_cursor`](pf_frame::OutputFormat).
    pub cursor_forward: bool,
    /// Gamescope cursor from XFixes, not `SPA_META_Cursor`. Distinct from
    /// `cursor_forward`: stock gamescope neither embeds nor carries the channel,
    /// so the host composites. `false` when gamescope paints the cursor itself
    /// (`pf_vdisplay::gamescope_composites_cursor`) — otherwise a second pointer.
    pub gamescope_cursor: bool,
    /// The source composites 10-bit SDR itself (our gamescope from `+pfhdr26`, or Windows SDR
    /// wide colour), so a 10-bit SDR session takes real 10-bit pixels under BT.709 instead of
    /// widening an 8-bit capture. Set only via [`sdr10_native_for`]; `false` everywhere else.
    pub sdr10_native: bool,
    /// H.264/HEVC slices per frame the client takes: GameStream's `videoEncoderSlicesPerFrame`,
    /// a native client's [`client_slices`]. 1 for a client that never asked (TV-SoC decoders
    /// wedge on multi-slice frames). Applied to every encoder this plan opens so slicing cannot
    /// change shape across a rebuild.
    pub slices: u32,
    /// A joiner's own view and fit: the encoder frames the owner's picture for it. `None`
    /// streams the source as captured, or a mirrored head fitted to the negotiated size.
    pub reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    /// The session streams a host monitor, not a display of its own: the device's screen as the
    /// handshake resolved it. Set only through [`Self::mirroring`], which picks the capture.
    pub mirrored: bool,
    /// The encoder's own RGB conversion writes the range it is asked for
    /// ([`crate::encode::Resolved::rgb_full_range`]). Read through [`Self::full_range`].
    pub rgb_full_range: bool,
    /// The colours the client asked to get at studio range. Read through [`Self::full_range`].
    pub studio_asked: StudioAsk,
}

/// The colours whose full range the client's display shows as studio, so it asked for studio:
/// SDR by [`FEATURE_STUDIO_RANGE`], HDR by [`FEATURE_STUDIO_RANGE_HDR`].
///
/// [`FEATURE_STUDIO_RANGE`]: punktfunk_core::quic::v2::registry::FEATURE_STUDIO_RANGE
/// [`FEATURE_STUDIO_RANGE_HDR`]: punktfunk_core::quic::v2::registry::FEATURE_STUDIO_RANGE_HDR
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StudioAsk {
    pub sdr: bool,
    pub hdr: bool,
}

impl StudioAsk {
    /// The ask a negotiated feature set carries.
    pub fn of(features: &punktfunk_core::quic::v2::features::FeatureSet) -> Self {
        use punktfunk_core::quic::v2::registry::{FEATURE_STUDIO_RANGE, FEATURE_STUDIO_RANGE_HDR};
        StudioAsk {
            sdr: features.has(FEATURE_STUDIO_RANGE),
            hdr: features.has(FEATURE_STUDIO_RANGE_HDR),
        }
    }
}

/// The range a `codec` session at `hdr` and `bit_depth` encodes and advertises. Studio where the
/// client asked for it for that colour, or where AMF converts the desktop's 8-bit RGB itself on
/// a GPU that writes nothing else. PyroWave stays full: its client converts with its own
/// shaders. The Welcome, a mid-stream format and every encoder's open all ask here.
pub(crate) fn full_range(
    encoder: EncoderBackend,
    rgb_full_range: bool,
    studio_asked: StudioAsk,
    codec: crate::encode::Codec,
    hdr: bool,
    bit_depth: u8,
) -> bool {
    let asked = if hdr {
        studio_asked.hdr
    } else {
        studio_asked.sdr
    };
    let amf_studio = encoder == EncoderBackend::Amf && !rgb_full_range && bit_depth == 8;
    codec == crate::encode::Codec::PyroWave || !(asked || amf_studio)
}

/// [`full_range`] for a session not planned yet: the Welcome's, from the handshake's resolution.
pub(crate) fn full_range_for(
    encoder: &crate::encode::Resolved,
    studio_asked: StudioAsk,
    codec: crate::encode::Codec,
    hdr: bool,
    bit_depth: u8,
) -> bool {
    full_range(
        resolve_encoder(encoder),
        encoder.rgb_full_range(),
        studio_asked,
        codec,
        hdr,
        bit_depth,
    )
}

/// Slices per frame toward a native client that takes multi-slice frames but names no count.
pub const DEFAULT_SLICES: u32 = 4;

/// Slices per frame a native client takes: 1 without
/// [`VIDEO_CAP_MULTI_SLICE`](punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE), else its
/// `ClientHello` ask, else [`DEFAULT_SLICES`]. An ask past 32 (NVENC's limit) is no ask.
pub fn client_slices(video_caps: u8, asked: u8) -> u32 {
    if video_caps & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE == 0 {
        return 1;
    }
    match u32::from(asked) {
        n @ 1..=32 => n,
        _ => DEFAULT_SLICES,
    }
}

impl SessionPlan {
    /// `encoder` is the session's resolution. `hdr` is the handshake verdict, not derived from
    /// depth: 10-bit SDR exists. PyroWave's bits per pixel start at the host's; a caller that
    /// knows the client's sets them. `slices` is the client's count, [`Self::slices`]. The plan
    /// is a display of the session's own until [`Self::mirroring`] says otherwise.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        encoder: &crate::encode::Resolved,
        bit_depth: u8,
        hdr: bool,
        chroma: crate::encode::ChromaFormat,
        codec: crate::encode::Codec,
        cursor_blend: bool,
        cursor_forward: bool,
        slices: u32,
    ) -> Self {
        let capture = CaptureBackend::resolve(false);
        SessionPlan {
            capture,
            topology: resolve_topology(capture),
            encoder: resolve_encoder(encoder),
            rgb_full_range: encoder.rgb_full_range(),
            // A caller that knows the client's ask sets it.
            studio_asked: StudioAsk::default(),
            bit_depth,
            hdr,
            chroma,
            codec,
            wire_chunk: None,
            pyrowave_bpp: pf_host_config::config().pyrowave_bpp,
            cursor_blend,
            cursor_forward,
            // Callers that know the compositor overwrite these; default off for everyone else.
            gamescope_cursor: false,
            sdr10_native: false,
            slices,
            reframe_to: None,
            mirrored: false,
        }
    }

    /// This plan for a session that streams a host monitor (`mirrored`) or a display of its own,
    /// with the capture that takes it.
    pub fn mirroring(mut self, mirrored: bool) -> Self {
        self.mirrored = mirrored;
        self.capture = CaptureBackend::resolve(mirrored);
        self.topology = resolve_topology(self.capture);
        self
    }

    /// The range this plan encodes and advertises at the live `hdr` and `bit_depth` ([`full_range`]).
    pub fn full_range(&self, hdr: bool, bit_depth: u8) -> bool {
        full_range(
            self.encoder,
            self.rgb_full_range,
            self.studio_asked,
            self.codec,
            hdr,
            bit_depth,
        )
    }

    /// `gpu` from the plan's backend (no second probe); `hdr` from the plan; the Linux NVENC and
    /// native-NV12 gates from `encoder`, the session's resolution.
    pub fn output_format(&self, encoder: &crate::encode::Resolved) -> crate::capture::OutputFormat {
        let gpu = self.encoder.is_gpu();
        #[cfg(not(target_os = "linux"))]
        let _ = encoder;
        // Linux NVENC 4:4:4: zero-copy hands the encoder a GPU YUV444 surface
        // (`ImportKind::Tiled444`). Without it the encoder takes CPU RGB and CSCs
        // itself, so force GPU capture off here only. (VAAPI 4:4:4 keeps dmabuf;
        // Windows NVENC takes BGRA.)
        #[cfg(target_os = "linux")]
        let gpu = {
            let force_cpu_for_nvenc_444 = self.chroma.is_444()
                && !encoder.zero_copy_is_vaapi()
                && !crate::zerocopy::enabled();
            if gpu && force_cpu_for_nvenc_444 {
                // Name the session codec, not the gate. `zero_copy_is_vaapi()` reads the
                // host-global encoder pref, so a PyroWave session on an NVENC/auto host
                // lands here too — and it never touches NVENC or YUV444P; it loses dmabuf
                // passthrough.
                if self.codec == crate::encode::Codec::PyroWave {
                    tracing::warn!(
                        "4:4:4 PyroWave session with PUNKTFUNK_ZEROCOPY off: zero-copy GPU \
                         capture DISABLED — the wavelet encoder loses its raw-dmabuf passthrough \
                         and every frame becomes a full-resolution CPU readback plus an upload \
                         into its own Vulkan device; expect a materially lower fps ceiling (set \
                         PUNKTFUNK_ZEROCOPY=1 to restore the passthrough)"
                    );
                } else {
                    tracing::warn!(
                        "4:4:4 session on the NVENC path without PUNKTFUNK_ZEROCOPY: zero-copy \
                         GPU capture DISABLED — every frame is CPU RGB + swscale RGB→YUV444P; \
                         expect a lower fps ceiling than 4:2:0 at this mode (set \
                         PUNKTFUNK_ZEROCOPY=1 for the GPU 4:4:4 convert)"
                    );
                }
            }
            gpu && !force_cpu_for_nvenc_444
        };
        // PyroWave keeps `gpu = true`: the facade routes to raw-dmabuf passthrough
        // (`ZeroCopyPolicy::pyrowave_session` advertises importable modifiers). The
        // EGL→CUDA importer is skipped — only NVENC consumes those payloads.
        crate::capture::OutputFormat {
            gpu,
            hdr: self.hdr,
            // 10-bit without HDR = 10-bit SDR: Windows composes it in SDR wide colour where
            // it can, and otherwise the driver's encoder widens BGRA.
            ten_bit_sdr: self.bit_depth == 10 && !self.hdr,
            sdr10_native: self.sdr10_native,
            hw_cursor: self.cursor_forward,
            // 4:4:4 needs a full-chroma source: Windows stays on RGB (not NV12/P010)
            // so NVENC can CSC to 4:4:4.
            chroma_444: self.chroma.is_444(),
            // Windows: IDD-push makes the NV12 out-ring shareable and signals a shared
            // fence for Vulkan import. Linux: facade flips to raw-dmabuf (see above).
            pyrowave: self.codec == crate::encode::Codec::PyroWave,
            // Native NV12 (gamescope), resolved from the plan's codec so capture never
            // reaches into encode. `cursor_blend` subsumes `gamescope_cursor`.
            #[cfg(target_os = "linux")]
            nv12_native: encoder.native_nv12_ok(
                self.codec,
                self.bit_depth,
                self.hdr,
                self.cursor_blend,
                self.sdr10_native,
            ),
            #[cfg(not(target_os = "linux"))]
            nv12_native: false,
            // Capture's pods and converts take the plan's range at its own colour.
            full_range: self.full_range(self.hdr, self.bit_depth),
        }
    }
}

pub(crate) fn resolve_topology(capture: CaptureBackend) -> SessionTopology {
    if capture == CaptureBackend::Wgc {
        SessionTopology::CaptureWorker
    } else {
        SessionTopology::SingleProcess
    }
}

/// THE rule for [`SessionPlan::cursor_blend`], shared by every resolve caller
/// so they cannot drift.
///
/// * **Linux**: the encoder is the compositing stage. Blend for a cursor-forward
///   session (capture-mouse flip needs the host composite on demand), for
///   gamescope (no pointer in the capture; XFixes must be drawn), and for a
///   no-channel session whose compositor cannot embed the pointer
///   ([`compositor_embeds_pointer`]) when the backend can composite. A blend
///   costs the zero-CSC encode sources (RGB-direct, producer NV12): a
///   full-frame compute pass per frame, on the shader cores a game saturates.
/// * **Everywhere else**: never. Windows IDD composites the pointer itself
///   (`cursor_blend.rs` / DWM); no Windows encode backend reads `frame.cursor`.
///   Gated on Linux because the VAAPI/CUDA prediction and zero-copy switch
///   exist only there.
pub(crate) fn cursor_blend_for(
    cursor_forward: bool,
    compositor: pf_vdisplay::Compositor,
    codec: crate::encode::Codec,
    bit_depth: u8,
    hdr: bool,
    gamescope_route: Option<&pf_vdisplay::GamescopeRoute>,
    encoder: &crate::encode::Resolved,
) -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            cursor_forward,
            compositor,
            codec,
            bit_depth,
            hdr,
            gamescope_route,
            encoder,
        );
        false
    }
    #[cfg(target_os = "linux")]
    {
        if compositor == pf_vdisplay::Compositor::Gamescope {
            // gamescope capture has no SPA_META_Cursor; skip the blend-capable term or a
            // gamescope that paints its own pointer loses native-NV12 for a blend that
            // never receives an overlay.
            return gamescope_needs_host_cursor(true, gamescope_route);
        }
        if cursor_forward {
            return true;
        }
        if compositor_embeds_pointer(compositor) {
            return false;
        }
        encoder_blends_cursor(codec, bit_depth, hdr, encoder)
    }
}

/// `encoder`, the session's resolution, composites `frame.cursor`: the blend term of
/// [`cursor_blend_for`], for a caller that knows no compositor yet. Never off Linux.
#[cfg(any(target_os = "linux", feature = "gamestream"))]
pub(crate) fn encoder_blends_cursor(
    codec: crate::encode::Codec,
    bit_depth: u8,
    hdr: bool,
    encoder: &crate::encode::Resolved,
) -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (codec, bit_depth, hdr, encoder);
        false
    }
    #[cfg(target_os = "linux")]
    {
        // Same CUDA-payload prediction as `handshake::cursor_forward`: NVIDIA plus
        // the zero-copy switch. It picks the backend.
        let cuda_planned = !encoder.zero_copy_is_vaapi() && crate::zerocopy::enabled();
        encoder.cursor_blend_capable(codec, cuda_planned, bit_depth == 10, hdr)
    }
}

/// Whether a no-channel session can leave the pointer to the compositor (portal
/// `Embedded`, KWin `POINTER_EMBEDDED`). Mutter cannot: a virtual stream drops its
/// software cursor whenever a physical head holds a hardware one, and cursor-only
/// motion schedules no re-record (mutter#4939). Gamescope has its own arm.
#[cfg(target_os = "linux")]
pub(crate) fn compositor_embeds_pointer(compositor: pf_vdisplay::Compositor) -> bool {
    use pf_vdisplay::Compositor;
    matches!(
        compositor,
        Compositor::Kwin
            | Compositor::Wlroots
            | Compositor::Hyprland
            | Compositor::Niri
            | Compositor::Umbriel
    )
}

/// Gamescope keeps the cursor on a hardware plane and does not paint it into
/// the PipeWire node, so the host reads XFixes and blends. Patch level 2+
/// (`--pipewire-composite-cursor`) puts it in the node — then the host must
/// not blend, or the pointer is drawn twice. A session that composites also
/// forces compute colour-conversion, because the RGB-direct source has no
/// blend stage; cursor-in-node is the zero-copy end-to-end path.
#[cfg(not(target_os = "windows"))]
fn gamescope_needs_host_cursor(
    gamescope: bool,
    gamescope_route: Option<&pf_vdisplay::GamescopeRoute>,
) -> bool {
    gamescope && !pf_vdisplay::gamescope_composites_cursor(gamescope_route)
}

/// No gamescope on Windows: the pointer is the driver's.
#[cfg(target_os = "windows")]
fn gamescope_needs_host_cursor(
    _gamescope: bool,
    _gamescope_route: Option<&pf_vdisplay::GamescopeRoute>,
) -> bool {
    false
}

/// Kept beside [`cursor_blend_for`] because the two must agree: reader without
/// blend wastes an X11 connection; blend without reader streams no pointer.
pub(crate) fn gamescope_cursor_for(
    gamescope: bool,
    gamescope_route: Option<&pf_vdisplay::GamescopeRoute>,
) -> bool {
    gamescope_needs_host_cursor(gamescope, gamescope_route)
}

/// [`SessionPlan::sdr10_native`] for `plan` on `compositor`: a 10-bit SDR session whose
/// source composites 10-bit SDR itself ([`crate::capture::capturer_delivers_sdr10_for`]).
pub(crate) fn sdr10_native_for(
    plan: &SessionPlan,
    compositor: crate::vdisplay::Compositor,
    gamescope_route: Option<&pf_vdisplay::GamescopeRoute>,
) -> bool {
    plan.bit_depth == 10
        && !plan.hdr
        && crate::capture::capturer_delivers_sdr10_for(Some(compositor), gamescope_route)
}

#[cfg(target_os = "windows")]
fn resolve_encoder(encoder: &crate::encode::Resolved) -> EncoderBackend {
    match encoder.backend() {
        crate::encode::WindowsBackend::Nvenc => EncoderBackend::Nvenc,
        crate::encode::WindowsBackend::Amf => EncoderBackend::Amf,
        crate::encode::WindowsBackend::Qsv => EncoderBackend::Qsv,
        crate::encode::WindowsBackend::MediaFoundation => EncoderBackend::MediaFoundation,
        crate::encode::WindowsBackend::Software => EncoderBackend::Software,
    }
}

/// A software pin is GPU-less openh264, which must take CPU-staged capture
/// (`Software.is_gpu() == false`). Everything else is `PlatformAuto` (NVENC/VAAPI inside
/// `encode::open_video`).
#[cfg(not(target_os = "windows"))]
fn resolve_encoder(encoder: &crate::encode::Resolved) -> EncoderBackend {
    if encoder.is_gpu() {
        EncoderBackend::PlatformAuto
    } else {
        EncoderBackend::Software
    }
}

/// The HDR flag and depth an encoder opens at, and an epoch says, for a session negotiated at
/// `plan_hdr`/`bit_depth` whose source composes HDR now or not. An HDR session on a source
/// gone SDR (Windows HDR switched off, a head re-lit SDR) encodes 8-bit SDR until it is lit
/// again; a driver's pool refuses every surface not in the format it opened for.
pub(crate) fn live_depth(plan_hdr: bool, source_hdr: bool, bit_depth: u8) -> (bool, u8) {
    let hdr = plan_hdr && source_hdr;
    (hdr, if plan_hdr && !hdr { 8 } else { bit_depth })
}

impl SessionPlan {
    /// The plan of a session `mode_conflict: join` admitted. On Windows the owner's display is
    /// captured by a worker; a Linux joiner gets a second cast through the same portal path.
    pub(crate) fn sharing_live_display(mut self) -> Self {
        if cfg!(target_os = "windows") {
            self.capture = CaptureBackend::Wgc;
            self.topology = resolve_topology(self.capture);
        }
        self
    }
}

/// Open the encoder for `frame` through `open(width, height)` and return it with the framing
/// it encodes. A joiner's `reframe_to` frames the owner's picture for its view; a `mirrored`
/// head larger than the client's `negotiated` picture fits inside it. Either crops and scales
/// on ingest; a backend that cannot is reopened at the source's own size.
pub(crate) fn open_encoder_fitted(
    frame: &crate::capture::CapturedFrame,
    negotiated: (u32, u32),
    reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    mirrored: bool,
    mut open: impl FnMut(u32, u32) -> anyhow::Result<Box<dyn crate::encode::Encoder>>,
) -> anyhow::Result<(
    Box<dyn crate::encode::Encoder>,
    punktfunk_core::video_fit::Reframe,
)> {
    use punktfunk_core::video_fit::{Reframe, VideoFit};
    let captured = (frame.width, frame.height);
    let full = Reframe::full(captured);
    let Some((fit, view)) = reframe_to.or_else(|| mirrored.then_some((VideoFit::Fit, negotiated)))
    else {
        return Ok((open(captured.0, captured.1)?, full));
    };
    let framed = Reframe::plan(fit, captured, view);
    if framed.is_full() {
        return Ok((open(captured.0, captured.1)?, full));
    }
    let mut enc = open(framed.out.0, framed.out.1)?;
    let caps = enc.caps();
    let scaled = framed.out != (framed.crop[2], framed.crop[3]);
    let armed = if (scaled && !caps.downscales_input) || (framed.is_cropped() && !caps.crops_input)
    {
        Err(anyhow::anyhow!(
            "the backend neither crops nor scales on ingest"
        ))
    } else if caps.crops_input {
        enc.set_input_crop(framed.crop)
    } else {
        Ok(())
    };
    if let Err(e) = &armed {
        tracing::warn!(
            ?captured,
            crop = ?framed.crop,
            wanted = ?framed.out,
            error = %format!("{e:#}"),
            "this encode backend cannot frame the picture for this client — encoding the source at its own size"
        );
    } else {
        tracing::info!(
            ?captured,
            crop = ?framed.crop,
            encoder = ?framed.out,
            ?view,
            fit = fit.name(),
            "the encoder frames the picture for this client on ingest"
        );
        return Ok((enc, framed));
    }
    drop(enc);
    Ok((open(captured.0, captured.1)?, full))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{ChromaFormat, Codec};
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use pf_vdisplay::Compositor;

    /// The multi-slice bit gates the ask; no ask or an impossible one takes the default.
    #[test]
    fn a_native_client_gets_the_slices_it_asks_for() {
        use punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE as MULTI;
        assert_eq!(client_slices(0, 8), 1);
        assert_eq!(client_slices(MULTI, 0), DEFAULT_SLICES);
        assert_eq!(client_slices(MULTI, 8), 8);
        assert_eq!(client_slices(MULTI, 1), 1);
        assert_eq!(client_slices(MULTI, 33), DEFAULT_SLICES);
    }

    /// An encoder opens for what the source composes: an HDR session on an SDR source is 8-bit
    /// SDR, 10-bit SDR stays 10-bit, and an SDR session never turns HDR.
    #[test]
    fn an_encoder_opens_for_what_the_source_composes() {
        assert_eq!(live_depth(true, true, 10), (true, 10));
        assert_eq!(live_depth(true, false, 10), (false, 8));
        assert_eq!(live_depth(false, false, 10), (false, 10));
        assert_eq!(live_depth(false, true, 8), (false, 8));
    }

    /// The plan keeps the client's count: a GameStream ask above the native default survives.
    #[test]
    fn resolve_keeps_the_client_slice_count() {
        let encoder = crate::encode::resolve();
        for slices in [1, DEFAULT_SLICES, 8] {
            let plan = SessionPlan::resolve(
                &encoder,
                8,
                false,
                ChromaFormat::Yuv420,
                Codec::H264,
                false,
                false,
                slices,
            );
            assert_eq!(plan.slices, slices);
        }
    }

    /// A session set to a monitor captures it with WGC in a worker on Windows, whatever the
    /// host-wide screen is; its own screen is the driver's IDD push. Linux takes the portal.
    #[test]
    fn a_mirrored_session_plans_its_own_capture() {
        let plan = |mirrored| {
            SessionPlan::resolve(
                &crate::encode::resolve(),
                8,
                false,
                ChromaFormat::Yuv420,
                Codec::H264,
                false,
                false,
                1,
            )
            .mirroring(mirrored)
        };
        let (own, mirror) = (plan(false), plan(true));
        assert!(mirror.mirrored && !own.mirrored);
        if cfg!(target_os = "windows") {
            assert_eq!(
                (own.capture, mirror.capture),
                (CaptureBackend::IddPush, CaptureBackend::Wgc)
            );
            assert_eq!(mirror.topology, SessionTopology::CaptureWorker);
        } else {
            assert_eq!(
                (own.capture, mirror.capture),
                (CaptureBackend::Portal, CaptureBackend::Portal)
            );
        }
        assert_eq!(own.topology, SessionTopology::SingleProcess);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cursor_forward_forces_blend_on_linux() {
        let encoder = crate::encode::resolve();
        assert!(cursor_blend_for(
            true,
            Compositor::Kwin,
            Codec::H264,
            8,
            false,
            None,
            &encoder
        ));
        assert!(cursor_blend_for(
            true,
            Compositor::Mutter,
            Codec::H264,
            8,
            false,
            None,
            &encoder
        ));
    }

    /// The no-channel session on an embedding compositor keeps the zero-CSC encode
    /// sources: no blend, whatever the backend could composite.
    #[cfg(target_os = "linux")]
    #[test]
    fn embedding_compositor_skips_the_blend_without_a_channel() {
        let encoder = crate::encode::resolve();
        for c in [
            Compositor::Kwin,
            Compositor::Wlroots,
            Compositor::Hyprland,
            Compositor::Niri,
            Compositor::Umbriel,
        ] {
            assert!(compositor_embeds_pointer(c));
            assert!(!cursor_blend_for(
                false,
                c,
                Codec::Av1,
                8,
                false,
                None,
                &encoder
            ));
            assert!(!cursor_blend_for(
                false,
                c,
                Codec::H265,
                10,
                false,
                None,
                &encoder
            ));
        }
        assert!(!compositor_embeds_pointer(Compositor::Mutter));
        assert!(!compositor_embeds_pointer(Compositor::Gamescope));
    }

    /// Native passes its cursor channel and its own depth; GameStream no channel and depth 10
    /// only with HDR. Without a channel the rule is the compositor's arm, then the encoder's.
    #[cfg(any(target_os = "linux", feature = "gamestream"))]
    #[test]
    fn the_blend_rule_takes_both_loops_arguments() {
        use pf_vdisplay::Compositor;
        let linux = cfg!(target_os = "linux");
        let encoder = crate::encode::resolve();
        for c in [Compositor::Kwin, Compositor::Mutter, Compositor::Gamescope] {
            let gamescope = c == Compositor::Gamescope;
            for hdr in [false, true] {
                let gs_depth = if hdr { 10 } else { 8 };
                let gamestream =
                    cursor_blend_for(false, c, Codec::H265, gs_depth, hdr, None, &encoder);
                let encoder_arm = c == Compositor::Mutter
                    && encoder_blends_cursor(Codec::H265, gs_depth, hdr, &encoder);
                let want = linux
                    && if gamescope {
                        gamescope_cursor_for(true, None)
                    } else {
                        encoder_arm
                    };
                assert_eq!(gamestream, want, "{c:?} hdr={hdr}");
                let native = cursor_blend_for(true, c, Codec::H265, 10, hdr, None, &encoder);
                let want = linux && (!gamescope || gamescope_cursor_for(true, None));
                assert_eq!(native, want, "{c:?} hdr={hdr}");
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gamescope_cursor_reader_matches_blend_rule() {
        let cursor_blend = cursor_blend_for(
            false,
            Compositor::Gamescope,
            Codec::H265,
            10,
            true,
            None,
            &crate::encode::resolve(),
        );
        let gamescope_cursor = gamescope_cursor_for(true, None);

        assert_eq!(cursor_blend, gamescope_cursor);
        assert_eq!(cursor_blend, gamescope_needs_host_cursor(true, None));
        assert!(!gamescope_cursor_for(false, None));
    }

    /// Studio only where AMF converts 8-bit SDR itself on a GPU that writes nothing else.
    #[test]
    fn only_amf_8bit_sdr_on_a_studio_gpu_goes_studio() {
        use crate::encode::Codec::{Av1, PyroWave, H264, H265};
        use EncoderBackend::{Amf, Nvenc, PlatformAuto, Qsv};
        let none = StudioAsk::default();
        for codec in [H264, H265, Av1] {
            assert!(!full_range(Amf, false, none, codec, false, 8), "{codec:?}");
            assert!(
                full_range(Amf, true, none, codec, false, 8),
                "a GPU that writes full"
            );
        }
        assert!(
            full_range(Amf, false, none, PyroWave, false, 8),
            "PyroWave: our shaders"
        );
        assert!(
            full_range(Amf, false, none, H265, false, 10),
            "10-bit SDR: our converter"
        );
        assert!(
            full_range(Amf, false, none, H265, true, 10),
            "HDR: our converter"
        );
        for other in [Nvenc, Qsv, PlatformAuto] {
            assert!(full_range(other, false, none, H264, false, 8), "{other:?}");
        }
    }

    /// Each colour goes studio on every backend only where the client asked for that colour;
    /// the other stays full. PyroWave stays full.
    #[test]
    fn a_studio_ask_takes_only_its_own_colour() {
        use crate::encode::Codec::{Av1, PyroWave, H264, H265};
        use EncoderBackend::{Amf, MediaFoundation, Nvenc, PlatformAuto, Qsv, Software};
        let sdr = StudioAsk {
            sdr: true,
            hdr: false,
        };
        let hdr = StudioAsk {
            sdr: false,
            hdr: true,
        };
        let both = StudioAsk {
            sdr: true,
            hdr: true,
        };
        for backend in [Amf, MediaFoundation, Nvenc, PlatformAuto, Qsv, Software] {
            for codec in [H264, H265, Av1] {
                for depth in [8, 10] {
                    let what = format!("{backend:?} {codec:?} {depth}");
                    assert!(
                        !full_range(backend, true, sdr, codec, false, depth),
                        "SDR: {what}"
                    );
                    assert!(
                        full_range(backend, true, hdr, codec, false, depth),
                        "SDR unasked: {what}"
                    );
                }
                let what = format!("{backend:?} {codec:?}");
                assert!(
                    !full_range(backend, true, hdr, codec, true, 10),
                    "HDR: {what}"
                );
                assert!(
                    full_range(backend, true, sdr, codec, true, 10),
                    "HDR unasked: {what}"
                );
            }
            assert!(
                full_range(backend, true, both, PyroWave, false, 8),
                "PyroWave: {backend:?}"
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cursor_rules_are_disabled_on_windows() {
        let encoder = crate::encode::resolve();
        for cursor_forward in [false, true] {
            for compositor in [Compositor::Windows, Compositor::Gamescope] {
                assert!(!cursor_blend_for(
                    cursor_forward,
                    compositor,
                    Codec::H265,
                    10,
                    true,
                    None,
                    &encoder
                ));
                assert!(!gamescope_cursor_for(
                    compositor == Compositor::Gamescope,
                    None
                ));
            }
        }
    }
}
