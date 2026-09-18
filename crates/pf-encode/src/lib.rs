//! Hardware video encode. Binds the vendor SDKs; never rewrites codecs.
//! Low-latency preset, B-frames off.
//!
//! One [`Encoder`] trait, selected in [`open_video`]. Per-GPU backends: NVENC
//! (NVIDIA; GPU RGB→YUV, no host CSC), VAAPI (AMD/Intel; CPU RGB→NV12 or
//! dmabuf into a VA surface), plus optional Vulkan Video, direct-SDK NVENC,
//! AMF, QSV, PyroWave, and software openh264.
//!
//! Capture→encode is one-way: this crate depends on `pf-frame` and
//! `pf-zerocopy`, never on capture. Pin with `PUNKTFUNK_ENCODER`.
//! Evidence: `design/linux-direct-nvenc.md`, `design/linux-vulkan-video-encode.md`,
//! `design/native-amf-encoder.md`, `design/native-qsv-encoder.md`.

use anyhow::Result;
use pf_frame::{CapturedFrame, PixelFormat};

// The Windows backends, the `Encoder` contract, and the pieces the Linux
// backends share with them. One namespace: `pf_encode::*` is unchanged.
pub use pf_encode_win::*;

/// `quic::CODEC_*` bit → [`Codec`]. Unknown / `0` maps to HEVC (pre-negotiation
/// default). Inverse of [`codec_to_wire`].
pub fn codec_from_wire(bit: u8) -> Codec {
    match bit {
        punktfunk_core::quic::CODEC_H264 => Codec::H264,
        punktfunk_core::quic::CODEC_AV1 => Codec::Av1,
        punktfunk_core::quic::CODEC_PYROWAVE => Codec::PyroWave,
        _ => Codec::H265,
    }
}

pub fn codec_to_wire(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => punktfunk_core::quic::CODEC_H264,
        Codec::H265 => punktfunk_core::quic::CODEC_HEVC,
        Codec::Av1 => punktfunk_core::quic::CODEC_AV1,
        Codec::PyroWave => punktfunk_core::quic::CODEC_PYROWAVE,
    }
}

/// HEVC `chroma_format_idc`: `1` (4:2:0) or `3` (4:4:4). Same numeric
/// value as [`punktfunk_core::quic::Welcome::chroma_format`].
pub fn chroma_idc(chroma: ChromaFormat) -> u8 {
    match chroma {
        ChromaFormat::Yuv420 => punktfunk_core::quic::CHROMA_IDC_420,
        ChromaFormat::Yuv444 => punktfunk_core::quic::CHROMA_IDC_444,
    }
}

/// Wire volume ([`punktfunk_core::quic::HdrMeta`]) → the encoders'
/// [`pf_frame::HdrMeta`]. Same seven fields; both types are foreign here, so
/// a field copy stands in for `From`.
pub fn hdr_meta_from_wire(m: punktfunk_core::quic::HdrMeta) -> pf_frame::HdrMeta {
    pf_frame::HdrMeta {
        display_primaries: m.display_primaries,
        white_point: m.white_point,
        max_display_mastering_luminance: m.max_display_mastering_luminance,
        min_display_mastering_luminance: m.min_display_mastering_luminance,
        max_cll: m.max_cll,
        max_fall: m.max_fall,
    }
}

/// Inverse of [`hdr_meta_from_wire`], for the `0xCE` datagram.
pub fn hdr_meta_to_wire(m: pf_frame::HdrMeta) -> punktfunk_core::quic::HdrMeta {
    punktfunk_core::quic::HdrMeta {
        display_primaries: m.display_primaries,
        white_point: m.white_point,
        max_display_mastering_luminance: m.max_display_mastering_luminance,
        min_display_mastering_luminance: m.min_display_mastering_luminance,
        max_cll: m.max_cll,
        max_fall: m.max_fall,
    }
}

/// `quic::CODEC_*` bits this host can emit on the native path, given the
/// `quic::CODEC_*` bits this host can emit on the native path, given the
/// resolved backend. Fed to [`punktfunk_core::quic::resolve_codec`].
///
/// Software is H.264 only. Probed backends advertise what the GPU encodes
/// ([`vaapi_codec_support`] / [`windows_codec_support`]); NVENC falls back to
/// the GameStream superset when the probe cannot answer. An empty probe
/// means the GPU was unusable at probe time, not that it encodes nothing —
/// fall back to the superset so auto clients still land on HEVC.
pub fn host_wire_caps() -> u8 {
    // PyroWave ORs onto the H.26x set; `resolve_codec` ignores the bit unless
    // the client prefers it. Advertised whenever a Vulkan GPU could open;
    // software/GPU-less keeps it off. Resolve the backend once — this path
    // is polled, and the auto arm samples live GPU-preference state.
    #[cfg(target_os = "linux")]
    let backend = linux_resolved_backend();
    #[cfg(all(target_os = "linux", feature = "pyrowave"))]
    let pyro = if backend != LinuxBackend::Software {
        punktfunk_core::quic::CODEC_PYROWAVE
    } else {
        0u8
    };
    // Own Vulkan device by render-GPU id; the H.26x backend is irrelevant.
    // Software/GPU-less keeps the bit off. Interop is confirmed at encoder
    // open (`pyrowave_device_confirm_interop_support`); a failed open
    // renegotiates to HEVC.
    #[cfg(all(target_os = "windows", feature = "pyrowave"))]
    let pyro = if windows_resolved_backend() != WindowsBackend::Software {
        punktfunk_core::quic::CODEC_PYROWAVE
    } else {
        0u8
    };
    #[cfg(not(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave")))]
    let pyro = 0u8;
    let base = 'base: {
        /// GameStream `SERVER_CODEC_MODE_SUPPORT` for an unprobed backend.
        const GPU_SUPERSET: u8 = punktfunk_core::quic::CODEC_H264
            | punktfunk_core::quic::CODEC_HEVC
            | punktfunk_core::quic::CODEC_AV1;
        #[cfg(target_os = "linux")]
        {
            let _ = GPU_SUPERSET;
            // Shared with `/serverinfo`, so the two advertisements cannot drift.
            break 'base codec_support_wire_mask(linux_advertised_codec_support_for(backend))
                .unwrap_or(0);
        }
        #[cfg(target_os = "windows")]
        {
            if windows_resolved_backend() == WindowsBackend::Software {
                break 'base punktfunk_core::quic::CODEC_H264;
            }
            if windows_backend_is_probed() {
                if let Some(m) = codec_support_wire_mask(windows_codec_support()) {
                    break 'base m;
                }
            }
            GPU_SUPERSET
        }
        // No GPU encode backend on this target — keep the unprobed advertisement.
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = GPU_SUPERSET;
            if matches!(
                pf_host_config::config().encoder_pref.as_str(),
                "software" | "sw" | "openh264"
            ) {
                break 'base punktfunk_core::quic::CODEC_H264;
            }
            punktfunk_core::quic::CODEC_HEVC
        }
    };
    base | pyro
}

/// Open a hardware encoder for `format` and mode. NVENC on NVIDIA, VAAPI on
/// AMD/Intel. `cuda` is GPU frames (`AV_PIX_FMT_CUDA`) from the NVIDIA
/// zero-copy path; otherwise packed RGB/BGR CPU frames. The caller derives
/// `cuda` from the first captured frame. Linux auto-detects; override with
/// `PUNKTFUNK_ENCODER=auto|nvenc|vaapi`.
#[allow(clippy::too_many_arguments)]
pub fn open_video(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    // Backends whose fast path can't blend (Vulkan EFC) key off `cursor_blend`.
    cursor_blend: bool,
    // Client decoder slice ceiling. 1 = single-slice (some TVs wedge on
    // multi-slice AUs); 32 = no client limit. `PUNKTFUNK_NVENC_SLICES` overrides.
    max_slices: u32,
) -> Result<Box<dyn Encoder>> {
    let bitrate_bps = bitrate_bps.max(MIN_BITRATE_BPS);
    let (inner, backend) = open_video_backend(
        codec,
        format,
        width,
        height,
        fps,
        bitrate_bps,
        cuda,
        bit_depth,
        chroma,
        cursor_blend,
        max_slices,
    )?;
    // Open-time fallback (Vulkan→VAAPI) and gamescope (no embedded cursor) still
    // reach here. `open_video` cannot re-plan capture, so a warning is all it does.
    if cursor_blend && !inner.caps().blends_cursor {
        tracing::warn!(
            backend,
            "session negotiated a composited cursor but this encode backend does not blend \
             CapturedFrame::cursor — the pointer will be MISSING from the stream unless the \
             capturer composites it"
        );
    }
    Ok(track_session(inner, backend))
}

