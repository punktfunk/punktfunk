//! Session/frame construction for the Vulkan Video encoder: `make_frame*`, `make_video_image`,
//! `probe_rgb_direct`, and the parameter-set writers (`build_parameters_h265`/`_av1`, the AV1
//! sequence-header OBU). A `#[path]` child of `vulkan_video.rs`, so it sees the parent's private
//! items (`Frame` and friends). Steady-state encode stays in the parent.
//!
//! RGB-direct probe: `design/vulkan-rgb-direct-encode.md`.

// `unsafe_op_in_unsafe_fn` is off here: every call is already inside `unsafe fn` construction.
// Clearing the allow means deleting markers with no caller contract, not wrapping each call.
#![allow(unsafe_op_in_unsafe_fn)]

// `super::*` is the point of the child-module shape. `vk_util` is a crate-root sibling, so
// `crate::` — not the parent-relative `super::` the parent uses.
use super::*;
use crate::vk_util::{
    ext_advertised, find_mem_preferring, find_readback_mem, make_plain_image, make_view,
};
use anyhow::{bail, Result};
use ash::vk;
use pf_zerocopy::vkdev::VkImageAlloc;
use std::ffi::c_void;

pub(super) fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// The AV1 frame the driver codes for a `w`×`h` sequence header: padded up to
/// `codedPictureAlignment`. VCN4 aligns to 64×16, so 1080 rows code as 1088.
pub(super) fn av1_coded_frame(w: u32, h: u32, align: vk::Extent2D) -> vk::Extent2D {
    let up = |v: u32, a: u32| align_up(v.into(), a.max(1).into()) as u32;
    vk::Extent2D {
        width: up(w, align.width),
        height: up(h, align.height),
    }
}

/// The session's rate control as `(mode, (vbv_window_ms, vbv_initial_ms), bitrate)`. VBR
/// (average == max) when the driver advertises it and `rc_env` (`PUNKTFUNK_VULKAN_RC`) is not
/// `cbr`: Vulkan cannot suppress CBR's filler, so the mode is the only lever. CBR keeps the loose
/// (1000, 500) window, since a tighter one starts stuffing earlier. `bitrate` is clamped to the
/// driver's `hw_max`.
pub(super) fn rc_plan(
    modes: vk::VideoEncodeRateControlModeFlagsKHR,
    rc_env: Option<&str>,
    fps: u32,
    bitrate: u64,
    hw_max: u64,
) -> (vk::VideoEncodeRateControlModeFlagsKHR, (u32, u32), u64) {
    use vk::VideoEncodeRateControlModeFlagsKHR as Rc;
    let vbr_advertised = modes.contains(Rc::VBR);
    if !vbr_advertised && !modes.contains(Rc::CBR) {
        // DEFAULT mode needs a no-layer shape this backend does not speak.
        tracing::warn!(
            modes = modes.as_raw(),
            "vulkan-encode: driver advertises neither CBR nor VBR — installing CBR anyway \
             (pre-existing behaviour; may fail validation on this driver)"
        );
    }
    // `vbr` is honoured only when advertised; anything but `cbr` means auto.
    let vbr = vbr_advertised && !rc_env.is_some_and(|v| v.trim().eq_ignore_ascii_case("cbr"));
    let (mode, vbv_ms) = if vbr {
        (Rc::VBR, crate::vbv_window_ms(fps))
    } else {
        (Rc::CBR, (1000, 500))
    };
    tracing::info!(
        rc_mode = if vbr { "VBR (capped at target)" } else { "CBR" },
        vbv_window_ms = vbv_ms.0,
        vbv_initial_ms = vbv_ms.1,
        fps,
        hw_max_bitrate = hw_max,
        "vulkan-encode: rate control (VBR when the driver offers it — CBR must stuff filler \
         on calm content; PUNKTFUNK_VULKAN_RC overrides, PUNKTFUNK_VBV_FRAMES scales the VBR \
         window)"
    );
    if bitrate > hw_max {
        tracing::warn!(
            requested = bitrate,
            cap = hw_max,
            "vulkan-encode: requested bitrate exceeds the driver's maxBitrate — clamping"
        );
    }
    (mode, vbv_ms, bitrate.min(hw_max))
}

