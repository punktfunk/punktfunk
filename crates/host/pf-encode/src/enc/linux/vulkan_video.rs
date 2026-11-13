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

use super::vk_csc::{bind_rgb, csc_layout, csc_push, CursorPlane, LinearSrc, WordsCache};
use super::vk_reframe::Reframe;
use super::vk_ring::{Action, Observation, Ring};
use super::vk_util::{
    color_range, find_mem_preferring, import_failure_feeds_latch, imported_acquire_barrier,
    imported_release_barrier, make_host_buffer, make_plain_image, make_view, pixel_to_vk,
    readback_invalidate_size, reject_dmabuf, ImportCache,
};
use crate::cpu_ingest::CpuIngest;
use crate::env_knobs::Knobs;
use crate::rfi::{
    plan_frame, FramePlan, FrameRef, MidWave, Pending, Recovery, RecoveryPolicy, Wave,
};
use crate::{Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, ensure, Context, Result};
use ash::vk;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use pf_zerocopy::vkdev::{VkBufferAlloc, VkImageAlloc};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::fd::AsFd;

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

/// Whether the producer-planar `format` matches the session depth: NV12 is the 8-bit source,
/// P010 the 10-bit one. A crossed pair names a picture the encoder cannot program.
const fn native_planar_format_matches(format: PixelFormat, ten_bit: bool) -> bool {
    matches!(
        (format, ten_bit),
        (PixelFormat::Nv12, false) | (PixelFormat::P010, true)
    )
}

// RGB→NV12 BT.709 CSC. Source `rgb2yuv.comp`; regenerate with
// `glslangValidator -V rgb2yuv.comp -o rgb2yuv.spv`.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// 10-bit HDR twin (`rgb2yuv10.comp`): 2:10:10:10 PQ/BT.2020 RGB → 10-bit 4:2:0, BT.2020 NCL.
/// Separate module: storage-image FORMAT (`r8`/`rg8` vs `r16`/`rg16`) is layout, not specializable.
const CSC10_SPV: &[u8] = include_bytes!("rgb2yuv10.spv");
/// 10-bit SDR twin (`rgb2yuv10_709.comp`): same 10-bit store as [`CSC10_SPV`], BT.709 matrix. For a
/// Main10 / AV1-10 session that stays SDR — an 8-bit RGB capture widened to a 10-bit 709 stream.
const CSC10_709_SPV: &[u8] = include_bytes!("rgb2yuv10_709.spv");
/// The CSC shaders' `FULL_RANGE` specialization constant: studio range when clear.
const CSC_FULL_RANGE_SPEC_ID: u32 = 0;
/// DPB ring depth (under RADV `maxDpbSlots=17`); also the RFI recovery window.
const DPB_SLOTS: u32 = 8;
/// In-flight captures with GPU work outstanding. 2 overlaps CSC+encode with the next capture;
/// backpressure at the 2nd unread frame. Distinct from `DPB_SLOTS` (reference pool).
const RING_DEFAULT: usize = 2;

/// Encode-thread fence wait ceiling (5 s). Finite because this thread runs the stall watchdog's
/// `reset()`; an unbounded wait deadlocks recovery. Matches the Windows NVENC retrieve-thread budget.
const ENCODE_FENCE_TIMEOUT_NS: u64 = 5_000_000_000;

/// One slice of the fence waiter's wait: how long a settle or `Drop` waits out a fence
/// that never signals.
const FENCE_WAITER_SLICE_NS: u64 = 50_000_000;

/// One frame's phases, as [`PhaseStats`] indexes them.
const PHASE_IMPORT: usize = 0;
const PHASE_SUBMIT: usize = 1;
const PHASE_WAIT: usize = 2;
const PHASE_READ: usize = 3;
const PHASE_NAMES: [&str; 4] = ["import", "submit", "wait", "read"];
/// 100 ms is six frames at 60 Hz: a healthy phase ends under 10 ms, a field stall runs 300–900.
const PHASE_SPIKE: std::time::Duration = std::time::Duration::from_millis(100);
/// One warning per second at most: a run of short stalls would otherwise log every frame.
const PHASE_SPIKE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
/// The capture plane's provenance cadence, so one window reads across both lines.
const PHASE_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The slowest run of each frame phase per report window. The loop's cadence score moves one
/// step per frame and the capture queue supersedes a stalled consumer's frames, so a single
/// 300 ms stall in import, submit, fence wait or read leaves no other trace on the host.
#[derive(Default)]
struct PhaseStats {
    max_us: [u64; 4],
    frames: u64,
    /// Imports that missed [`ImportCache`]: the per-frame churn its depth implies.
    fresh_imports: u64,
    since: Option<std::time::Instant>,
    last_spike: Option<std::time::Instant>,
}

impl PhaseStats {
    /// Fold one phase's duration in. `true` = warn now: at or past [`PHASE_SPIKE`], with no
    /// warning in the last [`PHASE_SPIKE_EVERY`].
    fn note(&mut self, phase: usize, took: std::time::Duration, now: std::time::Instant) -> bool {
        self.since.get_or_insert(now);
        let us = took.as_micros().min(u128::from(u64::MAX)) as u64;
        self.max_us[phase] = self.max_us[phase].max(us);
        if took < PHASE_SPIKE || self.last_spike.is_some_and(|t| now < t + PHASE_SPIKE_EVERY) {
            return false;
        }
        self.last_spike = Some(now);
        true
    }

    /// The window's maxima once [`PHASE_REPORT_EVERY`] has passed, then a fresh window. The
    /// warning's clock carries over, so a stall astride the boundary still warns once.
    fn report(&mut self, now: std::time::Instant) -> Option<PhaseStats> {
        let since = *self.since.get_or_insert(now);
        if now < since + PHASE_REPORT_EVERY {
            return None;
        }
        let last_spike = self.last_spike;
        let done = std::mem::take(self);
        self.since = Some(now);
        self.last_spike = last_spike;
        Some(done)
    }
}
/// AV1 base quantizer (0..=255). CBR overrides per frame; used as the seed and for constant-Q.
const AV1_BASE_Q_IDX: u8 = 128;

/// `VK_KHR_video_encode_intra_refresh` latched at open (see [`intra_refresh_caps`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IntraRefreshCaps {
    /// `maxIntraRefreshCycleDuration`, in frames.
    max_cycle: u32,
    /// The cycle `PUNKTFUNK_IR_PERIOD_FRAMES` pins, read once so a frame does not scan the
    /// environment ([`crate::rfi::pinned_cycle`]).
    pinned: Option<u32>,
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
        pinned: crate::rfi::pinned_cycle(),
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
    /// `srcPictureResource.codedExtent`; see [`OpenSpec::native_nv12`]).
    /// Session/SPS/DPB stay app-aligned.
    true_extent: bool,
}