/// Tie `inner` to a `pf_gpu` live-session record labelled `backend` — the label of the
/// branch that opened, not re-derived (Vulkan Video falls back to VAAPI, and a dispatch mirror
/// would report the wrong one). GPU identity is [`pf_gpu::selected_gpu`]; dropping the returned
/// encoder ends the record. Public for encoders opened outside [`open_video`] (the Windows
/// driver proxy).
pub fn track_session(inner: Box<dyn Encoder>, backend: &'static str) -> Box<dyn Encoder> {
    let gpu = if backend == "software" {
        pf_gpu::ActiveGpu {
            id: String::new(),
            name: "CPU (openh264)".into(),
            vendor_id: 0,
            backend,
        }
    } else {
        match pf_gpu::selected_gpu() {
            Some(sel) => pf_gpu::ActiveGpu {
                id: sel.info.id,
                name: sel.info.name,
                vendor_id: sel.info.vendor_id,
                backend,
            },
            None => pf_gpu::ActiveGpu {
                id: String::new(),
                name: "GPU".into(),
                vendor_id: 0,
                backend,
            },
        }
    };
    Box::new(TrackedEncoder {
        inner,
        _session: pf_gpu::session_begin(gpu),
    })
}

/// Ties the `pf_gpu` live-session record to the encoder's lifetime; pure delegation
/// otherwise.
struct TrackedEncoder {
    inner: Box<dyn Encoder>,
    _session: pf_gpu::ActiveSession,
}

impl Encoder for TrackedEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        self.inner.submit(frame)
    }
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.inner.submit_indexed(frame, wire_index)
    }
    fn caps(&self) -> EncoderCaps {
        self.inner.caps()
    }
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe()
    }
    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.inner.set_hdr_meta(meta)
    }
    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        self.inner.invalidate_ref_frames(first_frame, last_frame)
    }
    fn distrust_references(&mut self) {
        self.inner.distrust_references()
    }
    fn set_pipelined(&mut self, on: bool) -> bool {
        self.inner.set_pipelined(on)
    }
    fn set_wire_chunking(&mut self, shard_payload: usize) {
        self.inner.set_wire_chunking(shard_payload)
    }
    fn set_send_spread_us(&mut self, us: u32) {
        self.inner.set_send_spread_us(us)
    }
    fn set_input_ring_depth(&mut self, depth: usize) {
        self.inner.set_input_ring_depth(depth)
    }
    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        self.inner.set_input_crop(rect)
    }
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.inner.poll()
    }
    fn ready_event(&self) -> Option<isize> {
        self.inner.ready_event()
    }
    fn supports_chunked_poll(&self) -> bool {
        self.inner.supports_chunked_poll()
    }
    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        self.inner.poll_chunk()
    }
    fn ready_aus(&mut self, deadline: std::time::Instant) -> Option<usize> {
        self.inner.ready_aus(deadline)
    }
    fn telemetry(&self) -> Option<pf_frame::health::EncoderTelemetry> {
        self.inner.telemetry()
    }
    fn reset(&mut self) -> bool {
        self.inner.reset()
    }
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        self.inner.reconfigure_bitrate(bps.max(MIN_BITRATE_BPS))
    }
    fn applied_bitrate_bps(&self) -> Option<u64> {
        self.inner.applied_bitrate_bps()
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// No backend clamps a bitrate of 0: an AMF rebuild fails its `TargetBitrate`
/// and NVENC's bisect lands at 10 Mbps. One floor here, at the trait boundary,
/// matching the host's own 500 kbps ABR floor.
pub const MIN_BITRATE_BPS: u64 = 500_000;

/// openh264 rate-control misconfigures if handed a hardware-session bitrate.
#[cfg(target_os = "linux")]
const SW_BITRATE_CEIL: u64 = 100_000_000;

/// Linux half of [`open_video_backend`] with the pref injected. `set_var`
/// races `getenv` in parallel tests, and `pf_host_config::config()` latches
/// once. Shared dim/fps/chroma checks stay in the caller.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn open_video_backend_linux(
    pref: &str,
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    // Negotiated PyroWave bypasses `PUNKTFUNK_ENCODER` (that pref is a lab override).
    if codec == Codec::PyroWave {
        #[cfg(feature = "pyrowave")]
        {
            // Worker seam, not the encoder: GPU-priority needs `CAP_SYS_NICE`,
            // which only `punktfunk-encode-worker` may carry. See `pyrowave_remote`.
            return pyrowave_remote::open_preferring_worker(
                width,
                height,
                fps,
                bitrate_bps,
                chroma,
            )
            .map(|e| (e, "pyrowave"));
        }
        #[cfg(not(feature = "pyrowave"))]
        anyhow::bail!(
            "session negotiated PyroWave but this host was built without --features \
             punktfunk-host/pyrowave (the advertisement bit should not have been set)"
        );
    }
    // Default VAAPI. With `vulkan-encode` + `PUNKTFUNK_VULKAN_ENCODE`, HEVC/AV1
    // opens Vulkan Video first; a failed open falls back
    // so the stream does not die. `format`/`bit_depth`/`chroma` are VAAPI-only
    // — Vulkan imports the dmabuf and does its own CSC.
    let open_amd_intel = || -> Result<(Box<dyn Encoder>, &'static str)> {
        // HDR keeps Vulkan when the device probe says yes (same profile query
        // the open makes). Gamescope has no embedded cursor — CSC blend is the
        // only pointer path. A `no` goes to VAAPI here, not a failed open.
        #[cfg(feature = "vulkan-encode")]
        let is_hdr = format.is_hdr();
        // 10-bit SDR (8-bit capture, depth 10). HEVC stays on VAAPI (Main10 under BT.709); AV1 has
        // no VAAPI path, so it takes Vulkan with the depth forced (Vulkan gets `bit_depth`) and the
        // BT.709 colour axis (`rgb2yuv10_709.comp`).
        #[cfg(feature = "vulkan-encode")]
        let sdr10 = bit_depth == 10 && !is_hdr;
        // Depth Vulkan opens at: HDR-10, or AV1 10-bit SDR; else 8-bit.
        #[cfg(feature = "vulkan-encode")]
        let vk_ten_bit = bit_depth == 10 && (is_hdr || codec == Codec::Av1);
        #[cfg(feature = "vulkan-encode")]
        if !(sdr10 && codec == Codec::H265)
            && matches!(codec, Codec::H265 | Codec::Av1)
            && vulkan_encode_enabled()
            && vulkan_encode_available_at(codec, vk_ten_bit)
        {
            match vulkan_video::VulkanVideoEncoder::open(
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                cursor_blend,
                bit_depth,
            ) {
                Ok(e) => {
                    tracing::info!(
                        codec = ?codec,
                        "Linux Vulkan Video encode (real RFI via DPB reference slots) — \
                         set PUNKTFUNK_VULKAN_ENCODE=0 for VAAPI"
                    );
                    return Ok((Box::new(e) as Box<dyn Encoder>, "vulkan"));
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "Vulkan Video encode open failed — falling back to VAAPI"
                ),
            }
        }
        // The native session takes every capture shape, NV12 dmabufs included.
        // H.264 and HEVC; AV1 on AMD/Intel is Vulkan Video's. HDR is the packed 10-bit or P010
        // capture; 10-bit SDR arrives as an 8-bit surface at depth 10.
        vaapi_native::NativeVaapiEncoder::open(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            bit_depth,
            chroma,
            format.is_hdr(),
        )
        .map(|e| (Box::new(e) as Box<dyn Encoder>, "vaapi-native"))
    };
    let open_nvidia = || -> Result<(Box<dyn Encoder>, &'static str)> {
        open_nvenc(
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        )
        .map(|e| (e, "nvenc"))
    };
    // Same resolver the capability mirrors consult, so the alias table exists once.
    match resolve_linux_backend(pref, linux_auto_is_vaapi, cuda) {
        Some(LinuxBackend::Nvenc) => open_nvidia(),
        Some(LinuxBackend::AmdIntel) => open_amd_intel(),
        Some(LinuxBackend::Vulkan) => {
            #[cfg(feature = "vulkan-encode")]
            {
                if !matches!(codec, Codec::H265 | Codec::Av1) {
                    anyhow::bail!(
                        "the Vulkan Video encoder supports HEVC + AV1; the session negotiated {codec:?}"
                    );
                }
                vulkan_video::VulkanVideoEncoder::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    cursor_blend,
                    bit_depth,
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "vulkan"))
            }
            #[cfg(not(feature = "vulkan-encode"))]
            {
                let _ = (format, bit_depth, chroma);
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=vulkan requires a build with --features vulkan-encode"
                )
            }
        }
        // Explicit lab override; ignores the negotiated codec (every AU is intra).
        Some(LinuxBackend::Pyrowave) => {
            #[cfg(feature = "pyrowave")]
            {
                tracing::warn!(
                    ?codec,
                    "PUNKTFUNK_ENCODER=pyrowave forces the all-intra wavelet stream \
                     regardless of the negotiated codec — only a pyrowave-feature client \
                     that ALSO preferred CODEC_PYROWAVE can display it (lab override; \
                     normal sessions negotiate it instead)"
                );
                // Forced onto a session negotiated for another codec, whose
                // chroma may be HEVC 4:4:4 — PyroWave does not. Same worker
                // seam as the negotiated arm; this must not skip it.
                pyrowave_remote::open_preferring_worker(
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    ChromaFormat::Yuv420,
                )
                .map(|e| (e, "pyrowave"))
            }
            #[cfg(not(feature = "pyrowave"))]
            {
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=pyrowave requires a build with --features punktfunk-host/pyrowave"
                )
            }
        }
        // Explicit-only: `auto` never picks it (a dead NVIDIA driver still
        // exposes `/dev/nvidiactl` and would resolve to NVENC). H.264 + CPU RGB.
        Some(LinuxBackend::Software) => {
            if codec != Codec::H264 {
                anyhow::bail!(
                    "the software encoder emits H.264 only; the session negotiated {codec:?} \
                     (a client must advertise CODEC_H264 to reach a software host)"
                );
            }
            let _ = (cuda, bit_depth); // software path is CPU + 8-bit only
            sw::OpenH264Encoder::open(
                format,
                width,
                height,
                fps,
                bitrate_bps.min(SW_BITRATE_CEIL),
            )
            .map(|e| (Box::new(e) as Box<dyn Encoder>, "software"))
        }
        None => anyhow::bail!(
            "unknown PUNKTFUNK_ENCODER={pref:?} — use auto (default), nvenc, vaapi, vulkan, pyrowave, or software"
        ),
    }
}

