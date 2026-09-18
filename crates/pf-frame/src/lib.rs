//! Capture↔encode frame vocabulary.
//!
//! [`PixelFormat`], [`CapturedFrame`], and [`FramePayload`] are the types both sides
//! speak. Leaf crate so `pf-capture` and `pf-encode` share them without depending on
//! each other. GPU payloads own the backends below: `FramePayload::Cuda` is a
//! [`pf_zerocopy::DeviceBuffer`], `FramePayload::D3d11` a [`dxgi::D3d11Frame`].
//!
//! Same seam: [`hdr`] (HDR10 static metadata / SEI), [`metronome`] (periodic-stall
//! detector), [`thread_qos`], [`session_tuning`], and on Windows [`dxgi`] (capture
//! identity + D3D11 device).

pub mod hdr;
pub use hdr::HdrMeta;
pub mod health;
pub mod metronome;
pub mod recovery;
pub mod session_tuning;
pub mod thread_qos;

#[cfg(target_os = "windows")]
pub mod dxgi;

/// Capture negotiates this; the encoder maps to an NVENC input (`rgb0`/`bgr0`/`rgba`/`bgra`)
/// and expands 3→4 bytes when needed. No host-side colour conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Bgrx,
    Rgbx,
    Bgra,
    Rgba,
    Rgb,
    Bgr,
    /// Packed `R10G10B10A2` (DXGI `R10G10B10A2_UNORM`), 4 bpp. HDR capture writes BT.2020 PQ
    /// here; NVENC ingests it as `ABGR10` for HEVC Main10 / HDR10.
    Rgb10a2,
    /// Same `R10G10B10A2` memory as [`Rgb10a2`](Self::Rgb10a2), but sRGB expanded 8→10 — not PQ.
    /// Separate so the encoder's colour signalling cannot stamp PQ VUI on SDR frames (BT.709).
    Rgb10a2Sdr,
    /// 8-bit BT.709 limited YUV 4:2:0 (DXGI `NV12`). D3D11 video-processor output so CSC
    /// does not contend with the 3D engine; NVENC ingests `NV12` natively (no RGB→YUV).
    Nv12,
    /// 10-bit BT.2020 PQ limited YUV 4:2:0 (DXGI `P010`). HDR analogue of [`Nv12`]; NVENC `YUV420_10BIT`.
    P010,
    /// Planar 8-bit YUV 4:4:4 (BT.709; range via `PUNKTFUNK_444_FULLRANGE`). GPU-only
    /// ([`FramePayload::Cuda`] / `DeviceBuffer::yuv444`); never a CPU payload. NVENC Range-Extensions.
    Yuv444,
    /// Packed `x:R:G:B 2:10:10:10` LE (SPA `xRGB_210LE`, DRM `XR30`, NVENC `ARGB10`).
    /// As a u32: B 0-9, G 10-19, R 20-29. Linux HDR screencast: PQ BT.2020. Not used on Windows.
    X2Rgb10,
    /// Packed `x:B:G:R 2:10:10:10` LE (SPA `xBGR_210LE`, DRM `XB30`, NVENC `ABGR10`).
    /// As a u32: R 0-9, G 10-19, B 20-29 — same memory as Windows [`Rgb10a2`](Self::Rgb10a2).
    /// Do not fold into `Rgb10a2`: Linux vs Windows HDR stay distinct.
    X2Bgr10,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixelFormat::Rgb | PixelFormat::Bgr => 3,
            // Three full-res 1-byte planes; GPU-only (no CPU payload).
            PixelFormat::Yuv444 => 3,
            _ => 4,
        }
    }

    /// Linux HDR (BT.2020 PQ) packed RGB. Not Windows `Rgb10a2`.
    pub fn is_hdr_rgb10(self) -> bool {
        matches!(self, PixelFormat::X2Rgb10 | PixelFormat::X2Bgr10)
    }

    /// BT.2020 PQ capture: packed 10-bit RGB or the producer's `P010`. Encoder colour and
    /// HDR metadata key on this, not on the packed-RGB layout.
    pub fn is_hdr(self) -> bool {
        self.is_hdr_rgb10() || self == PixelFormat::P010
    }
}

#[cfg(test)]
mod pixel_format_tests {
    use super::PixelFormat;