/// Probe the RGB-direct encode source (`design/vulkan-rgb-direct-encode.md`): can this device
/// take the captured RGB dmabuf directly, with the VCN EFC doing the CSC, via
/// `VK_VALVE_video_encode_rgb_conversion`?
///
/// `Ok((x_offset, y_offset))` is the chroma-siting bits the session must be created with
/// (preferred available bit per axis). `Err` is the first missing requirement.
///
/// `ten_bit` (depth) + `hdr` (colour) + `full_range` + `src_fmt` describe the planned session:
/// HDR wants the BT.2020 model, SDR the BT.709 one at either depth, each at the session's range
/// ([`rgb_range_for`]), as the VUI and OBU signal; the 10-bit packed-RGB `src_fmt` is HDR's
/// encode source. An EFC that cannot serve the wanted model and range returns `Err` and the
/// session takes the compute CSC.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn probe_rgb_direct(
    instance: &ash::Instance,
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    codec_op: vk::VideoCodecOperationFlagsKHR,
    av1: bool,
    ten_bit: bool,
    hdr: bool,
    full_range: bool,
    src_fmt: vk::Format,
) -> Result<(u32, u32), &'static str> {
    use crate::vk_av1_encode as av1b;
    use crate::vk_valve_rgb as vrgb;
    let Ok(exts) = instance.enumerate_device_extension_properties(pd) else {
        return Err("probe-failed(ext-enum)");
    };
    if !ext_advertised(&exts, vrgb::EXTENSION_NAME) {
        return Err("no-ext(mesa<26.0-or-no-efc)");
    }
    let mut feat = vrgb::PhysicalDeviceVideoEncodeRgbConversionFeaturesVALVE {
        s_type: vrgb::stype(vrgb::ST_PHYSICAL_DEVICE_FEATURES),
        p_next: std::ptr::null_mut(),
        video_encode_rgb_conversion: vk::FALSE,
    };
    let mut f2 = vk::PhysicalDeviceFeatures2 {
        p_next: &mut feat as *mut _ as *mut c_void,
        ..Default::default()
    };
    instance.get_physical_device_features2(pd, &mut f2);
    if feat.video_encode_rgb_conversion == vk::FALSE {
        return Err("no-feature");
    }
    // Caps under the rgb-chained profile: colour math must match the compute CSC (`rgb2yuv.comp`
    // 709 / `rgb2yuv10.comp` 2020 / `rgb2yuv10_709.comp` 709 at ten bits). Depth-only profile here.
    let mut ps = ProfileStack::new(codec_op, ten_bit, true);
    let profile = *ps.wire(av1);
    let mut rgb_caps = vrgb::VideoEncodeRgbConversionCapabilitiesVALVE {
        s_type: vrgb::stype(vrgb::ST_CAPABILITIES),
        p_next: std::ptr::null_mut(),
        rgb_models: 0,
        rgb_ranges: 0,
        x_chroma_offsets: 0,
        y_chroma_offsets: 0,
    };
    let mut h265_caps = vk::VideoEncodeH265CapabilitiesKHR::default();
    let mut av1_caps: av1b::VideoEncodeAV1CapabilitiesKHR = std::mem::zeroed();
    av1_caps.s_type = av1b::stype(av1b::ST_CAPABILITIES);
    let mut enc_caps = vk::VideoEncodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default();
    if av1 {
        av1_caps.p_next = &mut rgb_caps as *mut _ as *mut c_void;
        enc_caps.p_next = &mut av1_caps as *mut _ as *mut c_void;
    } else {
        h265_caps.p_next = &mut rgb_caps as *mut _ as *mut c_void;
        enc_caps.p_next = &mut h265_caps as *mut _ as *mut c_void;
    }
    caps.p_next = &mut enc_caps as *mut _ as *mut c_void;
    let r = (vq_inst.fp().get_physical_device_video_capabilities_khr)(pd, &profile, &mut caps);
    if r != vk::Result::SUCCESS {
        return Err("no-rgb-profile(caps)");
    }
    // Model + range must match the shader. Siting should too: the shader and every decoder use
    // chroma location 0 (x cosited-even, y midpoint), and nothing in the bitstream says
    // otherwise. Prefer that bit per axis, else accept the other rather than lose EFC.
    let pick = |offered: u32, first: u32, second: u32| -> Option<u32> {
        [first, second].into_iter().find(|&bit| offered & bit != 0)
    };
    let (even, mid) = (
        vrgb::CHROMA_OFFSET_COSITED_EVEN,
        vrgb::CHROMA_OFFSET_MIDPOINT,
    );
    let offered = (rgb_caps.rgb_models, rgb_caps.rgb_ranges);
    let want = (rgb_model_for(hdr), rgb_range_for(full_range));
    if !efc_serves(offered, want, is_radv(instance, pd)) {
        return Err(match (hdr, full_range) {
            (true, true) => "no-2020-full",
            (true, false) => "no-2020-narrow",
            (false, true) => "no-709-full",
            (false, false) => "no-709-narrow",
        });
    }
    let (Some(x_offset), Some(y_offset)) = (
        pick(rgb_caps.x_chroma_offsets, even, mid),
        pick(rgb_caps.y_chroma_offsets, mid, even),
    ) else {
        return Err("no-chroma-siting");
    };
    // Encode-src under this profile must offer `src_fmt` with DRM-modifier tiling.
    // LINEAR XR24 imports as B8G8R8A8_UNORM; XR30/XB30 as packed 2:10:10:10.
    let profile_arr = [profile];
    let plist = vk::VideoProfileListInfoKHR::default().profiles(&profile_arr);
    let mut fmt_info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR);
    fmt_info.p_next = &plist as *const _ as *const c_void;
    let get_fmt = vq_inst.fp().get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    let r = get_fmt(pd, &fmt_info, &mut count, std::ptr::null_mut());
    if r != vk::Result::SUCCESS || count == 0 {
        return Err("no-rgb-format");
    }
    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    let r = get_fmt(pd, &fmt_info, &mut count, props.as_mut_ptr());
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return Err("no-rgb-format");
    }
    if !props[..count as usize]
        .iter()
        .any(|p| p.format == src_fmt && p.image_tiling == vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
    {
        return Err(if src_fmt == vk::Format::B8G8R8A8_UNORM {
            "no-bgra-modifier-tiling"
        } else {
            "no-rgb10-modifier-tiling"
        });
    }
    Ok((x_offset, y_offset))
}

/// A view of `aspect` that serves only `usage`. A view inherits the image's whole usage, and
/// the picture format has no `STORAGE` while its plane formats have no encode input. A plane
/// view needs the image's `MUTABLE_FORMAT`.
unsafe fn make_usage_view(
    device: &ash::Device,
    image: vk::Image,
    fmt: vk::Format,
    aspect: vk::ImageAspectFlags,
    usage: vk::ImageUsageFlags,
) -> Result<vk::ImageView> {
    let mut view_usage = vk::ImageViewUsageCreateInfo::default().usage(usage);
    Ok(device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .push_next(&mut view_usage)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(aspect)
                    .level_count(1)
                    .layer_count(1),
            ),
        None,
    )?)
}

/// The direct-planes picture's whole usage: the encode reads it, the CSC writes its planes.
/// Nothing copies into it. The probe asks the driver for exactly this.
const DIRECT_PLANES_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR.as_raw() | vk::ImageUsageFlags::STORAGE.as_raw(),
);

/// Can the compute CSC write the encode source's planes directly? True when this profile
/// lists `pic` for [`DIRECT_PLANES_USAGE`] with the create flags a plane view needs. A driver
/// that lists it and still misrenders is what `PUNKTFUNK_VULKAN_DIRECT_PLANES=0` is for.
pub(super) unsafe fn probe_yuv_storage_planes(
    vq_inst: &ash::khr::video_queue::Instance,
    pd: vk::PhysicalDevice,
    profile: &vk::VideoProfileInfoKHR,
    pic: vk::Format,
) -> bool {
    let profile_arr = [*profile];
    let plist = vk::VideoProfileListInfoKHR::default().profiles(&profile_arr);
    let mut fmt_info =
        vk::PhysicalDeviceVideoFormatInfoKHR::default().image_usage(DIRECT_PLANES_USAGE);
    fmt_info.p_next = &plist as *const _ as *const c_void;
    let get_fmt = vq_inst.fp().get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    if get_fmt(pd, &fmt_info, &mut count, std::ptr::null_mut()) != vk::Result::SUCCESS || count == 0
    {
        return false;
    }
    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    let r = get_fmt(pd, &fmt_info, &mut count, props.as_mut_ptr());
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return false;
    }
    planes_writable(&props[..count as usize], pic)
}

/// Pure half of [`probe_yuv_storage_planes`].
pub(super) fn planes_writable(props: &[vk::VideoFormatPropertiesKHR], pic: vk::Format) -> bool {
    let flags = vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE;
    props.iter().any(|p| {
        p.format == pic
            && p.image_tiling == vk::ImageTiling::OPTIMAL
            && p.image_usage_flags.contains(vk::ImageUsageFlags::STORAGE)
            && p.image_create_flags.contains(flags)
    })
}

#[cfg(test)]
mod direct_planes_tests {
    use super::*;

    fn listed(
        usage: vk::ImageUsageFlags,
        flags: vk::ImageCreateFlags,
    ) -> vk::VideoFormatPropertiesKHR<'static> {
        vk::VideoFormatPropertiesKHR::default()
            .format(NV12)
            .image_tiling(vk::ImageTiling::OPTIMAL)
            .image_usage_flags(usage)
            .image_create_flags(flags)
    }

    /// Storage on the picture format plus both create flags, or the scratch path stays.
    #[test]
    fn direct_planes_need_storage_and_the_view_flags_on_the_picture_format() {
        let full = vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE;
        let src = vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR;
        assert!(planes_writable(
            &[listed(src | vk::ImageUsageFlags::STORAGE, full)],
            NV12
        ));
        assert!(
            !planes_writable(&[listed(src, full)], NV12),
            "no storage usage"
        );
        assert!(
            !planes_writable(
                &[listed(
                    src | vk::ImageUsageFlags::STORAGE,
                    vk::ImageCreateFlags::MUTABLE_FORMAT
                )],
                NV12
            ),
            "no extended usage"
        );
        assert!(
            !planes_writable(&[listed(src | vk::ImageUsageFlags::STORAGE, full)], P010),
            "other format"
        );
        assert!(!planes_writable(&[], NV12));
        assert!(
            planes_writable(&[listed(DIRECT_PLANES_USAGE, full)], NV12),
            "the usage the direct picture is created with"
        );
    }
}

