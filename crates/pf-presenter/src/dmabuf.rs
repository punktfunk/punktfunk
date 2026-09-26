//! VAAPI dmabuf → Vulkan import of per-plane `VkImage`s with the surface's
//! explicit DRM format modifier.
//!
//! Formats: R8/R8G8 for NV12 and full-chroma NV24; R16/R16G16 for P010.
//! The export's modifier must pass this device's importability query — a
//! foreign GPU's tiling refuses before image create. A refusal or driver
//! rejection is a clean error; the caller demotes to software decode.
//! EGL sibling: `video_gl.rs`.

use anyhow::{bail, Context as _, Result};
use ash::vk;
use pf_client_core::video::{DmabufFrame, DrmFrameGuard};
use std::os::fd::{AsRawFd as _, BorrowedFd, IntoRawFd as _};

/// fourcc('N','V','1','2').
const DRM_FORMAT_NV12: u32 = 0x3231_564e;
/// fourcc('P','0','1','0'). 10 bits MSB-aligned in 16.
const DRM_FORMAT_P010: u32 = 0x3031_3050;
/// fourcc('N','V','2','4'). CSC keys chroma siting off plane widths, so the
/// full-size chroma plane needs no extra shader. Packed AYUV/Y410 is
/// single-plane and still demotes.
const DRM_FORMAT_NV24: u32 = 0x3432_564e;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

pub const DEVICE_EXTENSIONS: [&std::ffi::CStr; 4] = [
    ash::ext::external_memory_dma_buf::NAME,
    ash::khr::external_memory_fd::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];

/// The export's tiling modifier, or a refusal. Explicit-modifier images cannot
/// take INVALID, and guessing a tiling the exporter did not name is never right.
fn explicit_modifier(modifier: u64) -> Result<u64> {
    if modifier == DRM_FORMAT_MOD_INVALID {
        bail!("dmabuf export has no explicit DRM modifier");
    }
    Ok(modifier)
}

/// Whether `pdev` accepts a `DMA_BUF_EXT` import of `fmt` tiled as `modifier`
/// for a one-plane SAMPLED image. Extension presence admitted this lane; this
/// is the legal answer — an unsupported external image is UB
/// (`VK_ERROR_DEVICE_LOST` on first submit).
///
/// Two queries. The modifier list must name this exact modifier for a one-plane
/// image with SAMPLED_IMAGE — the same one-layout
/// `ImageDrmFormatModifierExplicitCreateInfoEXT` [`plane_image`] creates. Then
/// the external-image query must answer IMPORTABLE.
///
/// # Safety
/// `instance`/`pdev` must be live and paired.
unsafe fn modifier_importable(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    fmt: vk::Format,
    modifier: u64,
) -> bool {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: read-only query; `list`/`fp2` outlive the call.
    unsafe { instance.get_physical_device_format_properties2(pdev, fmt, &mut fp2) };
    let mut props = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list.p_drm_format_modifier_properties = props.as_mut_ptr();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: read-only query; `props`/`list`/`fp2` outlive the call.
    unsafe { instance.get_physical_device_format_properties2(pdev, fmt, &mut fp2) };
    props.truncate(list.drm_format_modifier_count as usize);
    let one_plane_sampled = props.iter().any(|p| {
        p.drm_format_modifier == modifier
            && p.drm_format_modifier_plane_count == 1
            && p.drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
    });
    if !one_plane_sampled {
        return false;
    }

    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(fmt)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::SAMPLED)
        .flags(vk::ImageCreateFlags::empty())
        .push_next(&mut modifier_info)
        .push_next(&mut external);
    let mut ext_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
    // SAFETY: `instance`/`pdev` are live and paired by the caller's contract;
    // the pNext chain outlives the call.
    unsafe {
        instance
            .get_physical_device_image_format_properties2(pdev, &info, &mut props)
            .is_ok()
            && ext_props
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    }
}

/// (format, modifier) importability answers. Immutable per device — the
/// physical-device queries run once per pair, not per frame, and refusals are
/// cached too.
#[derive(Default)]
pub(crate) struct ModifierCache {
    supported: std::collections::HashMap<(i32, u64), bool>,
}

impl ModifierCache {
    fn importable(
        &mut self,
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        fmt: vk::Format,
        modifier: u64,
    ) -> bool {
        *self
            .supported
            .entry((fmt.as_raw(), modifier))
            .or_insert_with(|| {
                // SAFETY: `instance`/`pdev` are live and paired by the caller's contract.
                unsafe { modifier_importable(instance, pdev, fmt, modifier) }
            })
    }
}

