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

use crate::vk_import::{Action, Event, Extent, Key, Policy};
use anyhow::Result;
use ash::vk;
use pf_frame::PixelFormat;
use pf_zerocopy::vkdev::{VkBufferAlloc, VkImageAlloc};

/// Set when this process, not the operator, put `video-encode` into `ANV_DEBUG`.
static ANV_ENCODE_OURS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The `ANV_DEBUG` value that opts Mesa's Intel driver into Vulkan Video encode, which it
/// exposes only under `video-encode`. Appended, never clobbered, so an operator's flags survive;
/// `PUNKTFUNK_VULKAN_ENCODE=0` still picks VAAPI. `None` when there is nothing to add.
/// A `Some` marks the flag as ours: the caller exports it before any Vulkan instance exists.
pub fn anv_video_encode_flag() -> Option<String> {
    if !cfg!(feature = "vulkan-encode") {
        return None;
    }
    let current = crate::env_knobs::Knobs::anv_debug();
    let value = with_token(current.as_deref(), "video-encode")?;
    ANV_ENCODE_OURS.store(true, std::sync::atomic::Ordering::Relaxed);
    Some(value)
}

/// `current` with `token` appended to its comma list; `None` when it is already there.
fn with_token(current: Option<&str>, token: &str) -> Option<String> {
    match current {
        Some(v) if v.split(',').any(|t| t == token) => None,
        Some(v) if !v.is_empty() => Some(format!("{v},{token}")),
        _ => Some(token.to_owned()),
    }
}

/// Whether this device's Vulkan Video encode may be used. Intel re-enabled Alchemist encode in
/// Mesa 26.2; older ANV encodes only when the operator set `ANV_DEBUG` themselves.
#[cfg(feature = "vulkan-encode")]
pub(crate) fn encode_trusted(props: &vk::PhysicalDeviceProperties) -> bool {
    encode_trusted_for(
        props.vendor_id,
        props.driver_version,
        ANV_ENCODE_OURS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

#[cfg(feature = "vulkan-encode")]
fn encode_trusted_for(vendor_id: u32, driver_version: u32, flag_is_ours: bool) -> bool {
    const INTEL: u32 = 0x8086;
    // ANV reports the Mesa release as `VK_MAKE_VERSION`; a `-devel` build reads one below.
    let mesa = (
        vk::api_version_major(driver_version),
        vk::api_version_minor(driver_version),
    );
    vendor_id != INTEL || !flag_is_ours || mesa >= (26, 2)
}

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
#[cfg(feature = "pyrowave")]
pub(crate) fn sampled_capture_modifiers(fourcc: u32) -> Vec<u64> {
    sampled_modifiers(fourcc, None).unwrap_or_default()
}

/// [`sampled_capture_modifiers`] of the device that owns `node`: the VA lane's candidates. A
/// VA driver imports its own device's layouts, and on a hybrid the first Vulkan device is the
/// other GPU. `None` when no Vulkan device owns the node: VA runs without a Vulkan driver.
pub(crate) fn node_capture_modifiers(fourcc: u32, node: &std::path::Path) -> Option<Vec<u64>> {
    sampled_modifiers(fourcc, Some(node))
}

/// The list of `node`'s owner, or of the first usable device without a `node`. `None` is a
/// `node` no device owns; every other failure is an empty list.
fn sampled_modifiers(fourcc: u32, node: Option<&std::path::Path>) -> Option<Vec<u64>> {
    let Some(fmt) = pf_zerocopy::drm::vk_format(fourcc) else {
        return Some(Vec::new());
    };
    // SAFETY: fresh instance, plain physical-device property queries, destroyed before
    // returning; nothing borrows across the call.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return node.is_none().then(Vec::new);
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3);
        let Ok(instance) = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) else {
            return node.is_none().then(Vec::new);
        };
        let pd = match node {
            Some(n) => {
                let (rdev, pci) = (node_rdev(n), node_pci_address(n));
                let owner = instance
                    .enumerate_physical_devices()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|&pd| device_owns_node(&instance, pd, rdev, pci));
                if owner.is_none() {
                    instance.destroy_instance(None);
                    return None;
                }
                owner
            }
            None => select_physical_device(&instance).ok().map(|p| p.pd),
        };
        let mods = pd
            .map(|pd| {
                let mut out: Vec<u64> = Vec::new();
                for p in pf_zerocopy::vkdev::format_modifiers(&instance, pd, fmt) {
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
        Some(mods)
    }
}

/// `(major, minor)` of a device node in the encoding `VkPhysicalDeviceDrmPropertiesEXT`
/// uses (glibc `gnu_dev_major`/`gnu_dev_minor`).
pub(crate) fn node_rdev(path: &std::path::Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfffu64);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xffu64);
    Some((major as i64, minor as i64))
}

