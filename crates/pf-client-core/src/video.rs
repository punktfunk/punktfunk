//! Video decode: reassembled access units → frames for the presenter.
//!
//! Ladder: Vulkan Video, D3D11VA/VAAPI, then openh264 or rav1d. [`Decoder::new`]
//! orders rungs by platform and GPU vendor. [`native_evidence`] records which
//! pairs have hardware validation; [`native_rung_admitted`] yields an unverified
//! rung only to a verified rung usable on the same device. `PUNKTFUNK_DECODER`
//! pins skip that rule and still fall through after init failure.
//! [`migrate_decoder_pref`] rewrites stored libavcodec names onto native pins.
//!
//! Hosts emit zero-reorder streams: one AU in, one picture out. The CPU rung
//! has no HEVC decoder; [`last_rung_verdict`] reconnects with a codec this
//! build can finish. The gates and their evidence live in `video_caps`.

// Windows-only: the D3D11VA pin bails when win32 external-memory import is missing.
#[cfg(windows)]
use anyhow::bail;
use anyhow::Result;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
pub(crate) use crate::video_caps::VENDOR_INTEL;
/// The capability and advertisement gates live in `video_caps`; call sites keep `video::` paths.
pub use crate::video_caps::{
    amd_vulkan_hdr_driver_notice, av1_advertised, av1_hardware_decodable, decodable_codecs,
    decodable_codecs_for, decode_pinned_to_software, hdr_presentable, hevc_444_hardware_decodable,
    last_rung_verdict, multi_slice_decodable, native_evidence, native_rung_admitted,
    native_scanout_wanted, native_vulkan_usable, software_decodable_codecs, ten_bit_decodable,
    usable_decode_ops, video_caps_for, wire_codec_name, LastRungVerdict, NativeRung, RungEvidence,
    RungLoss, V4l2Summary, AMD_VULKAN_HDR_DRIVER_FLOOR,
};
pub(crate) use crate::video_caps::{native_vulkan_gate, resolve_decoder_pref};
#[cfg(target_os = "linux")]
use crate::video_caps::{v4l2_auto_ok, vaapi_auto_ok};
#[cfg(target_os = "linux")]
pub use crate::video_caps::{vaapi_av1_decodable, vaapi_hevc_decodable};
pub use crate::video_color::{csc_rows, ColorDesc};
/// Re-export so SESSION (and its tests) can name the refusal by type.
/// The module stays private, like every other backend.
pub use crate::video_software::NoSoftwareRung;
use crate::video_software::SoftwareDecoder;
/// Defined in [`crate::video_types`] so `d3d11va` can name them without this
/// module. Call sites keep the `video::` paths.
pub use crate::video_types::{umd_version_parts, DecodeHealth, StreamFormat};
// Lives in portable `decoder_pref` (the Skia console reads the decoder row
// through it). Re-exported so desktop callers keep `video::migrate_decoder_pref`.
pub use crate::decoder_pref::migrate_decoder_pref;
#[cfg(target_os = "linux")]
use crate::video_vaapi_native::NativeVaapiDecoder;
/// The Vulkan handoff types live in [`crate::video_vk`] so Android can reach them without
/// the `desktop` half of this crate; re-exported so every desktop call site keeps naming
/// them here.
pub use crate::video_vk::{QueueLock, QueueLockGuard, VulkanDecodeDevice};
use crate::video_vk_native::{NativeCodec, NativeVulkanDecoder};

/// One decoded frame. `pts_ns` is the host capture timestamp for
/// capture→displayed latency at present time.
pub struct DecodedFrame {
    /// Host-clock capture pts (ns). Compare to local wall + `clock_offset_ns`
    /// at paintable-set.
    pub pts_ns: u64,
    /// Local wall (ns) when the decoder emitted this image (`decoded` stage).
    /// The presenter subtracts it from its paintable-set stamp for `display`.
    pub decoded_ns: u64,
    /// The host re-encoded the picture already sent (`USER_FLAG_REPEAT`): nothing new.
    pub repeat: bool,
    pub image: DecodedImage,
}

/// Re-export so the presenter names every frame type through `video::`.
#[cfg(windows)]
pub use crate::video_d3d11::{D3d11Frame, SlotFormat, SlotHandle};

pub enum DecodedImage {
    /// Tightly-packed 8-bit I420 for the presenter's planar CSC upload.
    Cpu(CpuPlanarFrame),
    /// A hardware picture as dma-bufs: a VAAPI DRM-PRIME export or a V4L2
    /// CAPTURE buffer. [`DmabufFrame::path`] names which for `stats:`.
    #[cfg(target_os = "linux")]
    NativeDmabuf(DmabufFrame),
    /// D3D11VA shareable NT-handle texture the presenter imports
    /// (`VK_KHR_external_memory_win32`) on GPUs without Vulkan Video.
    #[cfg(windows)]
    D3d11(crate::video_d3d11::D3d11Frame),
    /// Three R8 plane views on the presenter's device, fence-complete, GENERAL.
    /// Planar CSC samples them as BT.709 limited (the codec's colour contract).
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(crate::video_pyrowave::PyroWavePlanarFrame),
    /// pf-vkdecode image + plane views already on the presenter's device.
    /// Format is [`NativeVkFrame::vk_format`], never assumed. The presenter waits
    /// the timeline, samples, restores [`NativeVkFrame::layout`], and drops the
    /// frame to release the slot.
    NativeVk(NativeVkFrame),
}

/// Raw `VkFormat` code point, carried across the ash-free boundary.
///
/// Newtype, not a bare `i32`: [`NativeVkFrame`] also carries `poc` as `i32`,
/// and passing the wrong one compiles, warns once, and renders as 8-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawVkFormat(pub i32);

/// Picture formats the native decode lane can deliver — pf-vkdecode's
/// [`pf_vkdecode::OUTPUT_FORMATS`], not a copy. Public so the presenter can
/// pin its colour-math table without depending on pf-vkdecode.
pub fn native_picture_formats() -> Vec<RawVkFormat> {
    pf_vkdecode::OUTPUT_FORMATS
        .iter()
        .map(|f| RawVkFormat(f.as_raw()))
        .collect()
}

/// Layout a [`NativeVkFrame`] layer is in when its semaphore signals.
/// Ash-free, so the presenter can transition for sampling without naming `vk::ImageLayout`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeVkLayout {
    /// `VIDEO_DECODE_DST_KHR` — distinct-mode. The next decode into this slot
    /// discards the layer (UNDEFINED old-layout).
    DecodeDst,
    /// `VIDEO_DECODE_DPB_KHR` — coincide-mode DPB slot, possibly still a live
    /// reference. A consumer that samples it must transition it back in the same submit.
    DecodeDpb,
}

/// Token a presented or dropped [`NativeVkFrame`] hands back. `seq` names the
/// frame; a stale `generation` routes to the graveyard. `presented` means the
/// sampling submit enqueued the frame's `value + 1` timeline signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeReleaseToken {
    pub seq: u64,
    pub generation: u64,
    /// Sampling submit (with its `value + 1` signal) was enqueued. `false` when
    /// dropped unpresented (newest-wins, demotion drain, failed submit).
    pub presented: bool,
}

/// Sends [`NativeReleaseToken`] once on drop. The presenter holds the frame
/// until its sampling fence is waited, so drop means the GPU is done. A dead
/// channel (backend demoted or rebuilt) is ignored.
pub struct NativeReleaseGuard {
    tx: std::sync::mpsc::Sender<NativeReleaseToken>,
    token: Option<NativeReleaseToken>,
}

impl NativeReleaseGuard {
    pub(crate) fn new(
        tx: std::sync::mpsc::Sender<NativeReleaseToken>,
        token: NativeReleaseToken,
    ) -> Self {
        Self {
            tx,
            token: Some(token),
        }
    }

    /// Sampling submit, including the frame's `value + 1` timeline signal, was enqueued.
    /// The decoder then waits that write-back.
    pub fn mark_presented(&mut self) {
        if let Some(token) = &mut self.token {
            token.presented = true;
        }
    }
}

impl Drop for NativeReleaseGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            let _ = self.tx.send(token);
        }
    }
}