/// Open the platform encoder. The display label is the branch that opened
/// (`nvenc`/`vaapi`/`vulkan`/`amf`/`qsv`/`software`), including internal
/// fallbacks (Vulkan Video → VAAPI). Feeds the mgmt live-session record.
#[allow(clippy::too_many_arguments)]
fn open_video_backend(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    // Linux vulkan-encode + direct-NVENC only (`max_slices`: the splitter).
    let _ = (cursor_blend, max_slices);
    validate_dimensions(codec, width, height)?;
    // `fps` is `Rational(1, fps)` and `pts * 1e9 / fps`. 0 is a 1/0 rational
    // and a divide-by-zero; 1000 is a sanity ceiling.
    if fps == 0 || fps > 1000 {
        anyhow::bail!("invalid refresh/fps {fps}: must be 1..=1000 Hz");
    }
    // 4:4:4 is HEVC- and PyroWave-only. Degrade rather than emit a stream no
    // decoder expects.
    let chroma = if chroma.is_444() && codec != Codec::H265 && codec != Codec::PyroWave {
        tracing::warn!(
            ?codec,
            "4:4:4 requested for a non-HEVC codec — encoding 4:2:0"
        );
        ChromaFormat::Yuv420
    } else {
        chroma
    };
    #[cfg(target_os = "linux")]
    {
        open_video_backend_linux(
            pf_host_config::config().encoder_pref.as_str(),
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        )
    }
    #[cfg(target_os = "windows")]
    {
        // The pf-vdisplay driver holds the only Windows encoder: it opens the backend on the
        // pooled device inside WUDFHost and publishes access units into the session's AU
        // section, which `pf_capture::open_driver_encoder` wraps as the loop's `Encoder`.
        // Reaching here means a Windows session resolved to a non-IDD-push capture source,
        // which no longer exists.
        let _ = (
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
        );
        anyhow::bail!(
            "on Windows the pf-vdisplay driver encodes; the host opens no local video encoder \
             (the session must come from the IDD-push capture source)"
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            max_slices,
        );
        anyhow::bail!("video encode requires Linux or Windows")
    }
}

/// NVIDIA: the direct-SDK session (`--features nvenc`), which takes CUDA
/// frames zero-copy and uploads CPU frames itself. Without it there is no NVENC.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn open_nvenc(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
) -> Result<Box<dyn Encoder>> {
    #[cfg(feature = "nvenc")]
    {
        tracing::info!(codec = codec.label(), cuda, "Linux direct-SDK NVENC");
        Ok(Box::new(nvenc_cuda::NvencCudaEncoder::open(
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        )?) as Box<dyn Encoder>)
    }
    #[cfg(not(feature = "nvenc"))]
    {
        let _ = (
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        );
        anyhow::bail!(
            "{} on NVIDIA needs the direct-SDK NVENC backend, which this build left out — \
             build with --features punktfunk-host/nvenc",
            codec.label()
        )
    }
}

/// Vulkan Video HEVC/AV1 on AMD/Intel. Default on.
/// `PUNKTFUNK_VULKAN_ENCODE=0` (`false`/`no`/`off`) is the VAAPI hatch.
/// A failed open falls back to VAAPI. See `design/linux-vulkan-video-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_enabled() -> bool {
    pf_host_config::knob("PUNKTFUNK_VULKAN_ENCODE")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

/// Whether this session can ingest a producer's own NV12 without a host pass. Both AMD/Intel
/// lanes can: Vulkan Video imports it as its picture, the native libva session encodes it as
/// imported. AV1 is Vulkan Video's alone. The NVENC lane's fused convert reads RGB only.
#[cfg(target_os = "linux")]
pub fn linux_native_nv12_ok(codec: Codec) -> bool {
    if !linux_zero_copy_is_vaapi() {
        return false;
    }
    match codec {
        Codec::H264 | Codec::H265 => true,
        #[cfg(feature = "vulkan-encode")]
        Codec::Av1 => vulkan_encode_enabled() && vulkan_encode_available(codec),
        _ => false,
    }
}

