//! Vulkan Video HEVC + AV1 encoder (`VK_KHR_video_encode_h265` / `_av1`) with app-owned DPB
//! reference-frame invalidation. Loss recovery is a P-frame that re-references a known-good older
//! slot (no IDR): HEVC via an explicit short-term RPS, AV1 via `ref_frame_idx` plus
//! `primary_ref_frame = NONE` (breaks the CDF chain).
//!
//! Capture is packed RGB (dmabuf/CPU). The backend imports it, runs on-GPU RGB→4:2:0 compute CSC,
//! then encodes: 8-bit BT.709 (`rgb2yuv.comp`) or 10-bit BT.2020 (`rgb2yuv10.comp` + HEVC Main10).
//! Opt-in via `PUNKTFUNK_VULKAN_ENCODE`; gated to HEVC/AV1 plus a device that advertises the encode
//! op. AV1 encode structs that pinned `ash 0.38` predates live in `vk_av1_encode.rs`.
//! Evidence: `design/vkenc-probe-harness`.
// UNSAFE-LINT EXEMPTION: raw ash/Vulkan Video against an app-owned DPB. Wrapping each call would
// add one `unsafe {}` plus a SAFETY comment that only restates the signature. Clearing this file
// means deleting markers that carry no caller contract. See workspace Cargo.toml
// (`unsafe_op_in_unsafe_fn`).
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::too_many_arguments)]

use super::vk_util::{
    color_range, find_mem, import_failure_feeds_latch, make_host_buffer, make_plain_image,
    make_view, normalize_cpu_rgb, pixel_to_vk,
};
use crate::rfi::Wave;
use crate::{Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, ensure, Context, Result};
use ash::vk;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::AsRawFd;

const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
/// 10-bit 4:2:0 picture/DPB. `3PACK16` stores each 10-bit sample in the HIGH bits of a 16-bit
/// word; `rgb2yuv10.comp` scratch is size-compatible `R16`/`RG16` copied into this.
const P010: vk::Format = vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16;

/// Luma and chroma depth for the profile chain. Session create-info, DPB, views, and encode
/// source must all name this same depth or session creation fails.
const fn component_depth(ten_bit: bool) -> vk::VideoComponentBitDepthFlagsKHR {
    if ten_bit {
        vk::VideoComponentBitDepthFlagsKHR::TYPE_10
    } else {
        vk::VideoComponentBitDepthFlagsKHR::TYPE_8
    }
}

const fn h265_profile_idc(ten_bit: bool) -> vk::native::StdVideoH265ProfileIdc {
    if ten_bit {
        vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
    } else {
        vk::native::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
    }
}

const fn yuv_format(hdr: bool) -> vk::Format {
    if hdr {
        P010
    } else {
        NV12
    }
}
/// Max resident dmabuf imports. Above any PipeWire pool; imports alias existing buffers.
const IMPORT_CACHE_CAP: usize = 16;
// RGB→NV12 BT.709 CSC. Source `rgb2yuv.comp`; regenerate with
// `glslangValidator -V rgb2yuv.comp -o rgb2yuv.spv`.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// 10-bit HDR twin (`rgb2yuv10.comp`): 2:10:10:10 PQ/BT.2020 RGB → 10-bit 4:2:0, BT.2020 NCL.
/// Separate module: storage-image FORMAT (`r8`/`rg8` vs `r16`/`rg16`) is layout, not specializable.
const CSC10_SPV: &[u8] = include_bytes!("rgb2yuv10.spv");
/// 10-bit SDR twin (`rgb2yuv10_709.comp`): same 10-bit store as [`CSC10_SPV`], BT.709 matrix. For a
/// Main10 / AV1-10 session that stays SDR — an 8-bit RGB capture widened to a 10-bit 709 stream.
const CSC10_709_SPV: &[u8] = include_bytes!("rgb2yuv10_709.spv");
/// Cursor-overlay texture (px). Larger than any pointer; actual `w×h` uploads top-left and the
/// shader push-constant bounds sampling, so one allocation covers every cursor.
const CURSOR_MAX: u32 = 256;
/// DPB ring depth (under RADV `maxDpbSlots=17`); also the RFI recovery window.
const DPB_SLOTS: u32 = 8;
/// In-flight captures with GPU work outstanding. 2 overlaps CSC+encode with the next capture;
/// backpressure at the 2nd unread frame. Distinct from `DPB_SLOTS` (reference pool).
const RING_DEFAULT: usize = 2;

/// Encode-thread fence wait ceiling (5 s). Finite because this thread runs the stall watchdog's
/// `reset()`; an unbounded wait deadlocks recovery. Matches the Windows NVENC retrieve-thread budget.
const ENCODE_FENCE_TIMEOUT_NS: u64 = 5_000_000_000;
/// AV1 base quantizer (0..=255). CBR overrides per frame; used as the seed and for constant-Q.
const AV1_BASE_Q_IDX: u8 = 128;

fn ring_depth() -> usize {
    std::env::var("PUNKTFUNK_VULKAN_INFLIGHT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(2, 6))
        .unwrap_or(RING_DEFAULT)
}

/// `PUNKTFUNK_VULKAN_QUALITY` (default 0), clamped to `maxQualityLevels` at open. Spec order is
/// fastest→best. Without an explicit `ENCODE_QUALITY_LEVEL` control RADV never sends a VCN preset
/// op, so the session always installs the resolved level on its first frame.
fn quality_request() -> u32 {
    std::env::var("PUNKTFUNK_VULKAN_QUALITY")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// `PUNKTFUNK_VULKAN_RGB_DIRECT` for the RGB-direct encode source
/// (`design/vulkan-rgb-direct-encode.md`). Captured RGB is the encode source; VCN EFC does the
/// 709-narrow CSC inline. Default ON wherever the probe passes, except cursor-blend sessions
/// (EFC cannot composite; see [`VulkanVideoEncoder::open`]). `=0` disables; `=1` forces on
/// non-cursor sessions; unset = default. A cursor-blend session ignores `=1`.
/// Unrecognised / empty / whitespace falls back to default — do not treat anything-but-`"0"`
/// as force-on (a trailing space on `=0` would enable it).
fn rgb_request() -> Option<bool> {
    parse_rgb_request(std::env::var("PUNKTFUNK_VULKAN_RGB_DIRECT").ok().as_deref())
}

/// Pure half of [`rgb_request`]: accepted spellings without mutating the process environment
/// (parallel tests cannot).
fn parse_rgb_request(raw: Option<&str>) -> Option<bool> {
    match raw?.trim() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Unaligned RGB-direct true-extent (default ON; `PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT=0` restores
/// padded-copy staging): import the visible-size capture with TRUE-SIZE source `codedExtent` so
/// RADV derives nonzero VCN firmware padding (see [`RgbDirect::true_extent`]). EFC exists on
/// Mesa ≥ 26, where `codedExtent`-driven `session_init` is guaranteed.
fn rgb_true_extent_request() -> bool {
    std::env::var("PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT").as_deref() != Ok("0")
}

/// `PUNKTFUNK_VULKAN_DIRECT_PLANES=0`: keep the scratch-plane copies even where the driver lists
/// the picture format as a storage target (the A/B for a driver that lists it and misrenders).
fn direct_planes_request() -> bool {
    std::env::var("PUNKTFUNK_VULKAN_DIRECT_PLANES").as_deref() != Ok("0")
}

/// `VK_KHR_video_encode_intra_refresh` latched at open (see [`intra_refresh_caps`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IntraRefreshCaps {
    /// `maxIntraRefreshCycleDuration`, in frames.
    max_cycle: u32,
}

/// The two per-frame structs of a wave frame, built unconditionally so a record path can
/// chain them only when the frame carries the refresh bit. Zero-valued without a wave.
fn intra_refresh_chain(
    wave: Option<Wave>,
) -> (
    super::vk_intra_refresh::VideoEncodeIntraRefreshInfoKHR,
    super::vk_intra_refresh::VideoReferenceIntraRefreshInfoKHR,
) {
    use super::vk_intra_refresh as vir;
    let (cycle, index) = wave.map_or((0, 0), |w| (w.cycle, w.index));
    (
        vir::VideoEncodeIntraRefreshInfoKHR {
            s_type: vir::stype(vir::ST_INFO),
            p_next: std::ptr::null(),
            intra_refresh_cycle_duration: cycle,
            intra_refresh_index: index,
        },
        vir::VideoReferenceIntraRefreshInfoKHR {
            s_type: vir::stype(vir::ST_REFERENCE_INFO),
            p_next: std::ptr::null(),
            dirty_intra_refresh_regions: cycle - index,
        },
    )
}

/// Whether this session may run a row-based intra refresh wave. Needs the extension, its
/// feature, `BLOCK_ROW_BASED`, regions independent of our single slice, a cycle of at least 2
/// and one active reference. `PUNKTFUNK_INTRA_REFRESH=0` opts out. `Err` is the log reason.
fn intra_refresh_caps(
    advertised: bool,
    feature: bool,
    caps: &super::vk_intra_refresh::VideoEncodeIntraRefreshCapabilitiesKHR,
) -> std::result::Result<IntraRefreshCaps, &'static str> {
    use super::vk_intra_refresh as vir;
    if !crate::rfi::wave_enabled() {
        return Err("off(PUNKTFUNK_INTRA_REFRESH=0)");
    }
    if !advertised {
        return Err("unsupported(extension not advertised)");
    }
    if !feature {
        return Err("unsupported(videoEncodeIntraRefresh feature off)");
    }
    if caps.intra_refresh_modes & vir::MODE_BLOCK_ROW_BASED == 0 {
        return Err("unsupported(no BLOCK_ROW_BASED mode)");
    }
    if caps.partition_independent_intra_refresh_regions != vk::TRUE {
        return Err("unsupported(regions tied to slices; we encode one slice)");
    }
    if caps.max_intra_refresh_cycle_duration < 2 {
        return Err("unsupported(maxIntraRefreshCycleDuration < 2)");
    }
    if caps.max_intra_refresh_active_reference_pictures < 1 {
        return Err("unsupported(no active reference during a wave)");
    }
    Ok(IntraRefreshCaps {
        max_cycle: caps.max_intra_refresh_cycle_duration,
    })
}

/// RGB-direct session config: chroma-siting bits chosen from what the driver advertises
/// (see [`probe_rgb_direct`]).
struct RgbDirect {
    x_offset: u32, // vk_valve_rgb::CHROMA_OFFSET_*
    y_offset: u32,
    /// Mode is not 64×16-aligned, so the capture cannot be the encode source under the session's
    /// ALIGNED `codedExtent` (EFC would read past it). Each frame copies into a per-slot ALIGNED
    /// BGRA staging image with edge rows/columns duplicated, then encodes from there.
    padded: bool,
    /// Unaligned-mode default (`PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT=0` falls back to `padded`):
    /// import the visible-size buffer and pass TRUE-SIZE source `codedExtent`. RADV programs
    /// nonzero firmware padding from it (Mesa ≥ 24.2 `session_init` from
    /// `srcPictureResource.codedExtent`; see [`VulkanVideoEncoder::native_nv12`]).
    /// Session/SPS/DPB stay app-aligned.
    true_extent: bool,
}

/// Stack storage for a complete rgb-chained video profile. Post-open image creation (dmabuf
/// imports, CPU staging) must present a profile identical by value to the session's.
/// `wire()` links `p_next` into this struct's own addresses, so the value must not move between
/// `wire()` and the last use of `.profile`.
struct RgbProfileStack {
    rgb: super::vk_valve_rgb::VideoEncodeProfileRgbConversionInfoVALVE,
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    av1: super::vk_av1_encode::VideoEncodeAV1ProfileInfoKHR,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl RgbProfileStack {
    fn new(codec_op: vk::VideoCodecOperationFlagsKHR, ten_bit: bool) -> Self {
        use super::vk_av1_encode as av1b;
        use super::vk_valve_rgb as vrgb;
        Self {
            rgb: vrgb::VideoEncodeProfileRgbConversionInfoVALVE {
                s_type: vrgb::stype(vrgb::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                perform_encode_rgb_conversion: vk::TRUE,
            },
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY),
            h265: vk::VideoEncodeH265ProfileInfoKHR::default()
                .std_profile_idc(h265_profile_idc(ten_bit)),
            av1: av1b::VideoEncodeAV1ProfileInfoKHR {
                s_type: av1b::stype(av1b::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
            },
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(codec_op)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(component_depth(ten_bit))
                .chroma_bit_depth(component_depth(ten_bit)),
        }
    }

    /// Link `p_next` into this value's final address; returns `&self.profile`.
    fn wire(&mut self, av1: bool) -> &vk::VideoProfileInfoKHR<'static> {
        self.usage.p_next = &self.rgb as *const _ as *const c_void;
        if av1 {
            self.av1.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.av1 as *const _ as *const c_void;
        } else {
            self.h265.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.h265 as *const _ as *const c_void;
        }
        &self.profile
    }
}

/// Non-RGB profile for native NV12 DMA-BUF imports. Post-`open` image creation must match the
/// session profile by value.
struct NativeProfileStack {
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    av1: super::vk_av1_encode::VideoEncodeAV1ProfileInfoKHR,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl NativeProfileStack {
    fn new(codec_op: vk::VideoCodecOperationFlagsKHR, ten_bit: bool) -> Self {
        use super::vk_av1_encode as av1b;
        Self {
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY),
            h265: vk::VideoEncodeH265ProfileInfoKHR::default()
                .std_profile_idc(h265_profile_idc(ten_bit)),
            av1: av1b::VideoEncodeAV1ProfileInfoKHR {
                s_type: av1b::stype(av1b::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                // AV1 Main covers 8 and 10 bits; depth rides `VideoProfileInfoKHR` alone.
                std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
            },
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(codec_op)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(component_depth(ten_bit))
                .chroma_bit_depth(component_depth(ten_bit)),
        }
    }

    fn wire(&mut self, av1: bool) -> &vk::VideoProfileInfoKHR<'static> {
        if av1 {
            self.av1.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.av1 as *const _ as *const c_void;
        } else {
            self.h265.p_next = &self.usage as *const _ as *const c_void;
            self.profile.p_next = &self.h265 as *const _ as *const c_void;
        }
        &self.profile
    }
}

/// First physical device with a `VIDEO_ENCODE` queue family advertising `codec_op` (llvmpipe
/// advertises none). Shared by [`VulkanVideoEncoder::open_inner`] and [`probe_encode_caps`].
///
/// # Safety
/// `instance` must be a live `ash::Instance` and `devices` handles enumerated from it.
unsafe fn find_encode_device(
    instance: &ash::Instance,
    devices: &[vk::PhysicalDevice],
    codec_op: vk::VideoCodecOperationFlagsKHR,
) -> Option<(vk::PhysicalDevice, u32)> {
    for &pd in devices {
        let qf_len = instance.get_physical_device_queue_family_properties2_len(pd);
        let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); qf_len];
        let mut qf = vec![vk::QueueFamilyProperties2::default(); qf_len];
        for i in 0..qf_len {
            qf[i].p_next = &mut video[i] as *mut _ as *mut c_void;
        }
        instance.get_physical_device_queue_family_properties2(pd, &mut qf);
        for i in 0..qf_len {
            if qf[i]
                .queue_family_properties
                .queue_flags
                .contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                && video[i].video_codec_operations.contains(codec_op)
            {
                return Some((pd, i as u32));
            }
        }
    }
    None
}

/// What this device's Vulkan Video encode stack can do for one codec. An encode queue for the
/// codec may exist while the silicon declines the 10-bit profile; the two answers are independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VulkanEncodeCaps {
    pub supported: bool,
    pub eight_bit: bool,
    /// HEVC Main10 / AV1 Main at 10 bits (the HDR session profile).
    pub ten_bit: bool,
}

/// Probe [`VulkanEncodeCaps`] for `codec`. Uncached — [`crate::vulkan_encode_caps`] owns the
/// per-(GPU, codec) cache. Depth answers come from `vkGetPhysicalDeviceVideoCapabilitiesKHR`
/// against a profile at that depth, the same query session open makes.
pub(crate) fn probe_encode_caps(codec: Codec) -> VulkanEncodeCaps {
    if !matches!(codec, Codec::H265 | Codec::Av1) {
        return VulkanEncodeCaps::default();
    }
    let av1 = codec == Codec::Av1;
    let codec_op = codec_op_for(av1);
    // SAFETY: creates one Vulkan instance, issues only physical-device queries, and destroys it
    // on every path before returning — no handle derived from it escapes.
    // `Entry::load` only dlopens the loader (missing libvulkan → `Err`).
    // `find_encode_device` / `depth_supported` get that live instance and the device it returned.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return VulkanEncodeCaps::default();
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let Ok(instance) = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) else {
            return VulkanEncodeCaps::default();
        };
        let found = match instance.enumerate_physical_devices() {
            Ok(devices) => find_encode_device(&instance, &devices, codec_op).map(|(pd, _)| pd),
            Err(_) => None,
        };
        let caps = match found {
            Some(pd) => {
                let vq_inst = ash::khr::video_queue::Instance::new(&entry, &instance);
                VulkanEncodeCaps {
                    supported: true,
                    eight_bit: depth_supported(&vq_inst, pd, codec_op, av1, false),
                    ten_bit: depth_supported(&vq_inst, pd, codec_op, av1, true),
                }
            }
            None => VulkanEncodeCaps::default(),
        };
        instance.destroy_instance(None);
        caps
    }
}

/// Whether `pd` accepts an encode profile for `codec_op` at this bit depth. Profile chain is
/// byte-identical to [`VulkanVideoEncoder::open_inner`].
///
/// # Safety
/// `vq_inst` must wrap the live instance `pd` was enumerated from.
unsafe fn depth_supported(
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    codec_op: vk::VideoCodecOperationFlagsKHR,
    av1: bool,
    ten_bit: bool,
) -> bool {
    let mut ps = NativeProfileStack::new(codec_op, ten_bit);
    let profile = *ps.wire(av1);
    let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut av1_caps: super::vk_av1_encode::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
    av1_caps.s_type = super::vk_av1_encode::stype(super::vk_av1_encode::ST_CAPABILITIES);
    let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    if av1 {
        av1_caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
        caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
    } else {
        h265_caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
        caps.p_next = &mut h265_caps as *mut _ as *mut c_void;
    }
    (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps)
        == vk::Result::SUCCESS
}

fn codec_op_for(av1: bool) -> vk::VideoCodecOperationFlagsKHR {
    if av1 {
        vk::VideoCodecOperationFlagsKHR::from_raw(
            super::vk_av1_encode::VIDEO_CODEC_OPERATION_ENCODE_AV1,
        )
    } else {
        vk::VideoCodecOperationFlagsKHR::ENCODE_H265
    }
}

/// How `begin_encode_cmd` must acquire this frame's encode-source image.
#[derive(Clone, Copy, PartialEq)]
enum SrcAcquire {
    /// CSC path: `nv12_src` was written by this frame's compute batch (GENERAL layout; the
    /// csc_sem orders the queues).
    CscGeneral,
    /// First use of a DMA-BUF imported directly as the video source: acquire from the foreign
    /// producer (UNDEFINED preserves modifier-backed bytes) with a FOREIGN→encode-family transfer.
    DmabufFresh,
    /// Cached direct-source import: already VIDEO_ENCODE_SRC; visibility-only barrier for the
    /// producer's out-of-band rewrite of the bytes.
    DmabufCached,
    /// RGB-direct CPU upload: the compute queue copied the staging buffer in (semaphore
    /// ordered); transition TRANSFER_DST → VIDEO_ENCODE_SRC.
    CpuUpload,
}

/// Persistently mapped base of a slot's `bs_mem` (HOST_VISIBLE|HOST_COHERENT, mapped once).
/// `vkFreeMemory` implicitly unmaps, so teardown needs no explicit unmap.
struct BsPtr(*const u8);
// SAFETY: a Vulkan host mapping is a property of the allocation, not of the thread that mapped
// it (the spec places no thread affinity on mapped pointers); every dereference goes through the
// encoder's `&mut self`, so no concurrent access exists.
unsafe impl Send for BsPtr {}
impl Default for BsPtr {
    fn default() -> Self {
        Self(std::ptr::null())
    }
}

/// Trusted-reference view of `slot_wire` for [`crate::rfi`]: resident slots only. `-1` is empty
/// or distrusted (taint sweep); excluding it is exact because the loss start is non-negative.
fn trusted_refs(slot_wire: &[i64]) -> Vec<(usize, i64)> {
    slot_wire
        .iter()
        .enumerate()
        .filter_map(|(s, &w)| (w >= 0).then_some((s, w)))
        .collect()
}

/// S0 (past-reference) half of an HEVC short-term RPS that retains every resident DPB picture,
/// not just the active reference. HEVC 8.3.2: any DPB picture absent from the current RPS is
/// marked unused and reclaimed — an RPS naming only the active ref lets the decoder evict the
/// rest, so an RFI anchor then references a picture the client already discarded.
/// `used_by_curr_pic` is set only for the real reference. `setup_idx` is excluded: its old
/// occupant dies with this frame, which also keeps retained count at `DPB_SLOTS - 1` + current
/// = the SPS `max_dec_pic_buffering` budget.
///
/// Returns `(num_negative_pics, delta_poc_s0_minus1, used_by_curr_pic_s0_flag)`.
fn build_h265_rps_s0(
    slot_poc: &[i32],
    setup_idx: usize,
    ref_poc: i32,
    cur_poc: i32,
) -> (u8, [u16; 16], u16) {
    // Newest first: S0 is descending POC (ascending delta from `cur_poc`).
    let mut pocs: Vec<i32> = slot_poc
        .iter()
        .enumerate()
        .filter(|&(s, &p)| s != setup_idx && p >= 0 && p < cur_poc)
        .map(|(_, &p)| p)
        .collect();
    pocs.sort_unstable_by(|a, b| b.cmp(a));
    pocs.truncate(16); // delta_poc_s0_minus1 capacity (STD_VIDEO_H265_MAX_DPB_SIZE)
    let mut deltas = [0u16; 16];
    let mut used = 0u16;
    let mut prev = cur_poc;
    for (i, &p) in pocs.iter().enumerate() {
        // Gap to the previous S0 entry (cumulative DeltaPocS0), not to the current picture.
        deltas[i] = (prev - p - 1) as u16;
        if p == ref_poc {
            used |= 1 << i;
        }
        prev = p;
    }
    (pocs.len() as u8, deltas, used)
}