/// One natively decoded frame (pf-vkdecode). Raw `u64` handles; this crate
/// stays ash-free. Valid until the guard drops AND `generation` is current.
/// The backend keeps the decoder alive until every shipped token returns.
pub struct NativeVkFrame {
    /// Raw `VkImage`; the picture occupies array layer [`Self::layer`].
    pub image: u64,
    /// Picture `VkFormat` — what the image was created with and what
    /// [`Self::plane_views`] alias. Never infer from the codec: H.265 format is
    /// the stream's and can change mid-stream. Assuming 8-bit over P010 displays wrong.
    pub vk_format: RawVkFormat,
    /// Per-plane `VkImageView`s for [`Self::vk_format`] — the presenter's planar CSC contract.
    pub plane_views: [u64; 2],
    pub layer: u32,
    /// Layout when the semaphore signals. The presenter must restore it after sampling.
    pub layout: NativeVkLayout,
    /// Timeline pair (raw `VkSemaphore` + value). The presenter waits it on the GPU, never the host.
    pub semaphore: u64,
    pub semaphore_value: u64,
    /// Decoder session generation the handles belong to (rides the release token).
    pub generation: u64,
    /// Display size (conformance-window crop). What [`DecodedImage::dimensions`] reports.
    pub width: u32,
    pub height: u32,
    /// Allocated/coded extent (`>=` display). Scale UVs by display/coded or
    /// alignment padding smears: 1080p pools 1088 rows (multiple of 16).
    pub coded_width: u32,
    pub coded_height: u32,
    /// Crop origin in the coded picture. Hosts emit (0,0); the UV-scale path
    /// assumes that. Carried so a nonzero origin is checkable, not silent.
    pub crop_x: u32,
    pub crop_y: u32,
    /// SPS colour for this picture (VUI → H.273; unspecified is BT.709 limited).
    /// Per frame: the host switches HDR in-band.
    pub color: ColorDesc,
    /// IDR re-anchor. H.265 CRA/BLA does not set this (NALU type). Hosts emit
    /// IDR-only re-entry; a CRA's leading pictures may be undecodable.
    pub keyframe: bool,
    pub poc: i32,
    /// Intra-refresh recovery-point SEI. [`Self::keyframe`] is never set for a
    /// wave, so without this the pump holds the last good picture until its 500 ms
    /// backstop. Fed to [`punktfunk_core::reanchor::ReanchorGate::on_local_recovery`].
    pub recovery: punktfunk_core::reanchor::LocalRecovery,
    /// Every predicted-from picture decoded from a fully-available reference chain.
    /// Corroborates `USER_FLAG_RECOVERY_ANCHOR`: host tracks receipt, this tracks
    /// decode. When they disagree the freeze lifts onto a concealed picture.
    pub references_clean: bool,
    /// Decode-order ordinal (strictly increasing per session). After a failed AU
    /// H.265 flushes its DPB and may deliver pictures decoded before the loss.
    /// The pump stamps this at arm and ignores older [`Self::recovery`].
    pub decode_order: u64,
    /// The picture carries TRANSFER_SRC: a consumer may copy it out (native scanout).
    pub copyable: bool,
    /// Sends the release token on drop — see [`NativeReleaseGuard`].
    pub guard: NativeReleaseGuard,
}

impl DecodedImage {
    /// Intra keyframe (IDR) — the pump's post-loss re-anchor. Every rung answers
    /// from the bitstream. Not an intra-refresh recovery point; that is [`Self::local_recovery`].
    pub fn is_keyframe(&self) -> bool {
        match self {
            DecodedImage::Cpu(f) => f.keyframe,
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => f.keyframe,
            #[cfg(windows)]
            DecodedImage::D3d11(f) => f.keyframe,
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => f.keyframe,
            DecodedImage::NativeVk(f) => f.keyframe,
        }
    }

    /// Intra-refresh recovery-point SEI. Only rungs with their own parser answer
    /// (native Vulkan, CPU H.264). Everyone else reports
    /// [`punktfunk_core::reanchor::LocalRecovery::NONE`]. The CPU rung has no
    /// [`Self::decode_order`]; openh264 is one-AU-in, one-picture-out.
    pub fn local_recovery(&self) -> punktfunk_core::reanchor::LocalRecovery {
        match self {
            DecodedImage::NativeVk(f) => f.recovery,
            DecodedImage::Cpu(f) => f.recovery,
            _ => punktfunk_core::reanchor::LocalRecovery::NONE,
        }
    }

    /// Corroboration for `USER_FLAG_RECOVERY_ANCHOR`. Only a rung that planned the
    /// AU knows its references: native Vulkan, native VAAPI, and native D3D11 answer.
    /// The CPU and PyroWave rungs report
    /// [`punktfunk_core::reanchor::AnchorEvidence::Unavailable`] — silence is not refutation.
    pub fn anchor_evidence(&self) -> punktfunk_core::reanchor::AnchorEvidence {
        use punktfunk_core::reanchor::AnchorEvidence;
        let clean = match self {
            DecodedImage::NativeVk(f) => f.references_clean,
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => f.references_clean,
            #[cfg(windows)]
            DecodedImage::D3d11(f) => f.references_clean,
            _ => return AnchorEvidence::Unavailable,
        };
        if clean {
            AnchorEvidence::ReferencesClean
        } else {
            AnchorEvidence::ReferencesDamaged
        }
    }

    /// Decode-order ordinal where the lane knows one — see [`NativeVkFrame::decode_order`]. `None` elsewhere.
    pub fn decode_order(&self) -> Option<u64> {
        match self {
            DecodedImage::NativeVk(f) => Some(f.decode_order),
            _ => None,
        }
    }

    /// Display pixel size. A frame at the target size is the mid-stream-resize
    /// end signal: the new-mode picture is on glass before the host rebuilds.
    pub fn dimensions(&self) -> (u32, u32) {
        match self {
            DecodedImage::Cpu(f) => (f.width, f.height),
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => (f.width, f.height),
            #[cfg(windows)]
            DecodedImage::D3d11(f) => (f.width, f.height),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => (f.width, f.height),
            DecodedImage::NativeVk(f) => (f.width, f.height),
        }
    }

    /// The rung that decoded this frame, as the `stats:` decode-path tag. A machine
    /// interface: additive only, and surviving tags keep their exact spelling.
    pub fn path_label(&self) -> &'static str {
        match self {
            DecodedImage::Cpu(f) => f.path,
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => f.path,
            #[cfg(windows)]
            DecodedImage::D3d11(_) => "native-d3d11va",
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(_) => "pyrowave",
            DecodedImage::NativeVk(_) => "native-vulkan",
        }
    }
}

/// Software-decoded 8-bit 4:2:0: Y, Cb, Cr packed back-to-back at each plane's width.
///
/// Tight packing is load-bearing: the presenter uploads with `bufferRowLength = 0`,
/// so a padded row shears the picture. Decoder strides are undone in [`Self::from_i420`].
/// Planes carry the stream's Y′CbCr; the presenter CSC uses [`csc_rows`].
pub struct CpuPlanarFrame {
    pub width: u32,
    pub height: u32,
    /// Y, then Cb, then Cr — see [`Self::plane`].
    data: Vec<u8>,
    /// Byte offset of each plane's first row in [`Self::data`].
    offsets: [usize; 3],
    /// Bitstream colour (not the decoder — see `video_software`). Drives CSC
    /// matrix/range and, for PQ, the presenter's tone-map mode.
    pub color: ColorDesc,
    /// Intra keyframe (IDR) — the pump's post-loss re-anchor. See [`DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// Intra-refresh recovery (`RecoveryWatch`). [`Self::keyframe`] cannot answer
    /// for a wave. H.264 only; AV1 reports [`punktfunk_core::reanchor::LocalRecovery::NONE`].
    pub recovery: punktfunk_core::reanchor::LocalRecovery,
    /// The rung that decoded it, as the `stats:` decode-path tag: the CPU
    /// rung, or a hardware rung whose pictures only reach the screen by copy.
    pub path: &'static str,
}

impl CpuPlanarFrame {
    /// Chroma plane size for 4:2:0, rounding up — an odd luma dimension still
    /// has a chroma sample covering its last row/column.
    pub fn chroma_dims(width: u32, height: u32) -> (u32, u32) {
        (width.div_ceil(2), height.div_ceil(2))
    }

    /// Plane `i` (0 = Y, 1 = Cb, 2 = Cr), tightly packed.
    pub fn plane(&self, i: usize) -> &[u8] {
        let (w, h) = self.plane_dims(i);
        let start = self.offsets[i];
        &self.data[start..start + (w * h) as usize]
    }

    /// Plane `i` size in samples: luma is `(width, height)`, chroma is 4:2:0 halves.
    pub fn plane_dims(&self, i: usize) -> (u32, u32) {
        if i == 0 {
            (self.width, self.height)
        } else {
            Self::chroma_dims(self.width, self.height)
        }
    }