/// Visible/coded scale per axis — the fraction of the imported (coded) image
/// the visible picture occupies.
fn crop_scale(width: u32, height: u32, coded_width: u32, coded_height: u32) -> [f32; 2] {
    [
        width as f32 / coded_width as f32,
        height as f32 / coded_height as f32,
    ]
}

/// Frame bound to its cached plane images. GPU reads outlive submit: park until
/// the fence signals, then [`HwFrame::destroy`] (drops the decoder surface guard;
/// the images stay in [`ImportCache`]).
pub struct HwFrame {
    pub luma_view: vk::ImageView,
    pub chroma_view: vk::ImageView,
    pub color: pf_client_core::video::ColorDesc,
    /// Visible picture extent; the imported images are [`Self::coded_width`] ×
    /// [`Self::coded_height`], so sampling must crop ([`Self::uv_scale`]).
    pub width: u32,
    pub height: u32,
    /// Decode-complete semaphores the sampling submit must wait (binary, temporary
    /// payloads from the frame's sync_files). Empty when the decode was CPU-waited.
    pub sync_sems: Vec<vk::Semaphore>,
    /// Exported surface extent the plane images were created at.
    coded_width: u32,
    coded_height: u32,
    /// Fourcc. CSC picks its P010 vs 8-bit rows off this.
    fourcc: u32,
    images: [vk::Image; 2],
    /// Pool generation (high half of the frame's `pool_key`).
    generation: u64,
    _guard: DrmFrameGuard,
}

impl HwFrame {
    pub fn is_p010(&self) -> bool {
        self.fourcc == DRM_FORMAT_P010
    }

    /// UV scale cropping the coded-extent images to the visible picture.
    pub fn uv_scale(&self) -> [f32; 2] {
        crop_scale(self.width, self.height, self.coded_width, self.coded_height)
    }

    /// Plane images for the presenter's foreign-acquire barriers.
    pub fn luma_image(&self) -> vk::Image {
        self.images[0]
    }

    pub fn chroma_image(&self) -> vk::Image {
        self.images[1]
    }

    /// The decoder pool this frame's surface belongs to.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Release the decoder surface. The plane images belong to the cache, so
    /// nothing Vulkan is destroyed here. Only after the frame's fence signaled.
    pub fn destroy(self, _device: &ash::Device) {
        // `_guard` drops after the GPU reads: the VAAPI surface stays mapped until here.
    }
}

/// One decoder surface's plane images, imported once and reused every time the
/// surface comes round. An import costs two image creates, two memory imports and
/// a mapping each; a pool of ~17 surfaces at 4K did that per frame before.
struct Planes {
    generation: u64,
    coded_width: u32,
    coded_height: u32,
    fourcc: u32,
    modifier: u64,
    layout: [(u32, u32); 2],
    images: [vk::Image; 2],
    memories: [vk::DeviceMemory; 2],
    views: [vk::ImageView; 2],
}

impl Planes {
    fn destroy(self, device: &ash::Device) {
        // SAFETY: handles owned by `self`; the caller waited the last fence that
        // sampled them.
        unsafe {
            for v in self.views {
                device.destroy_image_view(v, None);
            }
            for i in self.images {
                device.destroy_image(i, None);
            }
            for m in self.memories {
                device.free_memory(m, None);
            }
        }
    }
}

/// Imported plane images keyed by the decoder's `pool_key`.
#[derive(Default)]
pub(crate) struct ImportCache {
    planes: std::collections::HashMap<u64, Planes>,
}

impl ImportCache {
    /// Drop every pool but `generation`. Only after the fence wait, with no frame of
    /// an older pool parked.
    pub(crate) fn retire_stale(&mut self, device: &ash::Device, generation: u64) {
        if self.planes.values().all(|p| p.generation == generation) {
            return;
        }
        let stale: Vec<u64> = self
            .planes
            .iter()
            .filter(|(_, p)| p.generation != generation)
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            if let Some(p) = self.planes.remove(&k) {
                p.destroy(device);
            }
        }
    }

    /// Drop every entry. GPU idle on them: after the fence wait or a device wait.
    pub(crate) fn destroy_all(&mut self, device: &ash::Device) {
        for (_, p) in self.planes.drain() {
            p.destroy(device);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.planes.is_empty()
    }
}