/// One in-flight frame's private GPU resources. `submit()` records into a free slot and returns;
/// `poll()` reads the oldest once its fence signals. Cannot be shared while a submission is
/// outstanding. [`Frame::default`] is the all-null placeholder `open_inner` pre-pushes so
/// `make_frame` can build in place; destroying one is a no-op (`vkDestroy*` ignores null).
#[derive(Default)]
struct Frame {
    compute_cmd: vk::CommandBuffer,
    cmd: vk::CommandBuffer,
    csc_sem: vk::Semaphore, // compute → encode, this frame only
    fence: vk::Fence,
    query_pool: vk::QueryPool,
    // `PUNKTFUNK_PERF` timestamps: [0]=batch start, [1]=end. Null otherwise.
    ts_pool: vk::QueryPool,
    // Pool existence is not proof it was written: padded-RGB CPU-upload records its own
    // command buffer and writes none. Reading an unreset query with `WAIT` is undefined.
    ts_written: bool,
    bs_buf: vk::Buffer,
    bs_mem: vk::DeviceMemory,
    bs_ptr: BsPtr,
    // Unaligned RGB-direct only ([`RgbDirect::padded`]): ALIGNED BGRA encode-src.
    pad_img: vk::Image,
    pad_mem: vk::DeviceMemory,
    pad_view: vk::ImageView,
    csc_set: vk::DescriptorSet, // Y/UV fixed; binding 0 (RGB) rewritten each use
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,
    /// CSC output and this frame's encode source. Format is [`yuv_format`] (NV12 or P010).
    nv12_src: vk::Image,
    nv12_mem: vk::DeviceMemory,
    nv12_view: vk::ImageView,
    /// The CSC writes `nv12_src`'s planes through `y_view`/`uv_view`; `y_img`/`uv_img` are null.
    direct_planes: bool,
    // CPU staging, keyed on (format, width, height) — not format alone. CSC sizes the image
    // to the SOURCE frame; a format-only key copies past the allocation on a size change.
    cpu_img: Option<(
        vk::Image,
        vk::DeviceMemory,
        vk::ImageView,
        vk::Format,
        u32,
        u32,
    )>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
    // Per-slot cursor overlay. Shared would race a prior frame's in-flight CSC read.
    cursor_img: vk::Image,
    cursor_mem: vk::DeviceMemory,
    cursor_view: vk::ImageView,
    cursor_stage: vk::Buffer,
    cursor_stage_mem: vk::DeviceMemory,
    cursor_serial: u64,
    cursor_ready: bool,
    pts_ns: u64,
    keyframe: bool,
    recovery_anchor: bool,
    recovery_point: bool,
    recovery_close: bool,
    /// Deferred-requeue hold cloned at submit, dropped when the fence signals. Extends
    /// "producer must not rewrite" across the async GPU read; the host's clone dies at the
    /// next capture, which with a ring of 2 is before this slot finishes.
    src_hold: Option<pf_frame::FrameHold>,
}

pub struct VulkanVideoEncoder {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    vq_dev: ash::khr::video_queue::Device,
    venc_dev: ash::khr::video_encode_queue::Device,
    encode_queue: vk::Queue,
    compute_queue: vk::Queue,
    encode_family: u32,
    compute_family: u32,
    /// Dmabuf-acquire `src` family: `QUEUE_FAMILY_FOREIGN_EXT` when
    /// `VK_EXT_queue_family_foreign` is advertised and enabled, else core-1.1
    /// `QUEUE_FAMILY_EXTERNAL` (adds a same-driver precondition FOREIGN does not have).
    foreign_qfi: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,

    codec: Codec,

    session: vk::VideoSessionKHR,
    session_mem: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    // HEVC = VPS/SPS/PPS; AV1 = temporal-delimiter OBU + sequence-header OBU.
    header: Vec<u8>,
    // Empty for HEVC; AV1 = a temporal-delimiter OBU (Vulkan emits only the frame OBU).
    frame_prefix: Vec<u8>,
    // HDR10 static metadata keyframes carry in-band (HEVC SEI, AV1 metadata OBUs).
    hdr_meta: Option<pf_frame::HdrMeta>,

    dpb_image: vk::Image,
    dpb_mem: vk::DeviceMemory,
    dpb_views: Vec<vk::ImageView>,
    slot_wire: Vec<i64>, // wire index per slot (-1 = empty) — RFI/loss domain
    slot_poc: Vec<i32>,  // HEVC POC per slot — reference-delta domain
    prev_slot: usize,

    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    // Keyed by (st_dev, st_ino): PipeWire dups a new fd per frame, same inode.
    import_cache: Vec<CachedImport>,

    frames: Vec<Frame>,
    ring: usize,                // next slot to record into
    in_flight: VecDeque<usize>, // submitted, not yet read; oldest first
    bs_size: u64,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,

    bitrate: u64,
    fps: u32,
    /// Rate-control mode, resolved once at open from `rateControlModes`: VBR when advertised,
    /// else CBR. Always `average_bitrate == max_bitrate`. CBR stuffs filler on underspent frames
    /// and Vulkan has no filler-suppression control; VBR permits the underspend.
    rc_mode: vk::VideoEncodeRateControlModeFlagsKHR,
    /// HRD window `(virtualBufferSizeInMs, initialVirtualBufferSizeInMs)`. House ~1-frame
    /// window ([`crate::vbv_window_ms`]) under VBR; loose `(1000, 500)` under CBR — tightening
    /// CBR is a filler regression. Latched so the four `VideoEncodeRateControlInfoKHR` sites
    /// (two declare, two install) cannot drift (`VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08254`).
    vbv_ms: (u32, u32),
    /// `VkVideoEncodeCapabilitiesKHR::maxBitrate`. Bounds `bitrate` at open and every retarget.
    /// 0 means "not filled in" — treated as unbounded.
    hw_max_bitrate: u64,
    /// Resolved quality level ([`quality_request`]). First-frame `ENCODE_QUALITY_LEVEL` and
    /// session-parameters must match.
    quality_level: u32,
    /// `PUNKTFUNK_PERF` timestamp period (ns). 0.0 = disabled or unsupported.
    ts_period_ns: f64,
    perf_at: std::time::Instant,
    /// Reused 3→4 expansion buffer for 24-bpp CPU payloads (`vk_util::normalize_cpu_rgb`).
    cpu_expand: Vec<u8>,
    /// RGB-direct (EFC) config. `Some` ⇒ picture format is BGRA, VCN does CSC; `None` ⇒ compute
    /// CSC. Fixed per session (picture format is baked into the video session).
    rgb: Option<RgbDirect>,
    /// The host's crop and scale ahead of the CSC ([`Encoder::set_input_crop`]). CSC sessions only.
    reframe: Option<reframe_stage::Reframe>,
    /// Producer supplied native NV12. Encodes the imported visible-size buffer directly: native
    /// sessions use TRUE-SIZE headers so RADV programs firmware padding and the source is never
    /// read past its extent. CSC/RGB paths keep app-aligned SPS (coded extent 64×16); an
    /// undersized direct source on those paths is an OOB-read class.
    native_nv12: bool,
    /// 10-bit session (HDR or 10-bit SDR). Every profile chain rebuilt after `open` must present
    /// the same depth, so it is carried here rather than re-derived.
    ten_bit: bool,
    /// HDR colour (BT.2020 PQ) vs BT.709 (SDR at either depth). Independent of `ten_bit`: a 10-bit
    /// SDR session has `ten_bit` set and `is_hdr` clear. A rebuilt session must keep the colour.
    is_hdr: bool,
    /// Row-based intra refresh is enabled on the session and its device. `None` = unavailable.
    intra_refresh: Option<IntraRefreshCaps>,
    /// Wave in flight, if any. Set at frame-build, read by the record paths for that same
    /// frame, advanced in `post_submit_bookkeeping`. An IDR or an anchor P abandons it.
    wave: Option<Wave>,

    /// Rate not yet installed. Next `record_submit` emits `ENCODE_RATE_CONTROL` (or folds it
    /// into the first frame's RESET+RC), then promotes it into `bitrate` — which must keep
    /// naming the session's current state (every begin-coding declares it).
    pending_bitrate: Option<u64>,

    width: u32,
    height: u32,
    render_w: u32, // pre-alignment — AV1 render_size / HEVC conformance window
    render_h: u32,
    poc: i32,          // HEVC POC; reused as AV1 order_hint
    enc_count: u64,    // DPB ring cursor
    auto_wire: i64,    // fallback when submit() (not submit_indexed) is used
    first_frame: bool, // RESET + DPB layout + RC install + IDR
    /// Whether the session object currently has a non-default RC mode. Not `!first_frame`:
    /// `reset()` re-arms `first_frame` without touching the session, so CBR is still current.
    /// Keying the begin-coding declaration on `first_frame` omitted it after every reset
    /// (`VUID-vkCmdBeginVideoCodingKHR-pBeginInfo-08253`). Set on install; cleared only by a
    /// new session.
    rc_installed: bool,
    force_kf: bool, // request_keyframe / non-recoverable loss → next frame is IDR
    pending_loss: Option<i64>, // invalidate_ref_frames(first) → recover on next frame
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: the encoder is used only from the single encode thread; all Vulkan handles are owned and
// never shared. Matches `NvencCudaEncoder`'s `unsafe impl Send`.
unsafe impl Send for VulkanVideoEncoder {}

impl VulkanVideoEncoder {
    /// Open a session. `cursor_blend` means the encoder may receive cursor bitmaps to composite.
    /// EFC cannot blend, so those sessions default to CSC; everywhere else RGB-direct is the
    /// default wherever the probe passes. `PUNKTFUNK_VULKAN_RGB_DIRECT` overrides both ways
    /// (see [`rgb_request`]).
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        cursor_blend: bool,
        // Negotiated depth. A 10-bit SDR session captures an 8-bit surface, so `format` is not
        // 10-bit and the depth must come from here; HDR is 10-bit by its packed/P010 format.
        bit_depth: u8,
    ) -> Result<Self> {
        // A producer's own planar picture: NV12 at eight bits, P010 at ten.
        let native_nv12 = matches!(format, PixelFormat::Nv12 | PixelFormat::P010);
        // Colour: HDR is BT.2020 PQ, keyed on the packed-10/P010 capture format. Dispatcher already
        // consulted `probe_encode_caps`; the profile query inside open re-checks.
        let is_hdr = format.is_hdr();
        // Depth: HDR, or a 10-bit SDR session on an 8-bit capture (`bit_depth == 10`, BT.709).
        let ten_bit = is_hdr || bit_depth >= 10;
        // RGB-direct needs the captured format as the session picture format. BGRA default is
        // only for CPU-only layouts, which never reach that arm.
        let src_rgb_fmt = pixel_to_vk(format).unwrap_or(vk::Format::B8G8R8A8_UNORM);
        // Cursor-blend outranks an explicit RGB-direct pin: EFC cannot composite, and
        // negotiation promised the client a composited pointer.
        if cursor_blend && rgb_request() == Some(true) {
            tracing::info!(
                "PUNKTFUNK_VULKAN_RGB_DIRECT=1 ignored for this session — it composites the \
                 pointer, which the EFC front-end cannot; using the compute-CSC path"
            );
        }
        let want_rgb = !native_nv12 && !cursor_blend && rgb_request().unwrap_or(true);
        Self::open_opts_inner(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            want_rgb,
            native_nv12,
            ten_bit,
            is_hdr,
            src_rgb_fmt,
        )
    }

    /// `open` with the RGB-direct request explicit (smoke tests: env mutation races parallel
    /// tests). `want_rgb` engages RGB-direct only if [`probe_rgb_direct`] also passes.
    #[cfg(test)]
    pub(crate) fn open_opts(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        want_rgb: bool,
    ) -> Result<Self> {
        Self::open_opts_depth(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            want_rgb,
            false,
            false,
        )
    }

