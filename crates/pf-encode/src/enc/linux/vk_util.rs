//! Shared ash/Vulkan leaf helpers for the Linux encode backends
//! (`vulkan_video.rs`, `pyrowave.rs`).
// `unsafe_op_in_unsafe_fn` off HERE. Wrapping each ash call would add a
// SAFETY comment that only restates the signature. Exit: delete unmarked
// calls; do not wrap them.
#![allow(unsafe_op_in_unsafe_fn)]
// Compiled for every Linux build: `sampled_capture_modifiers` feeds the VAAPI
// modifier offer even without `vulkan-encode`/`pyrowave`, which own the rest.
#![cfg_attr(
    not(any(feature = "vulkan-encode", feature = "pyrowave")),
    allow(dead_code, unused_imports)
)]

use anyhow::Result;
use ash::vk;
use pf_frame::PixelFormat;

pub(super) fn ext_advertised(exts: &[vk::ExtensionProperties], name: &std::ffi::CStr) -> bool {
    // Bounded: a missing NUL is `Err` (non-match), not a walk past the array.
    exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name))
}

pub(crate) struct PickedDevice {
    pub pd: vk::PhysicalDevice,
    /// Graphics+compute queue family. PyroWave's device create-info requires graphics;
    /// CSC + codec run on it.
    #[cfg(feature = "pyrowave")]
    pub family: u32,
    #[cfg(feature = "pyrowave")]
    pub vendor_id: u32,
    #[cfg(feature = "pyrowave")]
    pub device_id: u32,
}

/// First non-CPU Vulkan device with a graphics+compute family.
///
/// Do not switch this to `pf_gpu::selected_gpu()`: that picks "the NVIDIA GPU"
/// whenever `/dev/nvidiactl` exists, which on an Intel-compositor + NVIDIA-present
/// laptop is the GPU that cannot import the compositor's dmabufs and trips the
/// process-wide raw-dmabuf latch. Do not anchor on `/dev/dri/renderD128`: render
/// minors are driver bind-order, not display topology (amdgpu binds first → idle
/// iGPU while the compositor allocates on NVIDIA).
///
/// The right oracle is which device allocated the capture buffers; that plumbing
/// is not here. Shared with every capture-modifier probe so capture and encode
/// never disagree about the device across an in-place resize that does not
/// renegotiate.
///
/// # Safety
/// `instance` must be live; only physical-device property/queue queries.
pub(crate) unsafe fn select_physical_device(instance: &ash::Instance) -> Result<PickedDevice> {
    for pd in instance.enumerate_physical_devices()? {
        let props = instance.get_physical_device_properties(pd);
        if props.device_type == vk::PhysicalDeviceType::CPU {
            continue;
        }
        let Some(family) = instance
            .get_physical_device_queue_family_properties(pd)
            .iter()
            .position(|q| {
                q.queue_flags
                    .contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE)
            })
        else {
            continue;
        };
        #[cfg(not(feature = "pyrowave"))]
        let _ = family;
        return Ok(PickedDevice {
            pd,
            #[cfg(feature = "pyrowave")]
            family: family as u32,
            #[cfg(feature = "pyrowave")]
            vendor_id: props.vendor_id,
            #[cfg(feature = "pyrowave")]
            device_id: props.device_id,
        });
    }
    Err(anyhow::anyhow!(
        "no Vulkan GPU with a graphics+compute queue"
    ))
}

/// DRM modifiers this device can import as a SAMPLED packed-RGB image for
/// `fourcc`. The upper bound every capture offer narrows from; driver order,
/// deduplicated, one memory plane. Unknown fourcc, or no loader/instance/device,
/// is an empty list, not an error.
pub(crate) fn sampled_capture_modifiers(fourcc: u32) -> Vec<u64> {
    let Some(fmt) = fourcc_to_vk(fourcc) else {
        return Vec::new();
    };
    // SAFETY: fresh instance, plain physical-device property queries, destroyed before
    // returning; nothing borrows across the call.
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
        let pd = select_physical_device(&instance).ok().map(|p| p.pd);
        let mods = pd
            .map(|pd| {
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(pd, fmt, &mut fp2);
                let n = list.drm_format_modifier_count as usize;
                let mut props = vec![vk::DrmFormatModifierPropertiesEXT::default(); n];
                list.p_drm_format_modifier_properties = props.as_mut_ptr();
                let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
                instance.get_physical_device_format_properties2(pd, fmt, &mut fp2);
                props.truncate(list.drm_format_modifier_count as usize);
                let mut out: Vec<u64> = Vec::new();
                for p in props {
                    if !p
                        .drm_format_modifier_tiling_features
                        .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                        // Capture hands one fd/offset/stride.
                        || p.drm_format_modifier_plane_count != 1
                    {
                        continue;
                    }
                    if !out.contains(&p.drm_format_modifier) {
                        out.push(p.drm_format_modifier);
                    }
                }
                out
            })
            .unwrap_or_default();
        instance.destroy_instance(None);
        mods
    }
}