/// May the capture hand the NVENC lane its held dmabufs? The encoder's zero-copy worker then
/// converts each straight into a registered input slot (`PUNKTFUNK_NVENC_RAW`). Off without
/// direct-SDK NVENC, on the VAAPI plane, or once the raw-dmabuf latch tripped.
#[cfg(target_os = "linux")]
pub fn linux_nvenc_raw_dmabuf_ok() -> bool {
    #[cfg(feature = "nvenc")]
    {
        !linux_zero_copy_is_vaapi()
            && pf_zerocopy::nvenc_raw_enabled()
            && !pf_zerocopy::raw_dmabuf_import_disabled()
            && pf_zerocopy::fused_convert_available()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// May an HDR capture stay zero-copy on NVIDIA (packed 10-bit PQ/BT.2020 CUDA)?
///
/// Only direct-SDK NVENC can: it registers `ARGB10`/`ABGR10` and CSCs in the
/// encoder. When it is compiled out, the capturer must not build the HDR
/// importer.
#[cfg(target_os = "linux")]
pub fn linux_hdr_cuda_ok() -> bool {
    #[cfg(feature = "nvenc")]
    {
        !linux_zero_copy_is_vaapi()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// Whether the resolved backend composites [`CapturedFrame::cursor`]. Answered
/// before capture opens: blend-capable backends take cursor-as-metadata; else
/// the compositor must embed the pointer.
///
/// `cuda_planned` is the caller's CUDA-payload prediction; `ten_bit` the
/// negotiated depth. A CPU payload is uploaded, and never blended.
/// 10-bit keeps Vulkan Video only where the device advertises that profile
/// (`vulkan_encode_available_at`, the same query the open makes).
#[cfg(target_os = "linux")]
pub fn cursor_blend_capable(codec: Codec, cuda_planned: bool, ten_bit: bool) -> bool {
    // Negotiated PyroWave is selected before the pref; its CSC composites the cursor.
    if codec == Codec::PyroWave {
        return true;
    }
    let direct_nvenc = cfg!(feature = "nvenc");
    let vulkan_csc = {
        // Compute-CSC arm (the one that blends). Probe last: it opens a Vulkan instance.
        #[cfg(feature = "vulkan-encode")]
        {
            // Same as `open_amd_intel`, depth included, so prediction and open agree.
            matches!(codec, Codec::H265 | Codec::Av1)
                && vulkan_encode_enabled()
                && vulkan_encode_available_at(codec, ten_bit)
        }
        #[cfg(not(feature = "vulkan-encode"))]
        {
            let _ = ten_bit; // the depth only ever narrows the Vulkan arm
            false
        }
    };
    let backend = resolve_linux_backend(
        pf_host_config::config().encoder_pref.as_str(),
        linux_auto_is_vaapi,
        cuda_planned,
    );
    cursor_blend_capable_for(backend, cuda_planned, direct_nvenc, vulkan_csc)
}

/// Dispatch-mirroring core of [`cursor_blend_capable`], device-free for tests.
/// `direct_nvenc` / `vulkan_csc` are the blend-capable arms, already gated.
#[cfg(target_os = "linux")]
fn cursor_blend_capable_for(
    backend: Option<LinuxBackend>,
    cuda_planned: bool,
    direct_nvenc: bool,
    vulkan_csc: bool,
) -> bool {
    match backend {
        Some(LinuxBackend::Pyrowave) => true,
        // Direct-SDK only (VkSlotBlend), and only a CUDA payload: an uploaded
        // CPU frame arrives after the blend point.
        Some(LinuxBackend::Nvenc) => cuda_planned && direct_nvenc,
        // Compute-CSC blends at either depth. Cursor sessions stay off native-NV12
        // / RGB-direct, so CSC eligibility (already carrying depth) is the answer.
        Some(LinuxBackend::AmdIntel) | Some(LinuxBackend::Vulkan) => vulkan_csc,
        // Capturer may composite inline; the encoder does not. Report encoder truth.
        Some(LinuxBackend::Software) | None => false,
    }
}

/// Can this GPU open a Vulkan Video encode session for `codec`? Cached per
/// (selected GPU, codec); probe runs outside the lock.
///
/// Only [`linux_native_nv12_ok`] consults this (no-fallback). Not wired into
/// [`open_video`]: non-NV12 already degrades to VAAPI on a failed open.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_available(codec: Codec) -> bool {
    vulkan_encode_caps(codec).supported
}

/// Can Vulkan Video encode `codec` at this depth on the selected GPU? Per
/// codec: a device can advertise HEVC Main10 and still decline 10-bit AV1.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_available_at(codec: Codec, ten_bit: bool) -> bool {
    let caps = vulkan_encode_caps(codec);
    caps.supported
        && if ten_bit {
            caps.ten_bit
        } else {
            caps.eight_bit
        }
}

/// Vulkan Video encode caps for `codec`, cached per (selected GPU, codec) so
/// a console GPU change re-probes. The probe opens its own Vulkan instance.
///
/// Cfg must include `vulkan-encode`: the return type lives in `vulkan_video`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_caps(codec: Codec) -> vulkan_video::VulkanEncodeCaps {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), vulkan_video::VulkanEncodeCaps>>> =
        OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let caps = vulkan_video::probe_encode_caps(codec);
    if caps.supported {
        tracing::info!(
            ?codec,
            eight_bit = caps.eight_bit,
            ten_bit = caps.ten_bit,
            "Vulkan Video encode probed — producer-native NV12 capture is eligible, and a 10-bit \
             session keeps this backend (real RFI + the compute CSC's cursor blend) where the \
             device accepts the profile"
        );
    } else {
        tracing::info!(
            ?codec,
            "Vulkan Video encode unavailable (no encode queue for this codec) — keeping the \
             packed-RGB capture negotiation (the native-NV12 path has no VAAPI fallback)"
        );
    }
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// NVIDIA-presence for the `auto` selector: these device nodes, no CUDA
/// context (that would allocate GPU state on every maybe-NVIDIA host).
#[cfg(target_os = "linux")]
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// The `auto` Linux backend decision, shared by [`open_video`] and
/// [`linux_zero_copy_is_vaapi`]. Manual GPU preference picks that vendor's
/// backend (NVIDIA still needs the proprietary device nodes); else the
/// presence probe. A build without `nvenc` carries no NVIDIA arm at all, so it
/// always answers VAAPI — whose Vulkan Video leg the NVIDIA driver serves too.
///
/// Resolves **`auto` only** — ignores `encoder_pref`. Capability probes must
/// use [`linux_zero_copy_is_vaapi`], which layers the pref on top.
#[cfg(target_os = "linux")]
fn linux_auto_is_vaapi() -> bool {
    if !cfg!(feature = "nvenc") {
        return true;
    }
    if let Some(g) = pf_gpu::manual_selection() {
        if g.vendor_id == pf_gpu::VENDOR_NVIDIA {
            return !nvidia_present();
        }
        return true;
    }
    !nvidia_present()
}

/// Resolved Linux encode backend. One alias table for [`open_video_backend`]
/// and every capability/advertisement mirror. Labels come from the open
/// sites — this picks which arm runs, not what actually opened.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxBackend {
    Nvenc,
    AmdIntel,
    Vulkan,
    Pyrowave,
    Software,
}

/// Pure core. `None` = unknown pref: [`open_video_backend`] bails, capability
/// mirrors map it to auto via [`linux_resolved_backend`]. `auto_is_vaapi` is
/// lazy — `/serverinfo` polls these mirrors; explicit prefs must not probe.
#[cfg(target_os = "linux")]
fn resolve_linux_backend(
    pref: &str,
    auto_is_vaapi: impl FnOnce() -> bool,
    cuda: bool,
) -> Option<LinuxBackend> {
    Some(match pref {
        "nvenc" | "nvidia" | "cuda" => LinuxBackend::Nvenc,
        "vaapi" | "amd" | "intel" | "vaapi-native" => LinuxBackend::AmdIntel,
        "vulkan" | "vulkan-video" => LinuxBackend::Vulkan,
        "pyrowave" => LinuxBackend::Pyrowave,
        "software" | "sw" | "openh264" => LinuxBackend::Software,
        // A CUDA frame can only be consumed by NVENC; else [`linux_auto_is_vaapi`].
        "auto" | "" => {
            if cuda || !auto_is_vaapi() {
                LinuxBackend::Nvenc
            } else {
                LinuxBackend::AmdIntel
            }
        }
        _ => return None,
    })
}

/// Capability-mirror wrapper: unknown pref → auto. `cuda = false` because
/// these answer pre-session questions.
#[cfg(target_os = "linux")]
fn linux_resolved_backend() -> LinuxBackend {
    let pref = pf_host_config::config().encoder_pref.as_str();
    resolve_linux_backend(pref, linux_auto_is_vaapi, false).unwrap_or_else(|| {
        if linux_auto_is_vaapi() {
            LinuxBackend::AmdIntel
        } else {
            LinuxBackend::Nvenc
        }
    })
}

/// Dmabuf modifiers PyroWave's Vulkan device imports for the capture fourcc.
/// VAAPI LINEAR-only starves tiled Mutter+NVIDIA allocations.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
pub fn pyrowave_capture_modifiers(fourcc: u32) -> Vec<u64> {
    pyrowave::capture_modifiers(fourcc)
}

/// True if the Linux GPU backend is VAAPI rather than NVENC — so capture
/// picks dmabuf passthrough vs EGL→CUDA. Mirrors [`open_video`].
#[cfg(target_os = "linux")]
pub fn linux_zero_copy_is_vaapi() -> bool {
    linux_zero_copy_is_vaapi_for(linux_resolved_backend())
}
/// Zero-copy plane for an already-resolved backend, so a polled caller
/// (`host_wire_caps`) pays for resolution once.
#[cfg(target_os = "linux")]
fn linux_zero_copy_is_vaapi_for(backend: LinuxBackend) -> bool {
    match backend {
        LinuxBackend::Nvenc => false,
        LinuxBackend::AmdIntel => true,
        // Raw dmabuf on any vendor — never the EGL→CUDA import.
        LinuxBackend::Pyrowave => true,
        // Preserved `_` fallthrough, not endorsed. Vulkan-on-NVIDIA is a known
        // mismatch (EGL→CUDA for a dmabuf importer); software is latent (H.264).
        LinuxBackend::Vulkan | LinuxBackend::Software => linux_auto_is_vaapi(),
    }
}

/// `quic::CODEC_*` bits of a [`CodecSupport`] probe, or `None` when it found
/// nothing — GPU unusable at probe time, not "zero codecs". Caller falls back
/// to the static superset.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn codec_support_wire_mask(caps: CodecSupport) -> Option<u8> {
    let mut m = 0u8;
    if caps.h264 {
        m |= punktfunk_core::quic::CODEC_H264;
    }
    if caps.h265 {
        m |= punktfunk_core::quic::CODEC_HEVC;
    }
    if caps.av1 {
        m |= punktfunk_core::quic::CODEC_AV1;
    }
    (m != 0).then_some(m)
}

/// NVIDIA encode-GUID list (cached once per process). Process-wide because
/// the direct backend opens on shared `cuda::context()` (device 0). Fail-open
/// contract: see [`nvenc_cuda::probe_support`].
#[cfg(all(target_os = "linux", feature = "nvenc"))]
pub fn nvenc_codec_support() -> CodecSupport {
    use std::sync::OnceLock;
    static LOGGED: OnceLock<()> = OnceLock::new();
    let probed = nvenc_cuda::probe_support();
    LOGGED.get_or_init(|| {
        tracing::info!(
            h264 = probed.codecs.h264,
            h265 = probed.codecs.h265,
            av1 = probed.codecs.av1,
            hevc_444 = probed.hevc_444,
            "NVENC encode capabilities probed"
        );
    });
    probed.codecs
}