    /// [`open_opts`](Self::open_opts) with depth and colour explicit. HDR takes the packed
    /// 2:10:10:10 source the capture negotiates (`xRGB_210LE` → `A2R10G10B10_UNORM_PACK32`); SDR
    /// (8- or 10-bit) takes the 8-bit BGRA source and, at ten bits, encodes Main10/AV1-10 as 709.
    #[cfg(test)]
    pub(crate) fn open_opts_depth(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        want_rgb: bool,
        ten_bit: bool,
        is_hdr: bool,
    ) -> Result<Self> {
        Self::open_opts_inner(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            want_rgb,
            false,
            ten_bit,
            is_hdr,
            if is_hdr {
                vk::Format::A2R10G10B10_UNORM_PACK32
            } else {
                vk::Format::B8G8R8A8_UNORM
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_opts_inner(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        want_rgb: bool,
        native_nv12: bool,
        ten_bit: bool,
        is_hdr: bool,
        src_rgb_fmt: vk::Format,
    ) -> Result<Self> {
        if !matches!(codec, Codec::H265 | Codec::Av1) {
            bail!("vulkan-encode backend supports HEVC + AV1 only (got {codec:?})");
        }
        // Align coded extent to encode granularity (64×16 on RADV). HEVC crops via a
        // conformance window; AV1 signals it via render_size.
        let w = (width + 63) & !63;
        let h = (height + 15) & !15;
        // SAFETY: `open_inner` only issues Vulkan calls whose preconditions it establishes itself
        // (valid instance/device, correctly-chained create-infos); all handles are freshly created
        // here and owned by the returned `Self`. No aliasing or outside invariants are involved.
        unsafe {
            Self::open_inner(
                codec,
                w,
                h,
                width,
                height,
                fps.max(1),
                bitrate_bps.max(1_000_000),
                want_rgb,
                native_nv12,
                ten_bit,
                is_hdr,
                src_rgb_fmt,
            )
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn open_inner(
        codec: Codec,
        w: u32,
        h: u32,
        rw: u32,
        rh: u32,
        fps: u32,
        bitrate: u64,
        want_rgb: bool,
        native_nv12: bool,
        ten_bit: bool,
        // Colour axis (BT.2020 PQ vs BT.709). Named `is_hdr`, not `hdr`: this fn binds `hdr` to the
        // parameter-set header bytes below, which would shadow it at the CSC-shader pick.
        is_hdr: bool,
        src_rgb_fmt: vk::Format,
    ) -> Result<Self> {
        use super::vk_av1_encode as av1b;
        use super::vk_intra_refresh as vir;
        use super::vk_valve_rgb as vrgb;
        let av1 = codec == Codec::Av1;
        let codec_op = codec_op_for(av1);
        let entry = ash::Entry::load().context("load vulkan loader")?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
            .context("create instance")?;
        // Mirror every created object into `guard` so an early `?`/`bail!` unwinds exactly what
        // was built ([`VkTeardown`]). Locals alias the handles; `Ok(Self)` disarms the guard.
        let mut guard = VkTeardown::new(instance.clone());

        let vq_inst = ash::khr::video_queue::Instance::new(&entry, &instance);

        // Same scan as the negotiation-time probe ([`find_encode_device`]); a mirrored copy
        // goes stale the first time dispatch grows a case.
        let (pd, encode_family) =
            find_encode_device(&instance, &instance.enumerate_physical_devices()?, codec_op)
                .context("no VK_KHR_video_encode queue for the requested codec on any device")?;
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // Compute family for CSC + timestamp support (`valid_bits==0` ⇒ no timestamps).
        let (compute_family, compute_ts_bits) = {
            let qf = instance.get_physical_device_queue_family_properties(pd);
            let fam = qf
                .iter()
                .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
                .context("no compute queue")?;
            (fam as u32, qf[fam].timestamp_valid_bits)
        };
        // `PUNKTFUNK_PERF` (`"0"` = off). `ts_period_ns==0.0` keeps timestamp sites off the hot path.
        let ts_period_ns =
            if std::env::var("PUNKTFUNK_PERF").is_ok_and(|v| v != "0") && compute_ts_bits > 0 {
                instance
                    .get_physical_device_properties(pd)
                    .limits
                    .timestamp_period as f64
            } else {
                0.0
            };

        // Encode source before profile: EFC RGB conversion changes profile identity;
        // producer-native NV12 uses the ordinary 4:2:0 profile.
        let aligned = rw == w && rh == h;
        let rgb_probe = if native_nv12 {
            Err("not-probed(native NV12 source selected)")
        } else {
            probe_rgb_direct(
                &instance,
                &vq_inst,
                pd,
                codec_op,
                av1,
                ten_bit,
                is_hdr,
                src_rgb_fmt,
            )
        };
        let rgb_cfg: Option<RgbDirect> = match (&rgb_probe, want_rgb) {
            (Ok((x, y)), true) => {
                let true_extent = !aligned && rgb_true_extent_request();
                Some(RgbDirect {
                    x_offset: *x,
                    y_offset: *y,
                    padded: !aligned && !true_extent,
                    true_extent,
                })
            }
            _ => None,
        };
        if native_nv12 {
            tracing::info!(
                native_nv12 = "active(direct-import)",
                source_width = rw,
                source_height = rh,
                fw_padding_width = w - rw,
                fw_padding_height = h - rh,
                "vulkan-encode: producer-native NV12 encode source (true-size headers: the \
                 driver aligns the bitstream SPS itself and the firmware edge-extends the \
                 padding — the source is never read past its extent)"
            );
        } else {
            tracing::info!(
                rgb_direct = match (&rgb_probe, want_rgb, &rgb_cfg) {
                    (
                        _,
                        _,
                        Some(RgbDirect {
                            true_extent: true, ..
                        }),
                    ) =>
                        "active(true-extent: unaligned mode, direct import with the true-size \
                         source codedExtent — RADV firmware padding covers the alignment rows; \
                         PUNKTFUNK_VULKAN_RGB_TRUE_EXTENT=0 restores the padded copy)",
                    (_, _, Some(RgbDirect { padded: false, .. })) => "active",
                    (_, _, Some(RgbDirect { padded: true, .. })) =>
                        "active(padded-copy: mode is not 64x16-aligned — staging blit + edge \
                         duplication instead of the direct import)",
                    (Ok(_), false, None) =>
                        "available(off: PUNKTFUNK_VULKAN_RGB_DIRECT=0, or a cursor-blend session \
                         — =1 forces)",
                    (Err(e), _, None) => e,
                    (Ok(_), true, None) => unreachable!("rgb gate and cfg disagree"),
                },
                "vulkan-encode: EFC RGB-direct encode source (design/vulkan-rgb-direct-encode.md)"
            );
        }

        // Encode profile, chained raw (vendored AV1 + rgb structs can't `push_next`). Must
        // match [`RgbProfileStack::wire`] when rgb is active — profile identity is by value:
        // profile → codec profile → usage (→ rgb-conversion when active).
        let rgb_info = vrgb::VideoEncodeProfileRgbConversionInfoVALVE {
            s_type: vrgb::stype(vrgb::ST_PROFILE_INFO),
            p_next: std::ptr::null(),
            perform_encode_rgb_conversion: vk::TRUE,
        };
        let mut h265_profile =
            vk::VideoEncodeH265ProfileInfoKHR::default().std_profile_idc(h265_profile_idc(ten_bit));
        let mut av1_profile = av1b::VideoEncodeAV1ProfileInfoKHR {
            s_type: av1b::stype(av1b::ST_PROFILE_INFO),
            p_next: std::ptr::null(),
            std_profile: vk::native::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN,
        };
        let mut usage = vk::VideoEncodeUsageInfoKHR::default()
            .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
            .video_content_hints(vk::VideoEncodeContentFlagsKHR::RENDERED)
            .tuning_mode(vk::VideoEncodeTuningModeKHR::ULTRA_LOW_LATENCY);
        if rgb_cfg.is_some() {
            usage.p_next = &rgb_info as *const _ as *const c_void;
        }
        // A device that cannot encode 10-bit fails this query with
        // VIDEO_PROFILE_FORMAT_NOT_SUPPORTED; a failed Vulkan open falls back to VAAPI.
        // No separate probe, and no way to reach a half-configured session.
        let depth = component_depth(ten_bit);
        let mut profile = vk::VideoProfileInfoKHR::default()
            .video_codec_operation(codec_op)
            .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
            .luma_bit_depth(depth)
            .chroma_bit_depth(depth);
        if av1 {
            av1_profile.p_next = &usage as *const _ as *const c_void;
            profile.p_next = &av1_profile as *const _ as *const c_void;
        } else {
            h265_profile.p_next = &usage as *const _ as *const c_void;
            profile.p_next = &h265_profile as *const _ as *const c_void;
        }

        // Device extensions, queried once: the intra-refresh caps chain below and the
        // `VK_EXT_queue_family_foreign` decision both read it.
        let dev_ext_props = instance
            .enumerate_device_extension_properties(pd)
            .unwrap_or_default();
        let ir_advertised = crate::vk_util::ext_advertised(&dev_ext_props, vir::EXTENSION_NAME);

        let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
        let mut av1_caps: av1b::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
        av1_caps.s_type = av1b::stype(av1b::ST_CAPABILITIES);
        let mut ir_caps: vir::VideoEncodeIntraRefreshCapabilitiesKHR = std::mem::zeroed();
        ir_caps.s_type = vir::stype(vir::ST_CAPABILITIES);
        let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
        let mut caps = vk::VideoCapabilitiesKHR::default().push_next(&mut enc_caps);
        if av1 {
            av1_caps.p_next = caps.p_next;
            caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
        } else {
            caps = caps.push_next(&mut h265_caps);
        }
        // Only chained when advertised: the layers reject a struct from an absent extension.
        if ir_advertised {
            ir_caps.p_next = caps.p_next;
            caps.p_next = &mut ir_caps as *mut _ as *mut c_void;
        }
        let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps);
        if r != vk::Result::SUCCESS {
            bail!("get_physical_device_video_capabilities: {r:?}");
        }
        let ir_feature = ir_advertised && {
            let mut f = vir::PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR {
                s_type: vir::stype(vir::ST_PHYSICAL_DEVICE_FEATURES),
                p_next: std::ptr::null_mut(),
                video_encode_intra_refresh: vk::FALSE,
            };
            let mut f2 = vk::PhysicalDeviceFeatures2 {
                p_next: &mut f as *mut _ as *mut c_void,
                ..Default::default()
            };
            instance.get_physical_device_features2(pd, &mut f2);
            f.video_encode_intra_refresh == vk::TRUE
        };
        let intra_refresh = intra_refresh_caps(ir_advertised, ir_feature, &ir_caps);
        tracing::info!(
            intra_refresh = match &intra_refresh {
                Ok(c) => format!("available(row-based, max cycle {} frames)", c.max_cycle),
                Err(e) => (*e).to_string(),
            },
            "vulkan-encode: VK_KHR_video_encode_intra_refresh (design/vulkan-intra-refresh.md)"
        );
        let intra_refresh = intra_refresh.ok();
        // Copy needed caps now: `caps` holds `&mut` borrows of the chained structs.
        let std_hdr = caps.std_header_version;
        let min_bs_align = caps.min_bitstream_buffer_size_alignment.max(1);
        let max_quality_levels = enc_caps.max_quality_levels;
        let rate_control_modes = enc_caps.rate_control_modes;
        let hw_max_bitrate = match enc_caps.max_bitrate {
            0 => u64::MAX, // not filled in — treat as unbounded
            n => n,
        };
        let av1_superblock128 = av1 && (av1_caps.superblock_sizes & av1b::SUPERBLOCK_SIZE_128 != 0);
        // Spec: valid levels are 0..maxQualityLevels, ordered fastest→best (`maxQualityLevels >= 1`).
        let quality_level = quality_request().min(max_quality_levels.saturating_sub(1));
        tracing::info!(
            quality_level,
            max_quality_levels,
            "vulkan-encode: quality level (0 = fastest preset; PUNKTFUNK_VULKAN_QUALITY overrides)"
        );
        // VBR (average == max) stops CBR bit-stuffing: Vulkan has no filler-suppression
        // control, so the mode is the only lever. CBR-only drivers keep the loose (1000, 500)
        // window — tightening CBR starts stuffing earlier.
        let vbr_advertised =
            rate_control_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::VBR);
        if !vbr_advertised
            && !rate_control_modes.contains(vk::VideoEncodeRateControlModeFlagsKHR::CBR)
        {
            // A driver with neither CBR nor VBR would need the DEFAULT-mode no-layer shape
            // this backend does not speak. Keep CBR but say so.
            tracing::warn!(
                modes = rate_control_modes.as_raw(),
                "vulkan-encode: driver advertises neither CBR nor VBR — installing CBR anyway \
                 (pre-existing behaviour; may fail validation on this driver)"
            );
        }
        // `PUNKTFUNK_VULKAN_RC=cbr|vbr`. `vbr` is honoured only when advertised; anything
        // else means auto.
        let vbr = match std::env::var("PUNKTFUNK_VULKAN_RC")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "cbr" => false,
            _ => vbr_advertised,
        };
        let (rc_mode, vbv_ms) = if vbr {
            (
                vk::VideoEncodeRateControlModeFlagsKHR::VBR,
                crate::vbv_window_ms(fps),
            )
        } else {
            (
                vk::VideoEncodeRateControlModeFlagsKHR::CBR,
                (1000u32, 500u32),
            )
        };
        tracing::info!(
            rc_mode = if vbr { "VBR (capped at target)" } else { "CBR" },
            vbv_window_ms = vbv_ms.0,
            vbv_initial_ms = vbv_ms.1,
            fps,
            hw_max_bitrate,
            "vulkan-encode: rate control (VBR when the driver offers it — CBR must stuff filler \
             on calm content; PUNKTFUNK_VULKAN_RC overrides, PUNKTFUNK_VBV_FRAMES scales the VBR \
             window)"
        );
        let bitrate = if bitrate > hw_max_bitrate {
            tracing::warn!(
                requested = bitrate,
                cap = hw_max_bitrate,
                "vulkan-encode: requested bitrate exceeds the driver's maxBitrate — clamping"
            );
            hw_max_bitrate
        } else {
            bitrate
        };
        // Enable `VK_EXT_queue_family_foreign` when advertised so dmabuf-acquire FOREIGN src
        // is spec-legal.
        let foreign_ok =
            crate::vk_util::ext_advertised(&dev_ext_props, ash::ext::queue_family_foreign::NAME);
        let foreign_qfi = if foreign_ok {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            tracing::warn!(
                "VK_EXT_queue_family_foreign not advertised — dmabuf acquires use the core \
                 QUEUE_FAMILY_EXTERNAL substitute (this arm has no fleet hardware; report it)"
            );
            vk::QUEUE_FAMILY_EXTERNAL
        };
        // AV1 extension name is raw — ash 0.38 lacks it.
        let mut dev_exts = vec![
            ash::khr::video_queue::NAME.as_ptr(),
            ash::khr::video_encode_queue::NAME.as_ptr(),
            if av1 {
                av1b::EXTENSION_NAME.as_ptr()
            } else {
                ash::khr::video_encode_h265::NAME.as_ptr()
            },
            ash::khr::external_memory_fd::NAME.as_ptr(),
            ash::ext::external_memory_dma_buf::NAME.as_ptr(),
            ash::ext::image_drm_format_modifier::NAME.as_ptr(),
        ];
        if rgb_cfg.is_some() {
            dev_exts.push(vrgb::EXTENSION_NAME.as_ptr());
        }
        if intra_refresh.is_some() {
            dev_exts.push(vir::EXTENSION_NAME.as_ptr());
        }
        if foreign_ok {
            dev_exts.push(ash::ext::queue_family_foreign::NAME.as_ptr());
        }
        let prio = [1.0f32];
        let mut qcis = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(encode_family)
            .queue_priorities(&prio)];
        if compute_family != encode_family {
            qcis.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(compute_family)
                    .queue_priorities(&prio),
            );
        }
        let mut sync2 =
            vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
        let mut device_ci = vk::DeviceCreateInfo::default()
            .queue_create_infos(&qcis)
            .enabled_extension_names(&dev_exts)
            .push_next(&mut sync2);
        // Spec: `videoEncodeAV1` must be enabled for any ENCODE_AV1 use. Vendored struct
        // (ash 0.38 predates it), chained raw like the profile.
        let mut av1_features = av1b::PhysicalDeviceVideoEncodeAV1FeaturesKHR {
            s_type: av1b::stype(av1b::ST_PHYSICAL_DEVICE_FEATURES),
            p_next: std::ptr::null_mut(),
            video_encode_av1: vk::TRUE,
        };
        if av1 {
            av1_features.p_next = device_ci.p_next as *mut c_void;
            device_ci.p_next = &av1_features as *const _ as *const c_void;
        }
        // Spec: must be enabled to chain the rgb profile.
        let mut rgb_features = vrgb::PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
            s_type: vrgb::stype(vrgb::ST_PHYSICAL_DEVICE_FEATURES),
            p_next: std::ptr::null_mut(),
            video_encode_rgb_conversion: vk::TRUE,
        };
        if rgb_cfg.is_some() {
            rgb_features.p_next = device_ci.p_next as *mut c_void;
            device_ci.p_next = &rgb_features as *const _ as *const c_void;
        }
        // Spec: the feature must be enabled before a session declares an intra refresh mode.
        let mut ir_features = vir::PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR {
            s_type: vir::stype(vir::ST_PHYSICAL_DEVICE_FEATURES),
            p_next: std::ptr::null_mut(),
            video_encode_intra_refresh: vk::TRUE,
        };
        if intra_refresh.is_some() {
            ir_features.p_next = device_ci.p_next as *mut c_void;
            device_ci.p_next = &ir_features as *const _ as *const c_void;
        }
        let device = instance
            .create_device(pd, &device_ci, None)
            .context("create device")?;
        let encode_queue = device.get_device_queue(encode_family, 0);
        let compute_queue = device.get_device_queue(compute_family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let vq_dev = ash::khr::video_queue::Device::new(&instance, &device);
        let venc_dev = ash::khr::video_encode_queue::Device::new(&instance, &device);
        guard.device = Some(device.clone());
        guard.vq_dev = Some(vq_dev.clone());

        // AV1 pins max level from caps via a chained create-info.
        let av1_sci = av1b::VideoEncodeAV1SessionCreateInfoKHR {
            s_type: av1b::stype(av1b::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            use_max_level: vk::TRUE,
            max_level: av1_caps.max_level,
        };
        // RGB-direct: picture format is captured RGB; chained create-info selects EFC conversion.
        // DPB stays NV12 (reconstruction is YUV). Built unconditionally; chained only when active.
        let mut rgb_sci = vrgb::VideoEncodeSessionRgbConversionCreateInfoVALVE {
            s_type: vrgb::stype(vrgb::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            rgb_model: rgb_model_for(is_hdr),
            rgb_range: vrgb::RANGE_NARROW,
            x_chroma_offset: rgb_cfg.as_ref().map_or(0, |c| c.x_offset),
            y_chroma_offset: rgb_cfg.as_ref().map_or(0, |c| c.y_offset),
        };
        let mut session_ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(encode_family)
            .video_profile(&profile)
            .picture_format(if rgb_cfg.is_some() {
                src_rgb_fmt
            } else {
                yuv_format(ten_bit)
            })
            .max_coded_extent(vk::Extent2D {
                width: w,
                height: h,
            })
            .reference_picture_format(yuv_format(ten_bit))
            .max_dpb_slots(DPB_SLOTS + 1)
            .max_active_reference_pictures(1)
            .std_header_version(&std_hdr);
        if av1 {
            session_ci.p_next = &av1_sci as *const _ as *const c_void;
        }
        if rgb_cfg.is_some() {
            // Chain ahead of whatever is already there (AV1 create-info keeps its place).
            rgb_sci.p_next = session_ci.p_next;
            session_ci.p_next = &rgb_sci as *const _ as *const c_void;
        }
        // The mode is fixed per session; a frame opts in per encode. Declaring it costs
        // nothing on RADV, which programs an empty stripe for frames without the bit.
        let mut ir_sci = vir::VideoEncodeSessionIntraRefreshCreateInfoKHR {
            s_type: vir::stype(vir::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            intra_refresh_mode: vir::MODE_BLOCK_ROW_BASED,
        };
        if intra_refresh.is_some() {
            ir_sci.p_next = session_ci.p_next;
            session_ci.p_next = &ir_sci as *const _ as *const c_void;
        }
        let mut session = vk::VideoSessionKHR::null();
        let r = (vq_dev.fp().create_video_session_khr)(
            device.handle(),
            &session_ci,
            std::ptr::null(),
            &mut session,
        );
        if r != vk::Result::SUCCESS {
            bail!("create_video_session: {r:?}");
        }
        guard.session = session;
        let get_mem = vq_dev.fp().get_video_session_memory_requirements_khr;
        let mut n = 0u32;
        let _ = get_mem(device.handle(), session, &mut n, std::ptr::null_mut());
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
        let _ = get_mem(device.handle(), session, &mut n, reqs.as_mut_ptr());
        let mut binds = Vec::new();
        for rq in &reqs {
            let mr = rq.memory_requirements;
            let ti = find_mem(
                &mem_props,
                mr.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            );
            let m = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(mr.size)
                    .memory_type_index(ti),
                None,
            )?;
            guard.session_mem.push(m);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(rq.memory_bind_index)
                    .memory(m)
                    .memory_offset(0)
                    .memory_size(mr.size),
            );
        }
        let r = (vq_dev.fp().bind_video_session_memory_khr)(
            device.handle(),
            session,
            binds.len() as u32,
            binds.as_ptr(),
        );
        if r != vk::Result::SUCCESS {
            bail!("bind_video_session_memory: {r:?}");
        }

        // Native NV12 (and AV1+RGB true-extent) author TRUE-SIZE headers: RADV programs firmware
        // padding from matching `codedExtent`. AV1 forbids sequence header (`flags-10324`) or
        // reference slots (`flags-10325`) disagreeing with source unless FRAME_SIZE_OVERRIDE /
        // MOTION_VECTOR_SCALING (RADV has neither). HEVC stays app-aligned (conformance window).
        let (hdr_w, hdr_h) =
            if native_nv12 || (av1 && rgb_cfg.as_ref().is_some_and(|c| c.true_extent)) {
                (rw, rh)
            } else {
                (w, h)
            };
        let (params, header, frame_prefix) = if av1 {
            build_parameters_av1(
                &device,
                &vq_dev,
                session,
                hdr_w,
                hdr_h,
                rw,
                rh,
                av1_caps.max_level,
                av1_superblock128,
                quality_level,
                ten_bit,
                is_hdr,
            )?
        } else {
            let (p, hdr) = build_parameters_h265(
                &device,
                &vq_dev,
                &venc_dev,
                session,
                hdr_w,
                hdr_h,
                rw,
                rh,
                quality_level,
                ten_bit,
                is_hdr,
            )?;
            (p, hdr, Vec::new())
        };
        guard.params = params;

        let mut profile_list =
            vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(&profile));
        let (dpb_image, dpb_mem) = make_video_image(
            &device,
            &mem_props,
            yuv_format(ten_bit),
            w,
            h,
            DPB_SLOTS,
            vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR,
            &mut profile_list,
            &[],
        )?;
        guard.dpb_image = dpb_image;
        guard.dpb_mem = dpb_mem;
        for slot in 0..DPB_SLOTS {
            guard
                .dpb_views
                .push(make_view(&device, dpb_image, yuv_format(ten_bit), slot)?);
        }

        // Per-frame images/buffers are built in `make_frame`; only the queue-family list is shared.
        let fams = if compute_family == encode_family {
            vec![]
        } else {
            vec![compute_family, encode_family]
        };

        let sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::NEAREST)
                .min_filter(vk::Filter::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;
        guard.sampler = sampler;
        // 8-bit is 709; 10-bit splits by colour — BT.2020 PQ for HDR, BT.709 for SDR-10.
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(match (ten_bit, is_hdr) {
            (true, true) => CSC10_SPV,
            (true, false) => CSC10_709_SPV,
            (false, _) => CSC_SPV,
        }))?;
        let shader =
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
        guard.shader = shader;
        let sb = |b: u32, t: vk::DescriptorType| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [
            sb(0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
            sb(1, vk::DescriptorType::STORAGE_IMAGE),
            sb(2, vk::DescriptorType::STORAGE_IMAGE),
            sb(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
        ];
        let csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        guard.csc_dsl = csc_dsl;
        let dsls = [csc_dsl];
        // Cursor `{ivec2 origin, ivec2 size}` = 16 bytes (`size.x<=0` disables the blend).
        let pc_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(16)];
        let csc_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&dsls)
                .push_constant_ranges(&pc_ranges),
            None,
        )?;
        guard.csc_layout = csc_layout;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(c"main");
        let csc_pipe = device
            .create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .layout(csc_layout)
                    .stage(stage)],
                None,
            )
            .map_err(|(_, e)| e)?[0];
        guard.csc_pipe = csc_pipe;
        device.destroy_shader_module(shader, None);
        // Shader is gone — null the guard so a later failure doesn't unwind it again.
        guard.shader = vk::ShaderModule::null();
        let nframes = ring_depth();
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                // Binding 0 (RGB) + binding 3 (cursor) per set.
                .descriptor_count(2 * nframes as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * nframes as u32),
        ];
        let csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(nframes as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        guard.csc_pool = csc_pool;

        let bs_size = align_up(3 * w as u64 * h as u64 + (1 << 16), min_bs_align);
        let cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(encode_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        guard.cmd_pool = cmd_pool;
        let compute_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(compute_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;
        guard.compute_pool = compute_pool;

        // The CSC targets the picture's planes where the driver allows a storage view on them;
        // otherwise it writes scratch planes copied into the picture (two extra 4K copies).
        let csc = rgb_cfg.is_none() && !native_nv12;
        let direct_planes = csc
            && direct_planes_request()
            && probe_yuv_storage_planes(&vq_inst, pd, &profile, yuv_format(ten_bit));
        if csc {
            tracing::info!(
                direct_planes,
                "vulkan-encode: compute CSC writes the picture planes"
            );
        }
        for _ in 0..nframes {
            // Pre-push a null Frame and build in place so a mid-`make_frame` failure leaves
            // the partial handles in the guard rather than losing them with the Err.
            guard.frames.push(Frame::default());
            make_frame(
                &device,
                &mem_props,
                w,
                h,
                &fams,
                &profile,
                &mut profile_list,
                csc_dsl,
                csc_pool,
                cmd_pool,
                compute_pool,
                bs_size,
                sampler,
                ts_period_ns > 0.0
                    && ((rgb_cfg.is_none() && !native_nv12)
                        || rgb_cfg.as_ref().is_some_and(|c| c.padded)),
                csc,
                rgb_cfg
                    .as_ref()
                    .is_some_and(|c| c.padded)
                    .then_some(src_rgb_fmt),
                ten_bit,
                direct_planes,
                guard.frames.last_mut().expect("frame just pushed"),
            )?;
        }

        // Move collections out and disarm the guard; from here `Self::Drop` is the teardown path.
        let session_mem = std::mem::take(&mut guard.session_mem);
        let dpb_views = std::mem::take(&mut guard.dpb_views);
        let frames = std::mem::take(&mut guard.frames);
        std::mem::forget(guard);

        Ok(Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            vq_dev,
            venc_dev,
            encode_queue,
            compute_queue,
            encode_family,
            foreign_qfi,
            compute_family,
            mem_props,
            codec,
            session,
            session_mem,
            params,
            header,
            frame_prefix,
            hdr_meta: None,
            dpb_image,
            dpb_mem,
            dpb_views,
            slot_wire: vec![-1; DPB_SLOTS as usize],
            slot_poc: vec![-1; DPB_SLOTS as usize],
            prev_slot: 0,
            csc_pipe,
            csc_layout,
            csc_dsl,
            csc_pool,
            sampler,
            import_cache: Vec::new(),
            frames,
            ring: 0,
            in_flight: VecDeque::new(),
            bs_size,
            cmd_pool,
            compute_pool,
            bitrate,
            fps,
            rc_mode,
            vbv_ms,
            hw_max_bitrate,
            quality_level,
            ts_period_ns,
            perf_at: std::time::Instant::now(),
            cpu_expand: Vec::new(),
            rgb: rgb_cfg,
            reframe: None,
            native_nv12,
            ten_bit,
            is_hdr,
            intra_refresh,
            wave: None,
            pending_bitrate: None,
            width: w,
            height: h,
            render_w: rw,
            render_h: rh,
            poc: 0,
            enc_count: 0,
            auto_wire: 0,
            first_frame: true,
            // Brand-new session mode is DEFAULT until the first frame's control command, so
            // frame 0 must not declare RC at begin-coding.
            rc_installed: false,
            force_kf: false,
            pending_loss: None,
            pending: VecDeque::new(),
        })
    }
}