/// Sync_file → binary semaphore imports (`VK_KHR_external_semaphore_fd`). A
/// temporary payload is consumed by one wait, so a small ring outlives the frames
/// in flight.
pub(crate) struct SyncImport {
    ext: ash::khr::external_semaphore_fd::Device,
    ring: Vec<vk::Semaphore>,
    next: usize,
}

impl SyncImport {
    /// Enough for four frames in flight with two objects each.
    const RING: usize = 8;

    /// `None` when the device cannot import a `SYNC_FD` semaphore.
    pub(crate) fn new(
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        device: &ash::Device,
    ) -> Option<Self> {
        let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let mut props = vk::ExternalSemaphoreProperties::default();
        // SAFETY: read-only query on a live instance/pdev; locals outlive the call.
        unsafe {
            instance.get_physical_device_external_semaphore_properties(pdev, &info, &mut props)
        };
        if !props
            .external_semaphore_features
            .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)
        {
            return None;
        }
        let mut ring = Vec::with_capacity(Self::RING);
        for _ in 0..Self::RING {
            // SAFETY: create on a live device; the info local outlives the call.
            match unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) } {
                Ok(s) => ring.push(s),
                Err(_) => {
                    for s in ring {
                        // SAFETY: created just above, never submitted.
                        unsafe { device.destroy_semaphore(s, None) };
                    }
                    return None;
                }
            }
        }
        Some(Self {
            ext: ash::khr::external_semaphore_fd::Device::new(instance, device),
            ring,
            next: 0,
        })
    }

    /// Import `sync` as the next ring semaphore's temporary payload. Vulkan owns the
    /// fd it is given, so this dups.
    fn import(&mut self, sync: &std::os::fd::OwnedFd) -> Result<vk::Semaphore> {
        let sem = self.ring[self.next];
        self.next = (self.next + 1) % self.ring.len();
        let owned = sync.try_clone().context("dup sync_file")?;
        let info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(sem)
            .flags(vk::SemaphoreImportFlags::TEMPORARY)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
            .fd(owned.as_raw_fd());
        // SAFETY: `sem` is a ring semaphore whose last wait completed (the ring is
        // deeper than the frames in flight); `info` outlives the call.
        unsafe { self.ext.import_semaphore_fd(&info) }.context("vkImportSemaphoreFdKHR")?;
        // The driver took the dup on success; `?` above still closes it.
        let _ = owned.into_raw_fd();
        Ok(sem)
    }

    pub(crate) fn destroy(self, device: &ash::Device) {
        for s in self.ring {
            // SAFETY: owned semaphores; the caller idled the device.
            unsafe { device.destroy_semaphore(s, None) };
        }
    }
}

/// Bind `frame` to its cached plane images, importing on first sight, and turn its
/// sync_files into semaphores (or wait them here when the device cannot import).
/// An unimportable modifier, or a driver rejection, is a clean error; the caller
/// demotes.
pub(crate) fn get_or_import(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    device: &ash::Device,
    ext_mem_fd: &ash::khr::external_memory_fd::Device,
    modifiers: &mut ModifierCache,
    cache: &mut ImportCache,
    sync: Option<&mut SyncImport>,
    frame: DmabufFrame,
) -> Result<HwFrame> {
    let generation = frame.pool_key >> 32;
    let layout = [
        frame
            .planes
            .first()
            .map_or((0, 0), |p| (p.offset, p.stride)),
        frame.planes.get(1).map_or((0, 0), |p| (p.offset, p.stride)),
    ];
    let hit = cache.planes.get(&frame.pool_key).is_some_and(|p| {
        p.coded_width == frame.coded_width
            && p.coded_height == frame.coded_height
            && p.fourcc == frame.fourcc
            && p.modifier == frame.modifier
            && p.layout == layout
    });
    if !hit {
        let planes = import(instance, pdev, device, ext_mem_fd, modifiers, &frame)?;
        if let Some(old) = cache.planes.insert(frame.pool_key, planes) {
            old.destroy(device);
        }
    }
    let p = &cache.planes[&frame.pool_key];
    let mut sync_sems = Vec::new();
    match sync {
        Some(s) => {
            for fd in &frame.sync_fds {
                sync_sems.push(s.import(fd)?);
            }
        }
        None => {
            for fd in &frame.sync_fds {
                // 100 ms fail-open: a decode this late is a stalled GPU, not a race worth a hang.
                let _ = pf_zerocopy::dmabuf_fence::wait_sync_file(fd.as_raw_fd(), 100);
            }
        }
    }
    Ok(HwFrame {
        luma_view: p.views[0],
        chroma_view: p.views[1],
        color: frame.color,
        width: frame.width,
        height: frame.height,
        sync_sems,
        coded_width: frame.coded_width,
        coded_height: frame.coded_height,
        fourcc: frame.fourcc,
        images: p.images,
        generation,
        _guard: frame.guard,
    })
}

