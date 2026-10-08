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
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Mutex, OnceLock};

// The `Encoder` contract and the pieces every backend shares, plus the Windows
// backends, which re-export the same contract. One namespace: `pf_encode::*`.
pub use pf_encode_core::*;
#[cfg(target_os = "windows")]
pub use pf_encode_win::*;

// Backend selection, one module per OS: what the resolved backend opens and can encode. Each
// answers the same `pub(crate)` questions; its OS-only public API re-exports from here.
#[cfg(target_os = "linux")]
#[path = "select/linux.rs"]
mod imp;
#[cfg(target_os = "windows")]
#[path = "select/windows.rs"]
mod imp;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
#[path = "select/other.rs"]
mod imp;
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub use imp::*;

/// A probe result per key. The key carries the selected GPU, so a console preference change
/// re-probes.
type ProbeCache<K, V> = OnceLock<Mutex<HashMap<K, V>>>;

/// `probe()` once per `key`, outside the lock. Concurrent first calls may both probe; the last
/// insert wins.
fn probe_cached<K: Eq + Hash, V: Copy>(
    cache: &ProbeCache<K, V>,
    key: K,
    probe: impl FnOnce() -> V,
) -> V {
    let cache = cache.get_or_init(Default::default);
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let v = probe();
    cache.lock().unwrap().insert(key, v);
    v
}

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
/// a field copy stands in for `From`. A client panel that reports zero primaries,
/// white point or peak keeps the generic HDR10 value there: a 0-nit mastering
/// display tone-maps to black.
pub fn hdr_meta_from_wire(m: punktfunk_core::quic::HdrMeta) -> pf_frame::HdrMeta {
    let g = pf_frame::hdr::generic_hdr10();
    pf_frame::HdrMeta {
        display_primaries: if m.display_primaries == [[0; 2]; 3] {
            g.display_primaries
        } else {
            m.display_primaries
        },
        white_point: if m.white_point == [0; 2] {
            g.white_point
        } else {
            m.white_point
        },
        max_display_mastering_luminance: match m.max_display_mastering_luminance {
            0 => g.max_display_mastering_luminance,
            v => v,
        },
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
/// resolved backend. Fed to [`punktfunk_core::quic::resolve_codec`].
///
/// Software is H.264 only. Probed backends advertise what the GPU encodes
/// ([`vaapi_codec_support`] / [`windows_codec_support`]); NVENC falls back to
/// the GameStream superset when the probe cannot answer. An empty probe
/// means the GPU was unusable at probe time, not that it encodes nothing —
/// fall back to the superset so auto clients still land on HEVC.
pub fn host_wire_caps() -> u8 {
    imp::wire_caps()
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
    // The handshake's HDR verdict; 10-bit without it is BT.709.
    hdr: bool,
    chroma: ChromaFormat,
    // Backends whose fast path can't blend (Vulkan EFC) key off `cursor_blend`.
    cursor_blend: bool,
    // Client decoder slice ceiling. 1 = single-slice (some TVs wedge on
    // multi-slice AUs); 32 = no client limit. `PUNKTFUNK_NVENC_SLICES` overrides.
    max_slices: u32,
) -> Result<Box<dyn Encoder>> {
    let (inner, backend) = open_video_backend(OpenParams {
        codec,
        format,
        width,
        height,
        fps,
        bitrate_bps: bitrate_bps.max(MIN_BITRATE_BPS),
        cuda,
        bit_depth,
        hdr,
        chroma,
        cursor_blend,
        max_slices,
    })?;
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
    fn set_reference_floor(&mut self, acked: Option<Acked>) {
        self.inner.set_reference_floor(acked)
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
    fn bitrate_retarget_is_synchronous(&self) -> bool {
        self.inner.bitrate_retarget_is_synchronous()
    }
    fn retarget_settled(&self) -> bool {
        self.inner.retarget_settled()
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

/// What a session asks [`open_video`] for, as one value the per-OS openers read. Only Linux
/// direct-SDK NVENC reads every field; other builds leave some unread.
#[derive(Clone, Copy)]
#[cfg_attr(not(all(target_os = "linux", feature = "nvenc")), allow(dead_code))]
struct OpenParams {
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    /// The session's colour: BT.2020 PQ, else BT.709 at either depth. Never read off the
    /// format — gamescope hands 10-bit SDR over as P010 or packed RGB too.
    hdr: bool,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
}

/// Open the platform encoder. The display label is the branch that opened
/// (`nvenc`/`vaapi`/`vulkan`/`amf`/`qsv`/`software`), including internal
/// fallbacks (Vulkan Video → VAAPI). Feeds the mgmt live-session record.
fn open_video_backend(p: OpenParams) -> Result<(Box<dyn Encoder>, &'static str)> {
    let OpenParams {
        codec,
        width,
        height,
        fps,
        chroma,
        ..
    } = p;
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
    imp::open(&OpenParams { chroma, ..p })
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

/// Whether the active backend can emit 4:4:4 HEVC. Cached per selected GPU
/// before Welcome. 4:4:4 is HEVC-only; VAAPI/AMF/QSV must be probed, never
/// assumed. Non-HEVC is always `false`.
pub fn can_encode_444(codec: Codec) -> bool {
    match codec {
        // Own RGB→YCbCr CSC from a full-chroma source — no GPU encode probe.
        // See `design/pyrowave-444-hdr.md`.
        Codec::PyroWave => cfg!(any(target_os = "linux", target_os = "windows")),
        Codec::H265 if cfg!(any(target_os = "linux", target_os = "windows")) => {
            static CACHE: ProbeCache<String, bool> = OnceLock::new();
            probe_cached(&CACHE, pf_gpu::selection_key(), || {
                let supported = imp::hevc_444();
                tracing::info!(supported, "HEVC 4:4:4 encode capability probed");
                supported
            })
        }
        _ => false,
    }
}

/// Whether the active backend can emit 10-bit for `codec` (HEVC Main10 / AV1).
/// Cached per (GPU, codec, Vulkan encoding row) before Welcome, like [`can_encode_444`]. Without
/// this gate `PUNKTFUNK_10BIT` would negotiate 10-bit and then emit 8-bit
/// (label HDR / stream SDR).
pub fn can_encode_10bit(codec: Codec) -> bool {
    if !codec.supports_10bit() || !cfg!(any(target_os = "linux", target_os = "windows")) {
        return false;
    }
    if codec == Codec::PyroWave {
        // Wavelet is depth-agnostic; the CSC runs on the encoder's own Vulkan device, so
        // 10-bit needs no encode-profile probe — just the `pyrowave` backend existing on
        // this OS. See `design/pyrowave-444-hdr.md`.
        return cfg!(target_os = "windows") || cfg!(feature = "pyrowave");
    }
    // The Vulkan encoding row decides what Linux AMD/Intel opens, per session.
    let vulkan = pf_host_config::row_bool("PUNKTFUNK_VULKAN_ENCODE");
    static CACHE: ProbeCache<(String, &'static str, bool), bool> = OnceLock::new();
    probe_cached(
        &CACHE,
        (pf_gpu::selection_key(), codec.label(), vulkan),
        || {
            let supported = imp::ten_bit(codec);
            tracing::info!(codec = ?codec, supported, "10-bit encode capability probed");
            supported
        },
    )
}

/// GPU-resident frames (software is the only CPU path). Single source for
/// [`pf_frame::OutputFormat`]'s `gpu` bit — capture must not re-derive it.
pub fn resolved_backend_is_gpu() -> bool {
    imp::is_gpu()
}

/// Encoder half of the 4:4:4 capture gate: ingest RGB and CSC to 4:4:4.
/// Only Windows NVENC. Linux 4:4:4 is capture-side (portal RGB → `yuv444p`).
pub fn resolved_backend_ingests_rgb_444() -> bool {
    imp::ingests_rgb_444()
}

/// Encoder half of the 10-bit SDR gate: this backend writes a 10-bit stream from the 8-bit
/// surface an SDR desktop captures.
///
/// Windows direct-NVENC ingests the IDD packed `Rgb10a2Sdr`; Windows AMF takes a BT.709 P010 the
/// driver's video processor produces (HEVC only). Linux direct-NVENC takes the plain 8-bit surface
/// and asks NVENC for 10-bit output. Linux VAAPI carries HEVC and Vulkan Video carries AV1, both
/// with the RGB→YUV matrix following the BT.709 colour, not depth. The GPU still has to pass
/// `can_encode_10bit`.
pub fn backend_carries_sdr10(codec: Codec) -> bool {
    imp::sdr10(codec)
}

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

// `#[path]` keeps `crate::*` names flat. The shared NVENC/RFI/policy/PyroWave-wire
// modules arrive through the `pf_encode_core` glob, the Windows backends through `pf_encode_win`.
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
// Shared ash helpers (dmabuf import, image/memory, the device pick) for the
// Linux Vulkan backends — plus `sampled_capture_modifiers`, which the VAAPI
// modifier offer needs even without those features.
#[cfg(target_os = "linux")]
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
// CPU frames the Linux PyroWave and Vulkan Video tests share.
#[cfg(all(
    test,
    target_os = "linux",
    any(feature = "pyrowave", feature = "vulkan-encode")
))]
#[path = "enc/linux/test_frames.rs"]
mod test_frames;
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

    /// A panel that reports no volume must not master the stream at 0 nits.
    #[test]
    fn hdr_meta_from_wire_fills_an_unreported_volume() {
        let g = pf_frame::hdr::generic_hdr10();
        let frame = hdr_meta_from_wire(punktfunk_core::quic::HdrMeta {
            max_cll: 700,
            ..Default::default()
        });
        assert_eq!(frame.display_primaries, g.display_primaries);
        assert_eq!(frame.white_point, g.white_point);
        assert_eq!(
            frame.max_display_mastering_luminance,
            g.max_display_mastering_luminance
        );
        assert_eq!(frame.min_display_mastering_luminance, 0);
        assert_eq!((frame.max_cll, frame.max_fall), (700, 0));
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

    /// Every `Encoder` method must be forwarded by `TrackedEncoder`: the host loop only ever
    /// holds the wrapped box.
    #[test]
    fn tracked_encoder_forwards_every_trait_method() {
        crate::smoke_pattern::assert_writes_every_encoder_method(
            include_str!("lib.rs"),
            "impl Encoder for TrackedEncoder {",
        );
    }
}