impl VulkanVideoEncoder {
    unsafe fn bind_rgb(&self, csc_set: vk::DescriptorSet, rgb_view: vk::ImageView) {
        let ii0 = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(rgb_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(csc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii0)],
            &[],
        );
    }

    /// Refresh slot `slot`'s cursor image and return `[origin_x, origin_y, size_w, size_h]`
    /// (size 0 ⇒ CSC skips the blend). Upload only when `serial` changed. First use always
    /// transitions to SHADER_READ_ONLY so binding 3 is a valid layout with no cursor.
    unsafe fn prep_cursor(
        &mut self,
        slot: usize,
        compute_cmd: vk::CommandBuffer,
        cursor: Option<&pf_frame::CursorOverlay>,
    ) -> Result<[i32; 4]> {
        let dev = self.device.clone();
        let img = self.frames[slot].cursor_img;
        let ready = self.frames[slot].cursor_ready;
        let barrier = |old: vk::ImageLayout, new: vk::ImageLayout, ss, sa, ds, da| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ss)
                .src_access_mask(sa)
                .dst_stage_mask(ds)
                .dst_access_mask(da)
                .old_layout(old)
                .new_layout(new)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        match cursor {
            Some(c) if !c.rgba.is_empty() => {
                let cw = c.w.min(CURSOR_MAX);
                let ch = c.h.min(CURSOR_MAX);
                if self.frames[slot].cursor_serial != c.serial {
                    // A PQ session blends the cursor re-encoded as PQ, not its sRGB bytes.
                    let rgba = if self.is_hdr {
                        c.pq_rgba()
                    } else {
                        c.rgba.clone()
                    };
                    let stage = self.frames[slot].cursor_stage;
                    let stage_mem = self.frames[slot].cursor_stage_mem;
                    let bytes = (cw as usize) * (ch as usize) * 4;
                    let ptr =
                        dev.map_memory(stage_mem, 0, bytes as u64, vk::MemoryMapFlags::empty())?;
                    std::ptr::copy_nonoverlapping(
                        rgba.as_ptr(),
                        ptr as *mut u8,
                        bytes.min(rgba.len()),
                    );
                    dev.unmap_memory(stage_mem);
                    let old = if ready {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    } else {
                        vk::ImageLayout::UNDEFINED
                    };
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            old,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                        )]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        compute_cmd,
                        stage,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            .image_extent(vk::Extent3D {
                                width: cw,
                                height: ch,
                                depth: 1,
                            })],
                    );
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.frames[slot].cursor_serial = c.serial;
                    self.frames[slot].cursor_ready = true;
                }
                Ok([c.x, c.y, cw as i32, ch as i32])
            }
            _ => {
                if !ready {
                    // UNDEFINED→READ_ONLY once so binding 3 is a valid layout for the guarded read.
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.frames[slot].cursor_ready = true;
                }
                Ok([0, 0, 0, 0])
            }
        }
    }

    /// Import a DMA-BUF with usage/profile matching this session's source mode. Native NV12 and
    /// aligned RGB-direct are profiled `VIDEO_ENCODE_SRC`. Padded RGB-direct is transfer-source only.
    unsafe fn import_dmabuf(
        &self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
        if self.native_nv12 {
            let mut ps =
                NativeProfileStack::new(codec_op_for(self.codec == Codec::Av1), self.ten_bit);
            let profile = *ps.wire(self.codec == Codec::Av1);
            let arr = [profile];
            let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
            return super::vk_util::import_rgb_dmabuf_as(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
                Some(&mut plist),
            );
        }
        if self.rgb.as_ref().is_some_and(|r| r.padded) {
            // Padded-copy: import is only a transfer source for the blit — no video profile.
            super::vk_util::import_rgb_dmabuf_as(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
                vk::ImageUsageFlags::TRANSFER_SRC,
                None,
            )
        } else if self.rgb.is_some() {
            let mut ps = RgbProfileStack::new(codec_op_for(self.codec == Codec::Av1), self.ten_bit);
            let profile = *ps.wire(self.codec == Codec::Av1);
            let arr = [profile];
            let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
            super::vk_util::import_rgb_dmabuf_as(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
                vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
                Some(&mut plist),
            )
        } else {
            super::vk_util::import_rgb_dmabuf(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
            )
        }
    }

    /// Import a dmabuf, reusing a cached import when the same underlying buffer recurs. Keyed by
    /// `(st_dev, st_ino)` because each `DmabufFrame` owns a fresh dup (new fd, same inode).
    /// `fresh` is true only on first import (UNDEFINED old-layout preserves modifier-tiled data).
    unsafe fn import_cached(
        &mut self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::ImageView, bool)> {
        let mut st: libc::stat = std::mem::zeroed();
        let key = if libc::fstat(d.fd.as_raw_fd(), &mut st) == 0 {
            (st.st_dev as u64, st.st_ino as u64)
        } else {
            // fstat failed → uncacheable sentinel; still owned by the cache and freed on evict/Drop.
            (u64::MAX, self.enc_count)
        };
        if let Some(pos) = self.import_cache.iter().position(|e| e.key == key) {
            let e = &self.import_cache[pos];
            if e.extent == (cw, ch) {
                return Ok((e.img, e.view, false));
            }
            // Key hit, wrong extent: inode now names a different allocation. Evict rather than
            // hand out a stale-sized image. In-flight frames may still read the old image, so
            // idle the device before destroying.
            let _ = self.device.device_wait_idle();
            let e = self.import_cache.remove(pos);
            self.device.destroy_image_view(e.view, None);
            self.device.destroy_image(e.img, None);
            self.device.free_memory(e.mem, None);
        }
        // Feed pf-zerocopy's raw-dmabuf degrade latch: a deterministic import refusal repeats
        // forever, and the latch (CPU delivery next session) is its only recovery. Excluded:
        // transient OOM (`import_failure_feeds_latch`), and native-NV12 entirely — an NV12
        // layout quirk should not cost all dmabuf capture.
        let (img, mem, view) = match self.import_dmabuf(d, cw, ch) {
            Ok(t) => {
                if !self.native_nv12 {
                    pf_zerocopy::note_raw_dmabuf_import_ok();
                }
                t
            }
            Err(e) => {
                if !self.native_nv12 && import_failure_feeds_latch(&e) {
                    pf_zerocopy::note_raw_dmabuf_import_failure(&format!("{e:#}"));
                }
                return Err(e);
            }
        };
        // FIFO eviction. Up to `ring_depth - 1` submitted frames may still execute against a
        // cached image, so destroying an evicted import is a GPU-side use-after-free unless we
        // idle first. Guarded on the length test so the steady-state (no-evict) path pays nothing.
        if self.import_cache.len() >= IMPORT_CACHE_CAP {
            let _ = self.device.device_wait_idle();
        }
        while self.import_cache.len() >= IMPORT_CACHE_CAP {
            let e = self.import_cache.remove(0);
            self.device.destroy_image_view(e.view, None);
            self.device.destroy_image(e.img, None);
            self.device.free_memory(e.mem, None);
        }
        self.import_cache.push(CachedImport {
            key,
            extent: (cw, ch),
            img,
            mem,
            view,
        });
        tracing::debug!(
            resident = self.import_cache.len(),
            "vulkan-encode: imported a new dmabuf buffer"
        );
        Ok((img, view, true))
    }

    /// Per-slot CPU-capture RGB image + staging, recreated on format/size change.
    unsafe fn ensure_cpu_rgb(
        &mut self,
        slot: usize,
        fmt: vk::Format,
        bytes: &[u8],
        src_w: u32,
        src_h: u32,
    ) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        // CSC: size to the real frame so clamp-to-edge duplicates true content (aligned-size
        // made the clamp land on unwritten rows). RGB-direct: aligned dims, CPU-side padding below.
        let (iw, ih) = if self.rgb.is_some() {
            (w, h)
        } else {
            (src_w, src_h)
        };
        // Widen before the multiply: `(iw * ih * 4) as u64` wraps in u32 once `iw * ih > 2^30`.
        let need = iw as u64 * ih as u64 * 4;
        if self.frames[slot]
            .cpu_img
            .map(|(_, _, _, f, iw, ih)| (f, iw, ih))
            != Some((fmt, iw, ih))
        {
            if let Some((i, m, v, ..)) = self.frames[slot].cpu_img.take() {
                dev.destroy_image_view(v, None);
                dev.destroy_image(i, None);
                dev.free_memory(m, None);
            }
            let (i, m, v) = if self.rgb.is_some() {
                // RGB-direct: uploaded RGB is the encode source — profiled, encode usage, shared
                // with the encode queue (compute only copies in; semaphore orders; CONCURRENT
                // avoids a QFOT).
                let av1 = self.codec == Codec::Av1;
                let mut ps = RgbProfileStack::new(codec_op_for(av1), self.ten_bit);
                let profile = *ps.wire(av1);
                let arr = [profile];
                let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
                let fams: &[u32] = if self.encode_family == self.compute_family {
                    &[]
                } else {
                    &[self.encode_family, self.compute_family]
                };
                let fams = fams.to_vec();
                let (i, m) = make_video_image(
                    &dev,
                    &self.mem_props,
                    fmt,
                    w,
                    h,
                    1,
                    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
                    &mut plist,
                    &fams,
                )?;
                // `make_video_image` unwinds internally; a view failure here would leak the pair.
                let v = match make_view(&dev, i, fmt, 0) {
                    Ok(v) => v,
                    Err(e) => {
                        dev.destroy_image(i, None);
                        dev.free_memory(m, None);
                        return Err(e);
                    }
                };
                (i, m, v)
            } else {
                make_plain_image(
                    &dev,
                    &self.mem_props,
                    fmt,
                    iw,
                    ih,
                    vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
                )?
            };
            self.frames[slot].cpu_img = Some((i, m, v, fmt, iw, ih));
        }
        if self.frames[slot]
            .cpu_stage
            .map(|(_, _, s)| s < need)
            .unwrap_or(true)
        {
            if let Some((b, m, _)) = self.frames[slot].cpu_stage.take() {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            let (buf, mem) = make_host_buffer(
                &dev,
                &self.mem_props,
                need,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            self.frames[slot].cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.frames[slot].cpu_stage.unwrap();
        // RGB-direct uploads the image the encoder reads, so an undersized source must be
        // padded (edge duplicate). CSC keeps the raw copy (shader clamps). Guards below must
        // not be hoisted — they are only sound because `self.rgb.is_some()`.
        // Every fallible step stays above the map: an error between map and unmap strands it.
        let pad = if self.rgb.is_some() && (src_w != w || src_h != h) {
            let (sw, sh) = (src_w as usize, src_h as usize);
            // Zero axis makes `sh - 1` underflow. Refuse by name so the padding write is infallible.
            if src_w == 0 || src_h == 0 {
                bail!("vulkan-encode (rgb-direct): CPU frame has a zero axis ({src_w}x{src_h})");
            }
            // Extent first: the payload-length check multiplies `sw * sh * 4` in usize on
            // caller-supplied dimensions. Bounding against encode extent keeps that multiply
            // from overflowing. A source larger than encode extent would panic on the row copy.
            if src_w > w || src_h > h {
                bail!(
                    "vulkan-encode (rgb-direct): CPU frame {}x{} exceeds the encode extent {}x{} \
                     — the RGB-direct source must fit inside the aligned encode size",
                    src_w,
                    src_h,
                    w,
                    h
                );
            }
            if bytes.len() < sw * sh * 4 {
                bail!(
                    "vulkan-encode (rgb-direct): CPU frame {}x{} needs {} bytes, got {}",
                    src_w,
                    src_h,
                    sw * sh * 4,
                    bytes.len()
                );
            }
            // Slice is sized from `(w, h)` while staging is sized from `need`. Check equality
            // above the map in release: a failed assert below the map would strand the mapping
            // (`VUID-vkMapMemory-memory-00678` on the next `map_memory`).
            let dst_len = (w as usize)
                .checked_mul(h as usize)
                .and_then(|n| n.checked_mul(4))
                .filter(|&n| n as u64 == need)
                .context("vulkan-encode (rgb-direct): staging buffer is not the encode extent")?;
            Some((sw, sh, dst_len))
        } else {
            None
        };
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        match pad {
            Some((sw, sh, dst_len)) => {
                let (dw, dh) = (w as usize, h as usize);
                // SAFETY: `dst_len == dw * dh * 4 == need`, checked above the map; staging is
                // kept only when size >= `need`. HOST_COHERENT so no explicit flush. Guards
                // above make every index in-bounds.
                let dst = std::slice::from_raw_parts_mut(p, dst_len);
                for y in 0..dh {
                    let sy = y.min(sh - 1);
                    let srow = &bytes[sy * sw * 4..][..sw * 4];
                    let drow = &mut dst[y * dw * 4..][..dw * 4];
                    drow[..sw * 4].copy_from_slice(srow);
                    let mut last = [0u8; 4];
                    last.copy_from_slice(&srow[(sw - 1) * 4..]);
                    for x in sw..dw {
                        drow[x * 4..(x + 1) * 4].copy_from_slice(&last);
                    }
                }
            }
            None => {
                let n = bytes.len().min(need as usize);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
            }
        }
        dev.unmap_memory(m);
        Ok(self.frames[slot].cpu_img.unwrap().2)
    }

    /// Record CSC + encode (+ RFI) into ring `slot` and submit without waiting. Fence is polled
    /// later (`read_slot`). `slot` must be free (prior submission already read back).
    unsafe fn record_submit(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        wire: i64,
    ) -> Result<()> {
        let (w, h_px) = (self.width, self.height);
        // Copy this slot's handles (Copy) so later `&mut self` helpers don't alias `self.frames`.
        let compute_cmd = self.frames[slot].compute_cmd;
        let cmd = self.frames[slot].cmd;
        let csc_sem = self.frames[slot].csc_sem;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let ts_pool = self.frames[slot].ts_pool;
        let bs_buf = self.frames[slot].bs_buf;
        let csc_set = self.frames[slot].csc_set;
        let y_img = self.frames[slot].y_img;
        let uv_img = self.frames[slot].uv_img;
        let nv12_src = self.frames[slot].nv12_src;
        let nv12_view = self.frames[slot].nv12_view;
        let direct_planes = self.frames[slot].direct_planes;

        // Pending rate retarget stays pending through recording: record fns declare the
        // session's current state at begin-coding and install via ENCODE_RATE_CONTROL; bookkeeping
        // promotes after. Promoting here made begin name a rate the session had not installed
        // after `reset()` (`rc_installed` survives; session keeps the old rate) — VUID-08254.
        let mut is_idr = self.first_frame || self.force_kf;
        let mut ref_slot = self.prev_slot;
        let mut recovery = false;
        if let Some(lf) = self.pending_loss.take() {
            if !is_idr {
                // Taint sweep already ran in `invalidate_ref_frames`; re-pick against the table now.
                match crate::rfi::pick_anchor(&trusted_refs(&self.slot_wire), lf) {
                    Some((s, _)) => {
                        ref_slot = s;
                        recovery = true;
                        tracing::debug!(
                            loss_first = lf,
                            anchor_slot = s,
                            anchor_wire = self.slot_wire[s],
                            "vulkan-encode: emitting clean recovery-anchor P-frame (references a known-good frame older than the loss, no IDR)"
                        );
                    }
                    None => match self.intra_refresh {
                        // A wave in flight restarts: the client re-armed at this loss and
                        // counts marks from there, so only a fresh start + close lift it.
                        Some(ir) => {
                            // RADV's stripe unit is the 64-px CTB row for HEVC and AV1.
                            let cycle = crate::rfi::wave_cycle(
                                self.height.div_ceil(64),
                                self.fps,
                                ir.max_cycle,
                                crate::rfi::pinned_cycle(),
                            );
                            self.wave = Some(Wave::start(cycle));
                            tracing::debug!(
                                loss_first = lf,
                                cycle,
                                "vulkan-encode: no resident reference older than the loss — \
                                 starting an intra refresh wave instead of an IDR"
                            );
                        }
                        None => {
                            is_idr = true;
                            tracing::debug!(loss_first = lf, "vulkan-encode: no resident reference older than the loss — forcing IDR");
                        }
                    },
                }
            }
        }
        // An IDR or an anchor P is clean on its own; a wave under way is abandoned. Both are
        // legal successors: neither references a picture with dirty regions.
        if is_idr || recovery {
            self.wave = None;
        }
        let poc: i32 = if is_idr { 0 } else { self.poc };
        let mut setup_idx = (self.enc_count % DPB_SLOTS as u64) as usize;
        if !is_idr && setup_idx == ref_slot {
            setup_idx = (setup_idx + 1) % DPB_SLOTS as usize;
        }

        if self.native_nv12 {
            self.record_submit_nv12(slot, frame, is_idr, recovery, ref_slot, setup_idx, poc)?;
            self.post_submit_bookkeeping(
                slot,
                frame.pts_ns,
                wire,
                is_idr,
                recovery,
                setup_idx,
                poc,
            );
            return Ok(());
        }
        if self.rgb.is_some() {
            self.record_submit_rgb(slot, frame, is_idr, recovery, ref_slot, setup_idx, poc)?;
            self.post_submit_bookkeeping(
                slot,
                frame.pts_ns,
                wire,
                is_idr,
                recovery,
                setup_idx,
                poc,
            );
            return Ok(());
        }

        // Shader samples with clamped 1:1 texelFetch, so a mismatch silently streams a
        // cropped/edge-padded picture. Refuse into the encoder-rebuild path instead. A reframed
        // session needs only a capture that holds its crop.
        let fits = match &self.reframe {
            Some(r) => r.covers(frame.width, frame.height),
            None => frame.width == self.render_w && frame.height == self.render_h,
        };
        if !fits {
            bail!(
                "vulkan-encode (csc): frame {}x{} != mode {}x{} — refusing a mismatched CSC \
                 source",
                frame.width,
                frame.height,
                self.render_w,
                self.render_h
            );
        }
        let dev = self.device.clone(); // handle clone so `&mut self` helpers still work
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        // Fallible prefix in one closure whose error arm resets `compute_cmd`. Leaving it
        // RECORDING makes the next `begin` violate VUID-vkBeginCommandBuffer-commandBuffer-00049.
        // Never PENDING here: nothing is submitted until stage 4, so the reset is legal.
        let prefix = (|| -> Result<([i32; 4], vk::ImageView)> {
            self.frames[slot].ts_written = false;
            if self.ts_period_ns > 0.0 {
                dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
                dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
                self.frames[slot].ts_written = true;
            }

            let cursor_pc = self.prep_cursor(slot, compute_cmd, frame.cursor.as_ref())?;

            let rgb_view = match &frame.payload {
                FramePayload::Dmabuf(d) => {
                    let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                    // Fresh: UNDEFINED preserves modifier-tiled bytes, FOREIGN→compute acquire.
                    // Cached: visibility-only; content stability is `Frame::src_hold`.
                    let (old, src_qf, dst_qf) = if fresh {
                        (
                            vk::ImageLayout::UNDEFINED,
                            self.foreign_qfi,
                            self.compute_family,
                        )
                    } else {
                        (
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::QUEUE_FAMILY_IGNORED,
                            vk::QUEUE_FAMILY_IGNORED,
                        )
                    };
                    let acq = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                        .old_layout(old)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(src_qf)
                        .dst_queue_family_index(dst_qf)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                    );
                    view
                }
                FramePayload::Cpu(bytes) => {
                    // Expand 24-bpp 3→4 first (`normalize_cpu_rgb`); `ensure_cpu_rgb` padding does
                    // 4-bpp index math on the raw bytes.
                    let mut scratch = std::mem::take(&mut self.cpu_expand);
                    let (norm_fmt, norm_bytes) =
                        normalize_cpu_rgb(frame.format, bytes, &mut scratch, false);
                    let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                    let view = match fmt {
                        Ok(f) => {
                            self.ensure_cpu_rgb(slot, f, norm_bytes, frame.width, frame.height)
                        }
                        Err(e) => Err(e),
                    };
                    self.cpu_expand = scratch;
                    let view = view?;
                    let (img, ..) = self.frames[slot].cpu_img.unwrap();
                    let (stage, ..) = self.frames[slot].cpu_stage.unwrap();
                    let to_dst = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        compute_cmd,
                        stage,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            // Staging is tightly packed and the image is source-sized; an
                            // aligned-size extent here shears rows.
                            .image_extent(vk::Extent3D {
                                width: frame.width,
                                height: frame.height,
                                depth: 1,
                            })],
                    );
                    let to_read = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        compute_cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                    );
                    view
                }
                _ => bail!("vulkan-encode: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
            };
            Ok((cursor_pc, rgb_view))
        })();
        let (cursor_pc, rgb_view) = match prefix {
            Ok(v) => v,
            Err(e) => {
                // RECORDING (never submitted yet); pool allows reset.
                let _ = dev.reset_command_buffer(compute_cmd, vk::CommandBufferResetFlags::empty());
                return Err(e);
            }
        };
        // A reframed session converts the scaled crop; the cursor went in at source size.
        let (cursor_pc, rgb_view) = match &self.reframe {
            Some(r) => (
                [0; 4],
                r.record(
                    &dev,
                    compute_cmd,
                    slot,
                    self.sampler,
                    rgb_view,
                    self.frames[slot].cursor_view,
                    cursor_pc,
                ),
            ),
            None => (cursor_pc, rgb_view),
        };
        self.bind_rgb(csc_set, rgb_view);

        // GENERAL for the CSC's targets, prior contents discarded: the picture itself when its
        // planes are written directly, else Y/UV scratch plus the picture as the copies' dst.
        let to_general = |img, dst_stage, dst_access| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(dst_stage)
                .dst_access_mask(dst_access)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        let pre = if direct_planes {
            vec![to_general(
                nv12_src,
                vk::PipelineStageFlags2::COMPUTE_SHADER,
                vk::AccessFlags2::SHADER_WRITE,
            )]
        } else {
            vec![
                to_general(
                    y_img,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_WRITE,
                ),
                to_general(
                    uv_img,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_WRITE,
                ),
                to_general(
                    nv12_src,
                    vk::PipelineStageFlags2::ALL_TRANSFER,
                    vk::AccessFlags2::TRANSFER_WRITE,
                ),
            ]
        };
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre),
        );

        dev.cmd_bind_pipeline(compute_cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
        dev.cmd_bind_descriptor_sets(
            compute_cmd,
            vk::PipelineBindPoint::COMPUTE,
            self.csc_layout,
            0,
            &[csc_set],
            &[],
        );
        let mut pc_bytes = [0u8; 16];
        for (i, v) in cursor_pc.iter().enumerate() {
            pc_bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
        }
        dev.cmd_push_constants(
            compute_cmd,
            self.csc_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &pc_bytes,
        );
        dev.cmd_dispatch(compute_cmd, (w / 2).div_ceil(8), (h_px / 2).div_ceil(8), 1);

        if !direct_planes {
            // Y/UV shader-write → transfer-read (stay GENERAL); then copy into nv12 planes.
            let yuv_rd = |img| {
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(color_range(0))
            };
            dev.cmd_pipeline_barrier2(
                compute_cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[yuv_rd(y_img), yuv_rd(uv_img)]),
            );
            let plane_copy = |src_aspect, dst_aspect, ew, eh| {
                vk::ImageCopy::default()
                    .src_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(src_aspect)
                            .layer_count(1),
                    )
                    .dst_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(dst_aspect)
                            .layer_count(1),
                    )
                    .extent(vk::Extent3D {
                        width: ew,
                        height: eh,
                        depth: 1,
                    })
            };
            dev.cmd_copy_image(
                compute_cmd,
                y_img,
                vk::ImageLayout::GENERAL,
                nv12_src,
                vk::ImageLayout::GENERAL,
                &[plane_copy(
                    vk::ImageAspectFlags::COLOR,
                    vk::ImageAspectFlags::PLANE_0,
                    w,
                    h_px,
                )],
            );
            dev.cmd_copy_image(
                compute_cmd,
                uv_img,
                vk::ImageLayout::GENERAL,
                nv12_src,
                vk::ImageLayout::GENERAL,
                &[plane_copy(
                    vk::ImageAspectFlags::COLOR,
                    vk::ImageAspectFlags::PLANE_1,
                    w / 2,
                    h_px / 2,
                )],
            );
        }
        if self.ts_period_ns > 0.0 {
            dev.cmd_write_timestamp2(
                compute_cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                ts_pool,
                1,
            );
        }
        dev.end_command_buffer(compute_cmd)?;

        if self.codec == Codec::Av1 {
            self.record_coding_av1(
                &dev,
                cmd,
                query_pool,
                bs_buf,
                nv12_src,
                nv12_view,
                SrcAcquire::CscGeneral,
                is_idr,
                recovery,
                ref_slot,
                setup_idx,
                poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev,
                cmd,
                query_pool,
                bs_buf,
                nv12_src,
                nv12_view,
                SrcAcquire::CscGeneral,
                is_idr,
                ref_slot,
                setup_idx,
                poc,
            )?;
        }

        // Compute signals `csc_sem`; encode waits it and signals `fence`. Per-slot cmd/sem/fence
        // make ring frames independent; DPB barrier orders N's reconstruct-write before N+1's read.
        dev.reset_fences(&[fence])?;
        let ccmds = [compute_cmd];
        let sems = [csc_sem];
        dev.queue_submit(
            self.compute_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ccmds)
                .signal_semaphores(&sems)],
            vk::Fence::null(),
        )?;
        let ecmds = [cmd];
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        dev.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default()
                .command_buffers(&ecmds)
                .wait_semaphores(&sems)
                .wait_dst_stage_mask(&wait_stages)],
            fence,
        )?;
        self.post_submit_bookkeeping(slot, frame.pts_ns, wire, is_idr, recovery, setup_idx, poc);
        Ok(())
    }

    /// Copy the visible frame into aligned staging plus edge-duplicate into 64×16 padding
    /// (same edge semantics as the CSC shader's clamped reads). Transfer-only. Staging lives
    /// in GENERAL (copy dst and, for the right-column pass, copy src); encode acquires via
    /// [`SrcAcquire::CscGeneral`].
    ///
    /// `planes`: `[(COLOR, 1)]` packed RGB, `[(PLANE_0, 1), (PLANE_1, 2)]` NV12. Multi-planar
    /// copy regions are in each plane's own coordinate space; barriers stay COLOR-aspect.
    /// Every divisor must divide visible and aligned extents.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_pad_blit(
        &self,
        dev: &ash::Device,
        compute_cmd: vk::CommandBuffer,
        src: vk::Image,
        src_fresh: bool,
        pad: vk::Image,
        ts_pool: vk::QueryPool,
        planes: &[(vk::ImageAspectFlags, u32)],
    ) -> Result<()> {
        let (rw, rh) = (self.render_w, self.render_h);
        let (w, h) = (self.width, self.height);
        dev.begin_command_buffer(
            compute_cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        if self.ts_period_ns > 0.0 {
            dev.cmd_reset_query_pool(compute_cmd, ts_pool, 0, 2);
            dev.cmd_write_timestamp2(compute_cmd, vk::PipelineStageFlags2::NONE, ts_pool, 0);
        }
        // Fresh import: FOREIGN hand-off, UNDEFINED preserves modifier-tiled bytes.
        // Cached: visibility-only. Staging: transfer-write, prior contents discarded.
        let src_acq = if src_fresh {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(self.foreign_qfi)
                .dst_queue_family_index(self.compute_family)
        } else {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        }
        .image(src)
        .subresource_range(color_range(0));
        let pad_dst = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::NONE)
            .src_access_mask(vk::AccessFlags2::NONE)
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(pad)
            .subresource_range(color_range(0));
        dev.cmd_pipeline_barrier2(
            compute_cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[src_acq, pad_dst]),
        );
        let region =
            |aspect: vk::ImageAspectFlags, sx: i32, sy: i32, dx: i32, dy: i32, cw: u32, ch: u32| {
                let layers = vk::ImageSubresourceLayers::default()
                    .aspect_mask(aspect)
                    .layer_count(1);
                vk::ImageCopy::default()
                    .src_subresource(layers)
                    .dst_subresource(layers)
                    .src_offset(vk::Offset3D { x: sx, y: sy, z: 0 })
                    .dst_offset(vk::Offset3D { x: dx, y: dy, z: 0 })
                    .extent(vk::Extent3D {
                        width: cw,
                        height: ch,
                        depth: 1,
                    })
            };
        // Pass 1: visible region, then each bottom padding row as a copy of the last visible row.
        let mut regions = Vec::new();
        for &(aspect, div) in planes {
            let (rw, rh, h) = (rw / div, rh / div, h / div);
            regions.push(region(aspect, 0, 0, 0, 0, rw, rh));
            for y in rh..h {
                regions.push(region(aspect, 0, rh as i32 - 1, 0, y as i32, rw, 1));
            }
        }
        dev.cmd_copy_image(
            compute_cmd,
            src,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            pad,
            vk::ImageLayout::GENERAL,
            &regions,
        );
        // Pass 2: duplicate last visible column over the full aligned height (valid after
        // pass 1 filled the bottom rows). Self-copy in GENERAL; W→R barrier between.
        if w > rw {
            let self_dep = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(pad)
                .subresource_range(color_range(0));
            dev.cmd_pipeline_barrier2(
                compute_cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&[self_dep]),
            );
            let mut cols = Vec::new();
            for &(aspect, div) in planes {
                let (rw, w, h) = (rw / div, w / div, h / div);
                for x in rw..w {
                    cols.push(region(aspect, rw as i32 - 1, 0, x as i32, 0, 1, h));
                }
            }
            dev.cmd_copy_image(
                compute_cmd,
                pad,
                vk::ImageLayout::GENERAL,
                pad,
                vk::ImageLayout::GENERAL,
                &cols,
            );
        }
        if self.ts_period_ns > 0.0 {
            dev.cmd_write_timestamp2(
                compute_cmd,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                ts_pool,
                1,
            );
        }
        dev.end_command_buffer(compute_cmd)?;
        Ok(())
    }

    /// Import the producer's visible-size NV12 buffer as the encode source. Safe at every mode
    /// because native sessions run true-size headers (see [`Self::native_nv12`]).
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_submit_nv12(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        if frame.format != PixelFormat::Nv12 {
            bail!(
                "vulkan-encode (native NV12): negotiated NV12 but received {:?}",
                frame.format
            );
        }
        if frame.width != self.render_w || frame.height != self.render_h {
            bail!(
                "vulkan-encode (native NV12): frame {}x{} != mode {}x{}",
                frame.width,
                frame.height,
                self.render_w,
                self.render_h
            );
        }
        if frame.width % 2 != 0 || frame.height % 2 != 0 {
            bail!("vulkan-encode (native NV12): 4:2:0 frame dimensions must be even");
        }
        let FramePayload::Dmabuf(d) = &frame.payload else {
            bail!("vulkan-encode (native NV12): producer frame is not a DMA-BUF");
        };
        if d.fourcc != pf_frame::drm_fourcc(PixelFormat::Nv12).expect("NV12 FourCC") {
            bail!(
                "vulkan-encode (native NV12): DMA-BUF FourCC {:#x} is not NV12",
                d.fourcc
            );
        }
        if d.modifier != 0 {
            bail!(
                "vulkan-encode (native NV12): only LINEAR is supported, got modifier {:#x}",
                d.modifier
            );
        }
        // No compositing stage: session plan negotiates native NV12 only when `!cursor_blend`.
        let dev = self.device.clone();
        let cmd = self.frames[slot].cmd;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let bs_buf = self.frames[slot].bs_buf;
        let (src_img, src_view, fresh) = self.import_cached(d, frame.width, frame.height)?;
        let acquire = if fresh {
            SrcAcquire::DmabufFresh
        } else {
            SrcAcquire::DmabufCached
        };
        if self.codec == Codec::Av1 {
            self.record_coding_av1(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, recovery,
                ref_slot, setup_idx, poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, ref_slot,
                setup_idx, poc,
            )?;
        }
        dev.reset_fences(&[fence])?;
        let ecmds = [cmd];
        dev.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default().command_buffers(&ecmds)],
            fence,
        )?;
        Ok(())
    }

    /// RGB-direct twin of [`record_submit`]'s source/encode/submit. No compute CSC: VCN EFC
    /// converts inline. Only the CPU path touches the compute queue (staging copy, semaphore-ordered).
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_submit_rgb(
        &mut self,
        slot: usize,
        frame: &CapturedFrame,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        let dev = self.device.clone();
        let compute_cmd = self.frames[slot].compute_cmd;
        let cmd = self.frames[slot].cmd;
        let csc_sem = self.frames[slot].csc_sem;
        let fence = self.frames[slot].fence;
        let query_pool = self.frames[slot].query_pool;
        let bs_buf = self.frames[slot].bs_buf;
        let ts_pool = self.frames[slot].ts_pool;
        // EFC cannot composite; `open` refuses RGB-direct for cursor-blend sessions.
        let padded = self.rgb.as_ref().is_some_and(|r| r.padded);
        // Only the padded Dmabuf arm records timestamps (`record_pad_blit`); CPU-upload writes none.
        self.frames[slot].ts_written = false;
        let (src_img, src_view, acquire, compute_active) = match &frame.payload {
            FramePayload::Dmabuf(d) if !padded => {
                // Imported buffer must cover the declared source extent (aligned coded extent,
                // or render size in true-extent mode). A mismatch takes the encoder-rebuild
                // path instead of letting EFC read past the allocation.
                let (need_w, need_h) = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
                    (self.render_w, self.render_h)
                } else {
                    (self.width, self.height)
                };
                if frame.width != need_w || frame.height != need_h {
                    bail!(
                        "vulkan-encode (rgb-direct): frame {}x{} does not cover the declared \
                         source extent {}x{} — refusing an out-of-bounds encode source",
                        frame.width,
                        frame.height,
                        need_w,
                        need_h
                    );
                }
                let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                let acq = if fresh {
                    SrcAcquire::DmabufFresh
                } else {
                    SrcAcquire::DmabufCached
                };
                (img, view, acq, false)
            }
            FramePayload::Dmabuf(d) => {
                // Unaligned: blit into per-slot ALIGNED staging and edge-duplicate. Encode
                // reads staging, never the capture buffer.
                if frame.width != self.render_w || frame.height != self.render_h {
                    bail!(
                        "vulkan-encode (rgb-direct/padded): frame {}x{} != mode {}x{} — \
                         refusing a mismatched blit source",
                        frame.width,
                        frame.height,
                        self.render_w,
                        self.render_h
                    );
                }
                let (img, _view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                let pad_img = self.frames[slot].pad_img;
                let pad_view = self.frames[slot].pad_view;
                self.record_pad_blit(
                    &dev,
                    compute_cmd,
                    img,
                    fresh,
                    pad_img,
                    ts_pool,
                    &[(vk::ImageAspectFlags::COLOR, 1)],
                )?;
                self.frames[slot].ts_written = self.ts_period_ns > 0.0;
                // Staging ends in GENERAL; `csc_sem` orders the hand-off ([`SrcAcquire::CscGeneral`]).
                (pad_img, pad_view, SrcAcquire::CscGeneral, true)
            }
            FramePayload::Cpu(bytes) => {
                // Expand 24-bpp 3→4 before 4-bpp staging math. BGRA-forced: session
                // `pictureFormat` is B8G8R8A8; an R-first source violates
                // VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207. `begin_command_buffer` is after
                // the fallible steps, so no reset-on-error wrap.
                let mut scratch = std::mem::take(&mut self.cpu_expand);
                let (norm_fmt, norm_bytes) =
                    normalize_cpu_rgb(frame.format, bytes, &mut scratch, true);
                let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                let view = match fmt {
                    Ok(f) => self.ensure_cpu_rgb(slot, f, norm_bytes, frame.width, frame.height),
                    Err(e) => Err(e),
                };
                self.cpu_expand = scratch;
                let view = view?;
                let (img, ..) = self.frames[slot].cpu_img.expect("ensure_cpu_rgb built it");
                let (stage, ..) = self.frames[slot]
                    .cpu_stage
                    .expect("ensure_cpu_rgb built it");
                // `compute_cmd` is only the staging→image copy; encode waits on `csc_sem`.
                dev.begin_command_buffer(
                    compute_cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
                let to_dst = vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                    .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(img)
                    .subresource_range(color_range(0));
                dev.cmd_pipeline_barrier2(
                    compute_cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                );
                dev.cmd_copy_buffer_to_image(
                    compute_cmd,
                    stage,
                    img,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: self.width,
                            height: self.height,
                            depth: 1,
                        })],
                );
                dev.end_command_buffer(compute_cmd)?;
                (img, view, SrcAcquire::CpuUpload, true)
            }
            _ => bail!(
                "vulkan-encode (rgb-direct): unsupported FramePayload (need Dmabuf or Cpu RGB)"
            ),
        };
        if self.codec == Codec::Av1 {
            self.record_coding_av1(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, recovery,
                ref_slot, setup_idx, poc,
            )?;
        } else {
            self.record_coding_h265(
                &dev, cmd, query_pool, bs_buf, src_img, src_view, acquire, is_idr, ref_slot,
                setup_idx, poc,
            )?;
        }
        dev.reset_fences(&[fence])?;
        let ecmds = [cmd];
        if compute_active {
            let ccmds = [compute_cmd];
            let sems = [csc_sem];
            dev.queue_submit(
                self.compute_queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&ccmds)
                    .signal_semaphores(&sems)],
                vk::Fence::null(),
            )?;
            let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            dev.queue_submit(
                self.encode_queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&ecmds)
                    .wait_semaphores(&sems)
                    .wait_dst_stage_mask(&wait_stages)],
                fence,
            )?;
        } else {
            dev.queue_submit(
                self.encode_queue,
                &[vk::SubmitInfo::default().command_buffers(&ecmds)],
                fence,
            )?;
        }
        Ok(())
    }

    /// Stash metadata `read_slot` needs once the fence signals, then advance DPB/GOP bookkeeping.
    fn post_submit_bookkeeping(
        &mut self,
        slot: usize,
        pts_ns: u64,
        wire: i64,
        is_idr: bool,
        recovery: bool,
        setup_idx: usize,
        poc: i32,
    ) {
        self.frames[slot].pts_ns = pts_ns;
        self.frames[slot].keyframe = is_idr;
        self.frames[slot].recovery_anchor = recovery;
        // The wave's start and close are the client's two marks. Every wave picture but the
        // close is part dirty, so it never becomes an RFI anchor: `slot_wire` stays -1.
        let wave = self.wave;
        self.frames[slot].recovery_point = wave.is_some_and(Wave::marks);
        self.frames[slot].recovery_close = wave.is_some_and(Wave::closes);
        if is_idr {
            self.slot_wire.iter_mut().for_each(|s| *s = -1);
            self.slot_poc.iter_mut().for_each(|s| *s = -1);
        }
        let dirty = wave.is_some_and(|w| !w.closes());
        self.slot_wire[setup_idx] = if dirty { -1 } else { wire };
        self.slot_poc[setup_idx] = poc;
        if let Some(w) = wave {
            self.wave = w.next();
            if self.wave.is_none() {
                tracing::debug!(
                    cycle = w.cycle,
                    wire,
                    "vulkan-encode: intra refresh wave closed"
                );
            }
        }
        self.prev_slot = setup_idx;
        self.poc = poc + 1;
        self.enc_count += 1;
        // RESET-install folds on `first_frame`, not `is_idr`: a mid-stream IDR still uses the
        // standalone ENCODE_RATE_CONTROL path.
        let was_first_frame = self.first_frame;
        self.first_frame = false;
        // Session now has a non-default RC mode. Unlike `first_frame`, this survives `reset()`.
        self.rc_installed = true;
        self.force_kf = false;
        if let Some(nb) = self.pending_bitrate.take() {
            // Retarget is recorded; later begins declare the new rate.
            self.bitrate = nb;
            tracing::debug!(
                mbps = nb / 1_000_000,
                folded_into_reset = was_first_frame,
                "vulkan-encode: rate control retargeted"
            );
        }
    }

    /// Begin `cmd` and record shared pre-encode setup: query-pool reset, source acquire
    /// ([`SrcAcquire`]), DPB transition. First frame: whole-image UNDEFINED → DPB. Afterwards:
    /// pipelining barrier ordering the previous reconstruct-write before this reference
    /// read/write (ring records N+1 while N still encodes).
    unsafe fn begin_encode_cmd(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        src_img: vk::Image,
        acquire: SrcAcquire,
    ) -> Result<()> {
        dev.begin_command_buffer(
            cmd,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;
        dev.cmd_reset_query_pool(cmd, query_pool, 0, 1);
        let dpb_barrier = if self.first_frame {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        } else {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(
                    vk::AccessFlags2::VIDEO_ENCODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
                )
                .old_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
        }
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(self.dpb_image)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: DPB_SLOTS,
        });
        let src_base = vk::ImageMemoryBarrier2::default()
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
            .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
            .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(src_img)
            .subresource_range(color_range(0));
        let src_barrier = match acquire {
            SrcAcquire::CscGeneral => src_base
                .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::GENERAL),
            SrcAcquire::DmabufFresh => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .src_queue_family_index(self.foreign_qfi)
                .dst_queue_family_index(self.encode_family),
            SrcAcquire::DmabufCached => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR),
            SrcAcquire::CpuUpload => src_base
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL),
        };
        let pre_enc = [src_barrier, dpb_barrier];
        dev.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&pre_enc),
        );
        Ok(())
    }

    /// HEVC Std structs + begin/encode/end. A recovery anchor is an ordinary P whose
    /// `RefPicList0` names the known-good slot; the full short-term RPS ([`build_h265_rps_s0`])
    /// keeps all resident DPB pictures alive at the decoder.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_h265(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        is_idr: bool,
        ref_slot: usize,
        setup_idx: usize,
        poc: i32,
    ) -> Result<()> {
        use super::vk_intra_refresh as vir;
        use ash::vk::native as h;
        // Aligned size for app-aligned sessions (pairs with aligned SPS); render size for
        // native NV12's true-size headers.
        let ext2d = if self.native_nv12 {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            vk::Extent2D {
                width: self.width,
                height: self.height,
            }
        };
        // RGB true-extent: RADV derives firmware padding from `srcPictureResource.codedExtent`.
        let src_extent = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            ext2d
        };
        let ref_poc = if is_idr { 0 } else { self.slot_poc[ref_slot] };

        let mut pic_flags: h::StdVideoEncodeH265PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_is_reference(1);
        if is_idr {
            pic_flags.set_IrapPicFlag(1);
        }
        pic_flags.set_pic_output_flag(1);
        let mut std_pic: h::StdVideoEncodeH265PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.pic_type = if is_idr {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        std_pic.PicOrderCntVal = poc;
        let (num_neg, deltas, used) = build_h265_rps_s0(&self.slot_poc, setup_idx, ref_poc, poc);
        // `ref_slot` is always resident and never the setup slot; a miss means DPB desynced.
        debug_assert!(is_idr || used != 0, "reference POC missing from the RPS");
        let mut rps: h::StdVideoH265ShortTermRefPicSet = std::mem::zeroed();
        rps.num_negative_pics = num_neg;
        rps.delta_poc_s0_minus1 = deltas;
        rps.used_by_curr_pic_s0_flag = used;
        let mut ref_lists: h::StdVideoEncodeH265ReferenceListsInfo = std::mem::zeroed();
        ref_lists.RefPicList0 = [0xff; 15];
        ref_lists.RefPicList1 = [0xff; 15];
        ref_lists.RefPicList0[0] = ref_slot as u8;
        if !is_idr {
            std_pic.pShortTermRefPicSet = &rps;
            std_pic.pRefLists = &ref_lists;
        }
        let mut sh_flags: h::StdVideoEncodeH265SliceSegmentHeaderFlags = std::mem::zeroed();
        sh_flags.set_first_slice_segment_in_pic_flag(1);
        sh_flags.set_slice_loop_filter_across_slices_enabled_flag(1);
        let mut std_sh: h::StdVideoEncodeH265SliceSegmentHeader = std::mem::zeroed();
        std_sh.flags = sh_flags;
        std_sh.slice_type = if is_idr {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_I
        } else {
            h::StdVideoH265SliceType_STD_VIDEO_H265_SLICE_TYPE_P
        };
        std_sh.MaxNumMergeCand = 5;
        let slice = vk::VideoEncodeH265NaluSliceSegmentInfoKHR::default()
            .constant_qp(0)
            .std_slice_segment_header(&std_sh);
        let slices = [slice];
        let mut h265_pic = vk::VideoEncodeH265PictureInfoKHR::default()
            .nalu_slice_segment_entries(&slices)
            .std_picture_info(&std_pic);

        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        setup_std.pic_type = std_pic.pic_type;
        setup_std.PicOrderCntVal = poc;
        let mut setup_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let mut setup_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_a);
        let begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res)
            .push_next(&mut setup_dpb_b);

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(ext2d)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_std: h::StdVideoEncodeH265ReferenceInfo = std::mem::zeroed();
        ref_std.pic_type = if ref_poc == 0 {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_IDR
        } else {
            h::StdVideoH265PictureType_STD_VIDEO_H265_PICTURE_TYPE_P
        };
        ref_std.PicOrderCntVal = ref_poc;
        let mut ref_dpb_a =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        let mut ref_dpb_b =
            vk::VideoEncodeH265DpbSlotInfoKHR::default().std_reference_info(&ref_std);
        // Wave frame: the reference is the previous frame, `cycle - index` of its regions
        // still dirty (VUID-10843). Absent = 0, which every non-wave frame requires.
        // `&mut`: `push_next` below writes `ref_ir.p_next` through the chain.
        let (ir_info, mut ref_ir) = intra_refresh_chain(self.wave);
        if self.wave.is_some() {
            ref_dpb_b.p_next = &mut ref_ir as *mut _ as *const c_void;
        }
        let ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_a);
        let ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res)
            .push_next(&mut ref_dpb_b);
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        // Chained manually (`push_next` would clobber `rc.p_next`). Declares CURRENT state
        // (`self.bitrate`), never a pending retarget (VUID-...-08254).
        let rc_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate)
            .max_bitrate(self.bitrate)
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1)];
        let h265_rc = vk::VideoEncodeH265RateControlInfoKHR::default()
            .flags(vk::VideoEncodeH265RateControlFlagsKHR::REGULAR_GOP)
            .gop_frame_count(u32::MAX)
            .idr_period(u32::MAX)
            .consecutive_b_frame_count(0)
            .sub_layer_count(1);
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(self.rc_mode)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(self.vbv_ms.0)
            .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
        rc.p_next = &h265_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        self.begin_encode_cmd(dev, cmd, query_pool, src_img, acquire)?;
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        // Declare the session's actual RC state, not `!first_frame` (`reset()` re-arms that).
        if self.rc_installed {
            begin.p_next = rc_ptr;
        }
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + RC install + quality. Without ENCODE_QUALITY_LEVEL RADV never sends a
            // VCN preset op. Quality chains ahead of RC (spec: a quality-level change must
            // carry ENCODE_RATE_CONTROL). Pending retarget folds into the install, not
            // `self.bitrate` before recording — begin must declare the old rate after `reset()`.
            let nb = self.pending_bitrate.unwrap_or(self.bitrate);
            let install_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut install_rc = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&install_layer)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            install_rc.p_next = &h265_rc as *const _ as *const c_void;
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = &install_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default().flags(
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                    | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
            );
            ctrl.p_next = &q as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        } else if let Some(nb) = self.pending_bitrate {
            // Mid-stream retarget: begin declared CURRENT; this control installs NEW. No RESET.
            let rc_layer2 = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut rc2 = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&rc_layer2)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            rc2.p_next = &h265_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(src_view);
        let mut enc = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(bs_buf)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bs_size)
            .src_picture_resource(src_res)
            .setup_reference_slot(&setup_slot)
            .push_next(&mut h265_pic);
        if !is_idr {
            enc = enc.reference_slots(&enc_refs);
        }
        let mut ir_info = ir_info;
        if self.wave.is_some() {
            enc.flags |= vk::VideoEncodeFlagsKHR::from_raw(vir::ENCODE_INTRA_REFRESH_BIT);
            ir_info.p_next = enc.p_next;
            enc.p_next = &ir_info as *const _ as *const c_void;
        }
        (self.venc_dev.fp().cmd_encode_video_khr)(cmd, &enc);
        dev.cmd_end_query(cmd, query_pool, 0);
        (self.vq_dev.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
        dev.end_command_buffer(cmd)?;
        Ok(())
    }

    /// AV1 Std structs + begin/encode/end. IDR, recovery and a wave start break the CDF chain
    /// (`primary_ref_frame = PRIMARY_REF_NONE` + `error_resilient_mode`). A normal P inherits
    /// context (name 0 → `ref_slot`). AV1's 8 virtual slots persist until `refresh_frame_flags`
    /// overwrites them — no per-frame RPS.
    #[allow(clippy::too_many_arguments)]
    unsafe fn record_coding_av1(
        &self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        query_pool: vk::QueryPool,
        bs_buf: vk::Buffer,
        src_img: vk::Image,
        src_view: vk::ImageView,
        acquire: SrcAcquire,
        is_idr: bool,
        recovery: bool,
        ref_slot: usize,
        setup_idx: usize,
        order: i32,
    ) -> Result<()> {
        use super::vk_av1_encode as av1;
        use super::vk_intra_refresh as vir;
        use ash::vk::native as h;
        // Aligned size for app-aligned sessions (pairs with aligned SPS); render size for
        // native NV12's true-size headers.
        let ext2d = if self.native_nv12 {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            vk::Extent2D {
                width: self.width,
                height: self.height,
            }
        };
        // RGB true-extent: RADV derives firmware padding from `srcPictureResource.codedExtent`.
        let src_extent = if self.rgb.as_ref().is_some_and(|r| r.true_extent) {
            vk::Extent2D {
                width: self.render_w,
                height: self.render_h,
            }
        } else {
            ext2d
        };

        let mut tile_flags: h::StdVideoAV1TileInfoFlags = std::mem::zeroed();
        tile_flags.set_uniform_tile_spacing_flag(1);
        let mut tile_info: h::StdVideoAV1TileInfo = std::mem::zeroed();
        tile_info.flags = tile_flags;
        tile_info.TileCols = 1;
        tile_info.TileRows = 1;

        let mut quant: h::StdVideoAV1Quantization = std::mem::zeroed();
        quant.base_q_idx = AV1_BASE_Q_IDX;

        let mut loop_filter: h::StdVideoAV1LoopFilter = std::mem::zeroed();
        // Spec 7.14.1 default_loop_filter_ref_deltas: intra +1, golden/bwd/altref2/altref -1.
        loop_filter.loop_filter_ref_deltas = [1, 0, 0, 0, -1, 0, -1, -1];

        let cdef: h::StdVideoAV1CDEF = std::mem::zeroed();

        let mut lr: h::StdVideoAV1LoopRestoration = std::mem::zeroed();
        lr.FrameRestorationType =
            [h::StdVideoAV1FrameRestorationType_STD_VIDEO_AV1_FRAME_RESTORATION_TYPE_NONE; 3];

        let gm: h::StdVideoAV1GlobalMotion = std::mem::zeroed();

        // Order hints of the 8 physical DPB slots; 0 where empty.
        let mut ref_order_hint = [0u8; 8];
        for (i, &poc) in self.slot_poc.iter().enumerate().take(8) {
            ref_order_hint[i] = poc.max(0) as u8;
        }

        // Recovery/IDR/wave start: error-resilient, no CDF inherit — the client's CDFs for a
        // concealed frame are undefined. Normal P inherits from name 0 → `ref_slot`.
        let independent = is_idr || recovery || self.wave.is_some_and(|w| w.index == 0);
        let mut pic_flags: av1::StdVideoEncodeAV1PictureInfoFlags = std::mem::zeroed();
        pic_flags.set_show_frame(1);
        if independent {
            pic_flags.set_error_resilient_mode(1);
        }
        // AV1 ignores `render_*_minus_1` unless this flag is set. Without it the decoder uses
        // the coded size and displays alignment padding.
        if self.render_w != src_extent.width || self.render_h != src_extent.height {
            pic_flags.set_render_and_frame_size_different(1);
        }
        let mut std_pic: av1::StdVideoEncodeAV1PictureInfo = std::mem::zeroed();
        std_pic.flags = pic_flags;
        std_pic.frame_type = if is_idr {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_KEY
        } else {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_INTER
        };
        std_pic.order_hint = order as u8;
        std_pic.primary_ref_frame = if independent {
            av1::PRIMARY_REF_NONE
        } else {
            0
        };
        std_pic.refresh_frame_flags = if is_idr { 0xff } else { 1u8 << setup_idx };
        std_pic.render_width_minus_1 = (self.render_w - 1) as u16;
        std_pic.render_height_minus_1 = (self.render_h - 1) as u16;
        std_pic.interpolation_filter = 0; // EIGHTTAP
        std_pic.TxMode = h::StdVideoAV1TxMode_STD_VIDEO_AV1_TX_MODE_SELECT;
        std_pic.ref_order_hint = ref_order_hint;
        if !is_idr {
            // Every reference name maps to the (recovery or previous) DPB slot.
            std_pic.ref_frame_idx = [ref_slot as i8; 7];
        }
        std_pic.pTileInfo = &tile_info;
        std_pic.pQuantization = &quant;
        std_pic.pLoopFilter = &loop_filter;
        std_pic.pCDEF = &cdef;
        std_pic.pLoopRestoration = &lr;
        // pSegmentation MUST be NULL (VUID-vkCmdEncodeVideoKHR-pStdPictureInfo-10350).
        std_pic.pGlobalMotion = &gm;

        let av1_pic = av1::VideoEncodeAV1PictureInfoKHR {
            s_type: av1::stype(av1::ST_PICTURE_INFO),
            p_next: std::ptr::null(),
            prediction_mode: if is_idr {
                av1::PREDICTION_MODE_INTRA_ONLY
            } else {
                av1::PREDICTION_MODE_SINGLE_REFERENCE
            },
            rate_control_group: if is_idr {
                av1::RC_GROUP_INTRA
            } else {
                av1::RC_GROUP_PREDICTIVE
            },
            // Must be zero when RC is not DISABLED (VUID-...-constantQIndex-10320). Q still
            // reaches the encoder through `pQuantization` for the header.
            constant_q_index: 0,
            p_std_picture_info: &std_pic,
            reference_name_slot_indices: if is_idr {
                [-1; av1::MAX_VIDEO_AV1_REFERENCES_PER_FRAME]
            } else {
                [ref_slot as i32; av1::MAX_VIDEO_AV1_REFERENCES_PER_FRAME]
            },
            primary_reference_cdf_only: 0,
            generate_obu_extension_header: 0,
        };

        // DPB slots carry the SOURCE extent. Without MOTION_VECTOR_SCALING every reference
        // slot's `codedExtent` must equal the source's (VUID-...-flags-10325).
        let setup_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(self.dpb_views[setup_idx]);
        let mut setup_ref_std: av1::StdVideoEncodeAV1ReferenceInfo = std::mem::zeroed();
        setup_ref_std.frame_type = std_pic.frame_type;
        setup_ref_std.OrderHint = order as u8;
        let setup_dpb = av1::VideoEncodeAV1DpbSlotInfoKHR {
            s_type: av1::stype(av1::ST_DPB_SLOT_INFO),
            p_next: std::ptr::null(),
            p_std_reference_info: &setup_ref_std,
        };
        let mut setup_slot = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(setup_idx as i32)
            .picture_resource(&setup_res);
        setup_slot.p_next = &setup_dpb as *const _ as *const c_void;
        let mut begin_setup = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(-1)
            .picture_resource(&setup_res);
        begin_setup.p_next = &setup_dpb as *const _ as *const c_void;

        let ref_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(self.dpb_views[ref_slot]);
        let mut ref_ref_std: av1::StdVideoEncodeAV1ReferenceInfo = std::mem::zeroed();
        ref_ref_std.frame_type = if self.slot_poc[ref_slot] == 0 {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_KEY
        } else {
            h::StdVideoAV1FrameType_STD_VIDEO_AV1_FRAME_TYPE_INTER
        };
        ref_ref_std.OrderHint = self.slot_poc[ref_slot].max(0) as u8;
        let ref_dpb = av1::VideoEncodeAV1DpbSlotInfoKHR {
            s_type: av1::stype(av1::ST_DPB_SLOT_INFO),
            p_next: std::ptr::null(),
            p_std_reference_info: &ref_ref_std,
        };
        let mut ref_begin = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res);
        ref_begin.p_next = &ref_dpb as *const _ as *const c_void;
        // Wave frame: the reference is the previous frame, `cycle - index` of its regions
        // still dirty (VUID-10843). Absent = 0, which every non-wave frame requires.
        let (mut ir_info, mut ref_ir) = intra_refresh_chain(self.wave);
        ref_ir.p_next = &ref_dpb as *const _ as *const c_void;
        let mut ref_enc = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(ref_slot as i32)
            .picture_resource(&ref_res);
        ref_enc.p_next = if self.wave.is_some() {
            &mut ref_ir as *mut _ as *const c_void
        } else {
            &ref_dpb as *const _ as *const c_void
        };
        let begin_p = [ref_begin, begin_setup];
        let begin_i = [begin_setup];
        let enc_refs = [ref_enc];

        // Declares CURRENT state (`self.bitrate`); see the HEVC twin for VUID-08254.
        let rc_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(self.bitrate)
            .max_bitrate(self.bitrate)
            .frame_rate_numerator(self.fps)
            .frame_rate_denominator(1)];
        let av1_rc = av1::VideoEncodeAV1RateControlInfoKHR {
            s_type: av1::stype(av1::ST_RATE_CONTROL_INFO),
            p_next: std::ptr::null(),
            flags: 0,
            gop_frame_count: 0,
            key_frame_period: 0,
            consecutive_bipredictive_frame_count: 0,
            temporal_layer_count: 1,
        };
        let mut rc = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(self.rc_mode)
            .layers(&rc_layer)
            .virtual_buffer_size_in_ms(self.vbv_ms.0)
            .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
        rc.p_next = &av1_rc as *const _ as *const c_void;
        let rc_ptr = &rc as *const _ as *const c_void;

        self.begin_encode_cmd(dev, cmd, query_pool, src_img, acquire)?;
        let begin_slots: &[vk::VideoReferenceSlotInfoKHR] =
            if is_idr { &begin_i } else { &begin_p };
        let mut begin = vk::VideoBeginCodingInfoKHR::default()
            .video_session(self.session)
            .video_session_parameters(self.params)
            .reference_slots(begin_slots);
        // Declare what the session actually has, not `!first_frame`.
        if self.rc_installed {
            begin.p_next = rc_ptr;
        }
        (self.vq_dev.fp().cmd_begin_video_coding_khr)(cmd, &begin);
        if self.first_frame {
            // RESET + RC + quality. Pending retarget folds into the install, not the declaration.
            let nb = self.pending_bitrate.unwrap_or(self.bitrate);
            let install_layer = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut install_rc = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&install_layer)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            install_rc.p_next = &av1_rc as *const _ as *const c_void;
            let mut q =
                vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(self.quality_level);
            q.p_next = &install_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default().flags(
                vk::VideoCodingControlFlagsKHR::RESET
                    | vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL
                    | vk::VideoCodingControlFlagsKHR::ENCODE_QUALITY_LEVEL,
            );
            ctrl.p_next = &q as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        } else if let Some(nb) = self.pending_bitrate {
            // Mid-stream retarget: begin declares CURRENT, this control installs NEW. No RESET.
            let rc_layer2 = [vk::VideoEncodeRateControlLayerInfoKHR::default()
                .average_bitrate(nb)
                .max_bitrate(nb)
                .frame_rate_numerator(self.fps)
                .frame_rate_denominator(1)];
            let mut rc2 = vk::VideoEncodeRateControlInfoKHR::default()
                .rate_control_mode(self.rc_mode)
                .layers(&rc_layer2)
                .virtual_buffer_size_in_ms(self.vbv_ms.0)
                .initial_virtual_buffer_size_in_ms(self.vbv_ms.1);
            rc2.p_next = &av1_rc as *const _ as *const c_void;
            let mut ctrl = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL);
            ctrl.p_next = &rc2 as *const _ as *const c_void;
            (self.vq_dev.fp().cmd_control_video_coding_khr)(cmd, &ctrl);
        }
        dev.cmd_begin_query(cmd, query_pool, 0, vk::QueryControlFlags::empty());
        let src_res = vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(src_extent)
            .image_view_binding(src_view);
        let mut enc = vk::VideoEncodeInfoKHR::default()
            .dst_buffer(bs_buf)
            .dst_buffer_offset(0)
            .dst_buffer_range(self.bs_size)
            .src_picture_resource(src_res)
            .setup_reference_slot(&setup_slot);
        if !is_idr {
            enc = enc.reference_slots(&enc_refs);
        }
        enc.p_next = &av1_pic as *const _ as *const c_void;
        if self.wave.is_some() {
            enc.flags |= vk::VideoEncodeFlagsKHR::from_raw(vir::ENCODE_INTRA_REFRESH_BIT);
            ir_info.p_next = enc.p_next;
            enc.p_next = &ir_info as *const _ as *const c_void;
        }
        (self.venc_dev.fp().cmd_encode_video_khr)(cmd, &enc);
        dev.cmd_end_query(cmd, query_pool, 0);
        (self.vq_dev.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
        dev.end_command_buffer(cmd)?;
        Ok(())
    }

    /// Read a completed slot's bitstream. HEVC keyframes carry VPS/SPS/PPS; AV1 opens every
    /// temporal unit with a TD OBU and prepends the sequence-header OBU on keyframes.
    /// Caller must have confirmed the slot's fence is signaled.
    unsafe fn read_slot(&mut self, slot: usize) -> Result<EncodedFrame> {
        let dev = self.device.clone();
        let f = &self.frames[slot];
        // Status rides as a trailing signed element (`VkQueryResultStatusKHR`: >0 COMPLETE,
        // 0 NOT_READY, <0 error). Without it a FAILED encode is shipped as bitstream.
        let mut fb = [[0i32; 3]; 1];
        dev.get_query_pool_results(
            f.query_pool,
            0,
            &mut fb,
            vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR,
        )?;
        let status = fb[0][2];
        if status <= 0 {
            anyhow::bail!(
                "vulkan-encode: encode feedback for slot {slot} reports status {status} \
                 (not COMPLETE) — dropping the frame rather than shipping its bitstream"
            );
        }
        let fb = [[fb[0][0] as u32, fb[0][1] as u32]];
        // Driver-reported (offset, bytes-written): validate against the allocation before the
        // `from_raw_parts` below, in u64 so the add cannot wrap.
        let (off64, len64) = (fb[0][0] as u64, fb[0][1] as u64);
        if off64.saturating_add(len64) > self.bs_size {
            anyhow::bail!(
                "vulkan-encode: driver reported bitstream feedback offset={off64} \
                 bytes_written={len64}, outside the {} byte bitstream buffer — the encode likely \
                 overflowed its destination range",
                self.bs_size
            );
        }
        let (off, len) = (off64 as usize, len64 as usize);
        // Device duration only; remaining host fence wait still includes queueing/sync.
        if self.ts_period_ns > 0.0
            && f.ts_pool != vk::QueryPool::null()
            && f.ts_written
            && self.perf_at.elapsed() >= std::time::Duration::from_secs(2)
        {
            let mut ts = [0u64; 2];
            if dev
                .get_query_pool_results(
                    f.ts_pool,
                    0,
                    &mut ts,
                    vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
                )
                .is_ok()
            {
                self.perf_at = std::time::Instant::now();
                let pre_encode_us =
                    (ts[1].saturating_sub(ts[0]) as f64 * self.ts_period_ns) / 1000.0;
                if self.rgb.as_ref().is_some_and(|r| r.padded) {
                    tracing::info!(
                        rgb_copy_us = format!("{pre_encode_us:.0}"),
                        au_bytes = len,
                        "vulkan-encode split (sampled): padded RGB copy device time before EFC; \
                         remaining fence wait still includes queue synchronization + RGB→YUV EFC \
                         + video encode"
                    );
                } else {
                    tracing::info!(
                        csc_us = format!("{pre_encode_us:.0}"),
                        au_bytes = len,
                        "vulkan-encode split (sampled): RGB→NV12 compute batch device time; \
                         remaining fence wait still includes queue synchronization + video encode"
                    );
                }
            }
        }
        let f = &self.frames[slot];
        let p = f.bs_ptr.0;
        debug_assert!(!p.is_null(), "bs_mem persistent mapping missing");
        let prefix: &[u8] = if f.keyframe {
            &self.header
        } else {
            &self.frame_prefix
        };
        let mut data = Vec::with_capacity(prefix.len() + len);
        data.extend_from_slice(prefix);
        // Parameter sets / sequence header first, then the HDR volume, then the picture.
        if let Some(m) = self.hdr_meta.filter(|_| f.keyframe && self.is_hdr) {
            match self.codec {
                Codec::Av1 => data.extend_from_slice(&pf_frame::hdr::av1_hdr_metadata_obus(&m)),
                _ => data.extend_from_slice(&pf_frame::hdr::hevc_hdr_sei_nal(&m)),
            }
        }
        data.extend_from_slice(std::slice::from_raw_parts(p.add(off), len));
        Ok(EncodedFrame {
            data,
            pts_ns: f.pts_ns,
            keyframe: f.keyframe,
            recovery_anchor: f.recovery_anchor,
            recovery_point: f.recovery_point,
            recovery_close: f.recovery_close,
            chunk_aligned: false,
        })
    }

    /// Acquire a free ring slot (drain the oldest if full), record+submit without waiting.
    unsafe fn enqueue(&mut self, frame: &CapturedFrame, wire: i64) -> Result<()> {
        // If every slot is outstanding, block on the oldest (the round-robin `ring` cursor).
        while self.in_flight.len() >= self.frames.len() {
            let slot = self.in_flight.pop_front().unwrap();
            // Bounded, not `u64::MAX`: this is the stall-watchdog thread. An infinite wait
            // against a wedged GPU parks the only path that can `reset()`.
            match self.device.wait_for_fences(
                &[self.frames[slot].fence],
                true,
                ENCODE_FENCE_TIMEOUT_NS,
            ) {
                Ok(()) => {}
                Err(vk::Result::TIMEOUT) => anyhow::bail!(
                    "vulkan-encode: fence for slot {slot} did not signal within {} ms — GPU or \
                     driver wedged; failing the submit so the session can reset",
                    ENCODE_FENCE_TIMEOUT_NS / 1_000_000
                ),
                Err(e) => return Err(e.into()),
            }
            self.frames[slot].src_hold = None;
            let done = self.read_slot(slot)?;
            self.pending.push_back(done);
        }
        let slot = self.ring;
        self.ring = (self.ring + 1) % self.frames.len();
        // Take the deferred-requeue hold before recording: encode reads the dmabuf until this
        // slot's fence retires. Assigned even if `record_submit` fails — over-hold is harmless,
        // released-while-referenced is the race this closes.
        self.frames[slot].src_hold = match &frame.payload {
            FramePayload::Dmabuf(d) => d.hold.clone(),
            _ => None,
        };
        self.record_submit(slot, frame, wire)?;
        self.in_flight.push_back(slot);
        Ok(())
    }
}