/// What the AMD/Intel plane can encode: the native VAAPI session for H.264 and
/// HEVC, Vulkan Video for HEVC and AV1 — the two arms [`open_video`] tries, so
/// nothing is advertised that would die at open. NVIDIA uses
/// [`nvenc_codec_support`]; callers gate on [`linux_zero_copy_is_vaapi`].
///
/// Cached per selected GPU, like [`windows_codec_support`]: a console
/// preference change moves the render node and the Vulkan device both probes
/// open.
#[cfg(target_os = "linux")]
pub fn vaapi_codec_support() -> CodecSupport {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, CodecSupport>>> = OnceLock::new();
    let key = pf_gpu::selection_key();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(c) = cache.lock().unwrap().get(&key) {
        return *c;
    }
    // The same query the open makes, at the depth it opens with. AV1 has no
    // native VAAPI path at all, so this is the only thing that can say yes.
    let vulkan = |c| {
        #[cfg(feature = "vulkan-encode")]
        {
            vulkan_encode_enabled() && vulkan_encode_available_at(c, false)
        }
        #[cfg(not(feature = "vulkan-encode"))]
        {
            let _: Codec = c;
            false
        }
    };
    let probe = |c| vaapi_native::probe_can_encode(c, false);
    let caps = CodecSupport {
        h264: probe(Codec::H264),
        h265: probe(Codec::H265) || vulkan(Codec::H265),
        av1: vulkan(Codec::Av1),
    };
    tracing::info!(
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "VAAPI encode capabilities probed"
    );
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// The codec set a Linux host may advertise: the resolved backend's probe,
/// narrowed to what that backend can open.
#[cfg(target_os = "linux")]
pub fn linux_advertised_codec_support() -> CodecSupport {
    linux_advertised_codec_support_for(linux_resolved_backend())
}

/// [`linux_advertised_codec_support`] for an already-resolved backend, so a
/// polled caller pays for resolution once.
#[cfg(target_os = "linux")]
fn linux_advertised_codec_support_for(backend: LinuxBackend) -> CodecSupport {
    let probed = match backend {
        // openh264, and it never probes.
        LinuxBackend::Software => None,
        _ if linux_zero_copy_is_vaapi_for(backend) => Some(vaapi_codec_support()),
        // Driver GUID list, like the VAAPI arm.
        #[cfg(feature = "nvenc")]
        LinuxBackend::Nvenc => Some(nvenc_codec_support()),
        _ => None,
    };
    narrow_to_openable(backend, probed)
}

/// A probe narrowed by what `backend` can open at all. An all-`false` probe
/// means the GPU was unusable at probe time, not that it encodes nothing, so
/// it fails open to the superset — narrowing can then only take codecs away.
/// Pure, so the ceilings are testable without a GPU.
#[cfg(target_os = "linux")]
fn narrow_to_openable(backend: LinuxBackend, probed: Option<CodecSupport>) -> CodecSupport {
    // A forced-vulkan pref is a ceiling, never a replacement: the arm encodes
    // HEVC/AV1 only (H.264 dies at open), and only in a build that has it.
    // A static HEVC|AV1 would add AV1 on GPUs whose probe withholds it.
    let ceiling = match backend {
        LinuxBackend::Software => CodecSupport {
            h264: true,
            h265: false,
            av1: false,
        },
        // The resolver knows the pref is vulkan; only this `cfg!` knows the
        // build can open it. Else: advertise-then-die-at-open.
        LinuxBackend::Vulkan => {
            let built = cfg!(feature = "vulkan-encode");
            CodecSupport {
                h264: false,
                h265: built,
                av1: built,
            }
        }
        _ => CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        },
    };
    let caps = probed
        .filter(|c| c.h264 || c.h265 || c.av1)
        .unwrap_or(ceiling);
    CodecSupport {
        h264: caps.h264 && ceiling.h264,
        h265: caps.h265 && ceiling.h265,
        av1: caps.av1 && ceiling.av1,
    }
}

/// Whether the active backend can emit 4:4:4 HEVC. Cached per selected GPU
/// before Welcome. 4:4:4 is HEVC-only; VAAPI/AMF/QSV must be probed, never
/// assumed. Non-HEVC is always `false`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_444(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if codec == Codec::PyroWave {
        // Own RGB→YCbCr CSC from a full-chroma source — no GPU encode probe.
        // See `design/pyrowave-444-hdr.md`.
        return true;
    }
    if codec != Codec::H265 {
        return false;
    }
    // Per selected GPU so a console preference change re-probes.
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let key = pf_gpu::selection_key();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            if linux_zero_copy_is_vaapi() {
                // The native VAAPI session is 4:2:0 only.
                false
            } else {
                // Direct SDK: the driver's `YUV444_ENCODE` cap.
                #[cfg(feature = "nvenc")]
                {
                    nvenc_cuda::probe_support().hevc_444
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    false
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            match windows_resolved_backend() {
                WindowsBackend::Nvenc => {
                    #[cfg(feature = "nvenc")]
                    {
                        nvenc::probe_can_encode_444(codec, pf_gpu::resolve_render_adapter_luid())
                    }
                    #[cfg(not(feature = "nvenc"))]
                    {
                        false
                    }
                }
                // VCN hardware limit — no probe. See `design/native-amf-encoder.md`.
                WindowsBackend::Amf => false,
                // VPL has no 4:4:4 encode query wired; stays an honest `false`.
                WindowsBackend::Qsv => false,
                // No MFT encodes 4:4:4 on any vendor.
                WindowsBackend::MediaFoundation | WindowsBackend::Software => false,
            }
        }
    };
    tracing::info!(supported, "HEVC 4:4:4 encode capability probed");
    cache.lock().unwrap().insert(key, supported);
    supported
}

/// No GPU encode backend on this target — 4:4:4 is never advertised.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_444(_codec: Codec) -> bool {
    false
}

/// Whether the active backend can emit 10-bit for `codec` (HEVC Main10 / AV1).
/// Cached per (GPU, codec) before Welcome, like [`can_encode_444`]. Without
/// this gate `PUNKTFUNK_10BIT` would negotiate 10-bit and then emit 8-bit
/// (label HDR / stream SDR).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_10bit(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if !codec.supports_10bit() {
        return false;
    }
    if codec == Codec::PyroWave {
        // Wavelet is depth-agnostic. HDR CSC exists on the Windows IDD-push
        // path only; Linux capture has no HDR. See `design/pyrowave-444-hdr.md`.
        return cfg!(target_os = "windows");
    }
    // Per (selected GPU, codec) so a console preference change re-probes.
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), bool>>> = OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            // Use [`linux_zero_copy_is_vaapi`] (not [`linux_auto_is_vaapi`]) so
            // `encoder_pref` is honored. AMD/Intel: `open_amd_intel` tries Vulkan
            // then VAAPI — 10-bit is available if either says yes.
            if linux_zero_copy_is_vaapi() {
                let vulkan10 = {
                    #[cfg(feature = "vulkan-encode")]
                    {
                        vulkan_encode_enabled() && vulkan_encode_available_at(codec, true)
                    }
                    #[cfg(not(feature = "vulkan-encode"))]
                    {
                        false
                    }
                };
                vulkan10 || vaapi_native::probe_can_encode(codec, true)
            } else {
                // Same as the 4:4:4 arm: the driver's `10BIT_ENCODE` cap.
                #[cfg(feature = "nvenc")]
                {
                    let t = nvenc_cuda::probe_support().ten_bit;
                    match codec {
                        Codec::H265 => t.h265,
                        Codec::Av1 => t.av1,
                        _ => false,
                    }
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    false
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            match windows_resolved_backend() {
                WindowsBackend::Nvenc => {
                    #[cfg(feature = "nvenc")]
                    {
                        nvenc::probe_can_encode_10bit(codec, pf_gpu::resolve_render_adapter_luid())
                    }
                    #[cfg(not(feature = "nvenc"))]
                    {
                        false
                    }
                }
                WindowsBackend::Amf => {
                    amf::probe_can_encode_10bit(codec, pf_gpu::resolve_render_adapter_luid())
                }
                // Native VPL Query; without the `qsv` feature there is no QSV
                // path, so this stays an honest `false`.
                WindowsBackend::Qsv => {
                    #[cfg(feature = "qsv")]
                    {
                        qsv::probe_can_encode_10bit(codec, pf_gpu::resolve_render_adapter_luid())
                    }
                    #[cfg(not(feature = "qsv"))]
                    {
                        false
                    }
                }
                // 8-bit 4:2:0 only — the MF backend rejects P010 at open.
                WindowsBackend::MediaFoundation | WindowsBackend::Software => false,
            }
        }
    };
    tracing::info!(codec = ?codec, supported, "10-bit encode capability probed");
    cache.lock().unwrap().insert(key, supported);
    supported
}

/// No GPU encode backend on this target — 10-bit is never negotiated.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_10bit(_codec: Codec) -> bool {
    false
}

// Windows backend selection. NVIDIA → NVENC, AMD → AMF, Intel → QSV.
// `auto` uses the selected render adapter so encode matches capture.

// Ungated: the pure reconciliation table is tested on every CI leg, not just Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsBackend {
    Nvenc,
    Amf,
    Qsv,
    /// Media Foundation: any vendor's hardware MFT. Second rung under the native SDKs on
    /// x64, and the only hardware encoder on an Adreno adapter.
    MediaFoundation,
    Software,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

/// PCI vendor a Windows hardware backend can open on (`None` for software).
/// Capture, virtual display, and encoder share one adapter.
pub fn windows_backend_vendor_id(backend: WindowsBackend) -> Option<u32> {
    match backend {
        WindowsBackend::Nvenc => Some(pf_gpu::VENDOR_NVIDIA),
        WindowsBackend::Amf => Some(pf_gpu::VENDOR_AMD),
        WindowsBackend::Qsv => Some(pf_gpu::VENDOR_INTEL),
        // Vendor-agnostic: every x64 vendor and Adreno ship an MFT, so an `mf` pin is
        // never contradicted by the selected adapter.
        WindowsBackend::MediaFoundation | WindowsBackend::Software => None,
    }
}