/// Node PCI `(domain, bus, device, function)` from sysfs — fallback when the
/// driver lacks `VK_EXT_physical_device_drm`.
pub(crate) fn node_pci_address(path: &std::path::Path) -> Option<(u32, u32, u32, u32)> {
    let node = path.file_name()?.to_str()?;
    let dev = std::fs::canonicalize(format!("/sys/class/drm/{node}/device")).ok()?;
    let addr = dev.file_name()?.to_str()?;
    let (rest, func) = addr.rsplit_once('.')?;
    let mut parts = rest.split(':');
    let domain = u32::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((domain, bus, device, u32::from_str_radix(func, 16).ok()?))
}

/// Whether `pd` owns the render node `rdev`/`pci` describe. DRM render major/minor is primary
/// (disambiguates twin-model GPUs); PCI bus info is the fallback. Neither
/// extension advertised → never matches.
///
/// # Safety
/// `instance` must be live; issues only physical-device property queries.
pub(crate) unsafe fn device_owns_node(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    rdev: Option<(i64, i64)>,
    pci: Option<(u32, u32, u32, u32)>,
) -> bool {
    let exts = instance
        .enumerate_device_extension_properties(pd)
        .unwrap_or_default();
    let has_ext =
        |name: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
    if let Some((major, minor)) = rdev {
        if has_ext(ash::ext::physical_device_drm::NAME) {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(pd, &mut p2);
            return drm.has_render == vk::TRUE
                && drm.render_major == major
                && drm.render_minor == minor;
        }
    }
    if let Some((domain, bus, device, function)) = pci {
        if has_ext(ash::ext::pci_bus_info::NAME) {
            let mut pcip = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut pcip);
            instance.get_physical_device_properties2(pd, &mut p2);
            return pcip.pci_domain == domain
                && pcip.pci_bus == bus
                && pcip.pci_device == device
                && pcip.pci_function == function;
        }
    }
    false
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

pub(crate) use pf_zerocopy::vkdev::memory_type as find_mem;

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

/// Memory type for a buffer the CPU reads back every frame (the encode bitstream), and whether
/// it is coherent. Cached first: RADV lists write-combined GTT before cached GTT, and plain
/// loads from write-combined memory run at a few hundred MB/s. A cached type without
/// coherence needs `vkInvalidateMappedMemoryRanges` before each read.
#[cfg_attr(not(feature = "vulkan-encode"), allow(dead_code))]
pub(crate) fn find_readback_mem(
    mp: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
) -> Result<(u32, bool)> {
    use vk::MemoryPropertyFlags as F;
    find_mem(
        mp,
        bits,
        F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED,
    )
    .map(|i| (i, true))
    .or_else(|_| find_mem(mp, bits, F::HOST_VISIBLE | F::HOST_CACHED).map(|i| (i, false)))
    .or_else(|_| find_mem(mp, bits, F::HOST_VISIBLE | F::HOST_COHERENT).map(|i| (i, true)))
}