/// Whether an EFC offering `(models, ranges)` converts the `(model, range)` a session wants.
///
/// RADV offers 709 narrow alone for VCN firmware that converts 709 narrow and 2020 full and
/// nothing else: two masks cannot name two pairs. It hands the session's pair to the firmware
/// unchecked, so 2020 full is served there too.
pub(super) fn efc_serves(offered: (u32, u32), want: (u32, u32), radv: bool) -> bool {
    use crate::vk_valve_rgb as vrgb;
    let two_pairs = radv && offered == (vrgb::MODEL_YCBCR_709, vrgb::RANGE_NARROW);
    (offered.0 & want.0 != 0 && offered.1 & want.1 != 0)
        || (two_pairs && want == (vrgb::MODEL_YCBCR_2020, vrgb::RANGE_FULL))
}

/// Whether Mesa RADV drives `pd`.
unsafe fn is_radv(instance: &ash::Instance, pd: vk::PhysicalDevice) -> bool {
    let mut driver = vk::PhysicalDeviceDriverProperties::default();
    let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut driver);
    instance.get_physical_device_properties2(pd, &mut props);
    driver.driver_id == vk::DriverId::MESA_RADV
}

/// EFC colour model for this session (2020 HDR / 709 SDR, at either depth) — the same matrices the
/// compute-CSC shaders use, so encode-src and SPS/sequence-header colour signalling stay
/// interchangeable. Keyed on colour, not depth: 10-bit SDR is BT.709 (`rgb2yuv10_709.comp`).
pub(super) fn rgb_model_for(hdr: bool) -> u32 {
    use crate::vk_valve_rgb as vrgb;
    if hdr {
        vrgb::MODEL_YCBCR_2020
    } else {
        vrgb::MODEL_YCBCR_709
    }
}

/// EFC range for the session's range: narrow is studio, as the compute CSC writes it with
/// `FULL_RANGE` clear.
pub(super) fn rgb_range_for(full_range: bool) -> u32 {
    use crate::vk_valve_rgb as vrgb;
    if full_range {
        vrgb::RANGE_FULL
    } else {
        vrgb::RANGE_NARROW
    }
}

pub(super) unsafe fn make_video_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    layers: u32,
    usage: vk::ImageUsageFlags,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    concurrent: &[u32],
) -> Result<VkImageAlloc> {
    make_video_image_flags(
        device,
        mp,
        fmt,
        w,
        h,
        layers,
        usage,
        vk::ImageCreateFlags::empty(),
        profile_list,
        concurrent,
    )
}

/// [`make_video_image`] with image create flags (`MUTABLE_FORMAT` for plane views).
/// Memory prefers `DEVICE_LOCAL` and takes any type the driver offers otherwise.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn make_video_image_flags(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    layers: u32,
    usage: vk::ImageUsageFlags,
    flags: vk::ImageCreateFlags,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    concurrent: &[u32],
) -> Result<VkImageAlloc> {
    let mut ci = vk::ImageCreateInfo::default()
        .flags(flags)
        .image_type(vk::ImageType::TYPE_2D)
        .format(fmt)
        .extent(vk::Extent3D {
            width: w,
            height: h,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(layers)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(profile_list);
    if concurrent.len() >= 2 {
        ci = ci
            .sharing_mode(vk::SharingMode::CONCURRENT)
            .queue_family_indices(concurrent);
    } else {
        ci = ci.sharing_mode(vk::SharingMode::EXCLUSIVE);
    }
    let img = device.create_image(&ci, None)?;
    let req = device.get_image_memory_requirements(img);
    // Destroy the image if alloc fails: callers only ever see the completed alloc, view null.
    let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let mem = match find_mem_preferring(mp, req.memory_type_bits, local).and_then(|ti| {
        Ok(device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ti),
            None,
        )?)
    }) {
        Ok(m) => m,
        Err(e) => {
            device.destroy_image(img, None);
            return Err(e);
        }
    };
    let alloc = VkImageAlloc {
        image: img,
        memory: mem,
        view: vk::ImageView::null(),
    };
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        alloc.destroy(device);
        return Err(e.into());
    }
    Ok(alloc)
}

/// Build one in-flight frame's private resources into `f`. `profile_list`/`profile` are
/// borrowed only during creation.
///
/// `f` is a [`Frame::default`] already parked in the caller's [`VkTeardown`] guard, so every
/// handle is owned by the unwind the moment it exists.
pub(super) unsafe fn make_frame(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fams: &[u32],
    profile: &vk::VideoProfileInfoKHR,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,
    bs_size: u64,
    sampler: vk::Sampler,
    with_ts: bool,
    csc: bool,
    pad_fmt: Option<vk::Format>,
    hdr: bool,
    direct_planes: bool,
    f: &mut Frame,
) -> Result<()> {
    // Padded-copy staging: aligned encode-src filled by a transfer blit each frame.
    // TRANSFER_SRC: `record_pad_blit` self-copies the last visible column.
    if let Some(fmt) = pad_fmt {
        f.pad = make_video_image(
            device,
            mem_props,
            fmt,
            w,
            h,
            1,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::TRANSFER_SRC,
            profile_list,
            fams,
        )?;
        f.pad.view = make_view(device, f.pad.image, fmt, 0)?;
    }
    // RGB-direct skips CSC: encode-src is the imported RGB (or lazy CPU staging).
    // Frame keeps the null handles; teardown treats them as empty.
    if csc {
        make_frame_csc(
            device,
            mem_props,
            w,
            h,
            fams,
            profile_list,
            csc_dsl,
            csc_pool,
            sampler,
            hdr,
            direct_planes,
            f,
        )?;
    }
    make_frame_common(
        device,
        mem_props,
        profile,
        profile_list,
        cmd_pool,
        compute_pool,
        bs_size,
        with_ts,
        f,
    )
}