/// Stack storage for a complete video profile: profile → codec profile → usage, then the VALVE
/// rgb-conversion struct when `rgb`. Profile identity is by value, so the session and every
/// post-open image (dmabuf imports, CPU staging) build theirs here. `wire()` links `p_next` into
/// this struct's own addresses, so the value must not move between `wire()` and the last use of
/// `.profile`.
struct ProfileStack {
    rgb: Option<super::vk_valve_rgb::VideoEncodeProfileRgbConversionInfoVALVE>,
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h265: vk::VideoEncodeH265ProfileInfoKHR<'static>,
    av1: super::vk_av1_encode::VideoEncodeAV1ProfileInfoKHR,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl ProfileStack {
    fn new(codec_op: vk::VideoCodecOperationFlagsKHR, ten_bit: bool, rgb: bool) -> Self {
        use super::vk_av1_encode as av1b;
        use super::vk_valve_rgb as vrgb;
        Self {
            rgb: rgb.then_some(vrgb::VideoEncodeProfileRgbConversionInfoVALVE {
                s_type: vrgb::stype(vrgb::ST_PROFILE_INFO),
                p_next: std::ptr::null(),
                perform_encode_rgb_conversion: vk::TRUE,
            }),
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

    /// Link `p_next` into this value's final address; returns `&self.profile`.
    fn wire(&mut self, av1: bool) -> &vk::VideoProfileInfoKHR<'static> {
        self.usage.p_next = self
            .rgb
            .as_ref()
            .map_or(std::ptr::null(), |r| r as *const _ as *const c_void);
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

/// The producer-native planar source line: sizes, and the firmware padding the driver adds.
fn log_native_nv12_source(native_fmt: PixelFormat, (rw, rh): (u32, u32), (w, h): (u32, u32)) {
    tracing::info!(
        native_nv12 = "active(direct-import)",
        format = ?native_fmt,
        source_width = rw,
        source_height = rh,
        fw_padding_width = w - rw,
        fw_padding_height = h - rh,
        "vulkan-encode: producer-native planar encode source (true-size headers: the \
         driver aligns the bitstream SPS itself and the firmware edge-extends the \
         padding — the source is never read past its extent)"
    );
}

/// The RGB-direct verdict line: active in which form, or why not.
fn log_rgb_direct_source(
    rgb_probe: &Result<(u32, u32), &'static str>,
    want_rgb: bool,
    rgb_cfg: &Option<RgbDirect>,
    rgb_request: Option<bool>,
) {
    tracing::info!(
        rgb_direct = match (rgb_probe, want_rgb, rgb_cfg) {
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
            (Ok(_), false, None) => match rgb_request {
                Some(false) => "available(off: PUNKTFUNK_VULKAN_RGB_DIRECT=0)",
                _ =>
                    "available(off: this session composites the pointer, which EFC \
                     cannot — a cursor channel or a compositor that embeds it restores \
                     RGB-direct; =1 does not)",
            },
            (Err(e), _, None) => e,
            (Ok(_), true, None) => unreachable!("rgb gate and cfg disagree"),
        },
        "vulkan-encode: EFC RGB-direct encode source (design/vulkan-rgb-direct-encode.md)"
    );
}

/// What the encode queue reports for a profile, copied out of the chained query structs so no
/// borrow of them outlives the query.
struct EncodeCaps {
    std_hdr: vk::ExtensionProperties,
    min_bs_align: u64,
    max_quality_levels: u32,
    rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    /// `u64::MAX` when the driver reports no limit.
    hw_max_bitrate: u64,
    /// HEVC CTB and largest transform, log2 ([`h265_block_log2`]).
    h265_blocks: (u8, u8),
    av1_superblock128: bool,
    av1_max_level: vk::native::StdVideoAV1Level,
    /// `codedPictureAlignment`: the grid the driver pads an AV1 frame to.
    av1_coded_align: vk::Extent2D,
    intra_refresh: Option<IntraRefreshCaps>,
}

/// Query `profile`'s encode capabilities on `pd` and log the intra-refresh verdict.
///
/// # Safety
/// `instance`, `vq_inst` and `pd` are live and paired, and `profile` is wired
/// ([`ProfileStack::wire`]).
unsafe fn query_encode_caps(
    instance: &ash::Instance,
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    profile: &vk::VideoProfileInfoKHR,
    av1: bool,
    dev_ext_props: &[vk::ExtensionProperties],
) -> Result<EncodeCaps> {
    use super::vk_av1_encode as av1b;
    use super::vk_intra_refresh as vir;
    let ir_advertised = crate::vk_util::ext_advertised(dev_ext_props, vir::EXTENSION_NAME);

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
    let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, profile, &mut caps);
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
    Ok(EncodeCaps {
        std_hdr,
        min_bs_align,
        max_quality_levels,
        rate_control_modes,
        hw_max_bitrate,
        h265_blocks: h265_block_log2(h265_caps.ctb_sizes, h265_caps.transform_block_sizes),
        av1_superblock128,
        av1_max_level: av1_caps.max_level,
        av1_coded_align: av1_caps.coded_picture_alignment,
        intra_refresh,
    })
}

/// The logical device and the extension loaders every later call goes through.
struct EncodeDevice {
    device: ash::Device,
    encode_queue: vk::Queue,
    compute_queue: vk::Queue,
    ext_fd: ash::khr::external_memory_fd::Device,
    vq_dev: ash::khr::video_queue::Device,
    venc_dev: ash::khr::video_encode_queue::Device,
}

/// Create the device with the extensions and features this session's codec, RGB-direct source,
/// intra refresh and foreign queue family call for.
///
/// # Safety
/// `instance` and `pd` are live and paired; `encode_family` and `compute_family` are queue
/// families of `pd`.
unsafe fn create_encode_device(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    encode_family: u32,
    compute_family: u32,
    av1: bool,
    rgb: bool,
    intra_refresh: bool,
    foreign_ok: bool,
) -> Result<EncodeDevice> {
    use super::vk_av1_encode as av1b;
    use super::vk_intra_refresh as vir;
    use super::vk_valve_rgb as vrgb;
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
    if rgb {
        dev_exts.push(vrgb::EXTENSION_NAME.as_ptr());
    }
    if intra_refresh {
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
    let mut sync2 = vk::PhysicalDeviceSynchronization2Features::default().synchronization2(true);
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
        av1_features.p_next = device_ci.p_next.cast_mut();
        device_ci.p_next = &av1_features as *const _ as *const c_void;
    }
    // Spec: must be enabled to chain the rgb profile.
    let mut rgb_features = vrgb::PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
        s_type: vrgb::stype(vrgb::ST_PHYSICAL_DEVICE_FEATURES),
        p_next: std::ptr::null_mut(),
        video_encode_rgb_conversion: vk::TRUE,
    };
    if rgb {
        rgb_features.p_next = device_ci.p_next.cast_mut();
        device_ci.p_next = &rgb_features as *const _ as *const c_void;
    }
    // Spec: the feature must be enabled before a session declares an intra refresh mode.
    let mut ir_features = vir::PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR {
        s_type: vir::stype(vir::ST_PHYSICAL_DEVICE_FEATURES),
        p_next: std::ptr::null_mut(),
        video_encode_intra_refresh: vk::TRUE,
    };
    if intra_refresh {
        ir_features.p_next = device_ci.p_next.cast_mut();
        device_ci.p_next = &ir_features as *const _ as *const c_void;
    }
    let device = instance
        .create_device(pd, &device_ci, None)
        .context("create device")?;
    let encode_queue = device.get_device_queue(encode_family, 0);
    let compute_queue = device.get_device_queue(compute_family, 0);
    let ext_fd = ash::khr::external_memory_fd::Device::new(instance, &device);
    let vq_dev = ash::khr::video_queue::Device::new(instance, &device);
    let venc_dev = ash::khr::video_encode_queue::Device::new(instance, &device);
    Ok(EncodeDevice {
        device,
        encode_queue,
        compute_queue,
        ext_fd,
        vq_dev,
        venc_dev,
    })
}

/// Allocate and bind the memory `session` asks for. Each allocation lands in `owned` before the
/// bind, so an early return leaves it to the caller's teardown.
///
/// # Safety
/// `device` and `vq_dev` are the live device `mem_props` describes, and `session` was created on
/// them and has no memory bound.
unsafe fn bind_session_memory(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    session: vk::VideoSessionKHR,
    owned: &mut Vec<vk::DeviceMemory>,
) -> Result<()> {
    let get_mem = vq_dev.fp().get_video_session_memory_requirements_khr;
    let mut n = 0u32;
    let _ = get_mem(device.handle(), session, &mut n, std::ptr::null_mut());
    let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); n as usize];
    let _ = get_mem(device.handle(), session, &mut n, reqs.as_mut_ptr());
    let mut binds = Vec::new();
    for rq in &reqs {
        let mr = rq.memory_requirements;
        let ti = find_mem_preferring(
            mem_props,
            mr.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let m = device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(mr.size)
                .memory_type_index(ti),
            None,
        )?;
        owned.push(m);
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
    Ok(())
}

/// Build the RGB to YUV compute pipeline. Every shader writes `full_range` or studio through its
/// `FULL_RANGE` specialization constant. Each handle is mirrored into `guard` as it is created;
/// the shader module is destroyed once the pipeline holds it.
///
/// # Safety
/// `device` is the live device `guard` tears down.
unsafe fn build_csc_pipeline(
    device: &ash::Device,
    guard: &mut VkTeardown,
    ten_bit: bool,
    is_hdr: bool,
    full_range: bool,
) -> Result<(vk::DescriptorSetLayout, vk::PipelineLayout, vk::Pipeline)> {
    // 8-bit is 709; 10-bit splits by colour — BT.2020 PQ for HDR, BT.709 for SDR-10.
    let spv = ash::util::read_spv(&mut std::io::Cursor::new(match (ten_bit, is_hdr) {
        (true, true) => CSC10_SPV,
        (true, false) => CSC10_709_SPV,
        (false, _) => CSC_SPV,
    }))?;
    let shader =
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
    guard.shader = shader;
    let (csc_dsl, csc_layout) = csc_layout(device)?;
    guard.csc_dsl = csc_dsl;
    guard.csc_layout = csc_layout;
    // `FULL_RANGE` is constant 0, a VkBool32.
    let range = vk::Bool32::from(full_range).to_ne_bytes();
    let range_entry = [vk::SpecializationMapEntry::default()
        .constant_id(CSC_FULL_RANGE_SPEC_ID)
        .offset(0)
        .size(range.len())];
    let spec = vk::SpecializationInfo::default()
        .map_entries(&range_entry)
        .data(&range);
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(shader)
        .name(c"main")
        .specialization_info(&spec);
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
    Ok((csc_dsl, csc_layout, csc_pipe))
}

/// First physical device with a `VIDEO_ENCODE` queue family advertising `codec_op` (llvmpipe
/// advertises none) whose driver [`super::vk_util::encode_trusted`] accepts. Shared by
/// [`VulkanVideoEncoder::open_inner`] and [`probe_encode_caps`].
///
/// # Safety
/// `instance` must be a live `ash::Instance` and `devices` handles enumerated from it.
unsafe fn find_encode_device(
    instance: &ash::Instance,
    devices: &[vk::PhysicalDevice],
    codec_op: vk::VideoCodecOperationFlagsKHR,
) -> Option<(vk::PhysicalDevice, u32)> {
    for &pd in devices {
        if !super::vk_util::encode_trusted(&instance.get_physical_device_properties(pd)) {
            continue;
        }
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
    /// The pairs the device's EFC converts; `None` without an EFC.
    pub efc: Option<crate::vcn_firmware::EfcPairs>,
    /// The device's VCN firmware; `None` off amdgpu.
    pub vcn: Option<crate::vcn_firmware::VcnFirmware>,
}

/// Probe [`VulkanEncodeCaps`] for `codec`, uncached: the resolution keeps the answer. Depth
/// answers come from `vkGetPhysicalDeviceVideoCapabilitiesKHR` against a profile at that depth,
/// the same query session open makes. `None` when the probe could not ask (the instance did not
/// come up, or no device list); no loader, no driver and no encode queue are answers.
pub(crate) fn probe_encode_caps(codec: Codec) -> Option<VulkanEncodeCaps> {
    if !matches!(codec, Codec::H265 | Codec::Av1) {
        return Some(VulkanEncodeCaps::default());
    }
    let av1 = codec == Codec::Av1;
    let codec_op = codec_op_for(av1);
    // SAFETY: creates one Vulkan instance, issues only physical-device queries, and destroys it
    // on every path before returning — no handle derived from it escapes.
    // `Entry::load` only dlopens the loader (missing libvulkan → `Err`).
    // `find_encode_device` / `depth_supported` get that live instance and the device it returned.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return Some(VulkanEncodeCaps::default());
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let instance = match entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) {
            Ok(instance) => instance,
            // The loader found no driver: this box has no Vulkan.
            Err(vk::Result::ERROR_INCOMPATIBLE_DRIVER) => {
                return Some(VulkanEncodeCaps::default());
            }
            Err(_) => return None,
        };
        let found = match instance.enumerate_physical_devices() {
            Ok(devices) => {
                Some(find_encode_device(&instance, &devices, codec_op).map(|(pd, _)| pd))
            }
            Err(_) => None,
        };
        let caps = match found {
            Some(Some(pd)) => {
                let vq_inst = ash::khr::video_queue::Instance::new(&entry, &instance);
                let eight_bit = depth_supported(&vq_inst, pd, codec_op, av1, false);
                Some(VulkanEncodeCaps {
                    supported: true,
                    eight_bit,
                    ten_bit: depth_supported(&vq_inst, pd, codec_op, av1, true),
                    efc: build::probe_efc_pairs(&instance, &vq_inst, pd, codec_op, av1, !eight_bit),
                    vcn: build::vcn_firmware_of(&instance, pd),
                })
            }
            Some(None) => Some(VulkanEncodeCaps::default()),
            None => None,
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
    let mut ps = ProfileStack::new(codec_op, ten_bit, false);
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

/// One-plane `DrmFormatModifierPropertiesEXT` entries for `fmt`, driver order.
///
/// # Safety
/// `instance`/`pd` must be live and paired.
unsafe fn one_plane_modifiers(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    fmt: vk::Format,
) -> Vec<u64> {
    pf_zerocopy::vkdev::format_modifiers(instance, pd, fmt)
        .into_iter()
        // Capture hands one fd/offset/stride.
        .filter(|p| p.drm_format_modifier_plane_count == 1)
        .map(|p| p.drm_format_modifier)
        .collect()
}

/// [`pf_zerocopy::vkdev::modifier_importable`] under the single video `profile`, when given.
///
/// # Safety
/// `instance`/`pd` must be live and paired; `profile` must be a fully-wired profile.
unsafe fn modifier_importable(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    fmt: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    profile: Option<&vk::VideoProfileInfoKHR>,
) -> bool {
    let list =
        profile.map(|p| vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(p)));
    pf_zerocopy::vkdev::modifier_importable(instance, pd, fmt, modifier, usage, list.as_ref())
}

/// DRM modifiers the encode device accepts for a `fourcc` capture dmabuf in every
/// packed import mode the runtime can create: SAMPLED (compute CSC), TRANSFER_SRC
/// (padded-copy fallback), and VIDEO_ENCODE_SRC under the session's RGB profile
/// at `ten_bit`; packed RGB needs the last two only on a device with EFC. Driver
/// order, LINEAR excluded, deduplicated. Unknown fourcc, or
/// any loader/instance/device/query failure, is an empty list — the offer stays
/// LINEAR rather than failing capture.
pub(crate) fn vulkan_capture_modifiers(codec: Codec, fourcc: u32, ten_bit: bool) -> Vec<u64> {
    if !matches!(codec, Codec::H265 | Codec::Av1) {
        return Vec::new();
    }
    let Some(fmt) = pf_zerocopy::drm::vk_format(fourcc) else {
        return Vec::new();
    };
    let av1 = codec == Codec::Av1;
    let codec_op = codec_op_for(av1);
    // SAFETY: fresh instance, physical-device queries only, destroyed on every path;
    // nothing derived from it escapes.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return Vec::new();
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let Ok(instance) = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) else {
            return Vec::new();
        };
        // Same pick as `open_inner`: capture allocates against the device that encodes.
        let pd = instance
            .enumerate_physical_devices()
            .ok()
            .and_then(|devices| find_encode_device(&instance, &devices, codec_op))
            .map(|(pd, _)| pd);
        let mods = pd
            .map(|pd| {
                // The probe must name the profile the import will use: planar fourccs
                // (NV12/P010) go in under the native profile, packed RGB under EFC conversion.
                let packed = !matches!(fmt, NV12 | P010);
                let mut ps = ProfileStack::new(codec_op, ten_bit, packed);
                let encode_profile = *ps.wire(av1);
                // Packed RGB is an encode source only under EFC. Without the extension the
                // runtime samples it for the compute CSC, and the two RGB-direct legs would
                // refuse every tiled modifier that path could take.
                let efc = instance
                    .enumerate_device_extension_properties(pd)
                    .is_ok_and(|exts| {
                        crate::vk_util::ext_advertised(&exts, crate::vk_valve_rgb::EXTENSION_NAME)
                    });
                let all_legs = [
                    (vk::ImageUsageFlags::SAMPLED, None),
                    (vk::ImageUsageFlags::TRANSFER_SRC, None),
                    (
                        vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
                        Some(&encode_profile),
                    ),
                ];
                let legs = if packed && !efc {
                    &all_legs[..1]
                } else {
                    &all_legs[..]
                };
                let mut accepted: Vec<u64> = Vec::new();
                for m in one_plane_modifiers(&instance, pd, fmt) {
                    // LINEAR is appended by the capture offer, never probed here.
                    if m == 0 || accepted.contains(&m) {
                        continue;
                    }
                    let importable = |&(usage, profile): &(_, _)| {
                        modifier_importable(&instance, pd, fmt, m, usage, profile)
                    };
                    if legs.iter().all(importable) {
                        accepted.push(m);
                    }
                }
                accepted
            })
            .unwrap_or_default();
        instance.destroy_instance(None);
        mods
    }
}