/// The size of an invalidate from offset 0 that covers `[0, end)` of a readback buffer of
/// `size` bytes bound at memory offset 0: `end` rounded up to `atom` (`nonCoherentAtomSize`),
/// or the whole mapping once that passes the buffer. Both are valid range ends.
#[cfg_attr(not(feature = "vulkan-encode"), allow(dead_code))]
pub(crate) fn readback_invalidate_size(end: u64, atom: u64, size: u64) -> u64 {
    let rounded = end.div_ceil(atom) * atom;
    if rounded <= size {
        rounded
    } else {
        vk::WHOLE_SIZE
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
#[cfg_attr(not(feature = "pyrowave"), allow(dead_code))]
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

/// Dmabuf imports reused while the same buffer recurs, by [`Policy`]. Keyed by
/// `(st_dev, st_ino)`: each `DmabufFrame` owns a fresh dup of the same inode. The cache owns
/// every entry; [`ImportCache::clear`] frees them.
#[derive(Default)]
pub(crate) struct ImportCache {
    policy: Policy<VkImageAlloc>,
}

impl ImportCache {
    /// The import cached for `key` at `extent`, else `import()`'s, as `(image, view, fresh)`.
    /// [`Policy`] decides; this idles the device where it says and destroys what it frees.
    ///
    /// A miss keeps `keep` older imports, those frames in flight read. RADV lists every resident
    /// import in every submission, and amdgpu orders that submission against whatever paints
    /// any of them. A victim `in_flight` reports idles the device first: the GPU may read it.
    ///
    /// # Safety
    /// `dev` created every cached handle and `import()`'s.
    pub(crate) unsafe fn get_or_import(
        &mut self,
        dev: &ash::Device,
        key: Key,
        extent: Extent,
        import: impl FnOnce() -> Result<VkImageAlloc>,
        in_flight: impl Fn(Key) -> bool,
        keep: usize,
    ) -> Result<(vk::Image, vk::ImageView, bool)> {
        let stale = match self.policy.step(Event::Want(key, extent)) {
            Action::Reuse(a) => return Ok((a.image, a.view, false)),
            Action::Import { stale } => stale,
            Action::Insert { .. } => unreachable!("import cache: an insert for a lookup"),
        };
        if let Some(a) = stale {
            let _ = dev.device_wait_idle();
            a.destroy(dev);
        }
        let t0 = std::time::Instant::now();
        let alloc = import()?;
        let busy: Vec<Key> = self.policy.keys().filter(|&k| in_flight(k)).collect();
        let event = Event::Imported {
            key,
            extent,
            item: alloc,
            keep,
            busy: &busy,
        };
        let Action::Insert { evict, idle_first } = self.policy.step(event) else {
            unreachable!("import cache: no insert for an import");
        };
        if idle_first {
            let _ = dev.device_wait_idle();
        }
        for a in evict {
            a.destroy(dev);
        }
        // Trace: a producer ring deeper than `keep` misses every frame.
        tracing::trace!(
            resident = self.policy.len(),
            miss_us = t0.elapsed().as_micros() as u64,
            "imported a new dmabuf buffer"
        );
        Ok((alloc.image, alloc.view, true))
    }

    /// Destroy every entry. The device must be idle.
    pub(crate) unsafe fn clear(&mut self, dev: &ash::Device) {
        for a in self.policy.drain() {
            a.destroy(dev);
        }
    }
}

/// Caller destroys the returned alloc.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn import_rgb_dmabuf(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    d: &pf_frame::DmabufFrame,
    cw: u32,
    ch: u32,
) -> Result<VkImageAlloc> {
    import_rgb_dmabuf_as(
        instance,
        pd,
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
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    d: &pf_frame::DmabufFrame,
    cw: u32,
    ch: u32,
    usage: vk::ImageUsageFlags,
    profile_list: Option<&mut vk::VideoProfileListInfoKHR>,
) -> Result<VkImageAlloc> {
    use anyhow::Context;
    use std::os::fd::AsFd;
    let fmt = pf_zerocopy::drm::vk_format(d.fourcc)
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
    let mut alloc = pf_zerocopy::vkdev::import_dmabuf_image(
        instance,
        pd,
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
        },
        profile_list,
    )?;
    // A view needs a usage that can read through it (VUID-VkImageViewCreateInfo-image-04441);
    // the padded-copy import is TRANSFER_SRC only and is read by `vkCmdCopyImage`.
    let viewable = vk::ImageUsageFlags::SAMPLED
        | vk::ImageUsageFlags::STORAGE
        | vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR;
    if usage.intersects(viewable) {
        alloc.view = match make_view(device, alloc.image, fmt, 0) {
            Ok(v) => v,
            Err(e) => {
                alloc.destroy(device);
                return Err(e);
            }
        };
    }
    Ok(alloc)
}

/// On failure every handle this call created is destroyed, so callers can `?`.
pub(crate) unsafe fn make_host_buffer(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<VkBufferAlloc> {
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
    let alloc = VkBufferAlloc {
        buffer: buf,
        memory: mem,
    };
    if let Err(e) = device.bind_buffer_memory(buf, mem, 0) {
        alloc.destroy(device);
        return Err(e.into());
    }
    Ok(alloc)
}

pub(crate) unsafe fn make_plain_image(
    device: &ash::Device,
    mp: &vk::PhysicalDeviceMemoryProperties,
    fmt: vk::Format,
    w: u32,
    h: u32,
    usage: vk::ImageUsageFlags,
) -> Result<VkImageAlloc> {
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
    // Unwind: callers only ever see the completed alloc.
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
    let mut alloc = VkImageAlloc {
        image: img,
        memory: mem,
        view: vk::ImageView::null(),
    };
    if let Err(e) = device.bind_image_memory(img, mem, 0) {
        alloc.destroy(device);
        return Err(e.into());
    }
    match make_view(device, img, fmt, 0) {
        Ok(view) => {
            alloc.view = view;
            Ok(alloc)
        }
        Err(e) => {
            alloc.destroy(device);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    /// Our `ANV_DEBUG` opt-in keeps the operator's flags and stays off ANV older than 26.2.
    #[cfg(feature = "vulkan-encode")]
    #[test]
    fn anv_encode_opt_in_appends_and_gates_on_mesa() {
        use super::{encode_trusted_for, with_token};
        assert_eq!(
            with_token(None, "video-encode").as_deref(),
            Some("video-encode")
        );
        assert_eq!(
            with_token(Some(""), "video-encode").as_deref(),
            Some("video-encode")
        );
        assert_eq!(
            with_token(Some("video-decode"), "video-encode").as_deref(),
            Some("video-decode,video-encode")
        );
        assert_eq!(
            with_token(Some("bindless,video-encode"), "video-encode"),
            None
        );

        let mesa = |minor| ash::vk::make_api_version(0, 26, minor, 0);
        assert!(!encode_trusted_for(0x8086, mesa(1), true));
        assert!(encode_trusted_for(0x8086, mesa(2), true));
        assert!(encode_trusted_for(
            0x8086,
            ash::vk::make_api_version(0, 27, 0, 0),
            true
        ));
        // The operator's own flag, and every other vendor, pass on any version.
        assert!(encode_trusted_for(0x8086, mesa(1), false));
        assert!(encode_trusted_for(0x1002, mesa(1), true));
    }

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
    fn readback_memory_prefers_cached_over_write_combined() {
        use vk::MemoryPropertyFlags as F;
        // RADV PHOENIX order: VRAM, VRAM, GTT write-combined, BAR, BAR, GTT cached.
        let mut mp = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 6,
            ..Default::default()
        };
        mp.memory_types[0].property_flags = F::DEVICE_LOCAL;
        mp.memory_types[1].property_flags = F::DEVICE_LOCAL;
        mp.memory_types[2].property_flags = F::HOST_VISIBLE | F::HOST_COHERENT;
        mp.memory_types[3].property_flags = F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT;
        mp.memory_types[4].property_flags = F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT;
        mp.memory_types[5].property_flags = F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED;
        assert_eq!(find_readback_mem(&mp, 0b111111).unwrap(), (5, true));
        // Without the cached type, today's write-combined pick.
        assert_eq!(find_readback_mem(&mp, 0b011111).unwrap(), (2, true));
        // A cached type that is not coherent still wins, and says so.
        mp.memory_types[5].property_flags = F::HOST_VISIBLE | F::HOST_CACHED;
        assert_eq!(find_readback_mem(&mp, 0b111111).unwrap(), (5, false));
        assert!(find_readback_mem(&mp, 0b000011).is_err());
    }

    #[test]
    fn readback_invalidate_covers_the_access_unit_in_whole_atoms() {
        const BUF: u64 = 24_948_736; // 4K: 3 × 3840 × 2160 + 64 KiB
        assert_eq!(readback_invalidate_size(200_000, 64, BUF), 200_000);
        assert_eq!(readback_invalidate_size(200_001, 64, BUF), 200_064);
        assert_eq!(readback_invalidate_size(BUF, 64, BUF), BUF);
        // Rounding past the buffer's end takes the whole mapping instead.
        assert_eq!(
            readback_invalidate_size(BUF - 1, 256, BUF - 100),
            vk::WHOLE_SIZE
        );
    }

    #[test]
    fn pixel_to_vk_maps_the_staged_orders() {
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