    #[test]
    fn p010_is_hdr_but_not_packed_rgb() {
        assert!(PixelFormat::P010.is_hdr());
        assert!(!PixelFormat::P010.is_hdr_rgb10());
        assert!(PixelFormat::X2Bgr10.is_hdr());
        assert!(!PixelFormat::Nv12.is_hdr());
        assert!(!PixelFormat::Bgrx.is_hdr());
    }
}

/// DRM FourCC from a 4-byte name, little-endian (`b"XR24"`).
#[cfg(target_os = "linux")]
const fn drm_fourcc_code(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

/// SPA/our [`PixelFormat`] → DRM FourCC for EGL import. SPA `BGRx` is DRM `XRGB8888`
/// (memory B,G,R,X).
#[cfg(target_os = "linux")]
pub fn drm_fourcc(format: PixelFormat) -> Option<u32> {
    use PixelFormat::*;
    Some(match format {
        Bgrx => drm_fourcc_code(b"XR24"), // DRM_FORMAT_XRGB8888
        Bgra => drm_fourcc_code(b"AR24"), // DRM_FORMAT_ARGB8888
        Rgbx => drm_fourcc_code(b"XB24"), // DRM_FORMAT_XBGR8888
        Rgba => drm_fourcc_code(b"AB24"), // DRM_FORMAT_ABGR8888
        // One LINEAR dmabuf, Y then interleaved UV (`DRM_FORMAT_NV12`).
        Nv12 => drm_fourcc_code(b"NV12"),
        X2Rgb10 => drm_fourcc_code(b"XR30"), // DRM_FORMAT_XRGB2101010
        X2Bgr10 => drm_fourcc_code(b"XB30"), // DRM_FORMAT_XBGR2101010
        // NV12 at 16 bits per sample, the 10-bit code high (`DRM_FORMAT_P010`).
        P010 => drm_fourcc_code(b"P010"),
        // 24-bit packed RGB/BGR have no dmabuf import here; use the CPU path.
        // Rgb10a2/Rgb10a2Sdr are Windows formats; Yuv444 is convert output, never a
        // capture source.
        Rgb | Bgr | Rgb10a2 | Rgb10a2Sdr | Yuv444 => return None,
    })
}

/// What a Windows capturer produces, resolved once per session and passed into
/// `capture_virtual_output`. Capture must not re-derive the encode backend from this —
/// a mismatch would put CPU frames on a GPU encoder. Linux portal capture ignores it
/// (PipeWire negotiates its own format).
#[derive(Clone, Copy, Debug)]
pub struct OutputFormat {
    /// GPU-resident D3D11 (zero-copy for NVENC/AMF/QSV). `false` only for the software encoder.
    pub gpu: bool,
    /// 10-bit HDR: IDD-push FP16 → `P010` (or `Rgb10a2` for 4:4:4). `false` = 8-bit SDR.
    pub hdr: bool,
    /// 10-bit SDR (`bit_depth == 10`, HDR off). Windows IDD-push expands BGRA 8→10 into
    /// [`PixelFormat::Rgb10a2Sdr`]; on Linux it makes the pipewire capturer keep packed RGB
    /// (skip the NV12 convert) so direct-NVENC can widen 8→10. NVENC encodes Main10 under
    /// BT.709 VUI. Mutually exclusive with `hdr`.
    pub ten_bit_sdr: bool,
    /// Full-chroma 4:4:4: capturer must not subsample. Windows IDD-push passes BGRA through
    /// (skip BGRA→NV12) so NVENC CSCs to 4:4:4 under the VUI matrix. Linux forces CPU RGB
    /// that the encoder swscales to `YUV444P`. `false` on every 4:2:0 session.
    pub chroma_444: bool,
    /// Windows wavelet session: IDD-push NV12 out-ring must be `SHARED | SHARED_NTHANDLE`
    /// with a shared fence after each convert (`design/pyrowave-windows-host-zerocopy.md`).
    /// Forces NV12 4:2:0 SDR (never BGRA-passthrough / P010). `false` off Windows / non-wavelet.
    pub pyrowave: bool,
    /// This session's encoder can ingest producer-native NV12 (Linux Vulkan Video on
    /// H265/AV1; `pf_encode::linux_native_nv12_ok`). Capture offers gamescope the NV12 pod
    /// only when set: every other Linux arm reads packed RGB. Always `false` on Windows.
    pub nv12_native: bool,
    /// Cursor-forward channel: Windows IDD-push delivers the driver's hardware-cursor
    /// section so DWM stops compositing the pointer; capturer surfaces it via
    /// `Capturer::cursor()`. Ignored on Linux (`SPA_META_Cursor` already separates it).
    pub hw_cursor: bool,
}

impl OutputFormat {
    /// GameStream + spike paths that do not build a [`SessionPlan`]. `gpu` is the encoder's
    /// residency (`pf_encode::resolved_backend_is_gpu`); capture never re-derives it.
    /// Native punktfunk/1 uses `SessionPlan::output_format()` instead.
    pub fn resolve(hdr: bool, gpu: bool) -> Self {
        OutputFormat {
            gpu,
            hdr,
            // GameStream/spike: no 10-bit SDR, 4:4:4, PyroWave, or cursor-forward (native-only).
            ten_bit_sdr: false,
            chroma_444: false,
            pyrowave: false,
            hw_cursor: false,
            // Codec unresolved here; Moonlight may pick H264 (VAAPI cannot ingest NV12).
            nv12_native: false,
        }
    }
}

/// Encode-time cursor bitmap for GPU payloads (Cuda/Dmabuf) whose pixels never hit the CPU.
/// CPU de-pad composites inline and leaves this `None`. `rgba` is `Arc` so every frame is a
/// refcount bump; `serial` changes only with the image so the encoder re-uploads on change
/// and otherwise moves a push-constant.
#[derive(Clone)]
pub struct CursorOverlay {
    /// Top-left in frame pixels (already = reported position − hotspot).
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// Straight-alpha RGBA, `w*h*4`.
    pub rgba: std::sync::Arc<Vec<u8>>,
    /// Bumps when `rgba`/`w`/`h` change; stable across position-only moves.
    pub serial: u64,
    /// Hotspot within `w`×`h`. Blend paths ignore it (`x`/`y` are already adjusted); the
    /// cursor-forward channel ships it so a locally-drawn OS cursor points at the right pixel.
    pub hot_x: u32,
    pub hot_y: u32,
    /// Compositor pointer visibility. `false` = host app hid the pointer. The encode loop
    /// strips invisible overlays before any blend, so encoders may treat `Some` as "draw it".
    pub visible: bool,
}

impl CursorOverlay {
    /// The bitmap to blend into a BT.2020 PQ frame ([`hdr::srgb_rgba_to_pq`]), cached per bitmap.
    pub fn pq_rgba(&self) -> std::sync::Arc<Vec<u8>> {
        hdr::pq_rgba_cached(&self.rgba)
    }
}

/// Where a captured frame's pixels came from. Host wall-clock PTS advances on every delivered
/// frame — repeats and cursor regenerations included — so it can never prove the SOURCE
/// (compositor/DWM presentation) made progress; this can. Only [`Source`](Self::Source) may feed
/// capture health, source cadence, or recovery-stability decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOrigin {
    /// A NEW source image from the compositor/DWM presentation path (or a capturer that does not
    /// distinguish — Linux compositor buffers, synthetic sources — where every delivery is one).
    Source,
    /// Unchanged source pixels re-composed only to move/redraw the cursor overlay (Windows
    /// IDD-push). Encodable and sendable, but no evidence the desktop image changed.
    CursorRegen,
    /// A repeat of the previous frame (a stream hold) — not a captured image at all. Maps to the
    /// existing repeat wire behavior; never serialized separately.
    Hold,
}