pub(crate) fn color_range(layer: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: layer,
        layer_count: 1,
    }
}

/// Ownership/visibility acquire for an EXCLUSIVE-sharing imported image: `foreign_qfi` →
/// `dst_qfi`, discarding (`fresh`, UNDEFINED) or keeping (GENERAL) prior contents.
pub(crate) fn imported_acquire_barrier(
    image: vk::Image,
    fresh: bool,
    foreign_qfi: u32,
    dst_qfi: u32,
    dst_stage: vk::PipelineStageFlags2,
    dst_access: vk::AccessFlags2,
    new_layout: vk::ImageLayout,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::NONE)
        .src_access_mask(vk::AccessFlags2::NONE)
        .dst_stage_mask(dst_stage)
        .dst_access_mask(dst_access)
        .old_layout(if fresh {
            vk::ImageLayout::UNDEFINED
        } else {
            vk::ImageLayout::GENERAL
        })
        .new_layout(new_layout)
        .src_queue_family_index(foreign_qfi)
        .dst_queue_family_index(dst_qfi)
        .image(image)
        .subresource_range(color_range(0))
}

/// Ownership release back to the foreign producer family after the last read of an imported
/// image, landing in GENERAL (the layout every later cached acquire expects).
pub(crate) fn imported_release_barrier(
    image: vk::Image,
    old_layout: vk::ImageLayout,
    src_qfi: u32,
    foreign_qfi: u32,
    src_stage: vk::PipelineStageFlags2,
    src_access: vk::AccessFlags2,
) -> vk::ImageMemoryBarrier2<'static> {
    vk::ImageMemoryBarrier2::default()
        .src_stage_mask(src_stage)
        .src_access_mask(src_access)
        .dst_stage_mask(vk::PipelineStageFlags2::NONE)
        .dst_access_mask(vk::AccessFlags2::NONE)
        .old_layout(old_layout)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(src_qfi)
        .dst_queue_family_index(foreign_qfi)
        .image(image)
        .subresource_range(color_range(0))
}