/// How `begin_encode_cmd` must acquire this frame's encode-source image.
#[derive(Clone, Copy, PartialEq)]
enum SrcAcquire {
    /// CSC path: `nv12` was written by this frame's compute batch (GENERAL layout; the
    /// csc_sem orders the queues).
    CscGeneral,
    /// First use of a DMA-BUF imported directly as the video source: acquire from the foreign
    /// producer (UNDEFINED preserves modifier-backed bytes) with a FOREIGN→encode-family transfer.
    DmabufFresh,
    /// Cached direct-source import: re-acquired from the foreign family out of GENERAL, the
    /// layout the previous release left it in. The producer's rewrite is fenced by
    /// `Frame::src_hold`, not by this barrier.
    DmabufCached,
    /// RGB-direct CPU upload: the compute queue copied the staging buffer in (semaphore
    /// ordered); transition TRANSFER_DST → VIDEO_ENCODE_SRC.
    CpuUpload,
}

/// Persistently mapped base of a slot's `bs` memory (HOST_VISIBLE|HOST_COHERENT, mapped once).
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

/// The slot a P-frame's picture overwrites: round robin from `enc_count`, past the reference
/// and the two [`crate::rfi::Kept`] pictures a late loss anchors on.
fn setup_slot(
    enc_count: u64,
    ref_slot: usize,
    slot_wire: &[i64],
    kept: &crate::rfi::Kept,
) -> usize {
    let n = slot_wire.len();
    (0..n)
        .map(|k| (enc_count as usize + k) % n)
        .find(|&s| s != ref_slot && !(slot_wire[s] >= 0 && kept.holds(slot_wire[s])))
        .expect("a DPB larger than the reference and the two kept pictures")
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
    bs: VkBufferAlloc,
    bs_ptr: BsPtr,
    /// False when `bs` sits in cached, non-coherent memory: `read_slot` invalidates first.
    bs_coherent: bool,
    // Unaligned RGB-direct only ([`RgbDirect::padded`]): ALIGNED BGRA encode-src.
    pad: VkImageAlloc,
    csc_set: vk::DescriptorSet, // Y/UV fixed; binding 0 (RGB) rewritten each use
    y: VkImageAlloc,
    uv: VkImageAlloc,
    /// CSC output and this frame's encode source. Format is [`yuv_format`] (NV12 or P010).
    nv12: VkImageAlloc,
    /// The CSC writes `nv12`'s planes through `y.view`/`uv.view`; `y`/`uv` own no image then.
    direct_planes: bool,
    // CPU staging, keyed on (format, width, height) — not format alone. CSC sizes the image
    // to the SOURCE frame; a format-only key copies past the allocation on a size change.
    cpu_img: Option<(VkImageAlloc, vk::Format, u32, u32)>,
    /// The upload buffer, its size and its mapping, kept until `vkFreeMemory` unmaps it.
    cpu_stage: Option<(VkBufferAlloc, u64, *mut u8)>,
    cursor: CursorPlane,
    pts_ns: u64,
    keyframe: bool,
    recovery_anchor: bool,
    recovery_point: bool,
    recovery_close: bool,
    /// Deferred-requeue hold cloned at submit, dropped when the fence signals. Extends
    /// "producer must not rewrite" across the async GPU read; the host's clone dies at the
    /// next capture, which with a ring of 2 is before this slot finishes.
    src_hold: Option<pf_frame::FrameHold>,
    /// The dmabuf this slot reads while it is in flight, by [`ImportCache`] key.
    src_key: Option<(u64, u64)>,
}