impl Encoder for VulkanVideoEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        let wire = self.auto_wire;
        self.auto_wire += 1;
        // SAFETY: `enqueue` records into a free owned slot; `&mut self` is exclusive. Poll waits.
        unsafe { self.enqueue(frame, wire) }
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.auto_wire = wire_index as i64 + 1;
        // SAFETY: exclusive `&mut self`; all Vulkan work confined to owned objects.
        unsafe { self.enqueue(frame, wire_index as i64) }
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: true,
            // Only CSC composites (`prep_cursor`). RGB-direct/EFC and native NV12 have no blend.
            blends_cursor: self.rgb.is_none() && !self.native_nv12,
            // `set_input_crop` moves an EFC session onto the CSC; a producer's NV12 has no RGB stage.
            downscales_input: !self.native_nv12,
            crops_input: !self.native_nv12,
            ..Default::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.hdr_meta = meta;
    }

    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        if first_frame < 0 || first_frame > last_frame {
            return false;
        }
        // Distrust = blank `slot_wire` only. `slot_poc` must keep naming every physically
        // resident DPB picture for `build_h265_rps_s0`, or a conforming decoder evicts them.
        // "Resident and older than this loss" is not "the client decoded it" — after an earlier
        // loss recovered at wire r, wires in [a, r-1] stay candidates until the ring rolls them.
        let plan = crate::rfi::plan_slot_recovery(&trusted_refs(&self.slot_wire), first_frame);
        for (s, w) in self.slot_wire.iter_mut().enumerate() {
            if plan.tainted & (1 << s) != 0 {
                *w = -1;
            }
        }
        match plan.anchor {
            Some(_) => {
                self.pending_loss = Some(first_frame);
                true
            }
            // No anchor, but a wave heals without one: frame-build starts it from this arm.
            // `true` keeps the caller on its RFI bookkeeping instead of the keyframe path.
            None if self.intra_refresh.is_some() => {
                self.pending_loss = Some(first_frame);
                true
            }
            None => {
                // Decline without arming IDR: caller owns the coalesced keyframe fallback.
                // Leave `pending_loss` armed — a stale arm re-resolves at frame-build; clearing
                // it would ship an untagged plain P during the caller's RFI-echo window.
                tracing::debug!(
                    first_frame,
                    last_frame,
                    "vulkan-encode RFI declined: no resident reference older than the loss — \
                     caller falls back to its (coalesced) keyframe path"
                );
                false
            }
        }
    }

    /// Withdraw anchor trust from every resident reference. Blank `slot_wire` only;
    /// `slot_poc` must keep naming every physically-resident DPB picture ([`build_h265_rps_s0`]).
    /// Leave `pending_loss` armed: a stale arm re-resolves at frame-build and forces IDR.
    fn distrust_references(&mut self) {
        let trusted = self.slot_wire.iter().filter(|&&w| w >= 0).count();
        if trusted == 0 {
            return;
        }
        self.slot_wire.iter_mut().for_each(|w| *w = -1);
        tracing::debug!(
            trusted,
            "vulkan-encode: client reported unrepaired damage — withdrawing RFI anchor trust from \
             every resident reference (prediction and the RPS are unaffected)"
        );
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Blocking, per the depth-1 pump contract. A non-blocking fence probe deferred every
        // AU a full frame period. `None` means nothing submitted.
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        let Some(&slot) = self.in_flight.front() else {
            return Ok(None);
        };
        // Bounded like `enqueue`: this is the stall-recovery thread.
        // SAFETY: waiting a fence owned by this encoder's slot under `&mut self`.
        match unsafe {
            self.device
                .wait_for_fences(&[self.frames[slot].fence], true, ENCODE_FENCE_TIMEOUT_NS)
        } {
            Ok(()) => {}
            Err(vk::Result::TIMEOUT) => anyhow::bail!(
                "vulkan-encode: fence for slot {slot} did not signal within {} ms — GPU or \
                 driver wedged; failing the poll so the session can reset",
                ENCODE_FENCE_TIMEOUT_NS / 1_000_000
            ),
            Err(e) => return Err(e.into()),
        }
        self.in_flight.pop_front();
        self.frames[slot].src_hold = None;
        // SAFETY: fence signaled ⇒ this slot's CSC+encode is complete; read its bitstream.
        Ok(Some(unsafe { self.read_slot(slot)? }))
    }

    fn reset(&mut self) -> bool {
        // Bounded wait: `reset()` runs because something looks wedged; an untimed idle would
        // park recovery. Timeout ⇒ no in-place rebuild. (`Drop` still waits unbounded.)
        let fences: Vec<vk::Fence> = self
            .in_flight
            .iter()
            .map(|&s| self.frames[s].fence)
            .collect();
        if !fences.is_empty() {
            // SAFETY: every in-flight fence was submitted with its batch; we hold `&mut self`.
            match unsafe {
                self.device
                    .wait_for_fences(&fences, true, ENCODE_FENCE_TIMEOUT_NS)
            } {
                Ok(()) => {}
                Err(e) => {
                    tracing::error!(
                        error = ?e,
                        "vulkan-encode: in-flight work did not go idle within {} ms — GPU or \
                         driver wedged; in-place rebuild abandoned",
                        ENCODE_FENCE_TIMEOUT_NS / 1_000_000
                    );
                    return false;
                }
            }
        }
        // Fences cover encode; each encode waited `csc_sem`. Residual is a compute batch whose
        // encode submit failed (never fenced). Error here is a lost device: no rebuild.
        // SAFETY: waiting this encoder's own device idle under `&mut self`.
        if unsafe { self.device.device_wait_idle() }.is_err() {
            return false;
        }
        // Only safe point outside teardown to drop the import cache (device idle).
        // SAFETY: device idle (waits above); each entry is owned by the cache, destroyed once.
        unsafe {
            for e in std::mem::take(&mut self.import_cache) {
                self.device.destroy_image_view(e.view, None);
                self.device.destroy_image(e.img, None);
                self.device.free_memory(e.mem, None);
            }
        }
        self.in_flight.clear();
        self.pending.clear();
        for f in &mut self.frames {
            f.src_hold = None;
        }
        self.ring = 0;
        self.first_frame = true;
        self.force_kf = false;
        self.pending_loss = None;
        self.wave = None;
        self.poc = 0;
        self.slot_wire.iter_mut().for_each(|s| *s = -1);
        self.slot_poc.iter_mut().for_each(|s| *s = -1);
        // Pending retarget survives: the restart's first frame folds it into RESET + RC install.
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Staged rate: next `record_submit` emits ENCODE_RATE_CONTROL — no session churn, no IDR.
        // Same floor as `open` and the same driver ceiling. Clamp is visible via `applied_bitrate_bps`.
        let clamped = bps.max(1_000_000).min(self.hw_max_bitrate);
        if clamped < bps {
            tracing::debug!(
                requested = bps,
                cap = self.hw_max_bitrate,
                "vulkan-encode: retarget clamped to the driver's maxBitrate"
            );
        }
        self.pending_bitrate = Some(clamped);
        true
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        // After the `hw_max_bitrate` clamp. A pending retarget is reported as applied: the next
        // recorded frame installs it, and callers read this right after `reconfigure_bitrate`.
        Some(self.pending_bitrate.unwrap_or(self.bitrate))
    }

    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        ensure!(
            !self.native_nv12,
            "vulkan-encode: a producer's NV12 source cannot be reframed"
        );
        let [_, _, w, h] = rect;
        ensure!(
            w >= self.render_w && h >= self.render_h,
            "vulkan-encode: a {w}x{h} crop would upscale into the {}x{} session",
            self.render_w,
            self.render_h
        );
        if self.rgb.is_some() {
            // EFC bakes an RGB picture format into the session and the reframe feeds the CSC,
            // so reopen as a CSC session. Called before the first submit: nothing is lost.
            *self = Self::open_opts_inner(
                self.codec,
                self.render_w,
                self.render_h,
                self.fps,
                self.bitrate,
                false,
                false,
                self.ten_bit,
                self.is_hdr,
                vk::Format::B8G8R8A8_UNORM,
            )?;
        }
        if let Some(mut old) = self.reframe.take() {
            // SAFETY: live device; waiting out every submission means none still reads it.
            unsafe {
                self.device.device_wait_idle()?;
                old.destroy(&self.device);
            }
        }
        // SAFETY: fresh handles on this encoder's live device, released in `Drop` or above.
        self.reframe = Some(unsafe {
            reframe_stage::Reframe::new(
                &self.device,
                &self.mem_props,
                rect,
                (self.render_w, self.render_h),
                self.frames.len(),
            )?
        });
        tracing::info!(
            crop = ?rect,
            out = ?(self.render_w, self.render_h),
            "vulkan-encode: cropping and scaling ahead of the CSC"
        );
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        while let Some(slot) = self.in_flight.pop_front() {
            // SAFETY: wait this slot's fence, then read back its own owned bitstream objects.
            unsafe {
                self.device
                    .wait_for_fences(&[self.frames[slot].fence], true, u64::MAX)?;
                let done = self.read_slot(slot)?;
                self.pending.push_back(done);
            }
        }
        Ok(())
    }
}

