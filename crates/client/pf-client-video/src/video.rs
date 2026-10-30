//! Video decode: reassembled access units → frames for the presenter.
//!
//! Ladder: Vulkan Video, D3D11VA/VAAPI, then openh264 or rav1d. The order per
//! platform and GPU vendor is `video_ladder`; [`Decoder::new`] walks it and a rung
//! that keeps failing demotes along it. [`native_evidence`] records which
//! pairs have hardware validation; [`native_rung_admitted`] yields an unverified
//! rung only to a verified rung usable on the same device. `PUNKTFUNK_DECODER`
//! pins skip that rule and still fall through after init failure.
//! [`migrate_decoder_pref`] rewrites stored libavcodec names onto native pins.
//!
//! Hosts emit zero-reorder streams: one AU in, one picture out. The CPU rung
//! has no HEVC decoder; [`last_rung_verdict`] reconnects with a codec this
//! build can finish. The gates and their evidence live in `video_caps`.

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

pub(crate) use crate::video_caps::resolve_decoder_pref;
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
#[cfg(target_os = "linux")]
pub use crate::video_caps::{vaapi_av1_decodable, vaapi_hevc_decodable};
pub use crate::video_color::{csc_rows, ColorDesc};
use crate::video_ladder::{demotions, ladder, Note, Os, Step};
use crate::video_rung::Rung;
/// Re-export so SESSION (and its tests) can name the refusal by type.
/// The module stays private, like every other backend.
pub use crate::video_software::NoSoftwareRung;
use crate::video_software::SoftwareDecoder;
/// Defined in [`crate::video_types`] so `d3d11va` can name them without this
/// module. Call sites keep the `video::` paths.
pub use crate::video_types::{umd_version_parts, DecodeHealth, StreamFormat};
#[cfg(target_os = "linux")]
use crate::video_vaapi_native::NativeVaapiDecoder;
/// The Vulkan handoff types live in [`crate::video_vk`] so Android can reach them without
/// the `desktop` half of this crate; re-exported so every desktop call site keeps naming
/// them here.
pub use crate::video_vk::{QueueLock, QueueLockGuard, VulkanDecodeDevice};
use crate::video_vk_native::{NativeCodec, NativeVulkanDecoder};
/// Lives in punktfunk-core, where every settings screen reads it; re-exported so
/// desktop callers keep `video::migrate_decoder_pref`.
pub use punktfunk_core::decoder_pref::migrate_decoder_pref;

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
pub use crate::video_d3d11::{
    D3d11Frame, PoolSlice, SharedSlot, SliceGuard, SliceMode, SliceRelease, SlotFence, SlotFormat,
    SlotHandle,
};

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

/// Hands a shipped picture's token back to its rung, once, on drop. The presenter
/// holds the frame until its sampling fence is waited, so drop means the GPU is done
/// reading. A dead channel is a rung demoted or rebuilt, and is ignored.
pub struct ReleaseGuard<T: Copy> {
    tx: std::sync::mpsc::Sender<T>,
    token: T,
}

impl<T: Copy> ReleaseGuard<T> {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<T>, token: T) -> Self {
        Self { tx, token }
    }

    /// The token this guard hands back.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn token(&self) -> T {
        self.token
    }
}

impl<T: Copy> Drop for ReleaseGuard<T> {
    fn drop(&mut self) {
        let _ = self.tx.send(self.token);
    }
}

/// The Vulkan rung's guard: its token also says whether the picture was presented.
pub type NativeReleaseGuard = ReleaseGuard<NativeReleaseToken>;