/// What a session opens for. [`VulkanVideoEncoder::open`] resolves it once and the session keeps
/// it, so a reopen restates only what changes.
#[derive(Clone)]
struct OpenSpec {
    codec: Codec,
    /// Source size before alignment: AV1 render_size, the HEVC conformance window.
    width: u32,
    height: u32,
    fps: u32,
    bitrate: u64,
    /// RGB-direct (EFC) if [`probe_rgb_direct`] also passes.
    want_rgb: bool,
    /// Producer supplied native NV12/P010. Encodes the imported visible-size buffer directly:
    /// native sessions use TRUE-SIZE headers so RADV programs firmware padding and the source
    /// is never read past its extent. CSC/RGB paths keep app-aligned SPS (coded extent 64×16);
    /// an undersized direct source on those paths is an OOB-read class.
    native_nv12: bool,
    /// The negotiated producer-planar format — `Nv12`, or `P010` at ten bits. Meaningful only
    /// under `native_nv12`; the submit gate compares the frame's format and FourCC against it.
    native_fmt: PixelFormat,
    /// 10-bit session (HDR or 10-bit SDR). Every profile chain rebuilt after `open` must present
    /// the same depth.
    ten_bit: bool,
    /// HDR colour (BT.2020 PQ) vs BT.709 (SDR at either depth). Independent of `ten_bit`: a 10-bit
    /// SDR session has `ten_bit` set and `is_hdr` clear. A rebuilt session must keep the colour.
    is_hdr: bool,
    /// The range the CSC or EFC writes and the VUI or OBU states; `false` is studio.
    full_range: bool,
    /// The captured format as the RGB-direct picture format.
    src_rgb_fmt: vk::Format,
    /// The env knobs the session opened with.
    knobs: Knobs,
}

pub struct VulkanVideoEncoder {
    _entry: ash::Entry,
    instance: ash::Instance,
    /// The physical device `device` was created on.
    pd: vk::PhysicalDevice,
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

    /// What this session was opened for. A reopen keeps it and changes only the source.
    spec: OpenSpec,

    session: vk::VideoSessionKHR,
    session_mem: Vec<vk::DeviceMemory>,
    params: vk::VideoSessionParametersKHR,
    // HEVC = VPS/SPS/PPS; AV1 = temporal-delimiter OBU + sequence-header OBU.
    header: Vec<u8>,
    // Empty for HEVC; AV1 = a temporal-delimiter OBU (Vulkan emits only the frame OBU).
    frame_prefix: Vec<u8>,
    // HDR10 static metadata keyframes carry in-band (HEVC SEI, AV1 metadata OBUs).
    hdr_meta: Option<pf_frame::HdrMeta>,

    /// The DPB array image; its per-layer views are `dpb_views`, so `dpb.view` is null.
    dpb: VkImageAlloc,
    dpb_views: Vec<vk::ImageView>,
    slot_wire: Vec<i64>, // wire index per slot (-1 = empty) — RFI/loss domain
    slot_poc: Vec<i32>,  // HEVC POC per slot — reference-delta domain
    prev_slot: usize,
    /// The newest frame the client confirmed, while the host holds confirmed references.
    reference_floor: Option<crate::Acked>,
    /// What the client confirmed it decoded clean, whatever the link.
    confirmed: Option<crate::Acked>,
    /// The two pictures [`setup_slot`] never overwrites: a late loss's anchor.
    kept: crate::rfi::Kept,

    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    import_cache: ImportCache,
    /// LINEAR sources this device reads as words, beside `import_cache`'s images.
    words: WordsCache,

    frames: Vec<Frame>,
    /// Which slot records next and which slots the GPU still owes.
    ring: Ring,
    /// Pipelined retrieve (`set_pipelined`): `poll` probes the oldest fence instead of
    /// waiting on it, and `fence_waiter` tells the loop when it signals.
    pipelined: bool,
    /// The loop's AU wake ([`Encoder::set_au_wake`]), carried across a reopen.
    au_wake: crate::AuWake,
    /// Waits each pipelined slot's fence and rings `au_wake`. Spawned by the first pipelined
    /// submit; `Drop` joins it before the device goes.
    fence_waiter: Option<crate::DoneWaiter<vk::Fence>>,
    bs_size: u64,
    /// `nonCoherentAtomSize`: `read_slot` rounds a non-coherent invalidate up to it.
    bs_atom: u64,
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
    /// Resolved quality level ([`Knobs::vulkan_quality`], clamped). First-frame
    /// `ENCODE_QUALITY_LEVEL` and session-parameters must match.
    quality_level: u32,
    /// `PUNKTFUNK_PERF` timestamp period (ns). 0.0 = disabled or unsupported.
    ts_period_ns: f64,
    perf_at: std::time::Instant,
    /// The window's slowest import, submit, wait and read, with the stall warning's clock.
    phases: PhaseStats,
    /// Lays out CPU payloads for the staging buffer; its scratch is reused across frames.
    cpu_ingest: CpuIngest,
    /// RGB-direct (EFC) config. `Some` ⇒ picture format is BGRA, VCN does CSC; `None` ⇒ compute
    /// CSC. Fixed per session (picture format is baked into the video session).
    rgb: Option<RgbDirect>,
    /// The host's crop and scale ahead of the CSC ([`Encoder::set_input_crop`]). CSC sessions only.
    reframe: Option<Reframe>,
    /// Row-based intra refresh is enabled on the session and its device. `None` = unavailable.
    intra_refresh: Option<IntraRefreshCaps>,
    /// Wave in flight, if any. Set at frame-build, read by the record paths for that same
    /// frame, advanced in `post_submit_bookkeeping`. An IDR or an anchor P abandons it.
    wave: Option<Wave>,

    /// Rate not yet installed. Next `record_submit` emits `ENCODE_RATE_CONTROL` (or folds it
    /// into the first frame's RESET+RC), then promotes it into `bitrate` — which must keep
    /// naming the session's current state (every begin-coding declares it).
    pending_bitrate: Option<u64>,

    // Coded extent: `spec`'s size aligned to 64×16.
    width: u32,
    height: u32,
    /// The AV1 frame size the bitstream states ([`av1_coded_frame`]). Unused for HEVC.
    av1_frame: vk::Extent2D,
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
    /// A cursor-blend session that may run on EFC while no cursor reaches it
    /// ([`Self::switch_for_cursor`]). False pins whatever `open` chose.
    cursor_switch: bool,
    /// This mode passed the EFC probe; without it a switch to EFC would reopen for nothing.
    rgb_capable: bool,
    /// First cursorless submit since the CSC last blended; EFC after [`EFC_AFTER_CURSORLESS`].
    cursorless_since: Option<std::time::Instant>,
}

/// How long a CSC session must see no cursor before it reopens on EFC. A pointer that blinks
/// or hides for a moment must not cost an IDR each way.
const EFC_AFTER_CURSORLESS: std::time::Duration = std::time::Duration::from_secs(2);

// SAFETY: the encoder is used only from the single encode thread; all Vulkan handles are owned and
// never shared. Matches `NvencCudaEncoder`'s `unsafe impl Send`.
unsafe impl Send for VulkanVideoEncoder {}