/// Cached dmabuf import ([`VulkanVideoEncoder::import_cached`]). Keyed by `(st_dev, st_ino)`;
/// a key hit must also prove the cached image still matches the caller's extent.
struct CachedImport {
    key: (u64, u64),
    extent: (u32, u32),
    img: vk::Image,
    mem: vk::DeviceMemory,
    view: vk::ImageView,
}

/// Every destructible Vulkan object, destroyed in dependency order. Both teardown paths run
/// through it: `open_inner` mirrors objects as created so an early `?` unwinds exactly what was
/// built; [`VulkanVideoEncoder`]'s `Drop` rebuilds one from its fields. Null handles (a failed
/// build prefix) are no-ops, so the full sequence is safe against any prefix.
struct VkTeardown {
    instance: Option<ash::Instance>,
    // Set together (wrapper constructors after `create_device` are infallible).
    device: Option<ash::Device>,
    vq_dev: Option<ash::khr::video_queue::Device>,
    import_cache: Vec<CachedImport>,
    frames: Vec<Frame>,
    compute_pool: vk::CommandPool,
    cmd_pool: vk::CommandPool,
    // Alive only until the post-pipeline destroy in `open_inner`; null from encoder `Drop`.
    shader: vk::ShaderModule,
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_pool: vk::DescriptorPool,
    csc_dsl: vk::DescriptorSetLayout,
    sampler: vk::Sampler,
    dpb_views: Vec<vk::ImageView>,
    dpb_image: vk::Image,
    dpb_mem: vk::DeviceMemory,
    params: vk::VideoSessionParametersKHR,
    session: vk::VideoSessionKHR,
    session_mem: Vec<vk::DeviceMemory>,
}