/// Frame provenance: [`FrameOrigin`] plus the source progress clocks. `UNTRACKED` (the default)
/// is a capturer that delivers only real frames and tracks no sequence — origin `Source`, both
/// clocks 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub origin: FrameOrigin,
    /// Monotonic count of NEW source images this capturer delivered — advances only for
    /// [`FrameOrigin::Source`] and survives ring rebuilds. `0` = untracked.
    pub source_seq: u64,
    /// The source's own present timestamp (Windows: raw QPC ticks from the driver's
    /// `PresentDisplayQPCTime`). Opaque and monotonic; compare, never convert. `0` = unknown.
    pub source_qpc: u64,
}

impl Provenance {
    pub const UNTRACKED: Self = Self {
        origin: FrameOrigin::Source,
        source_seq: 0,
        source_qpc: 0,
    };

    pub fn source(source_seq: u64, source_qpc: u64) -> Self {
        Self {
            origin: FrameOrigin::Source,
            source_seq,
            source_qpc,
        }
    }

    /// A cursor-only regeneration over the LAST source image (`source_seq` unchanged).
    pub fn cursor_regen(source_seq: u64) -> Self {
        Self {
            origin: FrameOrigin::CursorRegen,
            source_seq,
            source_qpc: 0,
        }
    }