/// CSC half of [`make_frame`]: the NV12/P010 encode-src, what the CSC writes (its planes, or
/// Y/UV scratch copied in afterwards), cursor, descriptors.
#[allow(clippy::too_many_arguments)]
unsafe fn make_frame_csc(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fams: &[u32],
    profile_list: &mut vk::VideoProfileListInfoKHR,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,
    hdr: bool,
    direct_planes: bool,
    f: &mut Frame,
) -> Result<()> {
    let pic = yuv_format(hdr);
    // The CSC's plane formats, size-compatible with the picture planes. 10-bit ycbcr planes
    // are not storage formats, so R16/RG16 and `rgb2yuv10.comp` writes the value into the
    // high bits.
    let (y_fmt, uv_fmt) = if hdr {
        (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM)
    } else {
        (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM)
    };
    f.direct_planes = direct_planes;
    if direct_planes {
        // The CSC writes the picture's planes through plane views: no scratch, no copies.
        // MUTABLE_FORMAT allows a plane view; EXTENDED_USAGE lets STORAGE be a view-format
        // capability the picture format itself lacks.
        f.nv12 = make_video_image_flags(
            device,
            mem_props,
            pic,
            w,
            h,
            1,
            DIRECT_PLANES_USAGE,
            vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE,
            profile_list,
            fams,
        )?;
        f.nv12.view = make_usage_view(
            device,
            f.nv12.image,
            pic,
            vk::ImageAspectFlags::COLOR,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR,
        )?;
        let storage = vk::ImageUsageFlags::STORAGE;
        f.y.view = make_usage_view(
            device,
            f.nv12.image,
            y_fmt,
            vk::ImageAspectFlags::PLANE_0,
            storage,
        )?;
        f.uv.view = make_usage_view(
            device,
            f.nv12.image,
            uv_fmt,
            vk::ImageAspectFlags::PLANE_1,
            storage,
        )?;
    } else {
        f.nv12 = make_video_image(
            device,
            mem_props,
            pic,
            w,
            h,
            1,
            vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST,
            profile_list,
            fams,
        )?;
        f.nv12.view = make_view(device, f.nv12.image, pic, 0)?;
        // Scratch planes, copied into the picture after the CSC (`vkCmdCopyImage` needs equal
        // texel-block size).
        f.y = make_plain_image(
            device,
            mem_props,
            y_fmt,
            w,
            h,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        f.uv = make_plain_image(
            device,
            mem_props,
            uv_fmt,
            w / 2,
            h / 2,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
    }
    // View and descriptor stay bound; only the image content changes.
    f.cursor = CursorPlane::new(device, mem_props)?;
    // Y/UV storage fixed; binding 0 (RGB) is rewritten per use. Binding 3 is the static cursor
    // (SHADER_READ_ONLY once prepped).
    let dsls = [csc_dsl];
    f.csc_set = device.allocate_descriptor_sets(
        &vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(csc_pool)
            .set_layouts(&dsls),
    )?[0];
    let y_info = [vk::DescriptorImageInfo::default()
        .image_view(f.y.view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let uv_info = [vk::DescriptorImageInfo::default()
        .image_view(f.uv.view)
        .image_layout(vk::ImageLayout::GENERAL)];
    let cur_info = [vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(f.cursor.img.view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    device.update_descriptor_sets(
        &[
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&y_info),
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&uv_info),
            vk::WriteDescriptorSet::default()
                .dst_set(f.csc_set)
                .dst_binding(3)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&cur_info),
        ],
        &[],
    );
    Ok(())
}

/// Mode-independent half of [`make_frame`]: bitstream buffer (persistently mapped), feedback
/// query, optional timestamp pool, command buffers, sync objects.
#[allow(clippy::too_many_arguments)]
unsafe fn make_frame_common(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    profile: &vk::VideoProfileInfoKHR,
    profile_list: &mut vk::VideoProfileListInfoKHR,
    cmd_pool: vk::CommandPool,
    compute_pool: vk::CommandPool,
    bs_size: u64,
    with_ts: bool,
    f: &mut Frame,
) -> Result<()> {
    f.bs.buffer = device.create_buffer(
        &vk::BufferCreateInfo::default()
            .size(bs_size)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .push_next(profile_list),
        None,
    )?;
    let bs_req = device.get_buffer_memory_requirements(f.bs.buffer);
    let (bs_type, bs_coherent) = find_readback_mem(mem_props, bs_req.memory_type_bits)?;
    f.bs_coherent = bs_coherent;
    f.bs.memory = device.allocate_memory(
        &vk::MemoryAllocateInfo::default()
            .allocation_size(bs_req.size)
            .memory_type_index(bs_type),
        None,
    )?;
    device.bind_buffer_memory(f.bs.buffer, f.bs.memory, 0)?;
    // Map once for the slot's lifetime; `read_slot` copies AUs out of it, invalidating first
    // when the type is not coherent. `vkFreeMemory` unmaps at teardown.
    f.bs_ptr =
        BsPtr(
            device.map_memory(f.bs.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
                as *const u8,
        );
    // Two timestamps bracket this slot's compute batch (CSC split).
    if with_ts {
        f.ts_pool = device.create_query_pool(
            &vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(2),
            None,
        )?;
    }
    let mut fb_ci = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default().encode_feedback_flags(
        vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
            | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
    );
    fb_ci.p_next = profile as *const _ as *const c_void;
    let mut query_ci = vk::QueryPoolCreateInfo::default()
        .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
        .query_count(1);
    query_ci.p_next = &fb_ci as *const _ as *const c_void;
    f.query_pool = device.create_query_pool(&query_ci, None)?;
    f.cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(1),
    )?[0];
    f.compute_cmd = device.allocate_command_buffers(
        &vk::CommandBufferAllocateInfo::default()
            .command_pool(compute_pool)
            .command_buffer_count(1),
    )?[0];
    f.csc_sem = device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?;
    f.fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
    Ok(())
}

/// The HEVC CTB and largest transform block as log2 sizes: the largest each that the driver
/// offers, the transform no larger than the CTB. VCN codes 64×64 CTBs, NVIDIA only 32×32. A
/// driver that fills in neither mask gets 64 and 32.
pub(super) fn h265_block_log2(
    ctb: vk::VideoEncodeH265CtbSizeFlagsKHR,
    tb: vk::VideoEncodeH265TransformBlockSizeFlagsKHR,
) -> (u8, u8) {
    use vk::VideoEncodeH265CtbSizeFlagsKHR as Ctb;
    use vk::VideoEncodeH265TransformBlockSizeFlagsKHR as Tb;
    let ctb_log2 = [(Ctb::TYPE_64, 6), (Ctb::TYPE_32, 5), (Ctb::TYPE_16, 4)]
        .into_iter()
        .find(|&(f, _)| ctb.contains(f))
        .map_or(6, |(_, l)| l);
    let tb_log2 = [
        (Tb::TYPE_32, 5),
        (Tb::TYPE_16, 4),
        (Tb::TYPE_8, 3),
        (Tb::TYPE_4, 2),
    ]
    .into_iter()
    .find(|&(f, l)| l <= ctb_log2 && tb.contains(f))
    .map_or(5.min(ctb_log2), |(_, l)| l);
    (ctb_log2, tb_log2)
}

/// Author VPS/SPS/PPS (Main/Main10, low-latency, conformance-window crop) and return the
/// session-parameters object plus the encoded header bytes for keyframes.
pub(super) unsafe fn build_parameters_h265(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    venc_dev: &ash::khr::video_encode_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    rw: u32,
    rh: u32,
    // `h265_block_log2` of the driver's caps.
    (ctb_log2, tb_log2): (u8, u8),
    quality_level: u32,
    // Depth (Main vs Main10). Must match the profile the session was created with
    // (`open_inner`'s `ten_bit`); a Main SPS on a Main10 session mislabels the samples.
    ten_bit: bool,
    // Colour (BT.709 vs BT.2020 PQ), independent of depth: 10-bit SDR is Main10 under BT.709.
    hdr: bool,
    // The range the CSC or EFC writes; `false` is studio.
    full_range: bool,
) -> Result<(vk::VideoSessionParametersKHR, Vec<u8>)> {
    use ash::vk::native as hh;
    let mut ptl: hh::StdVideoH265ProfileTierLevel = std::mem::zeroed();
    ptl.flags.set_general_progressive_source_flag(1);
    ptl.flags.set_general_frame_only_constraint_flag(1);
    ptl.general_profile_idc = if ten_bit {
        hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
    } else {
        hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
    };
    ptl.general_level_idc = hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0;

    let mut dpbm: hh::StdVideoH265DecPicBufMgr = std::mem::zeroed();
    dpbm.max_dec_pic_buffering_minus1[0] = (DPB_SLOTS - 1) as u8;
    dpbm.max_num_reorder_pics[0] = 0;
    dpbm.max_latency_increase_plus1[0] = 0;

    let mut vps: hh::StdVideoH265VideoParameterSet = std::mem::zeroed();
    vps.flags.set_vps_temporal_id_nesting_flag(1);
    vps.flags.set_vps_sub_layer_ordering_info_present_flag(1);
    vps.pDecPicBufMgr = &dpbm;
    vps.pProfileTierLevel = &ptl;

    let mut sps: hh::StdVideoH265SequenceParameterSet = std::mem::zeroed();
    sps.flags.set_sps_temporal_id_nesting_flag(1);
    sps.flags.set_sps_sub_layer_ordering_info_present_flag(1);
    sps.chroma_format_idc = hh::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420;
    sps.pic_width_in_luma_samples = w;
    sps.pic_height_in_luma_samples = h;
    sps.log2_max_pic_order_cnt_lsb_minus4 = 4;
    // Main10: `bit_depth_*_minus8 = 2`. Left 0 (8-bit) for Main.
    if ten_bit {
        sps.bit_depth_luma_minus8 = 2;
        sps.bit_depth_chroma_minus8 = 2;
    }
    // Minimum coding block 8×8, minimum transform 4×4; the hierarchy may split down to it.
    sps.log2_diff_max_min_luma_coding_block_size = ctb_log2 - 3;
    sps.log2_diff_max_min_luma_transform_block_size = tb_log2 - 2;
    sps.max_transform_hierarchy_depth_inter = ctb_log2 - 2;
    sps.max_transform_hierarchy_depth_intra = ctb_log2 - 2;
    sps.pProfileTierLevel = &ptl;
    sps.pDecPicBufMgr = &dpbm;
    if w != rw || h != rh {
        sps.flags.set_conformance_window_flag(1);
        sps.conf_win_right_offset = (w - rw) / 2; // 4:2:0 SubWidthC = 2
        sps.conf_win_bottom_offset = (h - rh) / 2; // 4:2:0 SubHeightC = 2
    }

    // `vui` must outlive `create_video_session_parameters_khr` — `sps_arr` copies the pointer.
    let vui = h265_vui(hdr, full_range);
    sps.flags.set_vui_parameters_present_flag(1);
    sps.pSequenceParameterSetVui = &vui;

    let mut pps: hh::StdVideoH265PictureParameterSet = std::mem::zeroed();
    pps.flags.set_cu_qp_delta_enabled_flag(1);
    pps.flags.set_pps_loop_filter_across_slices_enabled_flag(1);

    let vps_arr = [vps];
    let sps_arr = [sps];
    let pps_arr = [pps];
    let add = vk::VideoEncodeH265SessionParametersAddInfoKHR::default()
        .std_vp_ss(&vps_arr)
        .std_sp_ss(&sps_arr)
        .std_pp_ss(&pps_arr);
    let mut h265_ci = vk::VideoEncodeH265SessionParametersCreateInfoKHR::default()
        .max_std_vps_count(1)
        .max_std_sps_count(1)
        .max_std_pps_count(1)
        .parameters_add_info(&add);
    // Quality level is baked into the parameters object; it must match the first frame's
    // ENCODE_QUALITY_LEVEL control.
    let mut q_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality_level);
    let ci = vk::VideoSessionParametersCreateInfoKHR::default()
        .video_session(session)
        .push_next(&mut h265_ci)
        .push_next(&mut q_info);
    let mut params = vk::VideoSessionParametersKHR::null();
    let r = (vq_dev.fp().create_video_session_parameters_khr)(
        device.handle(),
        &ci,
        std::ptr::null(),
        &mut params,
    );
    if r != vk::Result::SUCCESS {
        bail!("create_video_session_parameters: {r:?}");
    }

    let mut get_h265 = vk::VideoEncodeH265SessionParametersGetInfoKHR::default()
        .write_std_vps(true)
        .write_std_sps(true)
        .write_std_pps(true)
        .std_vps_id(0)
        .std_sps_id(0)
        .std_pps_id(0);
    let get = vk::VideoEncodeSessionParametersGetInfoKHR::default()
        .video_session_parameters(params)
        .push_next(&mut get_h265);
    let get_fn = venc_dev.fp().get_encoded_video_session_parameters_khr;
    let mut fb = vk::VideoEncodeSessionParametersFeedbackInfoKHR::default();
    let mut size: usize = 0;
    let r = get_fn(
        device.handle(),
        &get,
        &mut fb,
        &mut size,
        std::ptr::null_mut(),
    );
    if r != vk::Result::SUCCESS {
        // `params` is live and not yet the caller's to unwind — destroy before bailing.
        (vq_dev.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            params,
            std::ptr::null(),
        );
        bail!("get header size: {r:?}");
    }
    let mut buf = vec![0u8; size];
    let r = get_fn(
        device.handle(),
        &get,
        &mut fb,
        &mut size,
        buf.as_mut_ptr() as *mut c_void,
    );
    if r != vk::Result::SUCCESS {
        (vq_dev.fp().destroy_video_session_parameters_khr)(
            device.handle(),
            params,
            std::ptr::null(),
        );
        bail!("get header bytes: {r:?}");
    }
    buf.truncate(size);
    Ok((params, buf))
}

/// MSB-first OBU bit-writer. Vulkan AV1 encode never emits the sequence header (H.26x does).
struct Av1BitWriter {
    buf: Vec<u8>,
    cur: u8,
    fill: u8,
}
impl Av1BitWriter {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            cur: 0,
            fill: 0,
        }
    }
    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b as u8 & 1);
        self.fill += 1;
        if self.fill == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.fill = 0;
        }
    }
    fn put(&mut self, val: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.bit((val >> i) & 1);
        }
    }
    /// Flush, zero-padding the final partial byte (OBU size field delimits the payload).
    fn finish(mut self) -> Vec<u8> {
        if self.fill > 0 {
            self.cur <<= 8 - self.fill;
            self.buf.push(self.cur);
        }
        self.buf
    }
}