/// Pure half of [`windows_resolved_backend`]: reconcile an explicit
/// `PUNKTFUNK_ENCODER` pin with the selected adapter. A hardware pin whose
/// vendor contradicts the selected GPU is overridden (`derived` is lazy) —
/// honoring it can only fail. Software has no vendor and is always honored;
/// with no selected GPU a pin is trusted as-is. [`open_video`] warns on override.
pub fn resolve_windows_backend(
    pinned: Option<WindowsBackend>,
    selected_vendor_id: Option<u32>,
    derived: impl FnOnce() -> WindowsBackend,
) -> WindowsBackend {
    match (pinned, selected_vendor_id) {
        (None, _) => derived(),
        (Some(pin), Some(vendor)) => match windows_backend_vendor_id(pin) {
            Some(required) if required != vendor => derived(),
            _ => pin,
        },
        (Some(pin), None) => pin,
    }
}

/// Explicit `PUNKTFUNK_ENCODER` pin. `None` for `auto`, unset, and unknown.
#[cfg(target_os = "windows")]
fn windows_pinned_backend() -> Option<WindowsBackend> {
    // Latched in HostConfig; do not re-read the env.
    match pf_host_config::config().encoder_pref.as_str() {
        "nvenc" | "hw" | "nvidia" | "cuda" => Some(WindowsBackend::Nvenc),
        "amf" | "amd" => Some(WindowsBackend::Amf),
        "qsv" | "intel" => Some(WindowsBackend::Qsv),
        "mf" | "mediafoundation" => Some(WindowsBackend::MediaFoundation),
        "sw" | "software" | "openh264" => Some(WindowsBackend::Software),
        _ => None,
    }
}

/// Has the selected adapter a hardware MFT? Cached per selected GPU, like
/// [`can_encode_444`]: [`windows_resolved_backend`] is uncached and runs on every
/// `/serverinfo` poll, where an `MFTEnum2` walk plus an adapter resolve would block the
/// async executor and log on each one.
#[cfg(target_os = "windows")]
fn mf_has_hardware_encoder() -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let key = pf_gpu::selection_key();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let has = mf::probe_has_hardware_encoder(pf_gpu::resolve_render_adapter_luid());
    cache.lock().unwrap().insert(key, has);
    has
}

/// Active Windows backend. `auto` → selected adapter's vendor; a contradicting
/// pin is overridden ([`resolve_windows_backend`]). Shared with GameStream.
#[cfg(target_os = "windows")]
pub fn windows_resolved_backend() -> WindowsBackend {
    let pinned = windows_pinned_backend();
    // Vendor query only to reconcile a pin — auto stays one inventory walk.
    let selected = if pinned.is_some() {
        pf_gpu::selected_gpu().map(|s| s.info.vendor_id)
    } else {
        None
    };
    resolve_windows_backend(pinned, selected, || match windows_gpu_vendor() {
        Some(GpuVendor::Nvidia) => WindowsBackend::Nvenc,
        Some(GpuVendor::Amd) => WindowsBackend::Amf,
        Some(GpuVendor::Intel) => WindowsBackend::Qsv,
        // No vendor with a native SDK. An adapter that still has a hardware MFT (Adreno)
        // is a GPU backend, and must resolve to one: `Software` here would also flip the
        // capturer to CPU staging, so the D3D11 input MF needs would never materialise.
        None if mf_has_hardware_encoder() => WindowsBackend::MediaFoundation,
        None => WindowsBackend::Software,
    })
}

/// GPU-resident frames (software is the only CPU path). Single source for
/// [`pf_frame::OutputFormat`]'s `gpu` bit — capture must not re-derive it.
#[cfg(target_os = "windows")]
pub fn resolved_backend_is_gpu() -> bool {
    !matches!(windows_resolved_backend(), WindowsBackend::Software)
}
#[cfg(target_os = "linux")]
pub fn resolved_backend_is_gpu() -> bool {
    linux_resolved_backend() != LinuxBackend::Software
}
/// No resolver on this target. `not(any(...))`, never a target list, so no
/// exotic target loses the fn.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn resolved_backend_is_gpu() -> bool {
    !matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    )
}

/// Encoder half of the 4:4:4 capture gate: ingest RGB and CSC to 4:4:4.
/// Only Windows NVENC. Linux 4:4:4 is capture-side (portal RGB → `yuv444p`).
#[cfg(target_os = "windows")]
pub fn resolved_backend_ingests_rgb_444() -> bool {
    windows_resolved_backend() == WindowsBackend::Nvenc
}
#[cfg(not(target_os = "windows"))]
pub fn resolved_backend_ingests_rgb_444() -> bool {
    false
}

/// Encoder half of the 10-bit SDR gate: this backend writes a 10-bit stream from the 8-bit
/// surface an SDR desktop captures.
///
/// Windows direct-NVENC ingests the IDD packed `Rgb10a2Sdr`; Windows AMF takes a BT.709 P010 the
/// driver's video processor produces (HEVC only). Linux direct-NVENC takes the plain 8-bit surface
/// and asks NVENC for 10-bit output. Linux VAAPI carries HEVC and Vulkan Video carries AV1, both
/// with the RGB→YUV matrix following the BT.709 colour, not depth. The GPU still has to pass
/// `can_encode_10bit`.
#[cfg(target_os = "windows")]
pub fn backend_carries_sdr10(codec: Codec) -> bool {
    // NVENC widens 8→10 from packed RGB for HEVC + AV1. AMF takes a BT.709 P010 the driver's video
    // processor produces (`EncodeInput::P010Sdr`) for HEVC Main10 only — AV1 10-bit SDR on AMF is
    // unbuilt. `can_encode_10bit` still gates on the real probe.
    match windows_resolved_backend() {
        WindowsBackend::Nvenc => true,
        WindowsBackend::Amf => codec == Codec::H265,
        _ => false,
    }
}
/// Can Vulkan Video encode `codec` at 10-bit on the selected GPU, and is it enabled? The AV1
/// 10-bit SDR path on AMD/Intel routes here; the probe is cached per (GPU, codec).
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_sdr10_available(codec: Codec) -> bool {
    vulkan_encode_enabled() && vulkan_encode_available_at(codec, true)
}
#[cfg(all(target_os = "linux", not(feature = "vulkan-encode")))]
fn vulkan_sdr10_available(_codec: Codec) -> bool {
    false
}
#[cfg(target_os = "linux")]
pub fn backend_carries_sdr10(codec: Codec) -> bool {
    // Direct NVENC (HEVC + AV1) widens 8→10 from packed RGB. On AMD/Intel, VAAPI carries HEVC
    // Main10 under BT.709, and Vulkan Video carries AV1 10-bit SDR (`rgb2yuv10_709.comp`) where the
    // device offers a 10-bit AV1 profile. The encoder degrades a planar surface to 8-bit if some
    // path delivers one.
    match linux_resolved_backend() {
        LinuxBackend::Nvenc => cfg!(feature = "nvenc"),
        LinuxBackend::AmdIntel => {
            codec == Codec::H265 || (codec == Codec::Av1 && vulkan_sdr10_available(codec))
        }
        LinuxBackend::Vulkan => {
            matches!(codec, Codec::H265 | Codec::Av1) && vulkan_sdr10_available(codec)
        }
        _ => false,
    }
}
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn backend_carries_sdr10(_codec: Codec) -> bool {
    false
}

/// True if the Windows codec advertisement comes from a real GPU probe
/// ([`windows_codec_support`]) rather than the static superset. AMF always;
/// QSV with `qsv`; NVENC with `nvenc`.
#[cfg(target_os = "windows")]
pub fn windows_backend_is_probed() -> bool {
    match windows_resolved_backend() {
        WindowsBackend::Amf => true,
        WindowsBackend::Qsv => cfg!(feature = "qsv"),
        WindowsBackend::Nvenc => cfg!(feature = "nvenc"),
        // MFT enumeration is the probe, and it needs no feature.
        WindowsBackend::MediaFoundation => true,
        WindowsBackend::Software => false,
    }
}

/// Encode-GPU vendor from the **selected** render adapter — the same one
/// capture and the IddCx pin sit on. Do not scan DXGI adapter 0: on hybrid
/// boxes that is often the iGPU while textures live on the dGPU. Uncached
/// (preference-dependent; session setup only). Unknown vendor → first known.
#[cfg(target_os = "windows")]
fn windows_gpu_vendor() -> Option<GpuVendor> {
    fn by_id(vendor_id: u32) -> Option<GpuVendor> {
        match vendor_id {
            pf_gpu::VENDOR_NVIDIA => Some(GpuVendor::Nvidia),
            pf_gpu::VENDOR_AMD => Some(GpuVendor::Amd),
            pf_gpu::VENDOR_INTEL => Some(GpuVendor::Intel),
            _ => None,
        }
    }
    let sel = pf_gpu::selected_gpu()?;
    by_id(sel.info.vendor_id)
        .or_else(|| pf_gpu::enumerate().iter().find_map(|g| by_id(g.vendor_id)))
}