/// First memory type in `bits` carrying every flag in `want`. A miss is an error, never
/// index 0: that type may sit outside `bits` or lack a flag the caller relies on.
pub(crate) fn find_mem(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Result<u32> {
    (0..mp.memory_type_count)
        .find(|&i| {
            bits & (1 << i) != 0 && mp.memory_types[i as usize].property_flags.contains(want)
        })
        .ok_or_else(|| anyhow::anyhow!("no Vulkan memory type with {want:?} in bits {bits:#x}"))
}

/// [`find_mem`] for `prefer`, else any type in `bits`. For video session and video image
/// memory, which a driver may legally place off the device-local heap.
#[cfg_attr(not(feature = "vulkan-encode"), allow(dead_code))]
pub(crate) fn find_mem_preferring(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    prefer: vk::MemoryPropertyFlags,
) -> Result<u32> {
    find_mem(mp, bits, prefer).or_else(|_| find_mem(mp, bits, vk::MemoryPropertyFlags::empty()))
}

/// DRM fourcc → VkFormat whose *color* components match; Vulkan does the byte swizzle.
pub(crate) fn fourcc_to_vk(fourcc: u32) -> Option<vk::Format> {
    // fourcc_code(a,b,c,d) = a | b<<8 | c<<16 | d<<24
    const XR24: u32 = 0x3432_5258; // XRGB8888
    const AR24: u32 = 0x3432_5241; // ARGB8888
    const XB24: u32 = 0x3432_4258; // XBGR8888
    const AB24: u32 = 0x3432_4241; // ABGR8888
    const NV12: u32 = 0x3231_564e; // DRM_FORMAT_NV12
    const P010: u32 = 0x3031_3050; // DRM_FORMAT_P010
                                   // DRM word layout == Vulkan PACK32 (not a byte swizzle). A2R10G10B10 is
                                   // optional; a reject means drop XR30 from the capture offer, not convert here.
    const XR30: u32 = 0x3033_5258; // DRM_FORMAT_XRGB2101010
    const XB30: u32 = 0x3033_4258; // DRM_FORMAT_XBGR2101010
    match fourcc {
        XR24 | AR24 => Some(vk::Format::B8G8R8A8_UNORM),
        XB24 | AB24 => Some(vk::Format::R8G8B8A8_UNORM),
        XR30 => Some(vk::Format::A2R10G10B10_UNORM_PACK32),
        XB30 => Some(vk::Format::A2B10G10R10_UNORM_PACK32),
        NV12 => Some(vk::Format::G8_B8R8_2PLANE_420_UNORM),
        P010 => Some(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16),
        _ => None,
    }
}

pub(crate) fn pixel_to_vk(fmt: PixelFormat) -> Option<vk::Format> {
    match fmt {
        PixelFormat::Bgrx | PixelFormat::Bgra => Some(vk::Format::B8G8R8A8_UNORM),
        PixelFormat::Rgbx | PixelFormat::Rgba => Some(vk::Format::R8G8B8A8_UNORM),
        // Sampling yields PQ in [0,1], which `rgb2yuv10.comp` wants.
        PixelFormat::X2Rgb10 => Some(vk::Format::A2R10G10B10_UNORM_PACK32),
        PixelFormat::X2Bgr10 => Some(vk::Format::A2B10G10R10_UNORM_PACK32),
        _ => None,
    }
}

/// Expand packed 24-bpp CPU RGB into `scratch` (caller-owned, reused) as 4-bpp
/// with pad 0xFF: no 24-bpp VkFormat is reliably sampleable.
///
/// `bgra_target = false` keeps channel order (CSC views match). `true` forces
/// B,G,R,X because VUID-vkCmdEncodeVideoKHR-pEncodeInfo-08207 requires the
/// encode source to match the session `pictureFormat` (`B8G8R8A8_UNORM`).
///
/// Payloads are tightly packed (`FramePayload::Cpu`); a truncated source
/// yields a truncated output — upload paths bound-check the bytes.
pub(crate) fn normalize_cpu_rgb<'a>(
    fmt: PixelFormat,
    bytes: &'a [u8],
    scratch: &'a mut Vec<u8>,
    bgra_target: bool,
) -> (PixelFormat, &'a [u8]) {
    let (bpp, r, g, b) = match fmt {
        PixelFormat::Rgb => (3usize, 0usize, 1usize, 2usize),
        PixelFormat::Bgr => (3, 2, 1, 0),
        PixelFormat::Rgbx | PixelFormat::Rgba => (4, 0, 1, 2),
        PixelFormat::Bgrx | PixelFormat::Bgra => (4, 2, 1, 0),
        _ => return (fmt, bytes),
    };
    if bpp == 4 && (!bgra_target || b == 0) {
        return (fmt, bytes); // 4-bpp already in session order: borrow
    }
    let px = bytes.len() / bpp;
    scratch.clear();
    scratch.resize(px * 4, 0xFF);
    let (dr, dg, db) = if bgra_target { (2, 1, 0) } else { (r, g, b) };
    for (dst, src) in scratch.chunks_exact_mut(4).zip(bytes.chunks_exact(bpp)) {
        dst[dr] = src[r];
        dst[dg] = src[g];
        dst[db] = src[b];
    }
    let out_fmt = if bgra_target || b == 0 {
        PixelFormat::Bgrx
    } else {
        PixelFormat::Rgbx
    };
    (out_fmt, scratch.as_slice())
}

pub(crate) unsafe fn make_view(
    device: &ash::Device,
    image: vk::Image,
    fmt: vk::Format,
    layer: u32,
) -> Result<vk::ImageView> {
    Ok(device.create_image_view(
        &vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(color_range(layer)),
        None,
    )?)
}

/// False for `ERROR_OUT_OF_{DEVICE,HOST}_MEMORY`: three tight OOMs must not
/// permanently latch a working host onto CPU capture. Deterministic refusals
/// (unsupported fourcc, driver reject) do count — they repeat forever.
pub(crate) fn import_failure_feeds_latch(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<vk::Result>() {
        Some(&r) => {
            r != vk::Result::ERROR_OUT_OF_DEVICE_MEMORY && r != vk::Result::ERROR_OUT_OF_HOST_MEMORY
        }
        None => true,
    }
}