fn leb128(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
    out
}

/// The VUI that names the CSC: 709 (SDR at either depth) or 2020-NCL + PQ (HDR; samples arrive
/// PQ, the matrix does not touch transfer), at the range the CSC or EFC writes. Omit it and
/// decoders guess colorimetry.
fn h265_vui(hdr: bool, full_range: bool) -> ash::vk::native::StdVideoH265SequenceParameterSetVui {
    // SAFETY: a plain C struct of integers and bitfields; all-zero is its empty value.
    let mut vui: ash::vk::native::StdVideoH265SequenceParameterSetVui =
        unsafe { std::mem::zeroed() };
    vui.flags.set_video_signal_type_present_flag(1);
    vui.flags.set_video_full_range_flag(u32::from(full_range));
    vui.flags.set_colour_description_present_flag(1);
    vui.video_format = 5; // unspecified — the CICP triplet below is what matters
                          // CICP: 1 = BT.709, 9 = BT.2020 primaries / 2020-NCL matrix, 16 = SMPTE 2084.
    let (prim, trc, mat) = if hdr { (9, 16, 9) } else { (1, 1, 1) };
    vui.colour_primaries = prim;
    vui.transfer_characteristics = trc;
    vui.matrix_coeffs = mat;
    vui
}

/// Bit-pack a `sequence_header_obu` (AV1 spec §5.5) into a size-delimited OBU. Field values
/// MUST match the `StdVideoAV1SequenceHeader` in `build_parameters_av1` so driver-emitted
/// frame OBUs parse against this header; the frame size is the driver's coded size
/// ([`av1_coded_frame`]). Single operating point, 4:2:0 at 8 or 10 bits,
/// order-hint on; CDEF, restoration, filter-intra, compound, warp, superres all off.
#[allow(clippy::too_many_arguments)]
fn av1_sequence_header_obu(
    sb128: bool,
    fwb: u32,
    fhb: u32,
    max_w_m1: u32,
    max_h_m1: u32,
    order_hint_bits_minus_1: u32,
    seq_level_idx: u32,
    ten_bit: bool,
    hdr: bool,
    full_range: bool,
) -> Vec<u8> {
    let mut w = Av1BitWriter::new();
    w.put(0, 3); // seq_profile = MAIN
    w.bit(0); // still_picture
    w.bit(0); // reduced_still_picture_header
    w.bit(0); // timing_info_present_flag
    w.bit(0); // initial_display_delay_present_flag
    w.put(0, 5); // operating_points_cnt_minus_1 = 0
    w.put(0, 12); // operating_point_idc[0]
    w.put(seq_level_idx, 5); // seq_level_idx[0]
    if seq_level_idx > 7 {
        w.bit(0); // seq_tier[0] = 0
    }
    w.put(fwb, 4); // frame_width_bits_minus_1
    w.put(fhb, 4); // frame_height_bits_minus_1
    w.put(max_w_m1, fwb + 1); // max_frame_width_minus_1
    w.put(max_h_m1, fhb + 1); // max_frame_height_minus_1
    w.bit(0); // frame_id_numbers_present_flag
    w.bit(sb128 as u32); // use_128x128_superblock
    w.bit(0); // enable_filter_intra
    w.bit(0); // enable_intra_edge_filter
    w.bit(0); // enable_interintra_compound
    w.bit(0); // enable_masked_compound
    w.bit(0); // enable_warped_motion
    w.bit(0); // enable_dual_filter
    w.bit(1); // enable_order_hint
    w.bit(0); // enable_jnt_comp
    w.bit(0); // enable_ref_frame_mvs
    w.bit(1); // seq_choose_screen_content_tools -> seq_force_screen_content_tools = SELECT
    w.bit(1); // seq_choose_integer_mv -> seq_force_integer_mv = SELECT
    w.put(order_hint_bits_minus_1, 3); // order_hint_bits_minus_1
    w.bit(0); // enable_superres
    w.bit(0); // enable_cdef
    w.bit(0); // enable_restoration
              // color_config() (AV1 spec §5.5.2). AV1 has no VUI; CICP lives here.
              // Neither 709 nor 2020+PQ is the spec's sRGB special case (that would force
              // color_range = 1 and drop the range bit). `high_bitdepth` alone is 10-bit:
              // `twelve_bit` follows it only for seq_profile 2; ours is MAIN (0).
    w.bit(ten_bit as u32); // high_bitdepth -> BitDepth = 10
    w.bit(0); // mono_chrome
    w.bit(1); // color_description_present_flag
    let (prim, trc, mat) = if hdr { (9u32, 16u32, 9u32) } else { (1, 1, 1) };
    w.put(prim, 8); // color_primaries         (1 = BT.709, 9 = BT.2020)
    w.put(trc, 8); // transfer_characteristics (1 = BT.709, 16 = SMPTE 2084)
    w.put(mat, 8); // matrix_coefficients      (1 = BT.709, 9 = BT.2020 NCL)
    w.bit(u32::from(full_range)); // color_range (1 = full swing, 0 = studio)
    w.put(0, 2); // chroma_sample_position = CSP_UNKNOWN (subsampling_x==subsampling_y==1 for profile 0)
    w.bit(0); // separate_uv_delta_q
    w.bit(0); // film_grain_params_present

    // trailing_bits(): stop `1` then zero-pad. Size delimits the OBU, but parsers still
    // require trailing_one_bit (dav1d/cbs reject a plain zero pad).
    w.bit(1);
    let payload = w.finish();
    let mut obu = vec![0x0au8]; // obu_header: type=OBU_SEQUENCE_HEADER(1), has_size_field=1
    obu.extend_from_slice(&leb128(payload.len() as u64));
    obu.extend_from_slice(&payload);
    obu
}