    /// Copy strided I420 into one tightly-packed allocation.
    ///
    /// Refuses rather than truncates: a short plane is a geometry disagreement,
    /// and reading the rows that are there would paint uninitialized memory.
    pub(crate) fn from_i420(
        width: u32,
        height: u32,
        planes: [&[u8]; 3],
        strides: [usize; 3],
        color: ColorDesc,
        keyframe: bool,
        recovery: punktfunk_core::reanchor::LocalRecovery,
    ) -> Result<CpuPlanarFrame> {
        anyhow::ensure!(width > 0 && height > 0, "empty picture {width}x{height}");
        let (cw, ch) = Self::chroma_dims(width, height);
        let dims = [(width, height), (cw, ch), (cw, ch)];
        let total: usize = dims.iter().map(|(w, h)| *w as usize * *h as usize).sum();
        let mut data = vec![0u8; total];
        let mut offsets = [0usize; 3];
        let mut at = 0usize;
        for i in 0..3 {
            let (w, h) = (dims[i].0 as usize, dims[i].1 as usize);
            anyhow::ensure!(
                strides[i] >= w,
                "plane {i}: stride {} is narrower than {w} samples",
                strides[i]
            );
            anyhow::ensure!(
                planes[i].len() >= (h - 1) * strides[i] + w,
                "plane {i}: decoder reported {} bytes for {w}x{h} at stride {}",
                planes[i].len(),
                strides[i]
            );
            offsets[i] = at;
            for row in 0..h {
                let src = row * strides[i];
                data[at..at + w].copy_from_slice(&planes[i][src..src + w]);
                at += w;
            }
        }
        Ok(CpuPlanarFrame {
            width,
            height,
            data,
            offsets,
            color,
            keyframe,
            recovery,
            path: "software",
        })
    }

    /// Take three already tight planes. Refuses a plane whose length is not
    /// its size: uploading it would shear the picture.
    #[cfg(target_os = "linux")]
    pub(crate) fn from_planes(
        width: u32,
        height: u32,
        planes: [Vec<u8>; 3],
        color: ColorDesc,
        keyframe: bool,
        path: &'static str,
    ) -> Result<CpuPlanarFrame> {
        let (cw, ch) = Self::chroma_dims(width, height);
        let sizes = [
            width as usize * height as usize,
            cw as usize * ch as usize,
            cw as usize * ch as usize,
        ];
        anyhow::ensure!(width > 0 && height > 0, "empty picture {width}x{height}");
        let mut offsets = [0usize; 3];
        let mut at = 0usize;
        for i in 0..3 {
            anyhow::ensure!(
                planes[i].len() == sizes[i],
                "plane {i}: {} bytes for a {} byte plane",
                planes[i].len(),
                sizes[i]
            );
            offsets[i] = at;
            at += sizes[i];
        }
        let [mut data, cb, cr] = planes;
        data.reserve_exact(sizes[1] + sizes[2]);
        data.extend_from_slice(&cb);
        data.extend_from_slice(&cr);
        Ok(CpuPlanarFrame {
            width,
            height,
            data,
            offsets,
            color,
            keyframe,
            recovery: punktfunk_core::reanchor::LocalRecovery::NONE,
            path,
        })
    }
}

/// GPU frame: dmabuf fds + plane layout for the Vulkan importer
/// (`pf-presenter::dmabuf`). Fds belong to `guard`'s mapped DRM frame; valid
/// until the guard drops. Tiled addressing is defined over the coded extent;
/// the importer crops sampling to the visible picture.
#[cfg(target_os = "linux")]
pub struct DmabufFrame {
    /// Visible picture extent.
    pub width: u32,
    pub height: u32,
    /// Exported VA surface extent. Tiled addressing is defined over this size;
    /// the presenter crops it to the visible picture.
    pub coded_width: u32,
    pub coded_height: u32,
    /// Combined DRM fourcc of the whole surface (NV12 for 8-bit VAAPI), from the
    /// decoder's software format — not the per-plane component formats.
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Source colour for the presenter's CSC pass (BT.709 narrow SDR, BT.2020 PQ HDR).
    pub color: ColorDesc,
    /// Intra keyframe (IDR/I) — the pump's post-loss re-anchor. See [`DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// Whole prediction chain was fully available. Corroborates a host
    /// `USER_FLAG_RECOVERY_ANCHOR`: see [`DecodedImage::anchor_evidence`].
    pub references_clean: bool,
    /// The decode's write fences as sync_files, one per exported object. Empty means
    /// the decoder already waited on the CPU; otherwise the importer waits them on
    /// the GPU (or polls) before it samples.
    pub sync_fds: Vec<std::os::fd::OwnedFd>,
    /// Identity of the surface behind the fds across the pool's lifetime: the
    /// importer keeps one `VkImage` per key instead of re-importing every frame.
    /// The high 32 bits change when the pool is rebuilt.
    pub pool_key: u64,
    /// The rung that decoded it, as the `stats:` decode-path tag.
    pub path: &'static str,
    pub guard: DrmFrameGuard,
}

#[cfg(target_os = "linux")]
pub struct DmabufPlane {
    pub fd: RawFd,
    pub offset: u32,
    pub stride: u32,
}

/// Keeps the decoded surface alive until GPU reads finish. Drop returns it
/// to the pool and closes the fds. The presenter dups imported fds and holds
/// the guard until its fence is waited.
#[cfg(target_os = "linux")]
pub struct DrmFrameGuard(
    /// Unread: the type is its `Drop` (return the surface or buffer to its
    /// decoder). Removing the field releases it at construction.
    #[allow(dead_code)]
    pub(crate) FrameGuard,
);

/// The rung's own hold behind a [`DrmFrameGuard`]; opaque to the presenter.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub(crate) enum FrameGuard {
    Va(crate::video_vaapi_native::VaFrameGuard),
    V4l2(crate::video_v4l2::V4l2FrameGuard),
}

enum Backend {
    /// pf-vkdecode on the presenter's device. Auto's top rung; pinnable as
    /// `native-vulkan`. Codec is chosen once at construction. Boxed: the decoder is large.
    NativeVulkan(Box<NativeVulkanDecoder>),
    /// Native VAAPI (`pf-vaapi`). Pinnable as `native-vaapi`; `auto` reaches it
    /// in vendor order. Unverified ([`native_evidence`]), so `auto` yields to
    /// proven Vulkan ([`native_rung_admitted`]). Boxed: two planners + pools.
    #[cfg(target_os = "linux")]
    NativeVaapi(Box<crate::video_vaapi_native::NativeVaapiDecoder>),
    /// A V4L2 decoder node (`video_v4l2`), stateful or stateless: the hardware
    /// rung where neither Vulkan Video nor VA-API exists. Pinnable as `native-v4l2`.
    #[cfg(target_os = "linux")]
    NativeV4l2(Box<crate::video_v4l2::NativeV4l2Decoder>),
    /// Native D3D11VA (`pf-dxvadec`): plans into the shareable-RGBA hand-off ring.
    /// Pinnable as `native-d3d11va`; `auto` reaches it. Boxed: two planners + session.
    #[cfg(windows)]
    NativeD3d11va(Box<crate::video_d3d11_native::NativeD3d11Decoder>),
    /// PyroWave compute on the presenter's device. No demotion rung: nothing else
    /// decodes it. Boxed: pinned create-info + plane ring.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    PyroWave(Box<crate::video_pyrowave::PyroWaveDecoder>),
    /// CPU rung (openh264 / rav1d). Last in every ladder, so it never demotes.
    /// The only rung that can fail to exist for a codec: see [`last_rung_verdict`].
    Software(SoftwareDecoder),
}

/// New-rung frames handed on before a demoted Vulkan rung's pools may go, and their floor
/// age: the same displacement PyroWave waits for its retired rings (depth-2 channel).
const RETIRE_HANDOVERS: u32 = 8;
const RETIRE_MIN_AGE: std::time::Duration = std::time::Duration::from_millis(250);