/// Windows encode-codec probe, cached per (backend, selected GPU) so a
/// console preference change re-probes. Call only when
/// [`windows_backend_is_probed`]. AV1 and HEVC must be probed, not assumed.
///
/// AMD: native factory probe (the path the session opens). QSV: native VPL
/// Query. NVIDIA: the driver's GUID list.
#[cfg(target_os = "windows")]
pub fn windows_codec_support() -> CodecSupport {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, CodecSupport>>> = OnceLock::new();
    let backend = windows_resolved_backend();
    let key = format!("{backend:?}:{}", pf_gpu::selection_key());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(c) = cache.lock().unwrap().get(&key) {
        return *c;
    }
    let probe_one = |codec: Codec| -> bool {
        match backend {
            WindowsBackend::Amf => {
                amf::probe_can_encode(codec, pf_gpu::resolve_render_adapter_luid())
            }
            WindowsBackend::Qsv => {
                #[cfg(feature = "qsv")]
                {
                    qsv::probe_can_encode(codec, pf_gpu::resolve_render_adapter_luid())
                }
                #[cfg(not(feature = "qsv"))]
                {
                    false
                }
            }
            WindowsBackend::MediaFoundation => {
                mf::probe_can_encode(codec, pf_gpu::resolve_render_adapter_luid())
            }
            // NVENC answers from one GUID-list session below. Software is never
            // probed. Defensive `false` → static-superset fallback.
            WindowsBackend::Nvenc | WindowsBackend::Software => false,
        }
    };
    let caps = match backend {
        // One throwaway session lists every GUID. Featureless builds fall
        // through to `probe_one`'s all-false (= static superset).
        #[cfg(feature = "nvenc")]
        WindowsBackend::Nvenc => nvenc::probe_codec_support(pf_gpu::resolve_render_adapter_luid()),
        _ => CodecSupport {
            h264: probe_one(Codec::H264),
            h265: probe_one(Codec::H265),
            av1: probe_one(Codec::Av1),
        },
    };
    tracing::info!(
        ?backend,
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "Windows encode capabilities probed"
    );
    // Concurrent first calls may double-probe; last insert wins.
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// Whether one more encode session fits the hardware budget. Display admission
/// declines rather than silently degrading a live sibling. NVENC is the only
/// hard cap today. See `design/windows-parallel-virtual-displays.md`.
#[cfg(target_os = "windows")]
pub fn can_open_another_session() -> bool {
    #[cfg(feature = "nvenc")]
    {
        nvenc::can_open_another_session()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        true
    }
}

// `#[path]` keeps `crate::*` names flat. The Windows backends and the shared
// NVENC/RFI/policy/PyroWave-wire modules arrive through the `pf_encode_win` glob.
// Direct-SDK NVENC (CUDA). `.so` at runtime, so `--features nvenc` is safe
// on a driver-less/AMD box. See `design/linux-direct-nvenc.md`.
#[cfg(all(target_os = "linux", feature = "nvenc"))]
#[path = "enc/linux/nvenc_cuda.rs"]
mod nvenc_cuda;
// Software (openh264) H.264 — the GPU-less Linux path. Windows has none: the driver
// needs a render adapter to exist at all (plan §9-1).
#[cfg(target_os = "linux")]
#[path = "enc/sw.rs"]
mod sw;

/// Open the software H.264 encoder by name, bypassing the backend ladder.
///
/// [`open_video`] resolves a backend from `PUNKTFUNK_ENCODER`, which the host reads **once** and
/// latches — so a caller that decides later cannot steer it, and `auto` never picks software
/// anyway. The browser plane is exactly that caller: it serves a GPU-less host and wants this
/// encoder specifically, not whatever the ladder would have chosen.
#[cfg(target_os = "linux")]
pub fn open_software_h264(
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
) -> Result<Box<dyn Encoder>> {
    sw::OpenH264Encoder::open(format, width, height, fps, bitrate_bps.min(SW_BITRATE_CEIL))
        .map(|e| Box::new(e) as Box<dyn Encoder>)
}
// Native VAAPI: reference invalidation, in-place retarget, HEVC Main 10 with
// HDR10. `design/native-vaapi-encoder.md`.
#[cfg(target_os = "linux")]
#[path = "enc/linux/vaapi_native.rs"]
mod vaapi_native;
// Vulkan Video on Linux (AMD/Intel). App-owned DPB (real RFI); on-GPU RGB→NV12
// CSC. Needs `--features vulkan-encode`. See `design/linux-vulkan-video-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vulkan_video.rs"]
mod vulkan_video;
// Vendored `VK_KHR_video_encode_av1` — pinned `ash` predates 1.3.290. Do not
// bump `ash` (breaks the SDL/Vulkan client).
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_av1_encode.rs"]
mod vk_av1_encode;
// Vendored `VK_VALVE_video_encode_rgb_conversion`. Same ash-pin as `vk_av1_encode`.
// See `design/vulkan-rgb-direct-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_valve_rgb.rs"]
mod vk_valve_rgb;
// Vendored `VK_KHR_video_encode_intra_refresh`. Same ash-pin. See `design/vulkan-intra-refresh.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_intra_refresh.rs"]
mod vk_intra_refresh;
// Shared ash helpers (dmabuf import, image/memory) for the Linux Vulkan backends.
#[cfg(all(
    target_os = "linux",
    any(feature = "vulkan-encode", feature = "pyrowave")
))]
#[path = "enc/linux/vk_util.rs"]
mod vk_util;
// PyroWave: Vulkan-compute intra wavelet. Explicit `PUNKTFUNK_ENCODER=pyrowave`.
// See `design/pyrowave-codec-plan.md`.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave.rs"]
mod pyrowave;
// `punktfunk-encode-worker` holds `CAP_SYS_NICE`; the host must not (a capped
// host is unidentifiable to KWin). `worker` is `pub` for that binary's `main`.
// See `design/gpu-priority-capability-worker.md`.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave_remote.rs"]
mod pyrowave_remote;
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/worker.rs"]
pub mod worker;
// Live tests pairing a `pf_encode_win` backend with what only this crate has
// (pf-capture's P010 converter).
#[cfg(all(test, target_os = "windows"))]
#[path = "enc/windows/live_tests.rs"]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin whose vendor contradicts the selected GPU is overridden — never
    /// "pin + proceed" (that feeds the reset ladder a deterministic failure).
    #[test]
    fn encoder_pin_reconciles_against_the_selected_adapter() {
        use WindowsBackend::*;
        let derived = |b: WindowsBackend| move || b;
        let unreachable = || -> WindowsBackend { panic!("derived must not be consulted") };
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_NVIDIA), derived(Nvenc)),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_INTEL), unreachable),
            Qsv
        );
        assert_eq!(
            resolve_windows_backend(Some(Nvenc), None, unreachable),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Software), Some(pf_gpu::VENDOR_NVIDIA), unreachable),
            Software
        );
        assert_eq!(
            resolve_windows_backend(None, Some(pf_gpu::VENDOR_AMD), derived(Amf)),
            Amf
        );
        assert_eq!(
            resolve_windows_backend(None, None, derived(Software)),
            Software
        );
        // MF has no vendor, so no adapter can contradict the pin — that is the whole
        // point of the rung: it opens on NVIDIA, AMD, Intel, and Adreno alike.
        for vendor in [
            pf_gpu::VENDOR_NVIDIA,
            pf_gpu::VENDOR_AMD,
            pf_gpu::VENDOR_INTEL,
            pf_gpu::VENDOR_QUALCOMM,
        ] {
            assert_eq!(
                resolve_windows_backend(Some(MediaFoundation), Some(vendor), unreachable),
                MediaFoundation
            );
        }
        assert_eq!(
            resolve_windows_backend(
                None,
                Some(pf_gpu::VENDOR_QUALCOMM),
                derived(MediaFoundation)
            ),
            MediaFoundation
        );
        assert_eq!(windows_backend_vendor_id(MediaFoundation), None);
    }

    #[test]
    fn codec_wire_roundtrip_and_label() {
        for c in [Codec::H264, Codec::H265, Codec::Av1, Codec::PyroWave] {
            assert_eq!(codec_from_wire(codec_to_wire(c)), c);
        }
        assert_eq!(codec_from_wire(0), Codec::H265);
        assert_eq!(Codec::H264.label(), "h264");
        assert_eq!(Codec::H265.label(), "hevc");
        assert_eq!(Codec::Av1.label(), "av1");
        assert_eq!(chroma_idc(ChromaFormat::Yuv420), 1);
        assert_eq!(chroma_idc(ChromaFormat::Yuv444), 3);
    }

    /// Every field must survive wire → frame → wire; a field the copy forgets
    /// would silently zero the SEI the encoder emits.
    #[test]
    fn hdr_meta_wire_roundtrip_keeps_every_field() {
        let wire = punktfunk_core::quic::HdrMeta {
            display_primaries: [[1, 2], [3, 4], [5, 6]],
            white_point: [7, 8],
            max_display_mastering_luminance: 9,
            min_display_mastering_luminance: 10,
            max_cll: 11,
            max_fall: 12,
        };
        let frame = hdr_meta_from_wire(wire);
        assert_eq!(frame.display_primaries, [[1, 2], [3, 4], [5, 6]]);
        assert_eq!(frame.white_point, [7, 8]);
        assert_eq!(frame.max_display_mastering_luminance, 9);
        assert_eq!(frame.min_display_mastering_luminance, 10);
        assert_eq!(frame.max_cll, 11);
        assert_eq!(frame.max_fall, 12);
        assert_eq!(hdr_meta_to_wire(frame), wire);
        assert_eq!(
            hdr_meta_to_wire(pf_frame::HdrMeta::default()),
            punktfunk_core::quic::HdrMeta::default()
        );
    }

    /// [`TerminalEncoderError`] must stay downcastable through `context` layers.
    /// A `format!`/stringify on any layer would break the reset ladder.
    #[test]
    fn terminal_encoder_error_survives_the_context_chain() {
        use anyhow::Context as _;
        let site: anyhow::Error = anyhow::Error::new(TerminalEncoderError)
            .context("capture device's adapter is not an Intel VPL implementation");
        let bubbled = Err::<(), _>(site)
            .context("QSV lazy bring-up")
            .context("encoder submit")
            .unwrap_err();
        assert!(bubbled.downcast_ref::<TerminalEncoderError>().is_some());
        assert!(format!("{bubbled:#}").contains("not an Intel VPL implementation"));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn codec_support_wire_mask_maps_the_probe_to_bits() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        let all = CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        };
        assert_eq!(
            codec_support_wire_mask(all),
            Some(CODEC_H264 | CODEC_HEVC | CODEC_AV1)
        );
        let hevc_only = CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        };
        assert_eq!(codec_support_wire_mask(hevc_only), Some(CODEC_HEVC));
        // All-false = GPU unusable, not "zero codecs" — `None` → static superset.
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        assert_eq!(codec_support_wire_mask(none), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_advertisement_never_offers_what_the_backend_cannot_open() {
        use LinuxBackend::*;
        let caps = |h264, h265, av1| CodecSupport { h264, h265, av1 };
        let probed = caps(true, true, false);
        // A GPU probe passes through on the vendor backends.
        assert_eq!(
            (
                narrow_to_openable(AmdIntel, Some(probed)).h264,
                narrow_to_openable(AmdIntel, Some(probed)).av1
            ),
            (true, false)
        );
        // A vulkan pin drops H.264 (it bails at open) and keeps what the probe found.
        let vulkan = narrow_to_openable(Vulkan, Some(caps(true, true, true)));
        assert_eq!(
            (vulkan.h264, vulkan.h265, vulkan.av1),
            (
                false,
                cfg!(feature = "vulkan-encode"),
                cfg!(feature = "vulkan-encode")
            )
        );
        // openh264 is H.264 whatever a GPU probe says.
        let sw = narrow_to_openable(Software, Some(caps(true, true, true)));
        assert_eq!((sw.h264, sw.h265, sw.av1), (true, false, false));
        // An unusable probe fails open to the superset, still narrowed.
        let unprobed = narrow_to_openable(AmdIntel, None);
        assert!(unprobed.h264 && unprobed.h265 && unprobed.av1);
        let empty = narrow_to_openable(AmdIntel, Some(caps(false, false, false)));
        assert!(empty.h264 && empty.h265 && empty.av1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cursor_blend_capability_mirrors_the_dispatch() {
        use LinuxBackend::*;
        assert!(cursor_blend_capable_for(
            Some(Pyrowave),
            false,
            false,
            false
        ));
        assert!(cursor_blend_capable_for(Some(Nvenc), true, true, false));
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), false, true, false),
            "a CPU payload is uploaded, past the blend point"
        );
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), true, false, false),
            "a build without the `nvenc` feature has no NVENC at all"
        );
        assert!(cursor_blend_capable_for(Some(AmdIntel), false, false, true));
        assert!(
            !cursor_blend_capable_for(Some(AmdIntel), false, false, false),
            "no eligible Vulkan CSC arm (H.264, PUNKTFUNK_VULKAN_ENCODE=0, unsupported \
             device) resolves to native VAAPI, which cannot blend"
        );
        assert!(cursor_blend_capable_for(Some(Vulkan), false, false, true));
        assert!(!cursor_blend_capable_for(Some(Software), false, true, true));
        assert!(!cursor_blend_capable_for(None, false, true, true));
    }

    /// Every `Encoder` method must be forwarded by `TrackedEncoder`. An
    /// unforwarded default silently no-ops — the host loop only holds the
    /// wrapper. Source-text parse: each item ends at the first column-0 `}`;
    /// method names sit on a line starting `fn `.
    #[test]
    fn tracked_encoder_forwards_every_trait_method() {
        fn item_block<'a>(src: &'a str, marker: &str) -> &'a str {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("marker {marker:?} not found — update this guard"));
            let body = &src[start..];
            let end = body
                .find("\n}")
                .unwrap_or_else(|| panic!("no column-0 close brace after {marker:?}"));
            &body[..end]
        }
        fn fn_names(block: &str) -> std::collections::BTreeSet<&str> {
            block
                .lines()
                .map(str::trim_start)
                .filter(|l| !l.starts_with("//"))
                .filter_map(|l| l.strip_prefix("fn "))
                .map(|rest| {
                    rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .next()
                        .expect("split yields at least one item")
                })
                .collect()
        }
        // `find` takes the first occurrence: the real impl precedes this test's copy.
        let trait_fns = fn_names(item_block(
            include_str!("../../pf-encode-win/src/codec.rs"),
            "pub trait Encoder: Send {",
        ));
        let impl_fns = fn_names(item_block(
            include_str!("lib.rs"),
            "impl Encoder for TrackedEncoder {",
        ));
        assert!(
            trait_fns.len() >= 12,
            "only {} trait methods parsed — the extraction markers have rotted, fix the parse \
             before trusting this guard",
            trait_fns.len()
        );
        let missing: Vec<_> = trait_fns.difference(&impl_fns).collect();
        assert!(
            missing.is_empty(),
            "Encoder methods NOT forwarded by TrackedEncoder: {missing:?} — the host loop only \
             ever holds the wrapped box, so an unforwarded default silently disables the feature \
             for every session. Forward each one in `impl Encoder for TrackedEncoder`."
        );
        // Reverse (impl fn absent from the trait) is a compile error; equality
        // guards a parse regression.
        assert_eq!(trait_fns, impl_fns);
    }

    /// Resolver alias table. The panicking closure is the laziness contract:
    /// an explicit pref must not run the auto probe (`/serverinfo` polls).
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_backend_resolver_table() {
        use LinuxBackend::*;
        let no_probe = || -> bool { panic!("explicit prefs must not run the auto probe") };
        for pref in ["nvenc", "nvidia", "cuda"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Nvenc));
        }
        for pref in ["vaapi", "amd", "intel"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(AmdIntel));
        }
        for pref in ["vulkan", "vulkan-video"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Vulkan));
        }
        assert_eq!(
            resolve_linux_backend("pyrowave", no_probe, false),
            Some(Pyrowave)
        );
        for pref in ["software", "sw", "openh264"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Software));
        }
        assert_eq!(resolve_linux_backend("", || true, false), Some(AmdIntel));
        assert_eq!(resolve_linux_backend("auto", || false, false), Some(Nvenc));
        // CUDA is NVENC-only and short-circuits the probe (`||` order).
        assert_eq!(resolve_linux_backend("auto", no_probe, true), Some(Nvenc));
        // Unknown pref: dispatch bails; mirrors map this to auto.
        assert_eq!(resolve_linux_backend("banana", no_probe, false), None);
        // Explicit pref is never overridden by `cuda`.
        assert_eq!(
            resolve_linux_backend("vaapi", no_probe, true),
            Some(AmdIntel)
        );
    }

    /// Linux dispatch through the resolver, GPU-free via the software arm.
    /// Pref is injected — `set_var` races `getenv` in parallel tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_video_backend_dispatches_software() {
        let (enc, label) = open_video_backend_linux(
            "software",
            Codec::H264,
            PixelFormat::Bgrx,
            64,
            64,
            30,
            1_000_000,
            false,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("software arm must open GPU-free");
        assert_eq!(label, "software");
        drop(enc);
        let err = match open_video_backend_linux(
            "software",
            Codec::H265,
            PixelFormat::Bgrx,
            64,
            64,
            30,
            1_000_000,
            false,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        ) {
            // `expect_err` needs `Ok: Debug`; `Box<dyn Encoder>` isn't.
            Ok(_) => panic!("software emits H.264 only; an H.265 session must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("H.264"), "{err:#}");
    }
    /// `auto` on an AMD/Intel box opens the native VAAPI session.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn auto_opens_the_native_vaapi_session() {
        let (enc, backend) = open_video_backend_linux(
            "auto",
            Codec::H264,
            PixelFormat::Bgra,
            320,
            240,
            60,
            2_000_000,
            false,
            8,
            ChromaFormat::Yuv420,
            false,
            32,
        )
        .expect("open");
        assert_eq!(backend, "vaapi-native");
        assert!(enc.caps().supports_rfi);
    }
}