impl ReleaseGuard<NativeReleaseToken> {
    /// Sampling submit, including the frame's `value + 1` timeline signal, was enqueued.
    /// The decoder then waits that write-back.
    pub fn mark_presented(&mut self) {
        self.token.presented = true;
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
    /// Zero on a device with no plane views; read [`Self::ycbcr_view`] there.
    pub plane_views: [u64; 2],
    /// The picture through a sampler Y'CbCr conversion built from
    /// `pf_vkdecode::ycbcr_conversion_info`. Zero where [`Self::plane_views`] exist.
    pub ycbcr_view: u64,
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
            DecodedImage::D3d11(f) => f.keyframe(),
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
            DecodedImage::D3d11(f) => f.references_clean(),
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
            DecodedImage::D3d11(f) => (f.width(), f.height()),
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            DecodedImage::PyroWave(f) => (f.width, f.height),
            DecodedImage::NativeVk(f) => (f.width, f.height),
        }
    }

    /// The rung that decoded this frame, as the `stats:` decode-path tag. A machine
    /// interface: additive only, and surviving tags keep their exact spelling. A D3D11VA
    /// pool slice adds how the presenter reads it: the pool itself or a copy.
    pub fn path_label(&self) -> &'static str {
        match self {
            DecodedImage::Cpu(f) => f.path,
            #[cfg(target_os = "linux")]
            DecodedImage::NativeDmabuf(f) => f.path,
            #[cfg(windows)]
            DecodedImage::D3d11(D3d11Frame::Slice(s)) => match s.mode {
                SliceMode::Bind => "native-d3d11va-bind",
                SliceMode::Copy => "native-d3d11va-copy",
            },
            #[cfg(windows)]
            DecodedImage::D3d11(D3d11Frame::Shared(_)) => "native-d3d11va",
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

/// The rung's own hold behind a [`DrmFrameGuard`]; opaque to the presenter. The
/// token goes back first, then the fds close.
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub(crate) enum FrameGuard {
    /// One fd per OBJECT, not per plane: planes share objects, and closing a shared
    /// fd twice would close an unrelated file.
    Va(
        ReleaseGuard<crate::video_vaapi_native::VaRelease>,
        Vec<std::os::fd::OwnedFd>,
    ),
    /// The pool's fds, shared: they stay open past a pool rebuild, so an imported
    /// frame never dangles.
    V4l2(
        ReleaseGuard<crate::video_v4l2::Release>,
        std::sync::Arc<Vec<std::os::fd::OwnedFd>>,
    ),
}

/// New-rung frames handed on before a demoted Vulkan rung's pools may go. The release
/// tokens do not cover this: the presenter re-samples its last picture on a redraw
/// until a new one displaces it, and the depth-2 pump channel can still hold an old
/// one. Eight new frames displace them all.
const RETIRE_HANDOVERS: u32 = 8;
/// Floor age for the same wait. Frames keep flowing while `present` is stalled on an
/// occluded swapchain, and would count the eight off with the old picture still up.
const RETIRE_MIN_AGE: std::time::Duration = std::time::Duration::from_millis(250);

pub struct Decoder {
    /// The rung decoding now ([`Rung`]).
    rung: Box<dyn Rung>,
    /// A native Vulkan rung demoted away, with the new rung's handovers since and when.
    /// The presenter can re-sample its last picture until a new one displaces it, so its
    /// pools outlive the swap ([`RETIRE_HANDOVERS`]).
    retiring: Option<(Box<dyn Rung>, u32, std::time::Instant)>,
    /// [`Self::set_decode_wake`]'s callback, re-armed on every rung the pump installs.
    wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Negotiated `quic::CODEC_*` bit. Every rung map and the software refusal
    /// key on it, so a demotion rebuilds for the same codec.
    wire_codec: u8,
    /// Consecutive hardware decode errors. One transient (a missing reference after loss) must not demote.
    fails: u32,
    /// When the current error streak started. Count alone is not enough: a
    /// startup loss burst fails 3+ AUs in milliseconds, before the first-error
    /// IDR (~100–300 ms RTT) can rescue the hardware decoder.
    first_fail: Option<std::time::Instant>,
    /// Needs a fresh IDR (after an error or demotion). The pump drains it; the GOP has no periodic keyframe.
    want_keyframe: bool,
    /// This rung has delivered at least one frame. A never-delivered rung is
    /// one the session never had; its streak must not cost the rung below. Reset on swap.
    delivered: bool,
    /// Presenter's device: a demotion rebuild reads every device fact from here.
    /// Cloned once per session; handles outlive every pump ([`VulkanDecodeDevice`]).
    vk: Option<VulkanDecodeDevice>,
    /// Negotiated picture shape for a mid-stream native rebuild ([`StreamFormat`]).
    stream: StreamFormat,
    /// The ladder rungs this session has run, oldest first. [`demotions`] never names
    /// one again: the two native rungs demote onto each other and could otherwise
    /// bounce the session.
    ran: Vec<NativeRung>,
    /// Rungs whose pictures the presenter could not show. A device fact, not a codec's:
    /// a new format's walk passes over them too.
    refused: Vec<NativeRung>,
    /// The resolved decoder preference the ladder was walked for; a new format walks it again.
    choice: String,
}

/// Consecutive decode errors before hardware demotion. A lone transient re-requests an IDR and stays.
const HW_DEMOTE_AFTER: u32 = 3;

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
/// Presence here is "pf-vkdecode has a decoder", not auto admission ([`native_vulkan_usable`]).
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

/// The ladder's first rung that opens for `wire` and `stream` under `choice`, else the CPU
/// rung. The walk passes over `refused`. `?` can carry `NoSoftwareRung` (HEVC with no
/// hardware), typed to the pump ([`last_rung_verdict`]).
fn first_rung(
    choice: &str,
    wire: u8,
    stream: StreamFormat,
    vk: Option<&VulkanDecodeDevice>,
    refused: &[NativeRung],
) -> Result<Box<dyn Rung>> {
    for step in ladder(Os::HOST, choice, wire, vk) {
        let rung = match step {
            Step::Try(rung) | Step::Pin(rung) if refused.contains(&rung) => {
                tracing::info!(
                    "{} passed over: the presenter can't show its pictures",
                    rung.label()
                );
                continue;
            }
            Step::Try(rung) | Step::Pin(rung) => rung,
            Step::Note(note) => {
                log_note(note, choice, wire, vk);
                continue;
            }
        };
        match build_rung(rung, wire, stream, vk) {
            Ok(built) => return Ok(built),
            // A pin's failure warns: it must not be quieter than auto's, which only informs.
            Err(e) if matches!(step, Step::Pin(_)) => {
                tracing::warn!(reason = %format!("{e:#}"),
                    "{} init failed — demoting to the standard ladder", rung.name());
            }
            Err(e) => {
                tracing::info!(reason = %format!("{e:#}"),
                    "{} unavailable — continuing down the ladder", rung.name());
            }
        }
    }
    // `?` can carry `NoSoftwareRung` (HEVC with no hardware). Stays typed to the pump ([`last_rung_verdict`]).
    Ok(Box::new(SoftwareDecoder::new(wire)?))
}

/// Build `rung` for `wire` on the presenter's device and log it active. Pin, auto and
/// demotion all build here; each keeps its own admission gate and failure log. `Err` is
/// a rung that cannot run: no decoder for the codec, no device it needs, or a failed init.
fn build_rung(
    rung: NativeRung,
    wire: u8,
    stream: StreamFormat,
    vk: Option<&VulkanDecodeDevice>,
) -> Result<Box<dyn Rung>> {
    #[cfg(test)]
    {
        if let Some(scripted) = crate::video_ladder::tests::scripted(rung) {
            return scripted;
        }
    }
    let built: Box<dyn Rung> = match rung {
        NativeRung::Vulkan => {
            let vk = vk.context("no presenter device")?;
            let (codec, _) =
                native_codec(wire).context("no Vulkan Video decoder for this codec")?;
            Box::new(NativeVulkanDecoder::new(vk, codec, stream)?)
        }
        #[cfg(windows)]
        NativeRung::D3d11va => {
            // A DXVA frame reaches the screen through the Vulkan presenter's win32 import,
            // or as a pool slice on the D3D11 presenter's own device.
            let v = vk.filter(|v| v.d3d11_import || v.d3d11_present).context(
                "the presenter neither imports D3D11 textures nor presents through D3D11",
            )?;
            let codec = native_d3d11_codec(wire).context("no DXVA decoder for this codec")?;
            let d = crate::video_d3d11_native::NativeD3d11Decoder::new(
                codec,
                stream,
                v.d3d11_device.as_ref().map(|h| h.device().clone()),
                v.adapter_luid,
                v.d3d11_hdr10,
                v.d3d11_fence,
            )?;
            Box::new(d.with_planar(v.d3d11_nv12, v.d3d11_p010))
        }
        #[cfg(target_os = "linux")]
        NativeRung::Vaapi => {
            let codec = native_vaapi_codec(wire).context("no VAAPI decoder for this codec")?;
            let vendor = vk.map(|v| v.vendor_id);
            Box::new(NativeVaapiDecoder::new_for_presenter(
                codec, stream, vendor,
            )?)
        }
        #[cfg(target_os = "linux")]
        NativeRung::V4l2 => Box::new(crate::video_v4l2::NativeV4l2Decoder::new(wire, stream)?),
        other => bail!("{} is not a hardware rung on this OS", other.name()),
    };
    tracing::info!(
        codec = wire_codec_name(wire),
        decoder = built.name(),
        "{} hardware decode active",
        rung.name()
    );
    Ok(built)
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
fn log_rung(active: &dyn Rung, wire: u8) {
    let (rung, evidence) = match active.id() {
        Some(id) => (id.name(), Some(native_evidence(id, wire))),
        // PyroWave is not in the evidence table: its own codec and decoder, nothing above or below it.
        None => (active.name(), None),
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

/// Say why the walk passes a rung over, when it gets there.
fn log_note(note: Note, choice: &str, wire: u8, vk: Option<&VulkanDecodeDevice>) {
    let codec = wire_codec_name(wire);
    match note {
        // The gate is an AND of three; name all three. `video_decode=true`
        // beside "refused" is unreadable without the family's advertised ops.
        Note::VulkanPinRefused => tracing::warn!(
            codec,
            video_decode = vk.is_some_and(|v| v.video_decode),
            decode_video_caps = format_args!("0x{:X}", vk.map_or(0, |v| v.decode_video_caps)),
            codec_op_needed = format_args!("0x{:X}", native_codec(wire).map_or(0, |(_, op)| op)),
            device = vk.map_or("", |v| v.device_name.as_str()),
            "PUNKTFUNK_DECODER=native-vulkan refused (needs an H.264, HEVC or AV1 \
             session and a presenter device whose decode family advertises that \
             codec) — standard ladder"
        ),
        Note::VaapiYields => tracing::info!(
            codec,
            evidence = native_evidence(NativeRung::Vaapi, wire).note,
            "native VAAPI is this device's first hardware rung, but it has decoded \
             nothing on any hardware and the rung below it — native Vulkan Video — \
             has, for this codec, on this device: taking Vulkan first \
             (PUNKTFUNK_DECODER=native-vaapi runs it anyway)"
        ),
        Note::VaapiGated => tracing::info!(
            "native VAAPI outside the presenter's automatic safety gate (pin overrides)"
        ),
        // A stored "software" preference otherwise skips hardware in silence.
        Note::SoftwareOnly => tracing::info!(
            "software decode by preference (Settings decoder / PUNKTFUNK_DECODER) — \
             hardware decode not attempted"
        ),
        // A typo would otherwise decode as `auto` in silence.
        Note::UnknownPreference => tracing::warn!(
            preference = choice,
            "unknown decoder preference read as auto"
        ),
    }
}

impl Decoder {
    /// The active rung is native Vulkan. It can still demote later.
    pub fn on_native_vulkan(&self) -> bool {
        self.rung.id() == Some(NativeRung::Vulkan)
    }

    /// Build the decode ladder. `wire` is the Welcome `quic::CODEC_*`; `pref` is
    /// Settings (`hardware` reads as auto); `vk` is the presenter's device.
    /// `PUNKTFUNK_DECODER` wins, then the setting; both default to auto.
    ///
    /// The order is `video_ladder::ladder`'s; the CPU rung follows it. `stream` is
    /// the construction-time shape probe.
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
        let rung = first_rung(&choice, wire, stream, vk, &[])?;
        // One exit every rung leaves through, so a session that never reaches the native
        // constructor still reports `PUNKTFUNK_AU_FAULT`.
        report_au_fault_env(rung.id() == Some(NativeRung::Vulkan));
        log_rung(rung.as_ref(), wire);
        Ok(Decoder {
            ran: rung.id().into_iter().collect(),
            refused: Vec::new(),
            retiring: None,
            wake: None,
            rung,
            wire_codec: wire,
            fails: 0,
            first_fail: None,
            want_keyframe: false,
            delivered: false,
            vk: vk.cloned(),
            stream,
            choice,
        })
    }

    /// The host changed the stream's codec or picture shape at the frame about to decode:
    /// walk the ladder again for `wire` and `stream` and install its first rung that opens,
    /// as a connect would. A Vulkan rung retires as on a demotion. The rungs run so far were
    /// another format's, but the presenter's refusals stand. `Err` is the typed refusal
    /// when nothing can decode it.
    pub fn reformat(&mut self, wire: u8, stream: StreamFormat) -> Result<()> {
        let rung = first_rung(&self.choice, wire, stream, self.vk.as_ref(), &self.refused)?;
        log_rung(rung.as_ref(), wire);
        (self.wire_codec, self.stream) = (wire, stream);
        self.ran.clone_from(&self.refused);
        self.install(rung);
        self.want_keyframe = false;
        Ok(())
    }

    /// Wait for a Vulkan-Video GPU decode (timeline). `false` declines the
    /// sample: not this backend, timeout, missing ledger pair, or stale generation.
    pub fn wait_hw_decoded(&mut self, timeline_sem: u64, value: u64, timeout_ns: u64) -> bool {
        self.rung.wait_timeline(timeline_sem, value, timeout_ns)
    }

    /// Whether a Vulkan-Video decode is complete now, without waiting. `true` off that
    /// backend: nothing is pending there.
    pub fn hw_decoded_now(&mut self, timeline_sem: u64, value: u64) -> bool {
        self.rung.timeline_done(timeline_sem, value)
    }

    /// The decode wait is also the media clock boost (Intel on i915): the pump waits
    /// it at once instead of one AU behind.
    pub fn hw_wait_boosted(&self) -> bool {
        self.rung.boosted()
    }

    /// Decode-integrity counters, or `None` where the backend cannot answer
    /// (CPU, PyroWave). `None` is "cannot see corruption"; `Some(default)` is "looked and saw none".
    pub fn decode_health(&self) -> Option<DecodeHealth> {
        self.rung.health()
    }

    /// Newest planned decode-order ordinal — the freeze watermark
    /// ([`NativeVkFrame::decode_order`]). 0 on lanes with no bitstream parser (and no local recovery).
    pub fn decode_order(&self) -> u64 {
        self.rung.decode_order()
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
        let mut rung = crate::video_pyrowave::PyroWaveDecoder::new(
            vk,
            width,
            height,
            shard_payload,
            chroma444,
            color,
            hdr16,
        )?;
        // The presenter's device outlives every frame it is handed, as it does for the
        // plane ring: nothing else holds it for a lent slot.
        if let Err(e) = rung.enable_in_place(std::sync::Arc::new(())) {
            tracing::info!(error = %format!("{e:#}"), "PyroWave AUs are copied into the decoder");
        }
        Ok(Decoder {
            rung: Box::new(rung),
            wire_codec: punktfunk_core::quic::CODEC_PYROWAVE,
            fails: 0,
            first_fail: None,
            want_keyframe: false,
            delivered: false,
            // PyroWave never demotes (failure renegotiates the codec). Demotion-rebuild
            // fields stay well-formed and unused.
            vk: None,
            stream: StreamFormat::SDR_420_8,
            ran: Vec::new(),
            refused: Vec::new(),
            retiring: None,
            choice: String::new(),
            wake: None,
        })
    }

    /// Drain the IDR request. The pump calls this each iteration so a demoted
    /// or erroring decoder can resync under the infinite GOP.
    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.want_keyframe)
    }

    /// Swap the rung in and reset the old one's health. One place so a call
    /// site cannot forget [`Self::ran`] and loop the ladder.
    /// Run `wake` off the pump's thread as each hardware decode completes, so the pump
    /// can block for the next AU and the pixels at once ([`Rung::set_decode_wake`]).
    pub fn set_decode_wake(&mut self, wake: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.rung.set_decode_wake(wake.clone());
        self.wake = Some(wake);
    }

    fn install(&mut self, rung: Box<dyn Rung>) {
        self.ran.extend(rung.id());
        let old = std::mem::replace(&mut self.rung, rung);
        if let Some(wake) = &self.wake {
            self.rung.set_decode_wake(wake.clone());
        }
        if old.id() == Some(NativeRung::Vulkan) {
            self.retiring = Some((old, 0, std::time::Instant::now()));
        }
        self.fails = 0;
        self.first_fail = None;
        self.delivered = false;
    }

    /// Demote from the failing `from` rung onto `rung`, built on this session's device.
    /// False when it could not be built: the ladder tries the next one.
    fn demote_into(&mut self, e: &anyhow::Error, from: &str, rung: NativeRung) -> bool {
        match build_rung(rung, self.wire_codec, self.stream, self.vk.as_ref()) {
            Ok(built) => {
                tracing::warn!(error = %format!("{e:#}"), fails = self.fails, from,
                    "hardware decode failing — demoting to {}", rung.name());
                self.install(built);
                true
            }
            Err(why) => {
                tracing::info!(reason = %format!("{why:#}"),
                    "{} unavailable for demotion — continuing down the ladder", rung.name());
                false
            }
        }
    }

    /// Leave the failing `from` rung for each hardware rung [`demotions`] offers, then the
    /// CPU rung. `Err` is the CPU rung's typed refusal: HEVC has none, so the pump asks for
    /// another codec.
    fn demote(&mut self, e: &anyhow::Error, from: &str) -> Result<()> {
        for rung in demotions(Os::HOST, self.wire_codec, self.vk.as_ref(), &self.ran) {
            if self.demote_into(e, from, rung) {
                return Ok(());
            }
        }
        tracing::warn!(error = %format!("{e:#}"), fails = self.fails,
            "{from} decode failing — demoting to software");
        self.install(Box::new(SoftwareDecoder::new(self.wire_codec)?));
        Ok(())
    }

    /// The presenter can't show this rung's pictures: demote as a failing decode does, and
    /// never build a rung on that lane again in the session. Decode still succeeds in that
    /// state, so the error streak never fires. Vulkan Video imports nothing, so a dma-buf or
    /// D3D11 import fault lands there before the CPU rung. No-op on the CPU rung.
    pub fn demote_for_presenter(&mut self) -> Result<()> {
        let id = self.rung.id();
        if id == Some(NativeRung::Software) {
            return Ok(());
        }
        let lane = match id {
            // Both reach the presenter as dma-bufs, on the lane it just refused.
            Some(NativeRung::Vaapi | NativeRung::V4l2) => {
                &[NativeRung::Vaapi, NativeRung::V4l2][..]
            }
            _ => id.as_slice(),
        };
        self.refused.extend(lane);
        // `ran` steers the demotion below off the refused lane too.
        self.ran.extend(lane);
        self.want_keyframe = true;
        let from = id.map_or(self.rung.name(), NativeRung::label);
        self.demote(
            &anyhow::anyhow!("the presenter can't show its pictures"),
            from,
        )
    }

    /// The freeze gate lifted on intra refresh marks. The wave healed the picture by
    /// overwrite, so the planner's damaged-chain marks are stale: without this, every later
    /// host anchor that descends from the wave is refuted until an IDR. Lanes without a
    /// planner have nothing to forget.
    pub fn forgive_unclean(&mut self) {
        self.rung.forgive_unclean();
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
        self.feed(|rung| rung.decode(au, user_flags, complete), complete)
    }

    /// [`Self::decode_frame`] with the AU's own buffer: PyroWave reads it where it lies.
    pub fn decode_au(
        &mut self,
        au: punktfunk_core::au::AuBuf,
        user_flags: u32,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        self.feed(
            move |rung| rung.decode_au(au, user_flags, complete),
            complete,
        )
    }

    /// Memory the rung reads AUs from in place, for the session to lend; `None` = the heap.
    pub fn au_memory(&self) -> Option<std::sync::Arc<dyn punktfunk_core::au::AuMemory>> {
        self.rung.au_memory()
    }

    /// Run one decode through the rung, counting a picture as a handover for the
    /// retiring rung.
    fn feed(
        &mut self,
        feed: impl FnOnce(&mut dyn Rung) -> Result<Option<DecodedImage>>,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        let out = self.decode_frame_inner(feed, complete);
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
        feed: impl FnOnce(&mut dyn Rung) -> Result<Option<DecodedImage>>,
        complete: bool,
    ) -> Result<Option<DecodedImage>> {
        let result = feed(self.rung.as_mut());
        // PyroWave and the CPU rung have nothing below them: their error is the pump's.
        let Some(from) = self.rung.id().filter(|r| *r != NativeRung::Software) else {
            return result;
        };
        // Hardware rungs: stream damage is not a decoder fault. Concealment is `Ok(None)`
        // plus the recovery request, which decides whether the `Ok` below may clear the
        // demotion streak. A driver `RESULT_STATUS` Failed stays an `Err`.
        let concealed = self.rung.take_recovery_request();
        debug_assert!(complete, "partial AUs are pyrowave-only");
        self.want_keyframe |= concealed;
        match result {
            Ok(f) => {
                // Only an answer that proves the rung works may clear the streak.
                if clears_demotion_streak(f.is_some(), concealed) {
                    self.fails = 0;
                    self.first_fail = None;
                }
                self.delivered |= f.is_some();
                Ok(f)
            }
            Err(e) => {
                let which = from.label();
                self.fails += 1;
                self.want_keyframe = true;
                let first = *self.first_fail.get_or_insert_with(std::time::Instant::now);
                // A device that cannot host the codec refuses every AU the same way;
                // waiting out the streak only costs the opening seconds.
                let device_fact = e.chain().any(|c| {
                    c.downcast_ref::<pf_vkdecode::VkDecodeError>()
                        .is_some_and(pf_vkdecode::VkDecodeError::is_device_fact)
                });
                if device_fact
                    || (self.fails >= HW_DEMOTE_AFTER && first.elapsed() >= HW_DEMOTE_MIN_STREAK)
                {
                    self.demote(&e, which)?;
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

    /// A new format rebuilds onto a rung for its codec. One nothing here decodes leaves the
    /// decoder as it was, with the typed refusal the pump turns into a codec ask.
    #[test]
    fn a_new_format_rebuilds_the_decoder_or_leaves_it() {
        use punktfunk_core::quic::{CODEC_H264, CODEC_HEVC};
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        let sdr = StreamFormat::SDR_420_8;
        let mut d = Decoder::new(CODEC_H264, "software", None, sdr).expect("software H.264");
        let e = d.reformat(CODEC_HEVC, sdr).expect_err("no software HEVC");
        assert!(e.downcast_ref::<NoSoftwareRung>().is_some(), "{e:#}");
        assert_eq!(d.wire_codec, CODEC_H264, "the old decoder stays");
        d.reformat(CODEC_H264, sdr).expect("software H.264 again");
        assert_eq!(d.wire_codec, CODEC_H264);
    }

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
            fails >= HW_DEMOTE_AFTER,
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
        assert!(fails >= HW_DEMOTE_AFTER);

        // Counterfactual: answering skipped AUs as clean `Ok(None)` zeroes the
        // streak and `HW_DEMOTE_AFTER` is unreachable. That is the AV1
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
            fails < HW_DEMOTE_AFTER,
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