impl VulkanVideoEncoder {
    /// Open a session. `cursor_blend` means the encoder may receive cursor bitmaps to composite.
    /// EFC cannot blend, so those sessions default to CSC; everywhere else RGB-direct is the
    /// default wherever the probe passes. `PUNKTFUNK_VULKAN_RGB_DIRECT` overrides both ways
    /// (see [`Knobs::vulkan_rgb_direct`]).
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        cursor_blend: bool,
        // Negotiated depth. A 10-bit SDR session may capture an 8-bit surface, so `format` is
        // not 10-bit and the depth must come from here.
        bit_depth: u8,
        // The session's colour: BT.2020 PQ, else BT.709. Not read off `format`: gamescope's
        // P010 and packed 10-bit are SDR too.
        hdr: bool,
        // The session's range. A producer's NV12 or P010 arrives at it from capture's pod.
        full_range: bool,
        knobs: &Knobs,
    ) -> Result<Self> {
        // A producer's own planar picture: NV12 at eight bits, P010 at ten.
        let native_nv12 = matches!(format, PixelFormat::Nv12 | PixelFormat::P010);
        // Dispatcher already consulted `probe_encode_caps`; the profile query inside open re-checks.
        let is_hdr = hdr;
        // Depth: HDR, or a 10-bit SDR session (BT.709) on an 8-bit or a 10-bit capture.
        let ten_bit = is_hdr || bit_depth >= 10;
        // Native planar is NV12 at eight bits or P010 at ten — a crossed pair (e.g. P010 bytes
        // on an 8-bit session) describes no source this encoder can program.
        ensure!(
            !native_nv12 || native_planar_format_matches(format, ten_bit),
            "vulkan-encode (native planar): {format:?} does not match session depth \
             (ten_bit={ten_bit})"
        );
        // RGB-direct needs the captured format as the session picture format. BGRA default is
        // only for CPU-only layouts, which never reach that arm.
        let src_rgb_fmt = pixel_to_vk(format).unwrap_or(vk::Format::B8G8R8A8_UNORM);
        // Cursor-blend outranks an explicit RGB-direct pin: EFC cannot composite, and
        // negotiation promised the client a composited pointer.
        if cursor_blend && knobs.vulkan_rgb_direct == Some(true) {
            tracing::info!(
                "PUNKTFUNK_VULKAN_RGB_DIRECT=1 ignored for this session — it composites the \
                 pointer, which the EFC front-end cannot; using the compute-CSC path"
            );
        }
        let want_rgb = !native_nv12 && !cursor_blend && knobs.vulkan_rgb_direct.unwrap_or(true);
        let mut enc = Self::open_opts_inner(OpenSpec {
            codec,
            width,
            height,
            fps,
            bitrate: bitrate_bps,
            want_rgb,
            native_nv12,
            native_fmt: format,
            ten_bit,
            is_hdr,
            full_range,
            src_rgb_fmt,
            knobs: knobs.clone(),
        })?;
        enc.cursor_switch = cursor_blend
            && !native_nv12
            && knobs.vulkan_rgb_direct != Some(false)
            && enc.rgb_capable;
        Ok(enc)
    }

    /// Move a cursor-blend session between the CSC (which blends) and EFC (which cannot) by
    /// what this frame carries: a visible cursor needs the CSC now; EFC returns after
    /// [`EFC_AFTER_CURSORLESS`] without one. Each move reopens the session: one IDR.
    fn switch_for_cursor(&mut self, frame: &CapturedFrame) -> Result<()> {
        if !self.cursor_switch || self.reframe.is_some() {
            return Ok(());
        }
        let cursor = frame.cursor.as_ref().is_some_and(|c| c.visible);
        if cursor {
            self.cursorless_since = None;
            if self.rgb.is_some() {
                self.reopen(false)?;
            }
            return Ok(());
        }
        if self.rgb.is_some() {
            return Ok(());
        }
        let since = *self
            .cursorless_since
            .get_or_insert_with(std::time::Instant::now);
        if since.elapsed() >= EFC_AFTER_CURSORLESS {
            self.cursorless_since = None;
            self.reopen(true)?;
        }
        Ok(())
    }

    /// Reopen the session on EFC (`rgb`) or the CSC, keeping what the caller already set.
    /// Finished AUs carry over; the new session starts on an IDR. A reopen that fails (video
    /// memory while sessions overlap) keeps the current session and stops switching.
    fn reopen(&mut self, rgb: bool) -> Result<()> {
        self.flush()?;
        let t0 = std::time::Instant::now();
        let opened = Self::open_opts_inner(self.respec(rgb));
        let mut next = match opened {
            Ok(next) => next,
            Err(e) => {
                tracing::warn!(
                    efc = rgb,
                    error = %format!("{e:#}"),
                    "vulkan-encode: cursor switch could not reopen the session — staying on the \
                     current one for the rest of it"
                );
                self.cursor_switch = false;
                return Ok(());
            }
        };
        if rgb && next.rgb.is_none() {
            // The probe refused EFC on this mode: stay on the CSC for good.
            self.cursor_switch = false;
            return Ok(());
        }
        next.pending = std::mem::take(&mut self.pending);
        next.auto_wire = self.auto_wire;
        next.hdr_meta = self.hdr_meta.take();
        next.pipelined = self.pipelined;
        next.au_wake = self.au_wake.clone();
        next.cursor_switch = true;
        tracing::info!(
            efc = rgb,
            cost_ms = t0.elapsed().as_millis() as u64,
            "vulkan-encode: {} — reopened the session, one IDR",
            if rgb {
                "no cursor to blend, back on EFC"
            } else {
                "a cursor to blend, moving to the compute CSC"
            }
        );
        *self = next;
        Ok(())
    }

    /// This session's spec for a reopen on EFC (`want_rgb`) or the CSC, at the rate it runs now.
    fn respec(&self, want_rgb: bool) -> OpenSpec {
        OpenSpec {
            want_rgb,
            bitrate: self.pending_bitrate.unwrap_or(self.bitrate),
            ..self.spec.clone()
        }
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
        Self::open_opts_inner(OpenSpec {
            codec,
            width,
            height,
            fps,
            bitrate: bitrate_bps,
            want_rgb,
            native_nv12: false,
            native_fmt: PixelFormat::Nv12,
            ten_bit,
            is_hdr,
            full_range: true,
            src_rgb_fmt: if is_hdr {
                vk::Format::A2R10G10B10_UNORM_PACK32
            } else {
                vk::Format::B8G8R8A8_UNORM
            },
            knobs: Knobs::from_env(),
        })
    }

    /// Open `spec` on the first device with an encode queue for its codec.
    fn open_opts_inner(spec: OpenSpec) -> Result<Self> {
        if !matches!(spec.codec, Codec::H265 | Codec::Av1) {
            bail!(
                "vulkan-encode backend supports HEVC + AV1 only (got {:?})",
                spec.codec
            );
        }
        // SAFETY: `open_inner` only issues Vulkan calls whose preconditions it establishes itself
        // (valid instance/device, correctly-chained create-infos); all handles are freshly created
        // here and owned by the returned `Self`. No aliasing or outside invariants are involved.
        unsafe { Self::open_inner(spec) }
    }

    unsafe fn open_inner(spec: OpenSpec) -> Result<Self> {
        // `is_hdr` is the colour axis (BT.2020 PQ vs BT.709), not `hdr`: this fn binds `hdr` to
        // the parameter-set header bytes below.
        let OpenSpec {
            codec,
            width: rw,
            height: rh,
            want_rgb,
            native_nv12,
            native_fmt,
            ten_bit,
            is_hdr,
            full_range,
            src_rgb_fmt,
            ..
        } = spec;
        // Align coded extent to encode granularity (64×16 on RADV). HEVC crops via a
        // conformance window; AV1 signals it via render_size.
        let w = (rw + 63) & !63;
        let h = (rh + 15) & !15;
        let fps = spec.fps.max(1);
        let bitrate = spec.bitrate.max(1_000_000);
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
        // `PUNKTFUNK_PERF`. `ts_period_ns==0.0` keeps timestamp sites off the hot path.
        let ts_period_ns = if pf_host_config::config().perf && compute_ts_bits > 0 {
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
                full_range,
                src_rgb_fmt,
            )
        };
        let rgb_cfg: Option<RgbDirect> = match (&rgb_probe, want_rgb) {
            (Ok((x, y)), true) => {
                let true_extent = !aligned && spec.knobs.vulkan_rgb_true_extent;
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
            log_native_nv12_source(native_fmt, (rw, rh), (w, h));
        } else {
            log_rgb_direct_source(&rgb_probe, want_rgb, &rgb_cfg, spec.knobs.vulkan_rgb_direct);
        }

        // Encode profile, from the same constructor every later image profile uses. A device
        // that cannot encode 10-bit fails the caps query below with
        // VIDEO_PROFILE_FORMAT_NOT_SUPPORTED, and a failed Vulkan open falls back to VAAPI.
        let mut ps = ProfileStack::new(codec_op, ten_bit, rgb_cfg.is_some());
        let profile = *ps.wire(av1);

        // Device extensions, queried once: the intra-refresh caps chain below and the
        // `VK_EXT_queue_family_foreign` decision both read it.
        let dev_ext_props = instance
            .enumerate_device_extension_properties(pd)
            .unwrap_or_default();
        let EncodeCaps {
            std_hdr,
            min_bs_align,
            max_quality_levels,
            rate_control_modes,
            hw_max_bitrate,
            h265_blocks,
            av1_superblock128,
            av1_max_level,
            av1_coded_align,
            intra_refresh,
        } = query_encode_caps(&instance, &vq_inst, pd, &profile, av1, &dev_ext_props)?;
        // Spec: valid levels are 0..maxQualityLevels, ordered fastest→best (`maxQualityLevels >= 1`).
        // RADV sends a VCN preset op only under an explicit level control, so the session
        // installs this level on its first frame.
        let quality_level = spec
            .knobs
            .vulkan_quality
            .unwrap_or(0)
            .min(max_quality_levels.saturating_sub(1));
        tracing::info!(
            quality_level,
            max_quality_levels,
            "vulkan-encode: quality level (0 = fastest preset; PUNKTFUNK_VULKAN_QUALITY overrides)"
        );
        let (rc_mode, vbv_ms, bitrate) = rc_plan(
            rate_control_modes,
            spec.knobs.vulkan_rc.as_deref(),
            fps,
            bitrate,
            hw_max_bitrate,
        );
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
        let EncodeDevice {
            device,
            encode_queue,
            compute_queue,
            ext_fd,
            vq_dev,
            venc_dev,
        } = create_encode_device(
            &instance,
            pd,
            encode_family,
            compute_family,
            av1,
            rgb_cfg.is_some(),
            intra_refresh.is_some(),
            foreign_ok,
        )?;
        guard.device = Some(device.clone());
        guard.vq_dev = Some(vq_dev.clone());

        // AV1 pins max level from caps via a chained create-info.
        let av1_sci = av1b::VideoEncodeAV1SessionCreateInfoKHR {
            s_type: av1b::stype(av1b::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            use_max_level: vk::TRUE,
            max_level: av1_max_level,
        };
        // RGB-direct: picture format is captured RGB; chained create-info selects EFC conversion.
        // DPB stays NV12 (reconstruction is YUV). Built unconditionally; chained only when active.
        let mut rgb_sci = vrgb::VideoEncodeSessionRgbConversionCreateInfoVALVE {
            s_type: vrgb::stype(vrgb::ST_SESSION_CREATE_INFO),
            p_next: std::ptr::null(),
            rgb_model: rgb_model_for(is_hdr),
            rgb_range: rgb_range_for(full_range),
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
        bind_session_memory(
            &device,
            &vq_dev,
            &mem_props,
            session,
            &mut guard.session_mem,
        )?;

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
        // The driver still codes AV1 on its own grid and pads the source to it, so the header
        // the client parses states that size and render_size carries `rw`×`rh`.
        let av1_frame = av1_coded_frame(hdr_w, hdr_h, av1_coded_align);
        let (params, header, frame_prefix) = if av1 {
            build_parameters_av1(
                &device,
                &vq_dev,
                session,
                hdr_w,
                hdr_h,
                av1_frame,
                av1_max_level,
                av1_superblock128,
                quality_level,
                ten_bit,
                is_hdr,
                full_range,
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
                h265_blocks,
                quality_level,
                ten_bit,
                is_hdr,
                full_range,
            )?;
            (p, hdr, Vec::new())
        };
        guard.params = params;

        let mut profile_list =
            vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(&profile));
        let dpb = make_video_image(
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
        guard.dpb = dpb;
        for slot in 0..DPB_SLOTS {
            guard
                .dpb_views
                .push(make_view(&device, dpb.image, yuv_format(ten_bit), slot)?);
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
        let (csc_dsl, csc_layout, csc_pipe) =
            build_csc_pipeline(&device, &mut guard, ten_bit, is_hdr, full_range)?;
        let nframes = spec.knobs.vulkan_inflight.unwrap_or(RING_DEFAULT);
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                // Binding 0 (RGB) + binding 3 (cursor) per set.
                .descriptor_count(2 * nframes as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * nframes as u32),
            // Binding 4: a LINEAR source's words.
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(nframes as u32),
        ];
        let csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(nframes as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        guard.csc_pool = csc_pool;
        guard.words = WordsCache::new(
            &device,
            &mem_props,
            instance.get_physical_device_properties(pd).vendor_id,
        )?;

        let bs_size = align_up(3 * w as u64 * h as u64 + (1 << 16), min_bs_align);
        let limits = instance.get_physical_device_properties(pd).limits;
        let bs_atom = limits.non_coherent_atom_size.max(1);
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
            && spec.knobs.vulkan_direct_planes
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
        let words = std::mem::take(&mut guard.words);
        std::mem::forget(guard);

        Ok(Self {
            _entry: entry,
            instance,
            pd,
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
            spec,
            session,
            session_mem,
            params,
            header,
            frame_prefix,
            hdr_meta: None,
            dpb,
            dpb_views,
            slot_wire: vec![-1; DPB_SLOTS as usize],
            reference_floor: None,
            confirmed: None,
            kept: crate::rfi::Kept::default(),
            slot_poc: vec![-1; DPB_SLOTS as usize],
            prev_slot: 0,
            csc_pipe,
            csc_layout,
            csc_dsl,
            csc_pool,
            sampler,
            import_cache: ImportCache::default(),
            words,
            ring: Ring::new(frames.len()),
            frames,
            bs_size,
            bs_atom,
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
            phases: PhaseStats::default(),
            cpu_ingest: CpuIngest::default(),
            rgb: rgb_cfg,
            reframe: None,
            intra_refresh,
            wave: None,
            pending_bitrate: None,
            width: w,
            height: h,
            av1_frame,
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
            cursor_switch: false,
            rgb_capable: rgb_probe.is_ok(),
            cursorless_since: None,
            pipelined: false,
            au_wake: Default::default(),
            fence_waiter: None,
        })
    }
}

impl VulkanVideoEncoder {
    /// Import a DMA-BUF with usage/profile matching this session's source mode. Native NV12 and
    /// aligned RGB-direct are profiled `VIDEO_ENCODE_SRC`. Padded RGB-direct is transfer-source only.
    unsafe fn import_dmabuf(
        &self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<VkImageAlloc> {
        if self.spec.native_nv12 {
            let mut ps = ProfileStack::new(
                codec_op_for(self.spec.codec == Codec::Av1),
                self.spec.ten_bit,
                false,
            );
            let profile = *ps.wire(self.spec.codec == Codec::Av1);
            let arr = [profile];
            let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
            return super::vk_util::import_rgb_dmabuf_as(
                &self.instance,
                self.pd,
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
                &self.instance,
                self.pd,
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
            let mut ps = ProfileStack::new(
                codec_op_for(self.spec.codec == Codec::Av1),
                self.spec.ten_bit,
                true,
            );
            let profile = *ps.wire(self.spec.codec == Codec::Av1);
            let arr = [profile];
            let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
            super::vk_util::import_rgb_dmabuf_as(
                &self.instance,
                self.pd,
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
                &self.instance,
                self.pd,
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                d,
                cw,
                ch,
            )
        }
    }

    /// `d`'s [`ImportCache`] key: `(st_dev, st_ino)`, or a sentinel on the frame count when
    /// `fstat` fails. `enqueue` stores the same key in the slot, so the cache sees it in flight.
    fn import_key(&self, d: &pf_frame::DmabufFrame) -> (u64, u64) {
        pf_zerocopy::fd_identity(d.fd.as_fd()).unwrap_or((u64::MAX, self.enc_count))
    }

    /// Import a dmabuf through [`ImportCache`]. A deterministic packed import refusal rebuilds
    /// this capture on its safe offer; transient OOM and native NV12 stay out of that sticky
    /// verdict. The lookup is the `import` phase; a miss counts as a fresh import.
    unsafe fn import_cached(
        &mut self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::ImageView, bool)> {
        let key = self.import_key(d);
        let t0 = std::time::Instant::now();
        // Out while `import_dmabuf` borrows `self`. A panic leaks the entries, never frees one.
        let mut cache = std::mem::take(&mut self.import_cache);
        let got = cache.get_or_import(
            &self.device,
            key,
            (cw, ch),
            || {
                let packed = !self.spec.native_nv12;
                let r = self.import_dmabuf(d, cw, ch);
                match &r {
                    Ok(_) if packed => d.health.note_raw_import_ok(),
                    Err(e) if packed && import_failure_feeds_latch(e) => {
                        reject_dmabuf(d, &format!("{e:#}"))
                    }
                    _ => {}
                }
                r
            },
            |k| {
                self.ring
                    .in_flight()
                    .iter()
                    .any(|&s| self.frames[s].src_key == Some(k))
            },
            self.ring.in_flight().len(),
        );
        self.import_cache = cache;
        self.note_phase(PHASE_IMPORT, t0.elapsed());
        if matches!(got, Ok((_, _, true))) {
            self.phases.fresh_imports += 1;
        }
        got
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
        if self.frames[slot].cpu_img.map(|(_, f, iw, ih)| (f, iw, ih)) != Some((fmt, iw, ih)) {
            if let Some((old, ..)) = self.frames[slot].cpu_img.take() {
                old.destroy(&dev);
            }
            let alloc = if self.rgb.is_some() {
                // RGB-direct: uploaded RGB is the encode source — profiled, encode usage, shared
                // with the encode queue (compute only copies in; semaphore orders; CONCURRENT
                // avoids a QFOT).
                let av1 = self.spec.codec == Codec::Av1;
                let mut ps = ProfileStack::new(codec_op_for(av1), self.spec.ten_bit, true);
                let profile = *ps.wire(av1);
                let arr = [profile];
                let mut plist = vk::VideoProfileListInfoKHR::default().profiles(&arr);
                let fams: &[u32] = if self.encode_family == self.compute_family {
                    &[]
                } else {
                    &[self.encode_family, self.compute_family]
                };
                let fams = fams.to_vec();
                let mut alloc = make_video_image(
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
                // `make_video_image` unwinds internally; a view failure here must free the pair.
                alloc.view = match make_view(&dev, alloc.image, fmt, 0) {
                    Ok(v) => v,
                    Err(e) => {
                        alloc.destroy(&dev);
                        return Err(e);
                    }
                };
                alloc
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
            self.frames[slot].cpu_img = Some((alloc, fmt, iw, ih));
        }
        if self.frames[slot]
            .cpu_stage
            .map(|(_, s, _)| s < need)
            .unwrap_or(true)
        {
            if let Some((old, ..)) = self.frames[slot].cpu_stage.take() {
                old.destroy(&dev);
            }
            let buf = make_host_buffer(
                &dev,
                &self.mem_props,
                need,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            // HOST_COHERENT, so a write through the mapping needs no flush.
            let mapped = dev.map_memory(buf.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty());
            let base = mapped.inspect_err(|_| buf.destroy(&dev))?.cast::<u8>();
            self.frames[slot].cpu_stage = Some((buf, need, base));
        }
        let (_, _, base) = self.frames[slot].cpu_stage.unwrap();
        // RGB-direct uploads the image the encoder reads, so an undersized source must be
        // padded (edge duplicate). CSC keeps the raw copy (shader clamps). Guards below must
        // not be hoisted — they are only sound because `self.rgb.is_some()`.
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
            // The rows below index by `(w, h)` while staging is sized from `need`: refuse a
            // mismatch by name, not as a panic in the row copy.
            let dst_len = (w as usize)
                .checked_mul(h as usize)
                .and_then(|n| n.checked_mul(4))
                .filter(|&n| n as u64 == need)
                .context("vulkan-encode (rgb-direct): staging buffer is not the encode extent")?;
            Some((sw, sh, dst_len))
        } else {
            None
        };
        let len = pad.map_or(bytes.len().min(need as usize), |(_, _, dst_len)| dst_len);
        // `base` maps at least `need >= len` bytes. The ring handed out this slot after its
        // last copy out of them retired, so nothing else reads or writes them now.
        let dst = std::slice::from_raw_parts_mut(base, len);
        match pad {
            Some((sw, sh, _)) => {
                let (dw, dh) = (w as usize, h as usize);
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
            None => dst.copy_from_slice(&bytes[..len]),
        }
        Ok(self.frames[slot].cpu_img.unwrap().0.view)
    }

    /// How this session recovers. It places each stripe and owns the dirty state, so a loss
    /// mid-wave restarts the sweep and an anchor or a confirmed reference abandons it. It reads
    /// only what the open latched, so it runs every frame without touching the environment.
    fn recovery_policy(&self) -> RecoveryPolicy {
        RecoveryPolicy {
            // RADV's stripe unit is the 64-px CTB row for HEVC and AV1.
            cycle: self.intra_refresh.map(|ir| {
                crate::rfi::wave_cycle(self.height.div_ceil(64), self.fps, ir.max_cycle, ir.pinned)
            }),
            mid_wave: MidWave::Restart,
            abandons: true,
        }
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
        debug_assert!(!p.is_null(), "bs persistent mapping missing");
        // Only the bytes read below: the AU, not the whole buffer.
        if !f.bs_coherent {
            let size = readback_invalidate_size(off64 + len64, self.bs_atom, self.bs_size);
            dev.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                .memory(f.bs.memory)
                .offset(0)
                .size(size)])?;
        }
        let prefix: &[u8] = if f.keyframe {
            &self.header
        } else {
            &self.frame_prefix
        };
        let mut data = Vec::with_capacity(prefix.len() + len);
        data.extend_from_slice(prefix);
        // Parameter sets / sequence header first, then the HDR volume, then the picture.
        if let Some(m) = self.hdr_meta.filter(|_| f.keyframe && self.spec.is_hdr) {
            match self.spec.codec {
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

    /// Unsignal `sem` with an empty wait-only batch: a compute batch whose encode submit
    /// failed leaves it signalled, and the slot's next encode would signal it again
    /// (VUID-vkQueueSubmit-pSignalSemaphores-00067). A failure here is a lost device,
    /// which `reset` reports.
    unsafe fn consume_signal(&self, sem: vk::Semaphore) {
        let sems = [sem];
        let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let _ = self.device.queue_submit(
            self.encode_queue,
            &[vk::SubmitInfo::default()
                .wait_semaphores(&sems)
                .wait_dst_stage_mask(&stages)],
            vk::Fence::null(),
        );
    }

    /// Steps the ring on an encode submit that failed after `slot`'s compute batch was queued.
    unsafe fn encode_submit_failed(&mut self, slot: usize) {
        for act in self.ring.step(Observation::EncodeSubmitFailed(slot)) {
            let Action::ConsumeSignal(s) = act else {
                unreachable!("vulkan-encode ring: {act:?} after a failed encode submit");
            };
            self.consume_signal(self.frames[s].csc_sem);
        }
    }

    /// Fold `took` into the window; a stall warns with its phase and length.
    fn note_phase(&mut self, phase: usize, took: std::time::Duration) {
        if self.phases.note(phase, took, std::time::Instant::now()) {
            tracing::warn!(
                phase = PHASE_NAMES[phase],
                took_ms = took.as_millis() as u64,
                "vulkan-encode: a frame phase stalled"
            );
        }
    }

    /// The window's slowest phases, on the capture plane's cadence. `submit` covers the record
    /// with its import, so an import stall shows in both.
    fn report_phases(&mut self) {
        let Some(w) = self.phases.report(std::time::Instant::now()) else {
            return;
        };
        tracing::info!(
            import_max_us = w.max_us[PHASE_IMPORT],
            submit_max_us = w.max_us[PHASE_SUBMIT],
            wait_max_us = w.max_us[PHASE_WAIT],
            read_max_us = w.max_us[PHASE_READ],
            frames = w.frames,
            fresh_imports = w.fresh_imports,
            "vulkan-encode phase maxima"
        );
    }

    /// Waits `slot`'s fence for at most [`ENCODE_FENCE_TIMEOUT_NS`]: the `wait` phase. Never
    /// unbounded: this thread is the one that can `reset()` a wedged GPU.
    unsafe fn wait_slot(&mut self, slot: usize) -> Result<(), vk::Result> {
        let t0 = std::time::Instant::now();
        let answer =
            self.device
                .wait_for_fences(&[self.frames[slot].fence], true, ENCODE_FENCE_TIMEOUT_NS);
        self.note_phase(PHASE_WAIT, t0.elapsed());
        answer
    }

    /// Steps the ring on `slot`'s fence answer. A signalled slot is read into `pending` (the
    /// `read` phase, then the window report); any other answer keeps the slot owed and
    /// returns `fail`'s error.
    unsafe fn on_fence(
        &mut self,
        slot: usize,
        answer: Result<(), vk::Result>,
        fail: impl FnOnce(vk::Result) -> anyhow::Error,
    ) -> Result<()> {
        let obs = match answer {
            Ok(()) => Observation::Signalled(slot),
            Err(vk::Result::TIMEOUT) => Observation::TimedOut(slot),
            Err(_) => Observation::Lost(slot),
        };
        for act in self.ring.step(obs) {
            match act {
                Action::Read(s) => {
                    // The fence also covers the source read: the producer may rewrite it now.
                    self.frames[s].src_hold = None;
                    let t0 = std::time::Instant::now();
                    let done = self.read_slot(s)?;
                    self.note_phase(PHASE_READ, t0.elapsed());
                    self.phases.frames += 1;
                    self.pending.push_back(done);
                    self.report_phases();
                }
                Action::KeepInFlight => {}
                Action::Fail(_) => return answer.map_err(fail),
                other => unreachable!("vulkan-encode ring: {other:?} for a fence answer"),
            }
        }
        Ok(())
    }

    /// Records `frame` into the ring's next slot and submits it without waiting: the `submit`
    /// phase, import included. A full ring first reads its oldest slot into `pending`.
    /// Pipelined, the slot's fence goes to the fence waiter.
    unsafe fn enqueue(&mut self, frame: &CapturedFrame, wire: i64) -> Result<()> {
        self.switch_for_cursor(frame)?;
        let slot = 'acquire: loop {
            for act in self.ring.step(Observation::Acquire) {
                match act {
                    Action::Wait(slot) => {
                        let answer = self.wait_slot(slot);
                        self.on_fence(slot, answer, |e| fence_error(slot, e, "submit"))?;
                    }
                    Action::Use(slot) => break 'acquire slot,
                    other => unreachable!("vulkan-encode ring: {other:?} for a new frame"),
                }
            }
        };
        // Encode reads the dmabuf until this slot's fence signals. Taken even if the record
        // fails: an over-hold is harmless, a release while the GPU reads is the race.
        (self.frames[slot].src_hold, self.frames[slot].src_key) = match &frame.payload {
            FramePayload::Dmabuf(d) => (d.hold.clone(), Some(self.import_key(d))),
            _ => (None, None),
        };
        // A frame's plan is spent only once it is submitted: `Return` hands the armed loss and
        // the wave back, so the retry plans the same frame.
        let unspent = (self.pending_loss, self.wave);
        let t0 = std::time::Instant::now();
        let recorded = self.record_submit(slot, frame, wire);
        self.note_phase(PHASE_SUBMIT, t0.elapsed());
        let obs = if recorded.is_ok() {
            Observation::Submitted(slot)
        } else {
            Observation::RecordFailed(slot)
        };
        for act in self.ring.step(obs) {
            match act {
                Action::Return(_) => (self.pending_loss, self.wave) = unspent,
                Action::ForgetCursor(s) => self.frames[s].cursor.forget(),
                other => unreachable!("vulkan-encode ring: {other:?} after a record"),
            }
        }
        if recorded.is_ok() && self.pipelined {
            self.arm_fence(slot);
        }
        recorded
    }

    /// Ring the loop's AU wake once `slot`'s fence signals, from the fence waiter. Without
    /// the thread the AU waits for the loop's next tick, as a probe alone left it.
    fn arm_fence(&mut self, slot: usize) {
        if self.fence_waiter.is_none() {
            let device = self.device.clone();
            // SAFETY: the waiter only waits; `submit` settles a fence before resetting it and
            // `Drop` joins the waiter before the device or any fence is destroyed.
            let wait = move |fence: vk::Fence| unsafe {
                device.wait_for_fences(&[fence], true, FENCE_WAITER_SLICE_NS)
                    != Err(vk::Result::TIMEOUT)
            };
            self.fence_waiter =
                crate::DoneWaiter::spawn("pf-vk-enc-done", wait, self.au_wake.clone());
        }
        if let Some(waiter) = &self.fence_waiter {
            waiter.arm(self.frames[slot].fence);
        }
    }
}

/// A fence error as `enqueue` and `poll` report it: a timeout names the bound.
fn fence_error(slot: usize, e: vk::Result, verb: &str) -> anyhow::Error {
    if e != vk::Result::TIMEOUT {
        return e.into();
    }
    anyhow::anyhow!(
        "vulkan-encode: fence for slot {slot} did not signal within {} ms — GPU or driver \
         wedged; failing the {verb} so the session can reset",
        ENCODE_FENCE_TIMEOUT_NS / 1_000_000
    )
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
            // Only CSC composites (`CursorPlane::prep`); a switching session reaches it on the first
            // cursor. Native NV12 has no blend.
            blends_cursor: (self.rgb.is_none() || self.cursor_switch) && !self.spec.native_nv12,
            // `set_input_crop` moves an EFC session onto the CSC; a producer's NV12 has no RGB stage.
            downscales_input: !self.spec.native_nv12,
            crops_input: !self.spec.native_nv12,
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
        let plan = crate::rfi::plan_slot_recovery(
            &trusted_refs(&self.slot_wire),
            first_frame,
            self.reference_floor.as_ref(),
        );
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

    fn set_reference_floor(&mut self, acked: Option<crate::Acked>) {
        self.reference_floor = acked;
    }

    fn note_confirmed(&mut self, acked: Option<crate::Acked>) {
        self.confirmed = acked;
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
        // Blocking at depth-1 sync. `pipelined` probes: the fence waiter's ring brings the
        // loop back for an unready oldest, so encode latency stops bounding its cadence.
        // `None` = nothing submitted, or (pipelined) the oldest AU is still on the GPU.
        if !self.pending.is_empty() {
            return Ok(self.pending.pop_front());
        }
        let Some(&slot) = self.ring.in_flight().front() else {
            return Ok(None);
        };
        let answer = if self.pipelined {
            // SAFETY: probing a fence owned by this encoder's slot under `&mut self`.
            match unsafe { self.device.get_fence_status(self.frames[slot].fence) } {
                Ok(true) => Ok(()),
                Ok(false) => return Ok(None),
                Err(e) => Err(e),
            }
        } else {
            // SAFETY: waiting a fence owned by this encoder's slot under `&mut self`.
            unsafe { self.wait_slot(slot) }
        };
        // SAFETY: `answer` is this slot's fence; a signalled one means its CSC and encode are
        // complete, so its bitstream is readable.
        unsafe { self.on_fence(slot, answer, |e| fence_error(slot, e, "poll"))? };
        Ok(self.pending.pop_front())
    }

    fn reset(&mut self) -> bool {
        // Bounded wait: `reset()` runs because something looks wedged; an untimed idle would
        // park recovery. Timeout ⇒ no in-place rebuild. (`Drop` still waits unbounded.)
        for act in self.ring.step(Observation::Reset) {
            let Action::WaitAll(slots) = act else {
                unreachable!("vulkan-encode ring: {act:?} for a reset");
            };
            let fences: Vec<vk::Fence> = slots.iter().map(|&s| self.frames[s].fence).collect();
            // SAFETY: every in-flight fence was submitted with its batch; we hold `&mut self`.
            if let Err(e) = unsafe {
                self.device
                    .wait_for_fences(&fences, true, ENCODE_FENCE_TIMEOUT_NS)
            } {
                tracing::error!(
                    error = ?e,
                    "vulkan-encode: in-flight work did not go idle within {} ms — GPU or \
                     driver wedged; in-place rebuild abandoned",
                    ENCODE_FENCE_TIMEOUT_NS / 1_000_000
                );
                return false;
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
        unsafe { self.import_cache.clear(&self.device) };
        // SAFETY: as above: the device is idle and the cache owns each import.
        unsafe { self.words.clear(&self.device) };
        self.ring.step(Observation::Idle);
        self.pending.clear();
        for f in &mut self.frames {
            f.src_hold = None;
        }
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

    /// `poll` switches between a bounded wait and a fence probe. Wind-back is immediate:
    /// the next blocking poll simply drains whatever the ring still holds.
    fn set_pipelined(&mut self, on: bool) -> bool {
        self.pipelined = on;
        self.pipelined
    }

    /// The fence waiter rings it once a pipelined slot's fence signals.
    fn set_au_wake(&mut self, wake: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        self.au_wake.set(wake);
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
            !self.spec.native_nv12,
            "vulkan-encode: a producer's NV12 source cannot be reframed"
        );
        let [_, _, w, h] = rect;
        ensure!(
            w >= self.spec.width && h >= self.spec.height,
            "vulkan-encode: a {w}x{h} crop would upscale into the {}x{} session",
            self.spec.width,
            self.spec.height
        );
        if self.rgb.is_some() {
            // EFC bakes an RGB picture format into the session and the reframe feeds the CSC,
            // so reopen as a CSC session. Called before the first submit: nothing is lost.
            *self = Self::open_opts_inner(self.respec(false))?;
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
            Reframe::new(
                &self.device,
                &self.mem_props,
                rect,
                (self.spec.width, self.spec.height),
                self.frames.len(),
            )?
        });
        tracing::info!(
            crop = ?rect,
            out = ?(self.spec.width, self.spec.height),
            "vulkan-encode: cropping and scaling ahead of the CSC"
        );
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        for act in self.ring.step(Observation::Flush) {
            let Action::Wait(slot) = act else {
                unreachable!("vulkan-encode ring: {act:?} for a flush");
            };
            // SAFETY: waits this slot's own fence, then reads back its own bitstream objects.
            unsafe {
                let answer = self.wait_slot(slot);
                self.on_fence(slot, answer, |e| {
                    anyhow::Error::new(e).context("vulkan-encode: flush fence wait")
                })?;
            }
        }
        Ok(())
    }
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
    import_cache: ImportCache,
    words: WordsCache,
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
    dpb: VkImageAlloc,
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
            import_cache: ImportCache::default(),
            words: WordsCache::default(),
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
            dpb: VkImageAlloc::default(),
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
                self.import_cache.clear(&device);
                self.words.destroy(&device);
                for f in std::mem::take(&mut self.frames) {
                    device.destroy_semaphore(f.csc_sem, None);
                    device.destroy_fence(f.fence, None);
                    device.destroy_query_pool(f.query_pool, None);
                    device.destroy_query_pool(f.ts_pool, None);
                    f.pad.destroy(&device);
                    // Persistently mapped; `vkFreeMemory` implicitly unmaps.
                    f.bs.destroy(&device);
                    for img in [f.y, f.uv, f.nv12] {
                        img.destroy(&device);
                    }
                    if let Some((img, ..)) = f.cpu_img {
                        img.destroy(&device);
                    }
                    if let Some((stage, ..)) = f.cpu_stage {
                        stage.destroy(&device);
                    }
                    f.cursor.destroy(&device);
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
                self.dpb.destroy(&device);
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
        // Joined first: its wait must not outlive the fences or the device.
        drop(self.fence_waiter.take());
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
            words: std::mem::take(&mut self.words),
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
            dpb: self.dpb,
            params: self.params,
            session: self.session,
            session_mem: std::mem::take(&mut self.session_mem),
        });
    }
}

// The per-frame record. `#[path]` child sees this file's private items.
#[path = "vk_record.rs"]
mod record;

// Construction + parameter-set builders. `#[path]` child sees this file's private items.
#[path = "vk_build.rs"]
mod build;
use self::build::{
    align_up, av1_coded_frame, build_parameters_av1, build_parameters_h265, h265_block_log2,
    make_frame, make_video_image, probe_rgb_direct, probe_yuv_storage_planes, rc_plan,
    rgb_model_for, rgb_range_for,
};

#[cfg(test)]
#[path = "vulkan_video_tests.rs"]
mod tests;