/// AV1 session parameters + header framing. Vulkan AV1 encode emits only the per-frame OBU, so
/// the return is app-owned prefixes: a temporal-delimiter OBU every temporal unit
/// (`frame_prefix`), and TD + the bit-packed sequence-header OBU for keyframes (`header`).
/// The Std header states `w`×`h`, the source size; the OBU states `frame`, the coded size.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn build_parameters_av1(
    device: &ash::Device,
    vq_dev: &ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    w: u32,
    h: u32,
    frame: vk::Extent2D,
    max_level: ash::vk::native::StdVideoAV1Level,
    sb128: bool,
    quality_level: u32,
    // Depth. Must match the profile the session was created with (`open_inner`'s `ten_bit`)
    // and the OBU packed below.
    ten_bit: bool,
    // Colour (BT.709 vs BT.2020 PQ), independent of depth: 10-bit SDR is AV1 10-bit under BT.709.
    hdr: bool,
    // The range the CSC or EFC writes; `false` is studio. The Std header and the OBU both carry it.
    full_range: bool,
) -> Result<(vk::VideoSessionParametersKHR, Vec<u8>, Vec<u8>)> {
    use crate::vk_av1_encode as av1;
    use ash::vk::native as hh;

    let fwb = 31 - w.leading_zeros(); // bits for max_frame_width_minus_1 = w-1
    let fhb = 31 - h.leading_zeros();
    let order_hint_bits_minus_1: u32 = 7; // OrderHintBits = 8
    let seq_level_idx = max_level; // StdVideoAV1Level's numeric value IS the AV1 seq_level_idx

    // Std sequence header must match the OBU packed below or driver frame OBUs parse
    // against a header we did not write, `color_range` included.
    let mut cc_flags: hh::StdVideoAV1ColorConfigFlags = std::mem::zeroed();
    cc_flags.set_color_description_present_flag(1);
    cc_flags.set_color_range(u32::from(full_range));
    let mut cc: hh::StdVideoAV1ColorConfig = std::mem::zeroed();
    cc.flags = cc_flags;
    // The Std struct carries the DEPTH; the driver derives the OBU's `high_bitdepth` from it.
    cc.BitDepth = if ten_bit { 10 } else { 8 };
    cc.subsampling_x = 1;
    cc.subsampling_y = 1;
    let (prim, trc, mat) = if hdr {
        (
            hh::StdVideoAV1ColorPrimaries_STD_VIDEO_AV1_COLOR_PRIMARIES_BT_2020,
            hh::StdVideoAV1TransferCharacteristics_STD_VIDEO_AV1_TRANSFER_CHARACTERISTICS_SMPTE_2084,
            hh::StdVideoAV1MatrixCoefficients_STD_VIDEO_AV1_MATRIX_COEFFICIENTS_BT_2020_NCL,
        )
    } else {
        (
            hh::StdVideoAV1ColorPrimaries_STD_VIDEO_AV1_COLOR_PRIMARIES_BT_709,
            hh::StdVideoAV1TransferCharacteristics_STD_VIDEO_AV1_TRANSFER_CHARACTERISTICS_BT_709,
            hh::StdVideoAV1MatrixCoefficients_STD_VIDEO_AV1_MATRIX_COEFFICIENTS_BT_709,
        )
    };
    cc.color_primaries = prim;
    cc.transfer_characteristics = trc;
    cc.matrix_coefficients = mat;
    cc.chroma_sample_position =
        hh::StdVideoAV1ChromaSamplePosition_STD_VIDEO_AV1_CHROMA_SAMPLE_POSITION_UNKNOWN;

    // Only order-hint and (per caps) 128×128 superblocks. Extra tools (CDEF, restoration,
    // filter-intra, warp/compound, superres) make the driver emit frame-header sections
    // whose bit layout does not match this sequence header, and every inter frame desyncs.
    let mut sh_flags: hh::StdVideoAV1SequenceHeaderFlags = std::mem::zeroed();
    if sb128 {
        sh_flags.set_use_128x128_superblock(1);
    }
    sh_flags.set_enable_order_hint(1);
    let mut sh: hh::StdVideoAV1SequenceHeader = std::mem::zeroed();
    sh.flags = sh_flags;
    sh.seq_profile = hh::StdVideoAV1Profile_STD_VIDEO_AV1_PROFILE_MAIN;
    sh.frame_width_bits_minus_1 = fwb as u8;
    sh.frame_height_bits_minus_1 = fhb as u8;
    sh.max_frame_width_minus_1 = (w - 1) as u16;
    sh.max_frame_height_minus_1 = (h - 1) as u16;
    sh.order_hint_bits_minus_1 = order_hint_bits_minus_1 as u8;
    sh.seq_force_integer_mv = 2; // SELECT
    sh.seq_force_screen_content_tools = 2; // SELECT
    sh.pColorConfig = &cc;

    let op = av1::StdVideoEncodeAV1OperatingPointInfo {
        flags: std::mem::zeroed(),
        operating_point_idc: 0,
        seq_level_idx: seq_level_idx as u8,
        seq_tier: 0,
        decoder_buffer_delay: 0,
        encoder_buffer_delay: 0,
        initial_display_delay_minus_1: 0,
    };
    let ops = [op];
    let av1_spci = av1::VideoEncodeAV1SessionParametersCreateInfoKHR {
        s_type: av1::stype(av1::ST_SESSION_PARAMETERS_CREATE_INFO),
        p_next: std::ptr::null(),
        p_std_sequence_header: &sh,
        p_std_decoder_model_info: std::ptr::null(),
        std_operating_point_count: 1,
        p_std_operating_points: ops.as_ptr() as *const c_void,
    };
    // Quality level must match the first frame's ENCODE_QUALITY_LEVEL control.
    // Chained raw ahead of the vendored AV1 struct.
    let mut q_info = vk::VideoEncodeQualityLevelInfoKHR::default().quality_level(quality_level);
    q_info.p_next = &av1_spci as *const _ as *const c_void;
    let mut ci = vk::VideoSessionParametersCreateInfoKHR::default().video_session(session);
    ci.p_next = &q_info as *const _ as *const c_void;
    let mut params = vk::VideoSessionParametersKHR::null();
    let r = (vq_dev.fp().create_video_session_parameters_khr)(
        device.handle(),
        &ci,
        std::ptr::null(),
        &mut params,
    );
    if r != vk::Result::SUCCESS {
        bail!("create_video_session_parameters (av1): {r:?}");
    }

    let td = vec![0x12u8, 0x00]; // temporal_delimiter OBU (type=2, size=0)
    let seq_obu = av1_sequence_header_obu(
        sb128,
        31 - frame.width.leading_zeros(),
        31 - frame.height.leading_zeros(),
        frame.width - 1,
        frame.height - 1,
        order_hint_bits_minus_1,
        seq_level_idx,
        ten_bit,
        hdr,
        full_range,
    );
    let mut keyframe_prefix = td.clone();
    keyframe_prefix.extend_from_slice(&seq_obu);
    Ok((params, keyframe_prefix, td))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Independent walk of a packed sequence header (spec §5.5.1 order) returning
    /// `color_config()`. Not a mirror of the writer: a field-width or ordering change
    /// upstream of `color_config` would make the colour bits parse at the wrong offset.
    fn read_color_config(
        obu: &[u8],
        fwb: u32,
        fhb: u32,
        seq_level_idx: u32,
    ) -> (u8, u8, u8, u8, u8, u8) {
        // Payload starts after obu_header (1 byte) + leb128 size.
        assert_eq!(
            obu[0], 0x0a,
            "obu_header: OBU_SEQUENCE_HEADER + has_size_field"
        );
        let mut i = 1;
        while obu[i] & 0x80 != 0 {
            i += 1;
        }
        let payload = &obu[i + 1..];

        let mut pos = 0usize;
        let mut take = |bits: u32| -> u32 {
            let mut v = 0u32;
            for _ in 0..bits {
                let byte = payload[pos / 8];
                v = (v << 1) | u32::from((byte >> (7 - (pos % 8))) & 1);
                pos += 1;
            }
            v
        };

        assert_eq!(take(3), 0, "seq_profile = MAIN");
        take(1); // still_picture
        assert_eq!(take(1), 0, "reduced_still_picture_header");
        assert_eq!(take(1), 0, "timing_info_present_flag");
        assert_eq!(take(1), 0, "initial_display_delay_present_flag");
        assert_eq!(take(5), 0, "operating_points_cnt_minus_1");
        take(12); // operating_point_idc[0]
        assert_eq!(take(5), seq_level_idx, "seq_level_idx[0]");
        if seq_level_idx > 7 {
            take(1); // seq_tier[0]
        }
        assert_eq!(take(4), fwb, "frame_width_bits_minus_1");
        assert_eq!(take(4), fhb, "frame_height_bits_minus_1");
        take(fwb + 1); // max_frame_width_minus_1
        take(fhb + 1); // max_frame_height_minus_1
        take(1); // frame_id_numbers_present_flag
        take(1); // use_128x128_superblock
        take(1); // enable_filter_intra
        take(1); // enable_intra_edge_filter
        take(1); // enable_interintra_compound
        take(1); // enable_masked_compound
        take(1); // enable_warped_motion
        take(1); // enable_dual_filter
        let order_hint = take(1); // enable_order_hint
        assert_eq!(
            order_hint, 1,
            "enable_order_hint (our single-ref P-frame config)"
        );
        take(1); // enable_jnt_comp
        take(1); // enable_ref_frame_mvs
        assert_eq!(take(1), 1, "seq_choose_screen_content_tools = SELECT");
        // seq_force_screen_content_tools = SELECT (> 0), so seq_choose_integer_mv is present.
        assert_eq!(take(1), 1, "seq_choose_integer_mv = SELECT");
        take(3); // order_hint_bits_minus_1
        take(1); // enable_superres
        take(1); // enable_cdef
        take(1); // enable_restoration

        // `high_bitdepth` is returned, not asserted — the 10-bit case is why we read it.
        let high_bitdepth = take(1) as u8;
        assert_eq!(take(1), 0, "mono_chrome");
        let described = take(1) as u8;
        let (cp, tc, mc) = if described == 1 {
            (take(8) as u8, take(8) as u8, take(8) as u8)
        } else {
            (2, 2, 2) // CICP "unspecified"
        };
        let range = take(1) as u8;
        take(2); // chroma_sample_position
        assert_eq!(take(1), 0, "separate_uv_delta_q");
        assert_eq!(take(1), 0, "film_grain_params_present");
        assert_eq!(take(1), 1, "trailing_one_bit");
        (high_bitdepth, described, cp, tc, mc, range)
    }

    /// VCN4 pads AV1 to 64×16 and VCN5 to 8×2: 1080p codes as 1088 rows on the first only.
    /// A driver that reports no alignment pads nothing.
    #[test]
    fn av1_frame_is_the_size_the_driver_codes() {
        let at = |w, h| vk::Extent2D {
            width: w,
            height: h,
        };
        assert_eq!(av1_coded_frame(1920, 1080, at(64, 16)), at(1920, 1088));
        assert_eq!(av1_coded_frame(1366, 768, at(64, 16)), at(1408, 768));
        assert_eq!(av1_coded_frame(1920, 1080, at(8, 2)), at(1920, 1080));
        assert_eq!(av1_coded_frame(2560, 1440, at(64, 16)), at(2560, 1440));
        assert_eq!(av1_coded_frame(1920, 1080, at(0, 0)), at(1920, 1080));
    }

    /// Sequence header must signal BT.709 at full range — the CSC `rgb2yuv.comp` performs.
    /// Values must equal `StdVideoAV1ColorConfig` in `build_parameters_av1`: the driver
    /// packs frame OBUs against that struct while clients parse this header.
    #[test]
    fn av1_sequence_header_signals_bt709_full() {
        // 1920×1080 → 10/10 frame-size bits. Level 8 exercises seq_tier; sb128 both ways
        // because it sits above color_config.
        for (sb128, level) in [(false, 8u32), (true, 5u32)] {
            let obu =
                av1_sequence_header_obu(sb128, 10, 10, 1919, 1079, 7, level, false, false, true);
            let (depth10, described, cp, tc, mc, range) = read_color_config(&obu, 10, 10, level);
            assert_eq!(depth10, 0, "high_bitdepth (8-bit session)");
            assert_eq!(
                described, 1,
                "color_description_present_flag (sb128={sb128})"
            );
            assert_eq!(
                (cp, tc, mc),
                (1, 1, 1),
                "CICP BT.709 primaries/transfer/matrix"
            );
            assert_eq!(range, 1, "color_range = full swing");
        }
    }

    /// 10-bit HDR must signal BT.2020 + PQ with `high_bitdepth` set. That bit sits
    /// before the CICP bytes in `color_config()`, so a miss phases every field after it.
    #[test]
    fn av1_sequence_header_signals_bt2020_pq_at_10_bit() {
        for (sb128, level) in [(false, 8u32), (true, 5u32)] {
            let obu =
                av1_sequence_header_obu(sb128, 10, 10, 1919, 1079, 7, level, true, true, true);
            let (depth10, described, cp, tc, mc, range) = read_color_config(&obu, 10, 10, level);
            assert_eq!(depth10, 1, "high_bitdepth (sb128={sb128})");
            assert_eq!(described, 1, "color_description_present_flag");
            assert_eq!(
                (cp, tc, mc),
                (9, 16, 9),
                "CICP BT.2020 primaries / SMPTE 2084 transfer / BT.2020-NCL matrix"
            );
            assert_eq!(range, 1, "color_range = full swing");
        }
    }

    /// 10-bit SDR: `high_bitdepth` set but BT.709 CICP, not BT.2020/PQ. The colour axis is
    /// independent of depth (`rgb2yuv10_709.comp` performs the 709 matrix at ten bits).
    #[test]
    fn av1_sequence_header_signals_bt709_at_10_bit() {
        for (sb128, level) in [(false, 8u32), (true, 5u32)] {
            let obu =
                av1_sequence_header_obu(sb128, 10, 10, 1919, 1079, 7, level, true, false, true);
            let (depth10, described, cp, tc, mc, range) = read_color_config(&obu, 10, 10, level);
            assert_eq!(depth10, 1, "high_bitdepth (sb128={sb128})");
            assert_eq!(described, 1, "color_description_present_flag");
            assert_eq!(
                (cp, tc, mc),
                (1, 1, 1),
                "CICP BT.709 primaries/transfer/matrix at 10-bit"
            );
            assert_eq!(range, 1, "color_range = full swing");
        }
    }

    /// A studio session says studio at either depth and in HDR, in the OBU and the H.265 VUI
    /// alike.
    #[test]
    fn a_studio_session_signals_studio_range() {
        for (ten_bit, hdr) in [(false, false), (true, false), (true, true)] {
            let obu = av1_sequence_header_obu(false, 10, 10, 1919, 1079, 7, 8, ten_bit, hdr, false);
            let (depth10, _, cp, tc, mc, range) = read_color_config(&obu, 10, 10, 8);
            let cicp = if hdr { (9, 16, 9) } else { (1, 1, 1) };
            assert_eq!(
                (depth10, cp, tc, mc),
                (u8::from(ten_bit), cicp.0, cicp.1, cicp.2)
            );
            assert_eq!(range, 0, "color_range = studio swing (hdr={hdr})");
        }
        assert_eq!(h265_vui(false, false).flags.video_full_range_flag(), 0);
        assert_eq!(h265_vui(false, true).flags.video_full_range_flag(), 1);
        assert_eq!(h265_vui(true, false).flags.video_full_range_flag(), 0);
        assert_eq!(h265_vui(true, true).matrix_coeffs, 9);
    }

    /// A VCN that converts two pairs reports one. Only RADV's report is read that way.
    #[test]
    fn an_efc_reporting_709_narrow_alone_serves_2020_full_on_radv() {
        use crate::vk_valve_rgb::{
            MODEL_YCBCR_2020 as M2020, MODEL_YCBCR_709 as M709, RANGE_FULL as FULL,
            RANGE_NARROW as NARROW,
        };
        let two_pairs = (M709, NARROW);
        assert!(efc_serves(two_pairs, (M709, NARROW), true));
        assert!(efc_serves(two_pairs, (M2020, FULL), true));
        assert!(!efc_serves(two_pairs, (M2020, FULL), false));
        assert!(!efc_serves(two_pairs, (M709, FULL), true));
        assert!(!efc_serves(two_pairs, (M2020, NARROW), true));
        let all = (M709 | M2020, FULL | NARROW);
        for want in [(M709, FULL), (M709, NARROW), (M2020, FULL), (M2020, NARROW)] {
            assert!(efc_serves(all, want, false));
        }
        assert!(!efc_serves((M709 | M2020, NARROW), (M2020, FULL), true));
    }

    /// The EFC is asked for the range the bitstream signals.
    #[test]
    fn the_efc_range_follows_the_session() {
        use crate::vk_valve_rgb as vrgb;
        assert_eq!(rgb_range_for(true), vrgb::RANGE_FULL);
        assert_eq!(rgb_range_for(false), vrgb::RANGE_NARROW);
    }
}