    /// A hold/repeat of the last delivered frame (`source_seq` unchanged).
    pub fn hold(source_seq: u64) -> Self {
        Self {
            origin: FrameOrigin::Hold,
            source_seq,
            source_qpc: 0,
        }
    }
}

impl Default for Provenance {
    fn default() -> Self {
        Self::UNTRACKED
    }
}

/// A captured frame. [`format`](Self::format)/dimensions describe the pixels regardless of
/// where they live — [`payload`](Self::payload) is either a CPU buffer (the spike/fallback path)
/// or a GPU buffer already on the device (the zero-copy path, plan §9).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub pts_ns: u64,
    pub format: PixelFormat,
    pub payload: FramePayload,
    /// Encode-time overlay for GPU payloads; `None` when already composited on the CPU de-pad path.
    pub cursor: Option<CursorOverlay>,
    /// Where these pixels came from ([`FrameOrigin`]) and the source progress clocks.
    pub provenance: Provenance,
}

/// Keeps a producer's buffer behind a zero-copy frame out of the producer's pool.
///
/// A dmabuf fd stops the BO from being freed, not from being re-rendered into. Capture
/// attaches this to every raw-passthrough frame (pool depth permitting) and requeues
/// only when the last clone drops. An async reader (Vulkan encoder ring) clones it into
/// the slot and drops it on the slot fence so stability covers the read window. A
/// consumer that finishes while the frame is alive needs no extra clone.
///
/// Opaque: the guard lives in capture; everyone else only clones and drops.
#[cfg(target_os = "linux")]
pub type FrameHold = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// A captured frame still in a DMA-BUF. Packed RGB is one plane. Native Linux NV12
/// travels in one fd: Y at `offset`, interleaved UV at `plane1` when the producer
/// reported it, else at `offset + stride * frame_height` with the shared `stride`.
///
/// Owns a *dup* of the PipeWire fd so encode can import after capture returns. Content
/// stability across the read window is [`hold`](Self::hold); `None` falls back to pool
/// depth outrunning import+encode.
#[cfg(target_os = "linux")]
pub struct DmabufFrame {
    pub fd: std::os::fd::OwnedFd,
    /// DRM FourCC (`XR24` for BGRx, `NV12` for native 4:2:0).
    pub fourcc: u32,
    /// DRM format modifier (0 = LINEAR).
    pub modifier: u64,
    /// Second-plane `(offset, stride)` in the same fd (NV12 UV). `None` = contiguous
    /// fallback above. Always `None` for packed RGB.
    pub plane1: Option<(u32, u32)>,
    pub offset: u32,
    pub stride: u32,
    /// Deferred-requeue hold; `None` when the pool could not spare a buffer (`PUNKTFUNK_ZEROCOPY_HOLD=0`).
    pub hold: Option<FrameHold>,
}

pub enum FramePayload {
    /// Tightly-packed CPU pixels in `format`, `width*height*bytes_per_pixel` (no row padding).
    Cpu(Vec<u8>),
    /// Pitched BGRA on the shared CUDA context. Dmabuf already imported into this owned buffer.
    #[cfg(target_os = "linux")]
    Cuda(pf_zerocopy::DeviceBuffer),
    /// Raw DMA-BUF: packed RGB for GPU CSC, or producer-native NV12. Encoder imports without a host copy.
    #[cfg(target_os = "linux")]
    Dmabuf(DmabufFrame),
    /// GPU-resident D3D11 texture (Windows NVENC zero-copy). Owns the copied frame.
    #[cfg(target_os = "windows")]
    D3d11(dxgi::D3d11Frame),
}

impl CapturedFrame {
    pub fn is_cuda(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self.payload, FramePayload::Cuda(_))
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn is_dmabuf(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self.payload, FramePayload::Dmabuf(_))
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}
