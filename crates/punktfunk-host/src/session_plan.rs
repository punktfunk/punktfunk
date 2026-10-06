//! Per-session capture, topology, and encoder decision, resolved once from
//! [`HostConfig`](crate::config) plus handshake-negotiated depth, HDR, chroma, and codec.
//!
//! Capture and topology callers read this artifact instead of re-deriving from config.
//! [`EncoderBackend`] is recorded for logging; `encode::windows_resolved_backend` still
//! opens the encoder. [`SessionPlan::output_format`] is the one-way edge into capture so
//! the capturer never probes the encode backend again.
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
    /// Shared by [`SessionPlan::resolve`] and the standalone callers (GameStream / spike).
    pub fn resolve() -> Self {
        if !cfg!(target_os = "windows") {
            CaptureBackend::Portal
        } else if mirrored() {
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

/// Recorded for logging. The encoder open still goes through
/// `encode::windows_resolved_backend` (config-backed, GPU-vendor cached).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Linux: NVENC vs VAAPI is auto-detected inside `encode::open_video` (not modeled here).
    PlatformAuto,
    Nvenc,
    Amf,
    Qsv,
    /// Windows: any vendor's Media Foundation MFT.
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
    /// Encoder composites cursor bitmaps. Set only via [`cursor_blend_for`]: Linux
    /// when the encoder is the compositing stage; Windows always `false` (IDD
    /// composites the pointer). Encoders whose fast path cannot blend stay off
    /// those shapes — see [`Self::output_format`] and `encode::cursor_blend_capable`.
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
    /// Encoder slice-count ceiling from [`VIDEO_CAP_MULTI_SLICE`](punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE):
    /// 32 when the bit is set (backend default; no client-side cap), 1 when not
    /// (single-slice frames for TV-SoC decoders). Applied to every encoder this
    /// plan opens so slicing cannot change shape across a rebuild.
    pub max_slices: u32,
    /// A joiner's own view and fit: the encoder frames the owner's picture for it. `None`
    /// streams the source as captured, or a mirrored head fitted to the negotiated size.
    pub reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
}

impl SessionPlan {
    /// `hdr` is the handshake verdict, not derived from depth: 10-bit SDR exists.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        bit_depth: u8,
        hdr: bool,
        chroma: crate::encode::ChromaFormat,
        codec: crate::encode::Codec,
        cursor_blend: bool,
        cursor_forward: bool,
        multi_slice: bool,
    ) -> Self {
        let capture = CaptureBackend::resolve();
        SessionPlan {
            capture,
            topology: resolve_topology(capture),
            encoder: resolve_encoder(),
            bit_depth,
            hdr,
            chroma,
            codec,
            wire_chunk: None,
            cursor_blend,
            cursor_forward,
            // Callers that know the compositor overwrite these; default off for everyone else.
            gamescope_cursor: false,
            sdr10_native: false,
            max_slices: if multi_slice { 32 } else { 1 },
            reframe_to: None,
        }
    }

    /// `gpu` from the already-resolved `encoder` (no second probe); `hdr` from the plan.
    pub fn output_format(&self) -> crate::capture::OutputFormat {
        let gpu = self.encoder.is_gpu();
        // Linux NVENC 4:4:4: zero-copy hands the encoder a GPU YUV444 surface
        // (`ImportKind::Tiled444`). Without it the encoder takes CPU RGB and CSCs
        // itself, so force GPU capture off here only. (VAAPI 4:4:4 keeps dmabuf;
        // Windows NVENC takes BGRA.)
        #[cfg(target_os = "linux")]
        let gpu = {
            let force_cpu_for_nvenc_444 = self.chroma.is_444()
                && !crate::encode::linux_zero_copy_is_vaapi()
                && !crate::zerocopy::enabled();
            if gpu && force_cpu_for_nvenc_444 {
                // Name the session codec, not the gate. `linux_zero_copy_is_vaapi()` reads
                // the host-global encoder pref, so a PyroWave session on an NVENC/auto host
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
            nv12_native: crate::encode::linux_native_nv12_ok(
                self.codec,
                self.bit_depth,
                self.hdr,
                self.cursor_blend,
                self.sdr10_native,
            ),
            #[cfg(not(target_os = "linux"))]
            nv12_native: false,
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
        // Same CUDA-payload prediction as `handshake::cursor_forward`: NVIDIA plus
        // the zero-copy switch. Only a CUDA payload reaches the blend.
        let cuda_planned = !crate::encode::linux_zero_copy_is_vaapi() && crate::zerocopy::enabled();
        crate::encode::cursor_blend_capable(codec, cuda_planned, bit_depth == 10, hdr)
    }
}

/// Whether a no-channel session can leave the pointer to the compositor (portal
/// `Embedded`, KWin `POINTER_EMBEDDED`). Mutter cannot: a virtual stream drops its
/// software cursor whenever a physical head holds a hardware one, and cursor-only
/// motion schedules no re-record (mutter#4939). Gamescope has its own arm.
pub(crate) fn compositor_embeds_pointer(compositor: pf_vdisplay::Compositor) -> bool {
    use pf_vdisplay::Compositor;
    matches!(
        compositor,
        Compositor::Kwin | Compositor::Wlroots | Compositor::Hyprland
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
fn resolve_encoder() -> EncoderBackend {
    match crate::encode::windows_resolved_backend() {
        crate::encode::WindowsBackend::Nvenc => EncoderBackend::Nvenc,
        crate::encode::WindowsBackend::Amf => EncoderBackend::Amf,
        crate::encode::WindowsBackend::Qsv => EncoderBackend::Qsv,
        crate::encode::WindowsBackend::MediaFoundation => EncoderBackend::MediaFoundation,
        crate::encode::WindowsBackend::Software => EncoderBackend::Software,
    }
}

#[cfg(not(target_os = "windows"))]
fn resolve_encoder() -> EncoderBackend {
    // `PUNKTFUNK_ENCODER=software` forces GPU-less openh264, which must take
    // CPU-staged capture (`Software.is_gpu() == false`). Everything else stays
    // `PlatformAuto` (NVENC/VAAPI inside `encode::open_video`).
    match pf_host_config::config().encoder_pref.as_str() {
        "software" | "sw" | "openh264" => EncoderBackend::Software,
        _ => EncoderBackend::PlatformAuto,
    }
}

/// Whether a session opened now streams a pinned physical head instead of a virtual display.
pub(crate) fn mirrored() -> bool {
    pf_vdisplay::mirrors_pinned()
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
/// it encodes. A joiner's `reframe_to` frames the owner's picture for its view; a mirrored head
/// larger than the client's `negotiated` picture fits inside it. Either crops and scales on
/// ingest; a backend that cannot is reopened at the source's own size.
pub(crate) fn open_encoder_fitted(
    frame: &crate::capture::CapturedFrame,
    negotiated: (u32, u32),
    reframe_to: Option<(punktfunk_core::video_fit::VideoFit, (u32, u32))>,
    mut open: impl FnMut(u32, u32) -> anyhow::Result<Box<dyn crate::encode::Encoder>>,
) -> anyhow::Result<(
    Box<dyn crate::encode::Encoder>,
    punktfunk_core::video_fit::Reframe,
)> {
    use punktfunk_core::video_fit::{Reframe, VideoFit};
    let captured = (frame.width, frame.height);
    let full = Reframe::full(captured);
    let Some((fit, view)) =
        reframe_to.or_else(|| mirrored().then_some((VideoFit::Fit, negotiated)))
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
    use pf_vdisplay::Compositor;

    #[test]
    fn resolve_limits_single_slice_clients() {
        let plan = SessionPlan::resolve(
            8,
            false,
            ChromaFormat::Yuv420,
            Codec::H264,
            false,
            false,
            false,
        );

        assert_eq!(plan.max_slices, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cursor_forward_forces_blend_on_linux() {
        assert!(cursor_blend_for(
            true,
            Compositor::Kwin,
            Codec::H264,
            8,
            false,
            None
        ));
        assert!(cursor_blend_for(
            true,
            Compositor::Mutter,
            Codec::H264,
            8,
            false,
            None
        ));
    }

    /// The no-channel session on an embedding compositor keeps the zero-CSC encode
    /// sources: no blend, whatever the backend could composite.
    #[cfg(target_os = "linux")]
    #[test]
    fn embedding_compositor_skips_the_blend_without_a_channel() {
        for c in [Compositor::Kwin, Compositor::Wlroots, Compositor::Hyprland] {
            assert!(compositor_embeds_pointer(c));
            assert!(!cursor_blend_for(false, c, Codec::Av1, 8, false, None));
            assert!(!cursor_blend_for(false, c, Codec::H265, 10, false, None));
        }
        assert!(!compositor_embeds_pointer(Compositor::Mutter));
        assert!(!compositor_embeds_pointer(Compositor::Gamescope));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gamescope_cursor_reader_matches_blend_rule() {
        let cursor_blend =
            cursor_blend_for(false, Compositor::Gamescope, Codec::H265, 10, true, None);
        let gamescope_cursor = gamescope_cursor_for(true, None);

        assert_eq!(cursor_blend, gamescope_cursor);
        assert_eq!(cursor_blend, gamescope_needs_host_cursor(true, None));
        assert!(!gamescope_cursor_for(false, None));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cursor_rules_are_disabled_on_windows() {
        for cursor_forward in [false, true] {
            for compositor in [Compositor::Windows, Compositor::Gamescope] {
                assert!(!cursor_blend_for(
                    cursor_forward,
                    compositor,
                    Codec::H265,
                    10,
                    true,
                    None
                ));
                assert!(!gamescope_cursor_for(
                    compositor == Compositor::Gamescope,
                    None
                ));
            }
        }
    }
}