/// Context on an import error that [`reject_dmabuf`] took. The encode worker forwards only
/// these as `capture_rebuild`, so the host's latch sees what an in-process encoder feeds it.
#[derive(Debug)]
pub(crate) struct ImportRejected;

impl std::fmt::Display for ImportRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dmabuf import refused")
    }
}

/// Feed a deterministic dmabuf rejection to this capture's health. A completed
/// latch marks the capturer broken so the next tick rebuilds its offer.
pub(crate) fn reject_dmabuf(d: &pf_frame::DmabufFrame, reason: &str) {
    if d.health.note_raw_import_failure(d.modifier, reason) {
        d.rebuild.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Caller destroys all three returned handles.
pub(crate) unsafe fn import_rgb_dmabuf(
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    d: &pf_frame::DmabufFrame,
    cw: u32,
    ch: u32,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    import_rgb_dmabuf_as(
        device,
        ext_fd,
        mem_props,
        d,
        cw,
        ch,
        vk::ImageUsageFlags::SAMPLED,
        None,
    )
}

/// Import `d` as a `usage` image plus a view; fd ownership and the memory pick are
/// [`pf_zerocopy::vkdev::import_dmabuf_image`]'s. Also imports one-fd LINEAR NV12: UV layout
/// from plane-1, else shared-stride contiguous planes.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn import_rgb_dmabuf_as(
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    d: &pf_frame::DmabufFrame,
    cw: u32,
    ch: u32,
    usage: vk::ImageUsageFlags,
    profile_list: Option<&mut vk::VideoProfileListInfoKHR>,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    use anyhow::Context;
    use std::os::fd::AsFd;
    let fmt = fourcc_to_vk(d.fourcc)
        .with_context(|| format!("unsupported dmabuf fourcc {:#x}", d.fourcc))?;
    let two_plane = matches!(
        fmt,
        vk::Format::G8_B8R8_2PLANE_420_UNORM
            | vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
    );
    let planes: Vec<vk::SubresourceLayout> = if two_plane {
        let (uv_offset, uv_stride) = d.plane1.map(|(o, s)| (o as u64, s as u64)).unwrap_or((
            d.offset as u64 + d.stride as u64 * ch as u64,
            d.stride as u64,
        ));
        vec![
            vk::SubresourceLayout::default()
                .offset(d.offset as u64)
                .row_pitch(d.stride as u64),
            vk::SubresourceLayout::default()
                .offset(uv_offset)
                .row_pitch(uv_stride),
        ]
    } else {
        vec![vk::SubresourceLayout::default()
            .offset(d.offset as u64)
            .row_pitch(d.stride as u64)]
    };
    let (img, mem) = pf_zerocopy::vkdev::import_dmabuf_image(
        device,
        ext_fd,
        mem_props,
        &pf_zerocopy::vkdev::DmabufImage {
            fd: d.fd.as_fd(),
            format: fmt,
            width: cw,
            height: ch,
            modifier: d.modifier,
            planes: &planes,
            usage,
            initial_layout: vk::ImageLayout::UNDEFINED,
        },
        profile_list,
    )?;
    let view = match make_view(device, img, fmt, 0) {
        Ok(v) => v,
        Err(e) => {
            device.destroy_image(img, None);
            device.free_memory(mem, None);
            return Err(e);
        }
    };
    Ok((img, mem, view))
}

/// On failure every handle this call created is destroyed, so callers can `?`.
pub(crate) unsafe fn make_host_buffer(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let buf = device.create_buffer(
        &vk::BufferCreateInfo::default().size(size).usage(usage),
        None,
    )?;
    let req = device.get_buffer_memory_requirements(buf);
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let mem = match find_mem(mp, req.memory_type_bits, host).and_then(|ti| {
        Ok(device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(req.size)
                .memory_type_index(ti),
            None,
        )?)
    }) {
        Ok(m) => m,
        Err(e) => {
            device.destroy_buffer(buf, None);
            return Err(e);
        }
    };
    if let Err(e) = device.bind_buffer_memory(buf, mem, 0) {
        device.destroy_buffer(buf, None);
        device.free_memory(mem, None);
        return Err(e.into());
    }
    Ok((buf, mem))
}