/// Import both planes at the exported (coded) extent; [`HwFrame::uv_scale`]
/// crops sampling to the visible picture.
fn import(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    device: &ash::Device,
    ext_mem_fd: &ash::khr::external_memory_fd::Device,
    cache: &mut ModifierCache,
    frame: &DmabufFrame,
) -> Result<Planes> {
    // Test hook: fault every import so demotion is exercisable without a broken
    // driver. Per-frame lookup is fine — demotion silences it within three frames.
    if std::env::var_os("PUNKTFUNK_HW_FAULT").is_some_and(|v| v == "import") {
        bail!("injected import failure (PUNKTFUNK_HW_FAULT=import)");
    }
    let (luma_fmt, chroma_fmt, chroma_full_res) = match frame.fourcc {
        DRM_FORMAT_NV12 => (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM, false),
        DRM_FORMAT_P010 => (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM, false),
        DRM_FORMAT_NV24 => (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM, true),
        other => bail!("hw presenter handles NV12/P010/NV24 only (got {other:#x})"),
    };
    if frame.planes.len() != 2 {
        bail!(
            "2-plane YCbCr needs exactly 2 planes (got {})",
            frame.planes.len()
        );
    }
    if frame.width == 0
        || frame.height == 0
        || frame.coded_width < frame.width
        || frame.coded_height < frame.height
    {
        bail!(
            "dmabuf extent must be nonzero with visible <= coded (got {}x{} in {}x{})",
            frame.width,
            frame.height,
            frame.coded_width,
            frame.coded_height
        );
    }
    let modifier = explicit_modifier(frame.modifier)?;
    // The export's modifier is only legal if this device can import it; a
    // foreign GPU's tiling must refuse here, not at image create.
    for fmt in [luma_fmt, chroma_fmt] {
        if !cache.importable(instance, pdev, fmt, modifier) {
            bail!("dmabuf modifier {modifier:#x} not importable as sampled {fmt:?} on this device");
        }
    }

    // Plane images take the exported extent: tiled addressing is defined over
    // the coded size, so a visible-only image would misaddress the tail rows.
    let y = &frame.planes[0];
    let c = &frame.planes[1];
    let (luma_img, luma_mem) = plane_image(
        device,
        ext_mem_fd,
        frame.coded_width,
        frame.coded_height,
        luma_fmt,
        y.fd,
        y.offset,
        y.stride,
        modifier,
    )
    .context("luma plane")?;
    let (cw, ch) = if chroma_full_res {
        (frame.coded_width, frame.coded_height)
    } else {
        (
            frame.coded_width.div_ceil(2),
            frame.coded_height.div_ceil(2),
        )
    };
    let (chroma_img, chroma_mem) = match plane_image(
        device, ext_mem_fd, cw, ch, chroma_fmt, c.fd, c.offset, c.stride, modifier,
    )
    .context("chroma plane")
    {
        Ok(r) => r,
        Err(e) => {
            // SAFETY: `luma_img` / `luma_mem` were created in this call and never
            // submitted, so the GPU is idle on them.
            unsafe {
                device.destroy_image(luma_img, None);
                device.free_memory(luma_mem, None);
            }
            return Err(e);
        }
    };

    let view = |image, format| {
        // SAFETY: `image` is owned by this function; the create-info locals
        // outlive the call.
        unsafe {
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .layer_count(1),
                    ),
                None,
            )
        }
        .context("plane image view")
    };
    // SAFETY: luma/chroma image and memory were created in this call and never
    // submitted, so the GPU is idle on them.
    let destroy_images = |views: &[vk::ImageView]| unsafe {
        for v in views {
            device.destroy_image_view(*v, None);
        }
        device.destroy_image(luma_img, None);
        device.destroy_image(chroma_img, None);
        device.free_memory(luma_mem, None);
        device.free_memory(chroma_mem, None);
    };
    let luma_view = match view(luma_img, luma_fmt) {
        Ok(v) => v,
        Err(e) => {
            destroy_images(&[]);
            return Err(e);
        }
    };
    let chroma_view = match view(chroma_img, chroma_fmt) {
        Ok(v) => v,
        Err(e) => {
            destroy_images(&[luma_view]);
            return Err(e);
        }
    };

    Ok(Planes {
        generation: frame.pool_key >> 32,
        coded_width: frame.coded_width,
        coded_height: frame.coded_height,
        fourcc: frame.fourcc,
        modifier: frame.modifier,
        layout: [(y.offset, y.stride), (c.offset, c.stride)],
        images: [luma_img, chroma_img],
        memories: [luma_mem, chroma_mem],
        views: [luma_view, chroma_view],
    })
}