pub struct Decoder {
    backend: Backend,
    /// A native Vulkan rung demoted away, with the new rung's handovers since and when.
    /// The presenter can re-sample its last picture until a new one displaces it, so its
    /// pools outlive the swap ([`RETIRE_HANDOVERS`]).
    retiring: Option<(Box<NativeVulkanDecoder>, u32, std::time::Instant)>,
    /// Negotiated `quic::CODEC_*` bit. Every rung map and the software refusal
    /// key on it, so a demotion rebuilds for the same codec.
    wire_codec: u8,
    /// Consecutive hardware decode errors. One transient (a missing reference after loss) must not demote.
    vaapi_fails: u32,
    /// When the current error streak started. Count alone is not enough: a
    /// startup loss burst fails 3+ AUs in milliseconds, before the first-error
    /// IDR (~100–300 ms RTT) can rescue the hardware decoder.
    first_fail: Option<std::time::Instant>,
    /// Needs a fresh IDR (after an error or demotion). The pump drains it; the GOP has no periodic keyframe.
    want_keyframe: bool,
    /// This backend has delivered at least one frame. A never-delivered rung is
    /// one the session never had; its streak must not cost the rung below. Reset on swap.
    delivered: bool,
    /// Presenter's device, so demotion can build native Vulkan mid-stream.
    /// Cloned once per session; handles outlive every pump ([`VulkanDecodeDevice`]).
    vk: Option<VulkanDecodeDevice>,
    /// Negotiated picture shape for a mid-stream native rebuild ([`StreamFormat`]).
    stream: StreamFormat,
    /// Hardware rungs this session has run ([`RUNG_BIT_NATIVE_VULKAN`],
    /// [`RUNG_BIT_NATIVE_PLATFORM`]). The two native rungs sit in opposite vendor
    /// order; without this they could bounce the session. An entered rung is never re-entered.
    entered_rungs: u8,
    /// Presenter can import win32 external memory, so D3D11VA frames reach the screen. Kept for Vulkan→D3D11VA demotion.
    #[cfg(windows)]
    d3d11_import: bool,
    /// Presenter adapter LUID ([`VulkanDecodeDevice::adapter_luid`]) so demotion lands on the same GPU.
    #[cfg(windows)]
    adapter_luid: Option<[u8; 8]>,
    /// [`VulkanDecodeDevice::d3d11_hdr10`], for the same demotion rebuild.
    #[cfg(windows)]
    d3d11_hdr10: bool,
}

/// Native Vulkan ran this session — see [`Decoder::entered_rungs`].
const RUNG_BIT_NATIVE_VULKAN: u8 = 1 << 0;
/// Native platform rung (VAAPI on Linux, D3D11VA on Windows) ran this session.
const RUNG_BIT_NATIVE_PLATFORM: u8 = 1 << 1;

/// [`Decoder::entered_rungs`] bit this backend claims. 0 for rungs that cannot
/// be a demotion target twice (software is terminal; PyroWave never demotes).
fn rung_bit(backend: &Backend) -> u8 {
    match backend {
        Backend::NativeVulkan(_) => RUNG_BIT_NATIVE_VULKAN,
        #[cfg(target_os = "linux")]
        Backend::NativeVaapi(_) => RUNG_BIT_NATIVE_PLATFORM,
        #[cfg(windows)]
        Backend::NativeD3d11va(_) => RUNG_BIT_NATIVE_PLATFORM,
        _ => 0,
    }
}

/// Consecutive decode errors before hardware demotion. A lone transient re-requests an IDR and stays.
const VAAPI_DEMOTE_AFTER: u32 = 3;

/// Minimum streak age before demotion. Every error re-requests an IDR; a
/// successful decode resets the streak. 1 s lets that IDR arrive; a loss burst
/// of consecutive bad AUs must not strand the session on software first.
const HW_DEMOTE_MIN_STREAK: std::time::Duration = std::time::Duration::from_millis(1000);

/// May this `decode` answer clear the demotion streak?
///
/// Clearing it claims the decoder works. A delivered frame proves that, as
/// does a clean `Ok(None)` (buffered, or an H.265 RASL skip). Concealment is
/// not an `Err` but must not clear the streak: interleaved `Err`s would never
/// reach the threshold, and a forever-concealing rung would freeze with no escape.
fn clears_demotion_streak(delivered: bool, concealed: bool) -> bool {
    delivered || !concealed
}

/// `VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR` — raw flag in
/// [`VulkanDecodeDevice::decode_video_caps`] (this crate stays ash-free).
pub(crate) const VIDEO_CODEC_OP_DECODE_H264: u32 = 0x0000_0001;
/// `VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR`.
pub(crate) const VIDEO_CODEC_OP_DECODE_H265: u32 = 0x0000_0002;

/// `VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR`. What [`av1_hardware_decodable`]
/// reads and the caps bit [`native_codec`] demands for an AV1 session.
pub(crate) const VIDEO_CODEC_OP_DECODE_AV1: u32 = 0x0000_0004;

/// Native decoder for a wire codec plus the `VkVideoCodecOperationFlagBitsKHR`
/// the decode family must advertise, or `None` if pf-vkdecode cannot decode it.
///
/// Returned together: splitting them admits HEVC on an H.264-only family
/// (`vkCreateVideoSessionKHR` for an unsupported op is UB, not an error).
/// Presence here is "pf-vkdecode has a decoder", not auto admission ([`native_vulkan_gate`]).
pub(crate) fn native_codec(wire: u8) -> Option<(NativeCodec, u32)> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some((NativeCodec::H264, VIDEO_CODEC_OP_DECODE_H264)),
        punktfunk_core::quic::CODEC_HEVC => Some((NativeCodec::H265, VIDEO_CODEC_OP_DECODE_H265)),
        punktfunk_core::quic::CODEC_AV1 => Some((NativeCodec::Av1, VIDEO_CODEC_OP_DECODE_AV1)),
        _ => None,
    }
}

/// Native DXVA decoder for a wire codec, or `None`. No caps bit: DXVA
/// advertises a profile GUID, which [`crate::video_d3d11_native::NativeD3d11Decoder::new`] checks.
#[cfg(windows)]
fn native_d3d11_codec(wire: u8) -> Option<pf_dxvadec::Codec> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(pf_dxvadec::Codec::H264),
        punktfunk_core::quic::CODEC_HEVC => Some(pf_dxvadec::Codec::H265),
        punktfunk_core::quic::CODEC_AV1 => Some(pf_dxvadec::Codec::Av1),
        _ => None,
    }
}

/// Native VAAPI decoder for a wire codec, or `None`. No caps bit: VAAPI
/// advertises a profile/entrypoint pair, which [`crate::video_vaapi_native::NativeVaapiDecoder::new`] queries.
#[cfg(target_os = "linux")]
fn native_vaapi_codec(wire: u8) -> Option<pf_vaapi::Codec> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(pf_vaapi::Codec::H264),
        punktfunk_core::quic::CODEC_HEVC => Some(pf_vaapi::Codec::H265),
        punktfunk_core::quic::CODEC_AV1 => Some(pf_vaapi::Codec::Av1),
        _ => None,
    }
}

/// Log what `PUNKTFUNK_AU_FAULT` will do, including when the answer is nothing.
/// The injector lives on the native Vulkan decode entry; silence on any other
/// rung is indistinguishable from "injected and nothing detected it".
fn report_au_fault_env(native_rung: bool) {
    let Ok(spec) = std::env::var("PUNKTFUNK_AU_FAULT") else {
        return;
    };
    if spec.is_empty() {
        return;
    }
    match pf_vkdecode::AuFault::from_spec(&spec) {
        // The native backend logs the arming itself (mode + period).
        Some(_) if native_rung => {}
        Some(_) => tracing::warn!(
            value = %spec,
            "PUNKTFUNK_AU_FAULT is armed, but this session is NOT on the native \
             Vulkan rung — no AU will be corrupted and no detector will fire"
        ),
        None => tracing::warn!(
            value = %spec,
            "PUNKTFUNK_AU_FAULT not understood (want drop|truncate|flip[:period]) \
             — ignored"
        ),
    }
}

/// Name the landed rung and whether its evidence meets the automatic-priority bar.
/// `info` if verified, `warn` if not, with [`native_evidence`] verbatim.
/// The `stats:` decode-path tag names the rung but not its provenance.
fn log_rung(backend: &Backend, wire: u8) {
    let (rung, evidence) = match backend {
        Backend::NativeVulkan(_) => (
            NativeRung::Vulkan.name(),
            Some(native_evidence(NativeRung::Vulkan, wire)),
        ),
        #[cfg(windows)]
        Backend::NativeD3d11va(_) => (
            NativeRung::D3d11va.name(),
            Some(native_evidence(NativeRung::D3d11va, wire)),
        ),
        #[cfg(target_os = "linux")]
        Backend::NativeVaapi(_) => (
            NativeRung::Vaapi.name(),
            Some(native_evidence(NativeRung::Vaapi, wire)),
        ),
        #[cfg(target_os = "linux")]
        Backend::NativeV4l2(_) => (
            NativeRung::V4l2.name(),
            Some(native_evidence(NativeRung::V4l2, wire)),
        ),
        Backend::Software(_) => (
            NativeRung::Software.name(),
            Some(native_evidence(NativeRung::Software, wire)),
        ),
        // PyroWave is not in the evidence table: its own codec and decoder, nothing above or below it.
        #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
        Backend::PyroWave(_) => ("pyrowave", None),
    };
    let codec = wire_codec_name(wire);
    match evidence {
        Some(e) if e.verified => tracing::info!(
            rung,
            codec,
            automatic_priority_verified = true,
            evidence = e.note,
            "decode rung active"
        ),
        Some(e) => tracing::warn!(
            rung,
            codec,
            automatic_priority_verified = false,
            evidence = e.note,
            "decode rung active — evidence is below the automatic-priority bar \
             (evidence table, video_caps.rs)"
        ),
        None => tracing::info!(rung, codec, "decode rung active"),
    }
}