pub(crate) unsafe fn make_plain_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    usage: vk::ImageUsageFlags,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    let img = device.create_image(
        &vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(fmt)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(usage)
            .initial_layout(vk::ImageLayout::UNDEFINED),
        None,
    )?;
    let req = device.get_image_memory_requirements(img);
    // Unwind: callers only ever see the completed triple.
    let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let mem = match find_mem(mp, req.memory_type_bits, local).and_then(|ti| {
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
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        device.destroy_image(img, None);
        device.free_memory(mem, None);
        return Err(e.into());
    }
    match make_view(device, img, fmt, 0) {
        Ok(view) => Ok((img, mem, view)),
        Err(e) => {
            device.destroy_image(img, None);
            device.free_memory(mem, None);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ext_advertised_matches_exact_name() {
        let mut e = ash::vk::ExtensionProperties::default();
        let name = b"VK_EXT_queue_family_foreign\0";
        for (i, b) in name.iter().enumerate() {
            e.extension_name[i] = *b as std::ffi::c_char;
        }
        let exts = [ash::vk::ExtensionProperties::default(), e];
        assert!(super::ext_advertised(
            &exts,
            ash::ext::queue_family_foreign::NAME
        ));
        assert!(!super::ext_advertised(
            &exts[..1],
            ash::ext::queue_family_foreign::NAME
        ));
    }

    #[test]
    fn ext_advertised_rejects_unterminated_name_without_overrunning() {
        let mut bad = ash::vk::ExtensionProperties::default();
        bad.extension_name.fill(b'A' as std::ffi::c_char);
        // Last so an unbounded walk would leave the array.
        let exts = [ash::vk::ExtensionProperties::default(), bad];
        assert!(!super::ext_advertised(
            &exts,
            ash::ext::queue_family_foreign::NAME
        ));
        assert!(!super::ext_advertised(&exts, c"AAAA"));
    }

    use super::*;

    #[test]
    fn a_memory_type_miss_is_an_error_not_index_0() {
        let mut mp = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 3,
            ..Default::default()
        };
        mp.memory_types[1].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE;
        mp.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        assert_eq!(find_mem(&mp, 0b110, local).unwrap(), 2);
        assert!(find_mem(&mp, 0b011, local).is_err());
        assert_eq!(find_mem_preferring(&mp, 0b011, local).unwrap(), 0);
        assert_eq!(find_mem_preferring(&mp, 0b010, local).unwrap(), 1);
        assert!(find_mem_preferring(&mp, 0, local).is_err());
    }

    #[test]
    fn normalize_cpu_rgb_expands_24bpp_and_borrows_4bpp() {
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5, 6], &mut scratch, false);
        assert_eq!(f, PixelFormat::Rgbx);
        assert_eq!(b, &[1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);

        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // 5 bytes = one pixel + a 2-byte remainder that must be dropped.
        let mut scratch = Vec::new();
        let (_, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3, 4, 5], &mut scratch, false);
        assert_eq!(b, &[1, 2, 3, 0xFF]);

        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgrx, &src, &mut scratch, false);
        assert_eq!(f, PixelFormat::Bgrx);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());

        assert_eq!(
            pixel_to_vk(PixelFormat::Rgbx),
            Some(vk::Format::R8G8B8A8_UNORM)
        );
        assert_eq!(
            pixel_to_vk(PixelFormat::Bgrx),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
    }

    #[test]
    fn normalize_cpu_rgb_forces_bgra_for_the_encode_source() {
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgb, &[1, 2, 3], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgr, &[9, 8, 7], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[9, 8, 7, 0xFF]);

        // R-first 4-bpp: swap; source alpha replaced by the 0xFF pad.
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Rgbx, &[1, 2, 3, 4], &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgrx);
        assert_eq!(b, &[3, 2, 1, 0xFF]);

        let src = [10u8, 20, 30, 40];
        let mut scratch = Vec::new();
        let (f, b) = normalize_cpu_rgb(PixelFormat::Bgra, &src, &mut scratch, true);
        assert_eq!(f, PixelFormat::Bgra);
        assert!(std::ptr::eq(b.as_ptr(), src.as_ptr()));
        assert!(scratch.is_empty());
    }

    #[test]
    fn tiled_rejection_marks_the_capture_for_rebuild() {
        let rebuild = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let frame = pf_frame::DmabufFrame {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 7,
            plane1: None,
            offset: 0,
            stride: 256,
            hold: None,
            health: pf_zerocopy::zero_copy_health(0x4001),
            rebuild: rebuild.clone(),
        };
        reject_dmabuf(&frame, "test rejection");
        assert!(rebuild.load(std::sync::atomic::Ordering::Relaxed));
        assert!(frame.health.passthrough_tiled_refused());
        assert!(!frame.health.raw_disabled(), "LINEAR remains available");
    }
}