/// One plane as an explicit-modifier image. Vulkan takes the fd it is given,
/// so this dups; the frame guard keeps the original.
#[allow(clippy::too_many_arguments)]
fn plane_image(
    device: &ash::Device,
    ext_mem_fd: &ash::khr::external_memory_fd::Device,
    width: u32,
    height: u32,
    format: vk::Format,
    fd: std::os::fd::RawFd,
    offset: u32,
    stride: u32,
    modifier: u64,
) -> Result<(vk::Image, vk::DeviceMemory)> {
    let plane_layouts = [vk::SubresourceLayout {
        offset: u64::from(offset),
        size: 0, // 0 on import: the driver derives size.
        row_pitch: u64::from(stride),
        array_pitch: 0,
        depth_pitch: 0,
    }];
    let mut modifier_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(modifier)
        .plane_layouts(&plane_layouts);
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // SAFETY: create-info and pNext locals (`modifier_info`, `external_info`)
    // outlive the call.
    let image = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .push_next(&mut modifier_info)
                .push_next(&mut external_info)
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }
    .with_context(|| {
        format!("create {width}x{height} {format:?} image (modifier {modifier:#018x})")
    })?;

    let result = (|| {
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // SAFETY: `fd` is a live plane fd of the caller's `DmabufFrame`;
        // `fd_props` is a local outliving the call.
        unsafe {
            ext_mem_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut fd_props,
            )
        }
        .context("vkGetMemoryFdPropertiesKHR")?;
        // SAFETY: `image` was created above and has not been destroyed.
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        let bits = reqs.memory_type_bits & fd_props.memory_type_bits;
        let type_index = (0..32u32)
            .find(|i| bits & (1 << i) != 0)
            .context("no importable memory type for dmabuf")?;

        // Vulkan owns the fd it imports — dup so the decoder guard keeps the original.
        // SAFETY: `fd` is a plane fd of the caller's `DmabufFrame`. `DrmFrameGuard`
        // keeps those fds open until `import` moves it onto `HwFrame`. The borrow
        // ends at `try_clone_to_owned`.
        let owned = unsafe { BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .context("dup dmabuf fd")?;
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(owned.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        // SAFETY: `import_info` and `dedicated` are locals that outlive the call.
        // `import_info.fd` is `owned`'s dup, still open.
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .push_next(&mut import_info)
                    .push_next(&mut dedicated)
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        }
        .context("import dmabuf memory")?;
        // Vulkan takes the fd only on a successful import. `into_raw_fd` here;
        // `?` above still closes the dup.
        let _ = owned.into_raw_fd();
        // SAFETY: `image` and `memory` were created above and are still owned here.
        if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
            // SAFETY: `memory` was allocated in this call and never bound, so
            // the GPU is idle on it.
            unsafe { device.free_memory(memory, None) };
            return Err(e).context("bind imported memory");
        }
        Ok(memory)
    })();

    match result {
        Ok(memory) => Ok((image, memory)),
        Err(e) => {
            // SAFETY: `image` was created in this call and never bound, so the
            // GPU is idle on it.
            unsafe { device.destroy_image(image, None) };
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_modifier_is_never_guessed_linear() {
        assert!(explicit_modifier(DRM_FORMAT_MOD_INVALID).is_err());
        assert_eq!(explicit_modifier(0).unwrap(), 0);
    }

    #[test]
    fn visible_picture_crops_the_coded_surface() {
        assert_eq!(crop_scale(1920, 1080, 1920, 1088), [1.0, 1080.0 / 1088.0]);
    }
}