impl Decoder {
    /// The active rung is native Vulkan. It can still demote later.
    pub fn on_native_vulkan(&self) -> bool {
        matches!(self.backend, Backend::NativeVulkan(_))
    }

    /// Build the decode ladder. `wire` is the Welcome `quic::CODEC_*`; `pref` is
    /// Settings (`hardware` reads as auto); `vk` is the presenter's device.
    /// `PUNKTFUNK_DECODER` wins, then the setting; both default to auto.
    ///
    /// Auto order is [`VulkanDecodeDevice::prefer_vulkan_first`], then
    /// [`native_rung_admitted`]: an unproven rung does not go first when the rung
    /// below it is proven and usable. `stream` is the construction-time shape probe.
    pub fn new(
        wire: u8,
        pref: &str,
        vk: Option<&VulkanDecodeDevice>,
        stream: StreamFormat,
    ) -> Result<Decoder> {
        let stored = resolve_decoder_pref(std::env::var("PUNKTFUNK_DECODER").ok().as_deref(), pref);
        let choice = migrate_decoder_pref(&stored);
        if choice != stored {
            // Once per session, at `warn`: the stored name no longer exists, and
            // the rung below is not the one the settings file names.
            tracing::warn!(
                stored,
                using = choice,
                "the decoder preference named libavcodec's rung, which no longer exists \
                 (M10 removed FFmpeg from the client) — using the native rung for the \
                 same hardware path"
            );
        }
        #[cfg(windows)]
        let (d3d11_import, adapter_luid, d3d11_hdr10) = (
            vk.is_some_and(|v| v.d3d11_import),
            vk.and_then(|v| v.adapter_luid),
            vk.is_some_and(|v| v.d3d11_hdr10),
        );
        let done = |backend: Backend| {
            // One exit every backend leaves through, so a session that never
            // reaches the native constructor still reports `PUNKTFUNK_AU_FAULT`.
            report_au_fault_env(matches!(backend, Backend::NativeVulkan(_)));
            log_rung(&backend, wire);
            Ok(Decoder {
                entered_rungs: rung_bit(&backend),
                retiring: None,
                backend,
                wire_codec: wire,
                vaapi_fails: 0,
                first_fail: None,
                want_keyframe: false,
                delivered: false,
                vk: vk.cloned(),
                stream,
                #[cfg(windows)]
                d3d11_import,
                #[cfg(windows)]
                adapter_luid,
                #[cfg(windows)]
                d3d11_hdr10,
            })
        };
        let codec_name = wire_codec_name(wire);
        #[cfg(target_os = "linux")]
        let presenter_vendor = vk.map(|v| v.vendor_id);
        // Pins first: a pin skips vendor order. Refusal or init failure logs and
        // continues as `auto` — a pin's failure must not be quieter than auto's.
        let mut choice = choice;
        #[cfg(windows)]
        if choice == crate::video_d3d11_native::DECODER_PIN {
            match (native_d3d11_codec(wire), vk.filter(|v| v.d3d11_import)) {
                (Some(codec), Some(v)) => match crate::video_d3d11_native::NativeD3d11Decoder::new(
                    codec,
                    stream,
                    v.adapter_luid,
                    v.d3d11_hdr10,
                )
                .map(|d| d.with_planar(v.d3d11_nv12, v.d3d11_p010))
                {
                    Ok(d) => {
                        tracing::info!(
                            codec = codec_name,
                            decoder = d.name(),
                            "native D3D11VA hardware decode active \
                                 (pf-dxvadec, shared-texture hand-off)"
                        );
                        return done(Backend::NativeD3d11va(Box::new(d)));
                    }
                    Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                            "native D3D11VA init failed — demoting to the standard ladder"),
                },
                (None, _) => tracing::warn!(
                    codec = codec_name,
                    "PUNKTFUNK_DECODER=native-d3d11va refused (needs an H.264, HEVC or \
                     AV1 session) — standard ladder"
                ),
                (_, None) => tracing::warn!(
                    "PUNKTFUNK_DECODER=native-d3d11va refused (the presenter's device lacks \
                     the win32 external-memory import extensions) — standard ladder"
                ),
            }
            choice = "auto".to_string();
        }
        // Native VAAPI pin. Unverified; the pin is how a lab run reaches it when Vulkan is first.
        #[cfg(target_os = "linux")]
        if choice == crate::video_vaapi_native::DECODER_PIN {
            match native_vaapi_codec(wire) {
                Some(codec) => {
                    match NativeVaapiDecoder::new_for_presenter(codec, stream, presenter_vendor) {
                        Ok(d) => {
                            tracing::info!(
                                codec = codec_name,
                                decoder = d.name(),
                                "native VAAPI hardware decode active (pf-vaapi, zero-copy dmabuf)"
                            );
                            return done(Backend::NativeVaapi(Box::new(d)));
                        }
                        Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                            "native VAAPI init failed — demoting to the standard ladder"),
                    }
                }
                None => tracing::warn!(
                    codec = codec_name,
                    "PUNKTFUNK_DECODER=native-vaapi refused (needs an H.264, HEVC or \
                     AV1 session) — standard ladder"
                ),
            }
            choice = "auto".to_string();
        }
        #[cfg(target_os = "linux")]
        if choice == crate::video_v4l2::DECODER_PIN {
            match crate::video_v4l2::NativeV4l2Decoder::new(wire, stream) {
                Ok(d) => {
                    tracing::info!(
                        codec = codec_name,
                        decoder = d.name(),
                        "native V4L2 hardware decode active"
                    );
                    return done(Backend::NativeV4l2(Box::new(d)));
                }
                Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                    "native V4L2 init failed — demoting to the standard ladder"),
            }
            choice = "auto".to_string();
        }
        let mut native_tried = false;
        if choice == "native-vulkan" {
            if native_vulkan_gate(
                &choice,
                wire,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            ) {
                native_tried = true;
                let vk = vk.expect("gate demands video_decode, so vk is Some");
                let (codec, _) = native_codec(wire).expect("the gate admitted this codec");
                match NativeVulkanDecoder::new(vk, codec, stream) {
                    Ok(n) => {
                        tracing::info!(
                            codec = codec_name,
                            "native Vulkan Video hardware decode active \
                             (pf-vkdecode, presenter-shared device)"
                        );
                        return done(Backend::NativeVulkan(Box::new(n)));
                    }
                    Err(e) => tracing::warn!(reason = %format!("{e:#}"),
                        "native Vulkan decode init failed — demoting to the standard ladder"),
                }
            } else {
                // The gate is an AND of three; name all three. `video_decode=true`
                // beside "refused" is unreadable without the family's advertised ops.
                tracing::warn!(
                    codec = codec_name,
                    video_decode = vk.is_some_and(|v| v.video_decode),
                    decode_video_caps =
                        format_args!("0x{:X}", vk.map_or(0, |v| v.decode_video_caps)),
                    codec_op_needed =
                        format_args!("0x{:X}", native_codec(wire).map_or(0, |(_, op)| op)),
                    device = vk.map_or("", |v| v.device_name.as_str()),
                    "PUNKTFUNK_DECODER=native-vulkan refused (needs an H.264, HEVC or AV1 \
                     session and a presenter device whose decode family advertises that \
                     codec) — standard ladder"
                );
            }
            choice = "auto".to_string();
        }
        // Linux VAAPI rung, once: Intel/unknown take it before Vulkan, everyone
        // else after — NVIDIA and non-importing presenters never (`vaapi_auto_ok`).
        #[cfg(target_os = "linux")]
        let vaapi_rung = |choice: &str| -> Result<Option<Backend>> {
            if !vaapi_auto_ok(vk) {
                tracing::info!(
                    "native VAAPI outside the presenter's automatic safety gate (pin overrides)"
                );
                return Ok(None);
            }
            if let Some(codec) = native_vaapi_codec(wire) {
                match NativeVaapiDecoder::new_for_presenter(codec, stream, presenter_vendor) {
                    Ok(d) => {
                        tracing::info!(
                            codec = codec_name,
                            decoder = d.name(),
                            "native VAAPI hardware decode active (pf-vaapi, zero-copy dmabuf)"
                        );
                        return Ok(Some(Backend::NativeVaapi(Box::new(d))));
                    }
                    Err(e) => tracing::info!(reason = %format!("{e:#}"),
                        "native VAAPI unavailable — continuing down the ladder"),
                }
            }
            // `choice` is unread: a native pin is handled above, so by here it is the auto family.
            let _ = choice;
            Ok(None)
        };
        // Linux `auto`: VAAPI first unless Vulkan Video is the established
        // answer (NVIDIA: no usable VAAPI; VanGogh: VAAPI chroma-fringes).
        // Mesa exposes decode queues by default, which would move every AMD/Intel box onto Vulkan-on-Mesa.
        #[cfg(target_os = "linux")]
        let mut vaapi_tried = false;
        #[cfg(target_os = "linux")]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_first())
        {
            // Intel/unknown order: the rung below VAAPI is native Vulkan. If this
            // device can run that proven rung, VAAPI does not go first ([`native_rung_admitted`]).
            let below = native_vulkan_usable(
                wire,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            )
            .then_some(NativeRung::Vulkan);
            if native_rung_admitted(NativeRung::Vaapi, wire, below) {
                vaapi_tried = true;
                if let Some(b) = vaapi_rung(&choice)? {
                    return done(b);
                }
            } else {
                tracing::info!(
                    codec = codec_name,
                    evidence = native_evidence(NativeRung::Vaapi, wire).note,
                    "native VAAPI is this device's first hardware rung, but it has decoded \
                     nothing on any hardware and the rung below it — native Vulkan Video — \
                     has, for this codec, on this device: taking Vulkan first \
                     (PUNKTFUNK_DECODER=native-vaapi runs it anyway)"
                );
            }
        }
        // Windows `auto`: D3D11VA first unless Vulkan Video is the established
        // answer (NVIDIA/AMD). Intel advertises Vulkan Video, so the cap gate
        // alone does not keep it off that rung; DXVA is the path Windows players exercise.
        #[cfg(windows)]
        let d3d11_rung = |choice: &str| -> Result<Option<Backend>> {
            let Some(v) = vk.filter(|v| v.d3d11_import) else {
                // A pin that cannot work must log: a DXVA frame reaches the screen
                // only through the presenter's win32 import.
                if choice == crate::video_d3d11_native::DECODER_PIN {
                    bail!(
                        "PUNKTFUNK_DECODER=native-d3d11va but the presenter's device lacks the \
                         win32 external-memory import extensions — see the presenter log"
                    );
                }
                return Ok(None);
            };
            if let Some(codec) = native_d3d11_codec(wire) {
                match crate::video_d3d11_native::NativeD3d11Decoder::new(
                    codec,
                    stream,
                    v.adapter_luid,
                    v.d3d11_hdr10,
                )
                .map(|d| d.with_planar(v.d3d11_nv12, v.d3d11_p010))
                {
                    Ok(d) => {
                        tracing::info!(
                            codec = codec_name,
                            decoder = d.name(),
                            "native D3D11VA hardware decode active \
                             (pf-dxvadec, shared-texture hand-off)"
                        );
                        return Ok(Some(Backend::NativeD3d11va(Box::new(d))));
                    }
                    Err(e) => tracing::info!(reason = %format!("{e:#}"),
                        "native D3D11VA unavailable — continuing down the ladder"),
                }
            }
            Ok(None)
        };
        #[cfg(windows)]
        let mut d3d11_tried = false;
        #[cfg(windows)]
        if matches!(choice.as_str(), "auto" | "" | "hardware")
            && !vk
                .filter(|v| v.video_decode)
                .is_some_and(|v| v.prefer_vulkan_first())
            // Intel/unknown: `below` is `None` on purpose. Under DXVA AV1 the
            // next rung is CPU, not proven Vulkan. H.264/H.265 are verified, so they admit.
            && native_rung_admitted(NativeRung::D3d11va, wire, None)
        {
            d3d11_tried = true;
            if let Some(b) = d3d11_rung(&choice)? {
                return done(b);
            }
        }
        // Vulkan rung. `auto` reaches it from one place; [`native_vulkan_gate`]
        // is the whole admission. `native_tried` skips a repeat of the pin above.
        if !native_tried
            && native_vulkan_gate(
                &choice,
                wire,
                vk.is_some_and(|v| v.video_decode),
                vk.map_or(0, |v| v.decode_video_caps),
            )
        {
            let vk = vk.expect("gate demands video_decode, so vk is Some");
            let (codec, _) = native_codec(wire).expect("the gate admitted this codec");
            match NativeVulkanDecoder::new(vk, codec, stream) {
                Ok(n) => {
                    tracing::info!(
                        codec = codec_name,
                        "native Vulkan Video hardware decode active \
                         (pf-vkdecode auto rung, presenter-shared device)"
                    );
                    return done(Backend::NativeVulkan(Box::new(n)));
                }
                Err(e) => tracing::info!(reason = %format!("{e:#}"),
                    "native Vulkan decode unavailable — continuing down the ladder"),
            }
        }
        // VAAPI after Vulkan when that rung was not already tried.
        // `vaapi_auto_ok` may skip it to the final software attempt.
        #[cfg(target_os = "linux")]
        if choice != "software" && !vaapi_tried {
            if let Some(b) = vaapi_rung(&choice)? {
                return done(b);
            }
        }
        // V4L2 last: it is the rung of devices that have neither of the above.
        #[cfg(target_os = "linux")]
        if choice != "software" && v4l2_auto_ok(vk) {
            match crate::video_v4l2::NativeV4l2Decoder::new(wire, stream) {
                Ok(d) => {
                    tracing::info!(
                        codec = codec_name,
                        decoder = d.name(),
                        "native V4L2 hardware decode active"
                    );
                    return done(Backend::NativeV4l2(Box::new(d)));
                }
                Err(e) => tracing::info!(reason = %format!("{e:#}"),
                    "native V4L2 unavailable — continuing down the ladder"),
            }
        }
        // D3D11VA fallback when Vulkan is missing or failed. `d3d11_tried` skips the Intel/unknown first try.
        #[cfg(windows)]
        if choice != "software" && !d3d11_tried {
            if let Some(b) = d3d11_rung(&choice)? {
                return done(b);
            }
        }
        if choice == "software" {
            // Log why hardware was not attempted: a stored "software" pref otherwise silently skips it.
            tracing::info!(
                "software decode by preference (Settings decoder / PUNKTFUNK_DECODER) — \
                 hardware decode not attempted"
            );
        }
        // `?` can carry `NoSoftwareRung` (HEVC with no hardware). Stays typed to the pump ([`last_rung_verdict`]).
        done(Backend::Software(SoftwareDecoder::new(wire)?))
    }

    /// Wait for a Vulkan-Video GPU decode (timeline). `false` declines the
    /// sample: not this backend, timeout, missing ledger pair, or stale generation.
    pub fn wait_hw_decoded(&mut self, timeline_sem: u64, value: u64, timeout_ns: u64) -> bool {
        match &mut self.backend {
            Backend::NativeVulkan(d) => d.wait_timeline(timeline_sem, value, timeout_ns),
            _ => false,
        }
    }

    /// Whether a Vulkan-Video decode is complete now, without waiting. `true` off that
    /// backend: nothing is pending there.
    pub fn hw_decoded_now(&mut self, timeline_sem: u64, value: u64) -> bool {
        match &mut self.backend {
            Backend::NativeVulkan(d) => d.timeline_done(timeline_sem, value),
            _ => true,
        }
    }

    /// The decode wait is also the media clock boost (Intel on i915): the pump waits
    /// it at once instead of one AU behind.
    pub fn hw_wait_boosted(&self) -> bool {
        match &self.backend {
            Backend::NativeVulkan(d) => d.boosted(),
            _ => false,
        }
    }

    /// Decode-integrity counters, or `None` where the backend cannot answer
    /// (CPU, PyroWave). `None` is "cannot see corruption"; `Some(default)` is "looked and saw none".
    pub fn decode_health(&self) -> Option<DecodeHealth> {
        match &self.backend {
            Backend::NativeVulkan(d) => Some(d.health()),
            // DXVA sees concealment and refusals via the planner. No per-picture
            // status query, so `status_queries` is false and `failed` stays 0.
            #[cfg(windows)]
            Backend::NativeD3d11va(d) => Some(d.health()),
            // Same as DXVA: libva has no per-picture decode-status query.
            #[cfg(target_os = "linux")]
            Backend::NativeVaapi(d) => Some(d.health()),
            #[cfg(target_os = "linux")]
            Backend::NativeV4l2(d) => Some(d.health()),
            _ => None,
        }
    }

    /// Newest planned decode-order ordinal — the freeze watermark
    /// ([`NativeVkFrame::decode_order`]). 0 on lanes with no bitstream parser (and no local recovery).
    pub fn decode_order(&self) -> u64 {
        match &self.backend {
            Backend::NativeVulkan(d) => d.decode_order(),
            _ => 0,
        }
    }

    /// Open a PyroWave decoder: compute on the presenter's device.
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    pub fn new_pyrowave(
        vk: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
        chroma444: bool,
        color: ColorDesc,
        hdr16: bool,
    ) -> Result<Decoder> {
        // Never the native rung — see [`report_au_fault_env`].
        report_au_fault_env(false);
        Ok(Decoder {
            backend: Backend::PyroWave(Box::new(crate::video_pyrowave::PyroWaveDecoder::new(
                vk,
                width,
                height,
                shard_payload,
                chroma444,
                color,
                hdr16,
            )?)),
            wire_codec: punktfunk_core::quic::CODEC_PYROWAVE,
            vaapi_fails: 0,
            first_fail: None,
            want_keyframe: false,
            delivered: false,
            // PyroWave never demotes (failure renegotiates the codec). Demotion-rebuild
            // fields stay well-formed and unused.
            vk: None,
            stream: StreamFormat::SDR_420_8,
            entered_rungs: 0,
            retiring: None,
            #[cfg(windows)]
            d3d11_import: false,
            #[cfg(windows)]
            adapter_luid: None,
            #[cfg(windows)]
            d3d11_hdr10: false,
        })
    }

    /// Drain the IDR request. The pump calls this each iteration so a demoted
    /// or erroring decoder can resync under the infinite GOP.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Swap the backend in and reset the old one's health. One place so a call
    /// site cannot forget [`Self::entered_rungs`] and loop the ladder.
    fn install(&mut self, backend: Backend) {
        self.entered_rungs |= rung_bit(&backend);
        if let Backend::NativeVulkan(old) = std::mem::replace(&mut self.backend, backend) {
            self.retiring = Some((old, 0, std::time::Instant::now()));
        }
        self.vaapi_fails = 0;
        self.first_fail = None;
        self.delivered = false;
    }

    /// Demote from the failing `from` rung onto `built`, the `(decoder, backend)` of the
    /// rung named `rung`. False when it could not be built: the ladder tries the next one.
    fn demote_into(
        &mut self,
        e: &anyhow::Error,
        from: &str,
        rung: &str,
        built: Result<(&'static str, Backend)>,
    ) -> bool {
        match built {
            Ok((decoder, backend)) => {
                tracing::warn!(error = %format!("{e:#}"), fails = self.vaapi_fails, from, decoder,
                    "hardware decode failing repeatedly — demoting to {rung}");
                self.install(backend);
                true
            }
            Err(why) => {
                tracing::info!(reason = %format!("{why:#}"),
                    "{rung} unavailable for demotion — continuing down the ladder");
                false
            }
        }
    }

    /// Running rung is the platform native (VAAPI / D3D11VA). The demotion
    /// that goes sideways into native Vulkan fires only from here.
    fn is_native_platform_rung(&self) -> bool {
        #[cfg(target_os = "linux")]
        let it = matches!(self.backend, Backend::NativeVaapi(_));
        #[cfg(windows)]
        let it = matches!(self.backend, Backend::NativeD3d11va(_));
        #[cfg(not(any(target_os = "linux", windows)))]
        let it = false;
        it
    }

    /// Demote to software when the presenter cannot display hardware frames.
    /// Decode still succeeds in that state, so the error streak never fires
    /// and without this the stream stays black. No-op when already software.
    pub fn force_software(&mut self) -> Result<()> {
        if matches!(self.backend, Backend::Software(_)) {
            return Ok(());
        }
        tracing::warn!("presenter can't display hardware frames — demoting to software decode");
        // Same typed refusal as every software construction: HEVC has nothing below, so the pump reconnects.
        self.install(Backend::Software(SoftwareDecoder::new(self.wire_codec)?));
        self.want_keyframe = true;
        Ok(())
    }

    /// The freeze gate lifted on intra refresh marks. The wave healed the picture by
    /// overwrite, so the planner's damaged-chain marks are stale: without this, every later
    /// host anchor that descends from the wave is refuted until an IDR. Lanes without a
    /// planner have nothing to forget.
    pub fn forgive_unclean(&mut self) {
        match &mut self.backend {
            Backend::NativeVulkan(n) => n.forgive_unclean(),
            #[cfg(target_os = "linux")]
            Backend::NativeVaapi(v) => v.forgive_unclean(),
            #[cfg(target_os = "linux")]
            Backend::NativeV4l2(d) => d.forgive_unclean(),
            #[cfg(windows)]
            Backend::NativeD3d11va(d) => d.forgive_unclean(),
            _ => {}
        }
    }

    /// Feed one access unit (hosts are one-in/one-out). Hardware errors
    /// re-request an IDR; only a persistent streak demotes. `want_keyframe` is
    /// set either way — the infinite GOP has no other resync.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedImage>> {
        self.decode_frame(au, 0, true)
    }

    /// [`decode`](Self::decode) with wire facts. `user_flags` carries chunk
    /// alignment; `complete` is false for a partial delivery (PyroWave only,
    /// as localized blur).
    pub fn decode_frame(
        &mut self,
        au: &[u8],
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        let out = self.decode_frame_inner(au, user_flags, complete);
        if let (Ok(Some(_)), Some(r)) = (&out, self.retiring.as_mut()) {
            r.1 += 1;
            if r.1 >= RETIRE_HANDOVERS && r.2.elapsed() >= RETIRE_MIN_AGE {
                self.retiring = None;
            }
        }
        out
    }

    fn decode_frame_inner(
        &mut self,
        au: &[u8],
        // Only the PyroWave backend reads the flags; without that feature the param is unused.
        #[cfg_attr(
            not(all(any(target_os = "linux", windows), feature = "pyrowave")),
            allow(unused_variables)
        )]
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        // Native rungs: stream damage is not a decoder fault. Concealment is `Ok(None)`
        // plus the recovery request, which decides whether the `Ok` below may clear the
        // demotion streak. A driver `RESULT_STATUS` Failed stays an `Err`.
        let (result, concealed) = match &mut self.backend {
            Backend::NativeVulkan(n) => (
                n.decode(au).map(|f| f.map(DecodedImage::NativeVk)),
                n.take_recovery_request(),
            ),
            #[cfg(target_os = "linux")]
            Backend::NativeVaapi(v) => (
                v.decode(au).map(|f| f.map(DecodedImage::NativeDmabuf)),
                v.take_recovery_request(),
            ),
            #[cfg(target_os = "linux")]
            Backend::NativeV4l2(d) => (d.decode(au), d.take_recovery_request()),
            #[cfg(windows)]
            Backend::NativeD3d11va(d) => (
                d.decode(au).map(|f| f.map(DecodedImage::D3d11)),
                d.take_recovery_request(),
            ),
            // Nothing else decodes PyroWave: propagate the error; the pump renegotiates the codec.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Backend::PyroWave(p) => {
                let aligned = user_flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
                return Ok(p
                    .decode_frame(au, aligned, complete)?
                    .map(DecodedImage::PyroWave));
            }
            Backend::Software(s) => return Ok(s.decode(au)?.map(DecodedImage::Cpu)),
        };
        debug_assert!(complete, "partial AUs are pyrowave-only");
        self.want_keyframe |= concealed;
        match result {
            Ok(f) => {
                // Only an answer that proves the rung works may clear the streak.
                if clears_demotion_streak(f.is_some(), concealed) {
                    self.vaapi_fails = 0;
                    self.first_fail = None;
                }
                self.delivered |= f.is_some();
                Ok(f)
            }
            Err(e) => {
                let which = match self.backend {
                    Backend::NativeVulkan(_) => "native Vulkan Video",
                    #[cfg(windows)]
                    Backend::NativeD3d11va(_) => "native D3D11VA",
                    #[cfg(target_os = "linux")]
                    Backend::NativeVaapi(_) => "native VAAPI",
                    #[cfg(target_os = "linux")]
                    Backend::NativeV4l2(_) => "native V4L2",
                    // PyroWave returns above and software never reaches here.
                    _ => "hardware",
                };
                self.vaapi_fails += 1;
                self.want_keyframe = true;
                let first = *self.first_fail.get_or_insert_with(std::time::Instant::now);
                // A device that cannot host the codec refuses every AU the same way;
                // waiting out the streak only costs the opening seconds.
                let device_fact = e.chain().any(|c| {
                    c.downcast_ref::<pf_vkdecode::VkDecodeError>()
                        .is_some_and(pf_vkdecode::VkDecodeError::is_device_fact)
                });
                if device_fact
                    || (self.vaapi_fails >= VAAPI_DEMOTE_AFTER
                        && first.elapsed() >= HW_DEMOTE_MIN_STREAK)
                {
                    // A never-delivered native rung is a decoder the session never
                    // had; it must not cost the rung below. `entered_rungs` keeps
                    // the walk monotone (native rungs sit in opposite vendor order).
                    // `vaapi_auto_ok` bars the same rung on NVIDIA and on
                    // presenters that cannot import its dmabufs.
                    #[cfg(target_os = "linux")]
                    if self.entered_rungs & RUNG_BIT_NATIVE_PLATFORM == 0
                        && vaapi_auto_ok(self.vk.as_ref())
                    {
                        if let Some(codec) = native_vaapi_codec(self.wire_codec) {
                            let built = NativeVaapiDecoder::new_for_presenter(
                                codec,
                                self.stream,
                                self.vk.as_ref().map(|v| v.vendor_id),
                            )
                            .map(|d| (d.name(), Backend::NativeVaapi(Box::new(d))));
                            if self.demote_into(&e, which, "native VAAPI", built) {
                                return Ok(None);
                            }
                        }
                    }
                    #[cfg(windows)]
                    if self.entered_rungs & RUNG_BIT_NATIVE_PLATFORM == 0 && self.d3d11_import {
                        if let Some(codec) = native_d3d11_codec(self.wire_codec) {
                            let (nv12, p010) = self
                                .vk
                                .as_ref()
                                .map_or((false, false), |v| (v.d3d11_nv12, v.d3d11_p010));
                            let built = crate::video_d3d11_native::NativeD3d11Decoder::new(
                                codec,
                                self.stream,
                                self.adapter_luid,
                                self.d3d11_hdr10,
                            )
                            .map(|d| {
                                let d = d.with_planar(nv12, p010);
                                (d.name(), Backend::NativeD3d11va(Box::new(d)))
                            });
                            if self.demote_into(&e, which, "native D3D11VA", built) {
                                return Ok(None);
                            }
                        }
                    }
                    // Failing platform native on Intel/unknown has Vulkan below it.
                    // Only fires from a native platform rung.
                    if self.entered_rungs & RUNG_BIT_NATIVE_VULKAN == 0
                        && self.is_native_platform_rung()
                    {
                        if let Some(v) = self.vk.clone().filter(|v| v.video_decode) {
                            if native_vulkan_gate(
                                "auto",
                                self.wire_codec,
                                true,
                                v.decode_video_caps,
                            ) {
                                let (codec, _) =
                                    native_codec(self.wire_codec).expect("the gate admitted it");
                                let built =
                                    NativeVulkanDecoder::new(&v, codec, self.stream).map(|n| {
                                        (
                                            NativeRung::Vulkan.name(),
                                            Backend::NativeVulkan(Box::new(n)),
                                        )
                                    });
                                if self.demote_into(&e, which, "native Vulkan Video", built) {
                                    return Ok(None);
                                }
                            }
                        }
                    }
                    tracing::warn!(error = %format!("{e:#}"), fails = self.vaapi_fails,
                        "{which} decode failing repeatedly — demoting to software");
                    // Ladder bottom. H.264/AV1 always builds; HEVC `?` carries
                    // `NoSoftwareRung` to the pump, which reconnects without HEVC.
                    self.install(Backend::Software(SoftwareDecoder::new(self.wire_codec)?));
                } else {
                    tracing::debug!(backend = which, error = %e,
                        "decode error — requesting keyframe, keeping hardware decode");
                }
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video_caps::tests::decode_device;

    /// DXGI packs the four Device Manager fields high to low, 16 bits each.
    #[test]
    fn umd_version_splits_into_device_manager_fields() {
        let raw = (32i64 << 48) | (21025i64 << 16) | 10016;
        assert_eq!(umd_version_parts(raw), [32, 0, 21025, 10016]);
    }

    /// Presenter uploads with no stride: the copy undoes decoder padding, and a
    /// short plane is refused rather than read past.
    #[test]
    fn planar_frames_are_tightly_packed_and_short_planes_are_refused() {
        let color = ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        };
        // 4×2 luma, 2×1 chroma, all planes padded by 3 bytes per row.
        let y: Vec<u8> = vec![1, 2, 3, 4, 9, 9, 9, 5, 6, 7, 8, 9, 9, 9];
        let u: Vec<u8> = vec![10, 11, 9, 9, 9];
        let v: Vec<u8> = vec![20, 21, 9, 9, 9];
        let none = punktfunk_core::reanchor::LocalRecovery::NONE;
        let f =
            CpuPlanarFrame::from_i420(4, 2, [&y, &u, &v], [7, 5, 5], color, true, none).unwrap();
        assert_eq!(f.plane(0), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(f.plane(1), &[10, 11]);
        assert_eq!(f.plane(2), &[20, 21]);
        assert_eq!(f.plane_dims(0), (4, 2));
        assert_eq!(f.plane_dims(1), (2, 1));
        // Odd dimensions round chroma up; dropping the last sample would read past the plane.
        assert_eq!(CpuPlanarFrame::chroma_dims(5, 3), (3, 2));
        // A short plane is a geometry disagreement, not something to truncate.
        let short: Vec<u8> = vec![1, 2, 3];
        assert!(
            CpuPlanarFrame::from_i420(4, 2, [&short, &u, &v], [7, 5, 5], color, true, none)
                .is_err()
        );
        // A stride narrower than the picture is the same disagreement.
        assert!(
            CpuPlanarFrame::from_i420(4, 2, [&y, &u, &v], [2, 5, 5], color, true, none).is_err()
        );
    }

    /// An `Ok` clears the demotion streak only when it proves the rung works.
    /// Concealment with no picture proves nothing: interleaved `Err`s must still
    /// reach the threshold, and a forever-concealing rung must still have an escape.
    #[test]
    fn only_an_answer_that_proves_the_rung_works_clears_the_demotion_streak() {
        assert!(clears_demotion_streak(true, false));
        assert!(clears_demotion_streak(true, true));
        // Clean `Ok(None)` is proof (buffered, or an H.265 RASL skip).
        assert!(clears_demotion_streak(false, false));
        assert!(!clears_demotion_streak(false, true));

        // Alternating driver errors with concealment must still reach the threshold.
        let mut fails = 0u32;
        for concealed_ok in [false, true, false, true, false] {
            if concealed_ok {
                if clears_demotion_streak(false, true) {
                    fails = 0;
                }
            } else {
                fails += 1; // driver verdict Err
            }
        }
        assert!(
            fails >= VAAPI_DEMOTE_AFTER,
            "three driver errors interleaved with concealment must still reach the \
             demotion threshold — they got to {fails}"
        );

        // A re-anchor wait produces no picture; every AU of it is an error, so
        // a rung that never recovers reaches the threshold.
        let mut fails = 0u32;
        for errored in [true; 5] {
            // failing AU, then four skipped ones
            if errored {
                fails += 1;
            } else if clears_demotion_streak(false, false) {
                fails = 0;
            }
        }
        assert!(fails >= VAAPI_DEMOTE_AFTER);

        // Counterfactual: answering skipped AUs as clean `Ok(None)` zeroes the
        // streak and `VAAPI_DEMOTE_AFTER` is unreachable. That is the AV1
        // film-grain-without-profile path in `NativeVulkanDecoder::new`.
        let mut fails = 0u32;
        for errored in [true, false, false, true, false, false, true, false, false] {
            if errored {
                fails += 1;
            } else if clears_demotion_streak(false, false) {
                fails = 0;
            }
        }
        assert!(
            fails < VAAPI_DEMOTE_AFTER,
            "a recovery wait answered as a CLEAN AU zeroes the streak once per frame \
             — which is why it must not be answered that way; it got to {fails}"
        );
    }

    /// Ordering only: auto tries Vulkan first on NVIDIA/AMD and the platform rung
    /// first on Intel/unknown. Separate admission gates may skip a platform rung.
    #[test]
    fn vulkan_first_on_nvidia_and_amd_only() {
        assert!(decode_device(0x10DE, "NVIDIA GeForce RTX 5070 Ti").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD RADV VANGOGH").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD Custom GPU 0405 (RADV VANGOGH)").prefer_vulkan_first());
        assert!(decode_device(0x1002, "AMD Radeon RX 7800 XT (RADV NAVI32)").prefer_vulkan_first());
        assert!(
            !decode_device(0x8086, "Intel(R) Arc(tm) A770 Graphics (DG2)").prefer_vulkan_first()
        );
        // Discrete Arc advertises Vulkan Video and must still land on D3D11VA in auto.
        assert!(!decode_device(0x8086, "Intel(R) Arc(TM) B580 Graphics").prefer_vulkan_first());
        assert!(!decode_device(0x8086, "Intel(R) Arc(TM) Pro Graphics").prefer_vulkan_first());
    }
}