impl VkTeardown {
    /// Guard owning only the instance. Field-by-field: struct-update is not allowed on a `Drop` type.
    fn new(instance: ash::Instance) -> Self {
        Self {
            instance: Some(instance),
            device: None,
            vq_dev: None,
            import_cache: Vec::new(),
            frames: Vec::new(),
            compute_pool: vk::CommandPool::null(),
            cmd_pool: vk::CommandPool::null(),
            shader: vk::ShaderModule::null(),
            csc_pipe: vk::Pipeline::null(),
            csc_layout: vk::PipelineLayout::null(),
            csc_pool: vk::DescriptorPool::null(),
            csc_dsl: vk::DescriptorSetLayout::null(),
            sampler: vk::Sampler::null(),
            dpb_views: Vec::new(),
            dpb_image: vk::Image::null(),
            dpb_mem: vk::DeviceMemory::null(),
            params: vk::VideoSessionParametersKHR::null(),
            session: vk::VideoSessionKHR::null(),
            session_mem: Vec::new(),
        }
    }
}

impl Drop for VkTeardown {
    fn drop(&mut self) {
        // SAFETY: `device_wait_idle` first, then destroy idle handles owned solely by `self`,
        // each once (the takes prevent a double free), in dependency order. Null handles are
        // no-ops.
        unsafe {
            if let Some(device) = self.device.take() {
                let _ = device.device_wait_idle();
                for e in std::mem::take(&mut self.import_cache) {
                    device.destroy_image_view(e.view, None);
                    device.destroy_image(e.img, None);
                    device.free_memory(e.mem, None);
                }
                for f in std::mem::take(&mut self.frames) {
                    device.destroy_semaphore(f.csc_sem, None);
                    device.destroy_fence(f.fence, None);
                    device.destroy_query_pool(f.query_pool, None);
                    device.destroy_query_pool(f.ts_pool, None);
                    device.destroy_image_view(f.pad_view, None);
                    device.destroy_image(f.pad_img, None);
                    device.free_memory(f.pad_mem, None);
                    device.destroy_buffer(f.bs_buf, None);
                    // Persistently mapped; `vkFreeMemory` implicitly unmaps.
                    device.free_memory(f.bs_mem, None);
                    for (img, mem, view) in [
                        (f.y_img, f.y_mem, f.y_view),
                        (f.uv_img, f.uv_mem, f.uv_view),
                        (f.nv12_src, f.nv12_mem, f.nv12_view),
                    ] {
                        device.destroy_image_view(view, None);
                        device.destroy_image(img, None);
                        device.free_memory(mem, None);
                    }
                    if let Some((i, m, v, ..)) = f.cpu_img {
                        device.destroy_image_view(v, None);
                        device.destroy_image(i, None);
                        device.free_memory(m, None);
                    }
                    if let Some((b, m, _)) = f.cpu_stage {
                        device.destroy_buffer(b, None);
                        device.free_memory(m, None);
                    }
                    device.destroy_image_view(f.cursor_view, None);
                    device.destroy_image(f.cursor_img, None);
                    device.free_memory(f.cursor_mem, None);
                    device.destroy_buffer(f.cursor_stage, None);
                    device.free_memory(f.cursor_stage_mem, None);
                }
                device.destroy_command_pool(self.compute_pool, None);
                device.destroy_command_pool(self.cmd_pool, None);
                device.destroy_shader_module(self.shader, None);
                device.destroy_pipeline(self.csc_pipe, None);
                device.destroy_pipeline_layout(self.csc_layout, None);
                device.destroy_descriptor_pool(self.csc_pool, None);
                device.destroy_descriptor_set_layout(self.csc_dsl, None);
                device.destroy_sampler(self.sampler, None);
                for &v in &self.dpb_views {
                    device.destroy_image_view(v, None);
                }
                device.destroy_image(self.dpb_image, None);
                device.free_memory(self.dpb_mem, None);
                if let Some(vq_dev) = self.vq_dev.take() {
                    (vq_dev.fp().destroy_video_session_parameters_khr)(
                        device.handle(),
                        self.params,
                        std::ptr::null(),
                    );
                    (vq_dev.fp().destroy_video_session_khr)(
                        device.handle(),
                        self.session,
                        std::ptr::null(),
                    );
                }
                for &m in &self.session_mem {
                    device.free_memory(m, None);
                }
                device.destroy_device(None);
            }
            if let Some(instance) = self.instance.take() {
                instance.destroy_instance(None);
            }
        }
    }
}

impl Drop for VulkanVideoEncoder {
    fn drop(&mut self) {
        if let Some(mut stage) = self.reframe.take() {
            // SAFETY: the device is live until VkTeardown below; waiting out every submission
            // first means no queued command still reads the stage's images.
            unsafe {
                let _ = self.device.device_wait_idle();
                stage.destroy(&self.device);
            }
        }
        drop(VkTeardown {
            instance: Some(self.instance.clone()),
            device: Some(self.device.clone()),
            vq_dev: Some(self.vq_dev.clone()),
            import_cache: std::mem::take(&mut self.import_cache),
            frames: std::mem::take(&mut self.frames),
            compute_pool: self.compute_pool,
            cmd_pool: self.cmd_pool,
            shader: vk::ShaderModule::null(),
            csc_pipe: self.csc_pipe,
            csc_layout: self.csc_layout,
            csc_pool: self.csc_pool,
            csc_dsl: self.csc_dsl,
            sampler: self.sampler,
            dpb_views: std::mem::take(&mut self.dpb_views),
            dpb_image: self.dpb_image,
            dpb_mem: self.dpb_mem,
            params: self.params,
            session: self.session,
            session_mem: std::mem::take(&mut self.session_mem),
        });
    }
}

// The host's crop and scale ahead of the CSC.
#[path = "vk_reframe.rs"]
mod reframe_stage;

// Construction + parameter-set builders. `#[path]` child sees this file's private items.
#[path = "vk_build.rs"]
mod build;
use self::build::{
    align_up, build_parameters_av1, build_parameters_h265, make_frame, make_video_image,
    probe_rgb_direct, probe_yuv_storage_planes, rgb_model_for,
};

#[cfg(test)]
mod tests {
    use super::{build_h265_rps_s0, intra_refresh_caps, parse_rgb_request, VulkanVideoEncoder};
    use crate::{Codec, Encoder};
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

    /// The latch needs every gate at once; the 780M's measured caps pass, each missing one fails.
    #[test]
    fn intra_refresh_latch_needs_every_gate() {
        use crate::vk_intra_refresh as vir;
        let good = vir::VideoEncodeIntraRefreshCapabilitiesKHR {
            s_type: vir::stype(vir::ST_CAPABILITIES),
            p_next: std::ptr::null_mut(),
            intra_refresh_modes: vir::MODE_BLOCK_BASED
                | vir::MODE_BLOCK_ROW_BASED
                | vir::MODE_BLOCK_COLUMN_BASED,
            max_intra_refresh_cycle_duration: 256,
            max_intra_refresh_active_reference_pictures: 1,
            partition_independent_intra_refresh_regions: ash::vk::TRUE,
            non_rectangular_intra_refresh_regions: ash::vk::FALSE,
        };
        assert_eq!(
            intra_refresh_caps(true, true, &good).map(|c| c.max_cycle),
            Ok(256)
        );
        assert!(intra_refresh_caps(false, true, &good).is_err());
        assert!(intra_refresh_caps(true, false, &good).is_err());
        let mut c = vir::VideoEncodeIntraRefreshCapabilitiesKHR { ..good };
        c.intra_refresh_modes = vir::MODE_BLOCK_COLUMN_BASED;
        assert!(intra_refresh_caps(true, true, &c).is_err());
        let mut c = vir::VideoEncodeIntraRefreshCapabilitiesKHR { ..good };
        c.partition_independent_intra_refresh_regions = ash::vk::FALSE;
        assert!(intra_refresh_caps(true, true, &c).is_err());
        let mut c = vir::VideoEncodeIntraRefreshCapabilitiesKHR { ..good };
        c.max_intra_refresh_cycle_duration = 1;
        assert!(intra_refresh_caps(true, true, &c).is_err());
    }

    /// Full-retention RPS: every resident listed, setup occupant excluded, `used_by_curr_pic`
    /// marks only the real reference.
    #[test]
    fn h265_rps_retains_all_residents() {
        // Slots hold POCs 8..15, current 16, reconstructing over POC 8, referencing POC 15.
        let slot_poc = [8i32, 9, 10, 11, 12, 13, 14, 15];
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 0, 15, 16);
        assert_eq!(n, 7, "all residents except the dying setup occupant");
        // Newest-first cumulative deltas: POCs 15,14,...,9 → every step is 1.
        assert_eq!(&deltas[..7], &[0u16; 7], "delta_minus1 chain of 1-steps");
        assert_eq!(used, 1 << 0, "only the newest (POC 15) is actively used");

        // Recovery: reference an older picture (POC 12) while newer residents stay listed.
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 0, 12, 16);
        assert_eq!(n, 7);
        assert_eq!(used, 1 << 3, "POC 12 is 4th-newest → S0 index 3");
        assert_eq!(&deltas[..7], &[0u16; 7]);

        // Sparse DPB after IDR: only POCs 0..2 resident.
        let slot_poc = [0i32, 1, 2, -1, -1, -1, -1, -1];
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 3, 2, 3);
        assert_eq!(n, 3);
        assert_eq!(&deltas[..3], &[0, 0, 0]);
        assert_eq!(used, 1 << 0);

        // Non-adjacent POCs: current 10, residents {9, 6, 2} → deltas-minus1 {0, 2, 3}.
        let slot_poc = [2i32, -1, 6, -1, 9, -1, -1, -1];
        let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 7, 6, 10);
        assert_eq!(n, 3);
        assert_eq!(&deltas[..3], &[0, 2, 3]);
        assert_eq!(used, 1 << 1, "POC 6 is the 2nd-newest → S0 index 1");
    }

    fn cpu_frame(w: u32, h: u32, pts_ns: u64, fill: [u8; 4]) -> CapturedFrame {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&fill);
        }
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// BGRX frame of the shared moving texture at `frame` frames of motion
    /// (`pf_encode_win::smoke_pattern`): the encoder must reach for rows above to predict it.
    fn cpu_frame_scroll(w: u32, h: u32, pts_ns: u64, frame: u32) -> CapturedFrame {
        let buf = crate::smoke_pattern::scroll_pattern(w as usize, h as usize, frame as usize);
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    const SMOKE_LOST: usize = 4;
    /// Recovery-anchor index. RFI fires just before this submission; one normal P (frame 5,
    /// referencing lost frame 4) is encoded in between, so a conforming decoder processes that
    /// RPS before the anchor arrives. Frame 3 survives only because every P-frame's RPS lists
    /// all resident DPB pictures ([`build_h265_rps_s0`]).
    const SMOKE_ANCHOR: usize = 6;

    /// Full `open` → IDR → P-frames → RFI-recovery. [`SMOKE_LOST`] is dropped; one P still
    /// references it; [`SMOKE_ANCHOR`] re-anchors on pre-loss frame 3 (no IDR).
    fn run_smoke(codec: Codec) -> Vec<crate::EncodedFrame> {
        run_smoke_opts(codec, false).expect("smoke")
    }

    /// `run_smoke` with RGB-direct explicit. `None` = probe declined (soft-skip).
    fn run_smoke_opts(codec: Codec, rgb: bool) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc =
            VulkanVideoEncoder::open_opts(codec, w, h, 60, 10_000_000, rgb).expect("open");
        if rgb && enc.rgb.is_none() {
            eprintln!("run_smoke_opts: RGB-direct unavailable on this driver — skipping");
            return None;
        }
        assert!(enc.caps().supports_rfi, "must advertise RFI");

        let colors = [
            [40u8, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [200, 200, 40, 255],
            [40, 200, 200, 255],
            [200, 40, 200, 255],
            [120, 200, 80, 255],
            [80, 120, 200, 255],
        ];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            if i == SMOKE_ANCHOR {
                // Next frame must re-anchor on a resident pre-loss reference (newest older = 3).
                assert!(
                    enc.invalidate_ref_frames(SMOKE_LOST as i64, SMOKE_LOST as i64),
                    "RFI should find an older-than-loss slot"
                );
            }
            enc.submit_indexed(&cpu_frame(w, h, i as u64 * 16_666_667, *c), i as u32)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");

        let (mut keyframes, mut anchors) = (0usize, 0usize);
        for (i, au) in aus.iter().enumerate() {
            assert!(!au.data.is_empty(), "AU {i} empty");
            keyframes += au.keyframe as usize;
            anchors += au.recovery_anchor as usize;
            if i == 0 {
                assert!(au.keyframe, "frame 0 must be IDR");
            }
            if i == SMOKE_ANCHOR {
                assert!(
                    au.recovery_anchor && !au.keyframe,
                    "frame {SMOKE_ANCHOR} must be a clean recovery P-frame, not IDR"
                );
            }
        }
        assert_eq!(keyframes, 1, "exactly one IDR (frame 0)");
        assert_eq!(
            anchors, 1,
            "exactly one recovery anchor (frame {SMOKE_ANCHOR})"
        );
        Some(aus)
    }

    /// Dump full stream + client-view with AU [`SMOKE_LOST`] removed. Full stream must decode
    /// 0-error. Dropped dump: one missing-ref at frame 5, none at the anchor (a complaint about
    /// frame 3 means retention regressed).
    fn dump_smoke(aus: &[crate::EncodedFrame], ext: &str) {
        let Ok(home) = std::env::var("HOME") else {
            return;
        };
        let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
        let p1 = format!("{home}/vkenc-host-smoke.{ext}");
        let _ = std::fs::write(&p1, &full);
        eprintln!(
            "run_smoke: wrote {p1} ({} bytes, {} AUs)",
            full.len(),
            aus.len()
        );
        let dropped: Vec<u8> = aus
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != SMOKE_LOST)
            .flat_map(|(_, a)| a.data.iter().copied())
            .collect();
        let p2 = format!("{home}/vkenc-host-smoke-dropped.{ext}");
        let _ = std::fs::write(&p2, &dropped);
        eprintln!(
            "run_smoke: wrote {p2} (frame {SMOKE_LOST} dropped; frame 5 conceals, \
             recovery@{SMOKE_ANCHOR} anchors to frame 3 and must decode clean)"
        );
    }

    /// HEVC smoke. `#[ignore]`d: needs a real `VK_KHR_video_encode_h265` device.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke() {
        dump_smoke(&run_smoke(Codec::H265), "h265");
    }

    /// AV1 smoke. Dumps `.obu` (TD + seq-header prefixes ahead of each frame OBU).
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_av1 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke_av1() {
        dump_smoke(&run_smoke(Codec::Av1), "obu");
    }

    /// RGB-direct (EFC) smoke. Soft-skips where the extension/probe is unavailable.
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
    fn vulkan_smoke_rgb() {
        if let Some(aus) = run_smoke_opts(Codec::H265, true) {
            dump_smoke(&aus, "rgb.h265");
        }
    }

    /// RGB-direct AV1 twin of [`vulkan_smoke_rgb`].
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
    fn vulkan_smoke_rgb_av1() {
        if let Some(aus) = run_smoke_opts(Codec::Av1, true) {
            dump_smoke(&aus, "rgb.obu");
        }
    }

    /// Wave smoke frame plan, 256×256 (4 CTB rows → cycle 4): IDR + 2 P, then an RFI with
    /// every reference tainted, which used to force an IDR and now starts a wave.
    const WAVE_START: usize = 3;
    const WAVE_CYCLE: usize = 4;

    /// The wave replaces the IDR: start mark, plain wave frames, close mark, no IDR, no anchor.
    /// A later loss of the plain P after the wave must re-anchor on the close (a fully swept
    /// picture is trusted) while mid-wave pictures never are.
    fn run_wave_smoke(codec: Codec, ext: &str) {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            VulkanVideoEncoder::open_opts(codec, w, h, 60, 10_000_000, false).expect("open");
        if enc.intra_refresh.is_none() {
            eprintln!("run_wave_smoke: intra refresh unavailable on this driver — skipping");
            return;
        }
        // `PF_WAVE_RESTART=1`: two frames into the wave a frame inside its sweep is lost, so
        // it restarts there with a fresh start mark; only the restarted close may lift.
        let restart = std::env::var("PF_WAVE_RESTART").is_ok_and(|v| v == "1");
        let start2 = if restart { WAVE_START + 2 } else { WAVE_START };
        let close = start2 + WAVE_CYCLE - 1;
        let after_wave = close + 1; // the plain P after the close
        let anchor_p = after_wave + 1; // re-anchors on the close after `after_wave` is lost
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for i in 0..=anchor_p {
            if i == WAVE_START {
                assert!(
                    enc.invalidate_ref_frames(0, WAVE_START as i64 - 1),
                    "a wave-capable encoder answers an RFI with no anchor"
                );
                assert!(
                    enc.wave.is_none(),
                    "the wave starts at frame-build, not here"
                );
            }
            if restart && i == start2 {
                let lost = (i - 1) as i64;
                assert!(
                    enc.invalidate_ref_frames(lost, lost),
                    "a loss inside the sweep"
                );
            }
            if i == anchor_p {
                assert!(
                    enc.invalidate_ref_frames(after_wave as i64, after_wave as i64),
                    "the wave close is a trusted anchor"
                );
            }
            // Scrolling texture: vertical motion makes the encoder reach for rows above,
            // which is what the clean-region constraint has to refuse across a stripe.
            enc.submit_indexed(
                &cpu_frame_scroll(w, h, i as u64 * 16_666_667, i as u32),
                i as u32,
            )
            .expect("submit");
            if i == WAVE_START || i == start2 {
                assert_eq!(
                    enc.wave,
                    Some(crate::rfi::Wave {
                        cycle: WAVE_CYCLE as u32,
                        index: 1
                    }),
                    "frame {i} started a wave at frame-build"
                );
            }
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), anchor_p + 1, "one AU per submitted frame");
        assert!(enc.wave.is_none(), "the wave closed");

        assert!(aus[0].keyframe, "frame 0 is the IDR");
        for (i, au) in aus.iter().enumerate().skip(1) {
            assert!(!au.data.is_empty(), "AU {i} empty");
            assert!(
                !au.keyframe,
                "AU {i}: no IDR after frame 0 — the wave replaced it"
            );
            let start = i == WAVE_START || i == start2;
            assert_eq!(
                au.recovery_point,
                start || i == close,
                "AU {i}: recovery_point marks every start and the close"
            );
            assert_eq!(au.recovery_close, i == close, "AU {i}: the close bit");
            assert_eq!(
                au.recovery_anchor,
                i == anchor_p,
                "AU {i}: the only anchor P answers the post-wave loss"
            );
        }
        // Full stream, and the client's view with the pre-wave P frames lost (and the
        // restart's frame): decoded side by side, the close must match the full decode.
        if let Ok(home) = std::env::var("HOME") {
            let full: Vec<&[u8]> = aus.iter().map(|a| a.data.as_slice()).collect();
            let p = format!("{home}/vkenc-wave-smoke.{ext}");
            let _ = crate::smoke_pattern::write_capture(&p, &full);
            let dropped: Vec<&[u8]> = aus
                .iter()
                .enumerate()
                .filter(|(i, _)| *i == 0 || (*i >= WAVE_START && *i != start2 - 1))
                .map(|(_, a)| a.data.as_slice())
                .collect();
            let p2 = format!("{home}/vkenc-wave-smoke-dropped.{ext}");
            let _ = crate::smoke_pattern::write_capture(&p2, &dropped);
            eprintln!(
                "run_wave_smoke: wrote {p} ({} bytes, {} AUs) and {p2} (frames 1..{} dropped; \
                 the close at {close} must decode identical to the full stream)",
                full.iter().map(|a| a.len()).sum::<usize>(),
                aus.len(),
                WAVE_START,
            );
        }
    }

    /// HEVC wave smoke. Run under `VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation`: the layers
    /// are the only check of the dirty-region bookkeeping, RADV ignores it.
    #[test]
    #[ignore = "needs VK_KHR_video_encode_intra_refresh (RADV >= Mesa 25.3 on VCN hardware)"]
    fn vulkan_wave_smoke() {
        run_wave_smoke(Codec::H265, "h265");
    }

    /// AV1 twin of [`vulkan_wave_smoke`]; the wave start is error-resilient.
    #[test]
    #[ignore = "needs VK_KHR_video_encode_intra_refresh (RADV >= Mesa 25.3 on VCN hardware)"]
    fn vulkan_wave_smoke_av1() {
        run_wave_smoke(Codec::Av1, "obu");
    }

    /// Packed 2:10:10:10 (`xRGB_210LE` / `PixelFormat::X2Rgb10`) CPU frame. Channels are 10-bit
    /// code values (PQ container units).
    fn cpu_frame_rgb10(w: u32, h: u32, pts_ns: u64, rgb10: [u16; 3]) -> CapturedFrame {
        // x:R:G:B 2:10:10:10 LE — B in bits 0-9, G in 10-19, R in 20-29.
        let word = ((rgb10[0] as u32 & 0x3FF) << 20)
            | ((rgb10[1] as u32 & 0x3FF) << 10)
            | (rgb10[2] as u32 & 0x3FF);
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&word.to_le_bytes());
        }
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: PixelFormat::X2Rgb10,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// 10-bit twin of [`run_smoke_opts`]: Main10 / AV1-at-10, `G10X6…3PACK16` picture + DPB.
    /// Colour truth is out of band (`dump_smoke`); in-tree asserts encode + AU count + depth.
    /// `None` = device declined the 10-bit profile (soft skip).
    fn run_smoke_10bit(codec: Codec, rgb: bool) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc =
            match VulkanVideoEncoder::open_opts_depth(codec, w, h, 60, 10_000_000, rgb, true, true)
            {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("run_smoke_10bit({codec:?}, rgb={rgb}): open declined — {e:#}");
                    return None;
                }
            };
        if rgb && enc.rgb.is_none() {
            eprintln!("run_smoke_10bit: BT.2020 RGB-direct unavailable on this driver — skipping");
            return None;
        }
        assert!(enc.ten_bit, "a 10-bit session must report 10-bit");

        // Span the range so a wrong shift shows up as wildly wrong luminance in the dump.
        let colors: [[u16; 3]; 8] = [
            [160, 160, 800],
            [160, 800, 160],
            [800, 160, 160],
            [800, 800, 160],
            [160, 800, 800],
            [800, 160, 800],
            [480, 800, 320],
            [320, 480, 800],
        ];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            enc.submit_indexed(&cpu_frame_rgb10(w, h, i as u64 * 16_666_667, *c), i as u32)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");
        assert!(aus[0].keyframe, "frame 0 must be IDR");
        for (i, au) in aus.iter().enumerate() {
            assert!(!au.data.is_empty(), "AU {i} empty");
        }
        Some(aus)
    }

    /// HEVC Main10 through the compute CSC.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device with a 10-bit profile"]
    fn vulkan_smoke_10bit() {
        if let Some(aus) = run_smoke_10bit(Codec::H265, false) {
            dump_smoke(&aus, "10bit.h265");
        }
    }

    /// AV1 at 10 bits.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_av1 device with a 10-bit profile"]
    fn vulkan_smoke_10bit_av1() {
        if let Some(aus) = run_smoke_10bit(Codec::Av1, false) {
            dump_smoke(&aus, "10bit.obu");
        }
    }

    /// HDR with no host CSC: EFC does BT.2020 conversion. Soft-skips without `MODEL_YCBCR_2020`.
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion with the BT.2020 model at 10-bit"]
    fn vulkan_smoke_rgb_10bit() {
        if let Some(aus) = run_smoke_10bit(Codec::H265, true) {
            dump_smoke(&aus, "rgb.10bit.h265");
        }
    }

    /// 10-bit SDR twin of [`run_smoke_10bit`]: a Main10 / AV1-10 session fed an 8-bit BGRA capture
    /// (`bit_depth == 10`, HDR off) — `rgb2yuv10_709.comp` widens 8→10 under BT.709. `None` = the
    /// device declined the 10-bit profile.
    fn run_smoke_10bit_sdr(codec: Codec) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        // ten_bit + SDR: an 8-bit capture at depth 10. want_rgb=false forces the compute CSC (the
        // 709 10-bit shader), the path a composited-cursor desktop always takes.
        let mut enc = match VulkanVideoEncoder::open_opts_depth(
            codec, w, h, 60, 10_000_000, false, true, false,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("run_smoke_10bit_sdr({codec:?}): open declined — {e:#}");
                return None;
            }
        };
        assert!(enc.ten_bit, "a 10-bit session must report 10-bit");
        let colors = [
            [40u8, 40, 200, 255],
            [40, 200, 40, 255],
            [200, 40, 40, 255],
            [200, 200, 40, 255],
            [40, 200, 200, 255],
            [200, 40, 200, 255],
            [120, 200, 80, 255],
            [80, 120, 200, 255],
        ];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for (i, c) in colors.iter().enumerate() {
            enc.submit_indexed(&cpu_frame(w, h, i as u64 * 16_666_667, *c), i as u32)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");
        assert!(aus[0].keyframe, "frame 0 must be IDR");
        Some(aus)
    }

    /// HEVC Main10 SDR through the compute CSC (BT.709 at ten bits).
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device with a 10-bit profile"]
    fn vulkan_smoke_10bit_sdr() {
        if let Some(aus) = run_smoke_10bit_sdr(Codec::H265) {
            dump_smoke(&aus, "10bit.sdr.h265");
        }
    }

    /// AV1 10-bit SDR — the AMD/Intel path VAAPI cannot serve.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_av1 device with a 10-bit profile"]
    fn vulkan_smoke_10bit_sdr_av1() {
        if let Some(aus) = run_smoke_10bit_sdr(Codec::Av1) {
            dump_smoke(&aus, "10bit.sdr.obu");
        }
    }

    /// 24-bpp packed CPU frame. `rgb` is (r, g, b) regardless of `fmt`'s byte order.
    fn cpu_frame_24(w: u32, h: u32, pts_ns: u64, rgb: [u8; 3], fmt: PixelFormat) -> CapturedFrame {
        let px = match fmt {
            PixelFormat::Rgb => [rgb[0], rgb[1], rgb[2]],
            PixelFormat::Bgr => [rgb[2], rgb[1], rgb[0]],
            _ => unreachable!("24-bpp helper"),
        };
        let mut buf = vec![0u8; (w * h * 3) as usize];
        for p in buf.chunks_exact_mut(3) {
            p.copy_from_slice(&px);
        }
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: fmt,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// 24-bpp CPU session via `normalize_cpu_rgb`. Frames alternate Rgb/Bgr (staging re-key).
    fn run_smoke_cpu24(rgb_direct: bool) -> Option<Vec<crate::EncodedFrame>> {
        let env_dim = |k: &str, d: u32| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
        let mut enc = VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, rgb_direct)
            .expect("open");
        if rgb_direct && enc.rgb.is_none() {
            eprintln!("run_smoke_cpu24: RGB-direct unavailable on this driver — skipping");
            return None;
        }
        let colors: [[u8; 3]; 4] = [[200, 40, 40], [40, 200, 40], [40, 40, 200], [200, 200, 40]];
        let mut aus: Vec<crate::EncodedFrame> = Vec::new();
        for i in 0..8usize {
            let fmt = if i % 2 == 0 {
                PixelFormat::Rgb
            } else {
                PixelFormat::Bgr
            };
            let frame = cpu_frame_24(w, h, i as u64 * 16_666_667, colors[i / 2], fmt);
            enc.submit_indexed(&frame, i as u32).expect("submit 24-bpp");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), 8, "one AU per 24-bpp frame");
        assert!(aus[0].keyframe, "frame 0 must be IDR");
        Some(aus)
    }

    /// 24-bpp CPU frames through the CSC path.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
    fn vulkan_smoke_cpu_rgb24() {
        let aus = run_smoke_cpu24(false).expect("CSC mode never soft-skips");
        if let Ok(home) = std::env::var("HOME") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let p = format!("{home}/vkenc-host-smoke-cpu24.h265");
            let _ = std::fs::write(&p, &full);
            eprintln!("vulkan_smoke_cpu_rgb24: wrote {p} ({} bytes)", full.len());
        }
    }

    /// 24-bpp RGB-direct twin: expanded RGBA/BGRA is the encode source. Soft-skips without VALVE.
    #[test]
    #[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
    fn vulkan_smoke_rgb_cpu24() {
        if let Some(aus) = run_smoke_cpu24(true) {
            if let Ok(home) = std::env::var("HOME") {
                let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
                let p = format!("{home}/vkenc-host-smoke-rgb-cpu24.h265");
                let _ = std::fs::write(&p, &full);
                eprintln!("vulkan_smoke_rgb_cpu24: wrote {p} ({} bytes)", full.len());
            }
        }
    }

    /// CSC refuses a source that doesn't match the session mode (clamped texelFetch would
    /// silently crop/pad). Pins refusal in both directions, and that a refused submit does not
    /// wedge the session (bail is after frame-type bookkeeping).
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device; meaningful only under validation layers"]
    fn vulkan_csc_refuses_a_mismatched_source() {
        let mut enc = VulkanVideoEncoder::open_opts(Codec::H265, 512, 512, 60, 10_000_000, false)
            .expect("open");
        enc.submit_indexed(&cpu_frame(512, 512, 0, [40, 40, 200, 255]), 0)
            .expect("well-sized baseline");
        while enc.poll().expect("poll").is_some() {}
        // Guard is equality on render size, not a ceiling.
        let e = enc
            .submit_indexed(&cpu_frame(128, 128, 16_666_667, [200, 40, 40, 255]), 1)
            .expect_err("smaller source must refuse");
        assert!(e.to_string().contains("mismatched"), "{e:#}");
        let e = enc
            .submit_indexed(&cpu_frame(640, 640, 33_333_334, [200, 40, 40, 255]), 2)
            .expect_err("larger source must refuse");
        assert!(e.to_string().contains("mismatched"), "{e:#}");
        let mut got_au = false;
        for i in 3..11u64 {
            enc.submit_indexed(
                &cpu_frame(512, 512, i * 16_666_667, [40, 200, 40, 255]),
                i as u32,
            )
            .expect("well-sized after refusal");
            while let Ok(Some(_)) = enc.poll() {
                got_au = true;
            }
        }
        assert!(got_au, "no AU after the refused submits — session wedged");
        eprintln!("done — under validation layers this run must report ZERO VUID errors");
    }

    /// A joiner's reframe on an EFC-capable device: the session reopens on the CSC, and the
    /// decoded picture is the crop's red and white halves with none of the blue beside it or
    /// the green above and below.
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device and ffmpeg"]
    fn vulkan_reframe_crops_and_scales() {
        let (w, h) = (256u32, 144u32);
        let (sw, sh) = (768u32, 432u32);
        let mut enc =
            VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, true).expect("open");
        assert!(enc.caps().crops_input && enc.caps().downscales_input);
        enc.set_input_crop([128, 72, 512, 288]).expect("reframe");
        assert!(enc.rgb.is_none(), "the reframe runs on the CSC path");
        // BGRX: green rows outside the crop, blue columns beside it, red then white inside.
        let mut px = vec![0u8; (sw * sh * 4) as usize];
        for y in 0..sh {
            for x in 0..sw {
                let c: [u8; 4] = if !(72..360).contains(&y) {
                    [40, 200, 40, 255]
                } else if !(128..640).contains(&x) {
                    [200, 40, 40, 255]
                } else if x < 384 {
                    [40, 40, 200, 255]
                } else {
                    [235, 235, 235, 255]
                };
                let i = ((y * sw + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&c);
            }
        }
        let mut stream = Vec::new();
        for i in 0..4u64 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: sw,
                height: sh,
                pts_ns: i * 16_666_667,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(px.clone()),
                cursor: None,
            };
            enc.submit_indexed(&frame, i as u32).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
        let path = std::env::temp_dir().join("vkenc-reframe.h265");
        std::fs::write(&path, &stream).expect("write the stream");
        let Ok(out) = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
        else {
            eprintln!("no ffmpeg — skipping the picture check");
            return;
        };
        assert_eq!(
            out.stdout.len(),
            (w * h * 3) as usize,
            "decoded size: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 3) as usize;
            [out.stdout[i], out.stdout[i + 1], out.stdout[i + 2]]
        };
        let near = |got: [u8; 3], want: [u8; 3], what: &str| {
            assert!(
                got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 40),
                "{what}: got {got:?}, want {want:?}"
            );
        };
        near(at(3, h / 2), [200, 40, 40], "left edge is the crop's red");
        near(
            at(w - 4, h / 2),
            [235, 235, 235],
            "right edge is the crop's white",
        );
        near(at(w / 4, 2), [200, 40, 40], "top edge has no green");
        near(
            at(w * 3 / 4, h - 3),
            [235, 235, 235],
            "bottom edge has no green",
        );
    }

    /// Mid-stream [`Encoder::reset`] must not change what `vkCmdBeginVideoCodingKHR` declares.
    /// `reset()` re-arms `first_frame` without rebuilding the session, so CBR is still current
    /// — declaration keys on `rc_installed`. Also stages `reconfigure_bitrate` before the reset
    /// (pending rate survives; begin must still declare the old rate — VUID-...-08254).
    #[test]
    #[ignore = "needs a real VK_KHR_video_encode_h265 device; meaningful only under validation layers"]
    fn vulkan_reset_keeps_the_declared_rate_control_state() {
        let (w, h) = (256u32, 256u32);
        let mut enc =
            VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, false).expect("open");
        eprintln!("phase 1: 4 frames (installs CBR on frame 0)");
        for i in 0..4u64 {
            enc.submit_indexed(
                &cpu_frame(w, h, i * 16_666_667, [40, 40, 200, 255]),
                i as u32,
            )
            .expect("submit");
            while enc.poll().expect("poll").is_some() {}
        }
        eprintln!("phase 2: reconfigure_bitrate() then reset() — the 08254 coincidence");
        // Pending rate must survive reset; next begin must still declare the OLD rate.
        assert!(enc.reconfigure_bitrate(20_000_000), "retarget should stage");
        assert!(enc.reset(), "reset should succeed");
        eprintln!("phase 3: 4 more frames — first one re-declares the OLD rate, installs the NEW");
        for i in 4..8u64 {
            enc.submit_indexed(
                &cpu_frame(w, h, i * 16_666_667, [200, 40, 40, 255]),
                i as u32,
            )
            .expect("submit after reset");
            while enc.poll().expect("poll").is_some() {}
        }
        eprintln!("done — under validation layers this run must report ZERO VUID errors");
    }

    /// `PUNKTFUNK_VULKAN_RGB_DIRECT` accepts the same spellings as every sibling knob, trimmed.
    #[test]
    fn rgb_direct_knob_accepts_the_house_spellings() {
        for on in ["1", "true", "yes", "on", " 1", "1 ", "\ton\n"] {
            assert_eq!(parse_rgb_request(Some(on)), Some(true), "{on:?}");
        }
        for off in ["0", "false", "no", "off", " 0", "0 ", "\toff\n"] {
            assert_eq!(parse_rgb_request(Some(off)), Some(false), "{off:?}");
        }
    }

    /// Unset, empty, and unrecognised values mean default — never a force-on. Anything-but-`"0"`
    /// as force-on made a trailing space on `=0` enable the path the operator was disabling.
    #[test]
    fn rgb_direct_knob_never_force_enables_on_an_unrecognised_value() {
        assert_eq!(parse_rgb_request(None), None);
        for junk in ["", "   ", "2", "maybe", "0x0", "enabled"] {
            assert_eq!(parse_rgb_request(Some(junk)), None, "{junk:?}");
        }
    }
}
