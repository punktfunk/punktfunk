//! Vulkan helpers the LINEAR bridge ([`super::vulkan`]), the NVENC slot blend
//! ([`super::vkslot`]) and pf-encode's Vulkan encoders share: the NVIDIA compute device CUDA
//! runs on, the memory-type pick, a memory map held as a borrow, image and buffer owners that
//! carry their memory, and the explicit-modifier dmabuf import.

use anyhow::{anyhow, ensure, Context, Result};
use ash::vk;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// NVIDIA's PCI vendor id.
const NVIDIA: u32 = 0x10DE;

/// What a caller enables beyond `VK_KHR_external_memory_fd`.
#[derive(Clone, Copy, Default)]
pub(crate) struct DeviceWants {
    /// Require `VK_EXT_external_memory_dma_buf`.
    pub dma_buf: bool,
    /// Enable `VK_EXT_image_drm_format_modifier` + `VK_KHR_image_format_list` when present.
    pub modifiers: bool,
}

/// An opened device. The caller owns the handles and destroys the device, then the instance.
pub(crate) struct NvComputeDevice {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub device: ash::Device,
    pub phys: vk::PhysicalDevice,
    /// The first compute family; compute implies transfer.
    pub queue_family: u32,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    /// The DRM-modifier image extensions are on.
    pub modifier_import: bool,
    /// Timeline semaphores are on and export as OPAQUE_FD.
    pub timeline_export: bool,
    /// The global priority the queue was granted; `None` is the default priority.
    pub priority: Option<vk::QueueGlobalPriorityKHR>,
}

impl NvComputeDevice {
    /// Open the device. `priorities` is the global-priority ladder to try, strongest first; a
    /// class the driver refuses steps down, ending at the default priority.
    pub(crate) fn open(
        wants: DeviceWants,
        priorities: &[vk::QueueGlobalPriorityKHR],
    ) -> Result<NvComputeDevice> {
        // SAFETY: `Entry::load` dlopens the system loader, whose entry points match ash's
        // bindings. The create info is a local that outlives the synchronous call.
        let (entry, instance) = unsafe {
            let entry = ash::Entry::load().context("load libvulkan")?;
            let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default().application_info(&app),
                    None,
                )
                .context("vkCreateInstance")?;
            (entry, instance)
        };
        match open_on(&instance, wants, priorities) {
            Ok(o) => Ok(NvComputeDevice {
                entry,
                instance,
                device: o.device,
                phys: o.phys,
                queue_family: o.queue_family,
                mem_props: o.mem_props,
                modifier_import: o.modifier_import,
                timeline_export: o.timeline_export,
                priority: o.priority,
            }),
            Err(e) => {
                // SAFETY: nothing else holds the instance yet, and no device was created on it.
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }
}

struct Opened {
    device: ash::Device,
    phys: vk::PhysicalDevice,
    queue_family: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    modifier_import: bool,
    timeline_export: bool,
    priority: Option<vk::QueueGlobalPriorityKHR>,
}

fn open_on(
    instance: &ash::Instance,
    wants: DeviceWants,
    priorities: &[vk::QueueGlobalPriorityKHR],
) -> Result<Opened> {
    // SAFETY: every call queries or creates on the live `instance` with local create infos that
    // outlive each synchronous call; the ladder rebuilds them per attempt.
    unsafe {
        let phys = cuda_physical_device(instance)?;
        let mem_props = instance.get_physical_device_memory_properties(phys);
        let queue_family = instance
            .get_physical_device_queue_family_properties(phys)
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .ok_or_else(|| anyhow!("no compute-capable queue family"))?
            as u32;
        let dev_exts = instance
            .enumerate_device_extension_properties(phys)
            .unwrap_or_default();
        let has = |name: &std::ffi::CStr| {
            dev_exts
                .iter()
                .any(|p| p.extension_name_as_c_str() == Ok(name))
        };
        let modifier_import = wants.modifiers
            && has(ash::ext::image_drm_format_modifier::NAME)
            && has(ash::khr::image_format_list::NAME);
        let timeline_export = has(ash::khr::timeline_semaphore::NAME)
            && has(ash::khr::external_semaphore_fd::NAME)
            && {
                let mut tl = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
                let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut tl);
                instance.get_physical_device_features2(phys, &mut f2);
                tl.timeline_semaphore == vk::TRUE
            };
        // KHR is the promoted name; fall back to EXT. Neither: default priority only.
        let priority_ext = [vk::KHR_GLOBAL_PRIORITY_NAME, vk::EXT_GLOBAL_PRIORITY_NAME]
            .into_iter()
            .find(|&name| has(name));
        let mut ladder = priority_ext
            .map_or(&[][..], |_| priorities)
            .iter()
            .copied()
            .map(Some)
            .chain([None]);
        loop {
            let priority = ladder
                .next()
                .expect("the ladder ends at the default priority");
            let prio = [1.0f32];
            let mut gp_info = vk::DeviceQueueGlobalPriorityCreateInfoKHR::default()
                .global_priority(priority.unwrap_or(vk::QueueGlobalPriorityKHR::MEDIUM));
            let mut qci0 = vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&prio);
            let mut exts = vec![ash::khr::external_memory_fd::NAME.as_ptr()];
            if wants.dma_buf {
                exts.push(ash::ext::external_memory_dma_buf::NAME.as_ptr());
            }
            if modifier_import {
                exts.push(ash::ext::image_drm_format_modifier::NAME.as_ptr());
                exts.push(ash::khr::image_format_list::NAME.as_ptr());
            }
            if timeline_export {
                exts.push(ash::khr::timeline_semaphore::NAME.as_ptr());
                exts.push(ash::khr::external_semaphore_fd::NAME.as_ptr());
            }
            if let (Some(_), Some(ext)) = (priority, priority_ext) {
                qci0 = qci0.push_next(&mut gp_info);
                exts.push(ext.as_ptr());
            }
            let mut tl_enable =
                vk::PhysicalDeviceTimelineSemaphoreFeatures::default().timeline_semaphore(true);
            let qci = [qci0];
            let mut dci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&qci)
                .enabled_extension_names(&exts);
            if timeline_export {
                dci = dci.push_next(&mut tl_enable);
            }
            match instance.create_device(phys, &dci, None) {
                Ok(device) => {
                    return Ok(Opened {
                        device,
                        phys,
                        queue_family,
                        mem_props,
                        modifier_import,
                        timeline_export,
                        priority,
                    })
                }
                Err(e) if priority.is_some() && priority_refused(e) => {
                    tracing::debug!(
                        ?priority,
                        "global-priority queue not permitted — stepping down"
                    );
                }
                Err(e) => {
                    return Err(e).context("vkCreateDevice (external-memory extensions supported?)")
                }
            }
        }
    }
}

/// `create_device` refused the requested global-priority class: step down the ladder rather
/// than fail the open. `ERROR_NOT_PERMITTED_KHR` is the specified refusal;
/// `ERROR_INITIALIZATION_FAILED` counts as one too.
pub fn priority_refused(e: vk::Result) -> bool {
    matches!(
        e,
        vk::Result::ERROR_NOT_PERMITTED_KHR | vk::Result::ERROR_INITIALIZATION_FAILED
    )
}

/// The Vulkan device CUDA device 0 runs on, matched by `deviceUUID`, so OPAQUE_FD and GL
/// interop stay on one GPU. The first NVIDIA device when CUDA cannot name its UUID.
///
/// # Safety
/// `instance` is a live Vulkan 1.1 instance.
unsafe fn cuda_physical_device(instance: &ash::Instance) -> Result<vk::PhysicalDevice> {
    // SAFETY: caller contract; the properties chain is a local that outlives each query.
    let devices: Vec<_> = unsafe {
        instance
            .enumerate_physical_devices()
            .context("enumerate GPUs")?
            .into_iter()
            .map(|p| {
                let mut id = vk::PhysicalDeviceIDProperties::default();
                let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                instance.get_physical_device_properties2(p, &mut props);
                (p, props.properties.vendor_id, id.device_uuid)
            })
            .collect()
    };
    pick_cuda_device(&devices, super::cuda::device_uuid().ok())
        .ok_or_else(|| anyhow!("no NVIDIA Vulkan device"))
}

/// `(handle, vendor, uuid)` entries: the one with `cuda_uuid`, else the first NVIDIA one.
fn pick_cuda_device<T: Copy>(
    devices: &[(T, u32, [u8; 16])],
    cuda_uuid: Option<[u8; 16]>,
) -> Option<T> {
    cuda_uuid
        .and_then(|want| devices.iter().find(|d| d.2 == want))
        .or_else(|| devices.iter().find(|d| d.1 == NVIDIA))
        .map(|d| d.0)
}

/// The first memory type in `type_bits` with every flag in `flags`. A miss is an error, never
/// index 0: that type may sit outside `type_bits` or lack a flag the caller relies on.
pub fn memory_type(
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    flags: vk::MemoryPropertyFlags,
) -> Result<u32> {
    (0..mem_props.memory_type_count)
        .find(|&i| {
            type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
        .ok_or_else(|| anyhow!("no Vulkan memory type with {flags:?} in bits {type_bits:#x}"))
}

/// Host-visible device memory, mapped from its start. Derefs to the bytes; `vkUnmapMemory`
/// runs on drop, so no early return strands the mapping.
pub struct MappedMemory<'a> {
    device: &'a ash::Device,
    memory: vk::DeviceMemory,
    ptr: NonNull<u8>,
    len: usize,
}

impl<'a> MappedMemory<'a> {
    /// Map `memory` and view its first `len` bytes.
    ///
    /// # Safety
    /// `memory` is a live, unmapped `HOST_VISIBLE` allocation of `device`, at least `len` bytes.
    /// Until the guard drops, nothing frees or maps it and no submitted work writes the range.
    pub unsafe fn map(
        device: &'a ash::Device,
        memory: vk::DeviceMemory,
        len: usize,
    ) -> ash::prelude::VkResult<Self> {
        let flags = vk::MemoryMapFlags::empty();
        // SAFETY: the caller's contract. `WHOLE_SIZE` because Vulkan refuses a zero size.
        let ptr = unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, flags) }?;
        let Some(ptr) = NonNull::new(ptr.cast::<u8>()) else {
            // SAFETY: the map above succeeded.
            unsafe { device.unmap_memory(memory) };
            return Err(vk::Result::ERROR_MEMORY_MAP_FAILED);
        };
        Ok(Self {
            device,
            memory,
            ptr,
            len,
        })
    }
}

impl Deref for MappedMemory<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `map`'s caller vouched for `len` mapped bytes that nothing else writes, and
        // they stay mapped until `drop`.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for MappedMemory<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `deref`, and `&mut self` makes this the only view of the bytes.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MappedMemory<'_> {
    fn drop(&mut self) {
        // SAFETY: `self` holds the one mapping of `memory`, which the caller keeps alive.
        unsafe { self.device.unmap_memory(self.memory) };
    }
}

/// An image, the dedicated memory bound to it, and a view of it (null when it has none).
/// No `Drop`: the owner destroys it once the GPU is idle and before the device, so a field
/// left out of its teardown leaks and never dangles.
#[derive(Clone, Copy, Default)]
pub struct VkImageAlloc {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
}

impl VkImageAlloc {
    /// Destroy the view, then the image, then free the memory. Null handles are skipped.
    ///
    /// # Safety
    /// The handles belong to `device`, nothing queued still uses them, and `self` is not
    /// destroyed twice.
    pub unsafe fn destroy(self, device: &ash::Device) {
        // SAFETY: caller contract; each handle is destroyed once, dependents first.
        unsafe {
            if self.view != vk::ImageView::null() {
                device.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                device.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                device.free_memory(self.memory, None);
            }
        }
    }
}

/// A buffer and the dedicated memory bound to it. Owned like [`VkImageAlloc`].
#[derive(Clone, Copy, Default)]
pub struct VkBufferAlloc {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
}

impl VkBufferAlloc {
    /// Destroy the buffer, then free the memory (which unmaps a persistent mapping). Null
    /// handles are skipped.
    ///
    /// # Safety
    /// As [`VkImageAlloc::destroy`].
    pub unsafe fn destroy(self, device: &ash::Device) {
        // SAFETY: caller contract; each handle is destroyed once, the buffer first.
        unsafe {
            if self.buffer != vk::Buffer::null() {
                device.destroy_buffer(self.buffer, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                device.free_memory(self.memory, None);
            }
        }
    }
}

/// A dmabuf to import as a `TYPE_2D` image with an explicit DRM modifier layout.
pub struct DmabufImage<'a> {
    /// Borrowed: the import takes a dup of it.
    pub fd: std::os::fd::BorrowedFd<'a>,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    /// One `(offset, row_pitch)` layout per memory plane of `format`.
    pub planes: &'a [vk::SubresourceLayout],
    pub usage: vk::ImageUsageFlags,
}

/// Every DRM modifier `pd` lists for `format`, in driver order.
///
/// # Safety
/// `instance`/`pd` must be live and paired.
pub unsafe fn format_modifiers(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<vk::DrmFormatModifierPropertiesEXT> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut props = Vec::new();
    // SAFETY: caller contract; the count query fills `list`, `props` is sized to it before
    // the second query writes through `p_drm_format_modifier_properties`.
    unsafe {
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(pd, format, &mut fp2);
        props.resize(
            list.drm_format_modifier_count as usize,
            vk::DrmFormatModifierPropertiesEXT::default(),
        );
        list.p_drm_format_modifier_properties = props.as_mut_ptr();
        let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
        instance.get_physical_device_format_properties2(pd, format, &mut fp2);
    }
    props.truncate(list.drm_format_modifier_count as usize);
    props
}

/// The memory-plane count `pd` lists for `modifier` on `format`; `None` when it lists none.
///
/// # Safety
/// `instance`/`pd` must be live and paired.
unsafe fn modifier_plane_count(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
) -> Option<u32> {
    // SAFETY: caller contract.
    unsafe { format_modifiers(instance, pd, format) }
        .iter()
        .find(|p| p.drm_format_modifier == modifier)
        .map(|p| p.drm_format_modifier_plane_count)
}

/// The modifiers the CUDA device imports as a one-plane SAMPLED image of `fmt`, in driver order:
/// the check [`import_dmabuf_image`] makes before it creates the fused convert's source image.
/// Empty when no loader, instance or NVIDIA device answers.
pub fn sampled_import_modifiers(fmt: vk::Format) -> Vec<u64> {
    // SAFETY: `Entry::load` dlopens the system loader, whose entry points match ash's bindings.
    // The instance is fresh, serves only physical-device queries and is destroyed before return.
    unsafe {
        let Ok(entry) = ash::Entry::load() else {
            return Vec::new();
        };
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
        let Ok(instance) = entry.create_instance(
            &vk::InstanceCreateInfo::default().application_info(&app),
            None,
        ) else {
            return Vec::new();
        };
        let mods = cuda_physical_device(&instance)
            .map(|pd| {
                format_modifiers(&instance, pd, fmt)
                    .iter()
                    .filter(|p| p.drm_format_modifier_plane_count == 1)
                    .map(|p| p.drm_format_modifier)
                    .filter(|&m| {
                        modifier_importable(
                            &instance,
                            pd,
                            fmt,
                            m,
                            vk::ImageUsageFlags::SAMPLED,
                            None,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        instance.destroy_instance(None);
        mods
    }
}

/// Whether `pd` accepts a dmabuf import of `fmt` tiled as `modifier` with `usage`, under the
/// video profiles in `profile_list` when given. IMPORTABLE on the external properties is the
/// whole answer.
///
/// # Safety
/// `instance`/`pd` must be live and paired; `profile_list` must be fully wired.
pub unsafe fn modifier_importable(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    fmt: vk::Format,
    modifier: u64,
    usage: vk::ImageUsageFlags,
    profile_list: Option<&vk::VideoProfileListInfoKHR>,
) -> bool {
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // A copy: chaining the caller's list would leave its `p_next` on this frame's structs.
    let mut plist = profile_list.map(|l| vk::VideoProfileListInfoKHR {
        profile_count: l.profile_count,
        p_profiles: l.p_profiles,
        ..Default::default()
    });
    let mut info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(fmt)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(usage)
        .flags(vk::ImageCreateFlags::empty())
        .push_next(&mut modifier_info)
        .push_next(&mut external);
    if let Some(plist) = plist.as_mut() {
        info = info.push_next(plist);
    }
    let mut ext_props = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
    // SAFETY: caller contract; every struct in the chain is a local outliving the query.
    let queried =
        unsafe { instance.get_physical_device_image_format_properties2(pd, &info, &mut props) };
    queried.is_ok()
        && ext_props
            .external_memory_properties
            .external_memory_features
            .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
}

/// Import `size` bytes of `fd` as a buffer bound to its own dedicated memory: a LINEAR dmabuf's
/// bytes, for a consumer that reads rows at the producer's stride and not through an image
/// whose pitch the driver picks.
///
/// The import takes a dup of `fd`; Vulkan owns the dup once memory allocation succeeds. On
/// failure every handle this call created is destroyed. The caller destroys the returned alloc.
///
/// # Safety
/// `device` and `ext_fd` belong to the live device `mem_props` describes, with
/// `VK_EXT_external_memory_dma_buf` enabled.
pub unsafe fn import_dmabuf_buffer(
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    fd: std::os::fd::BorrowedFd<'_>,
    size: u64,
    usage: vk::BufferUsageFlags,
) -> Result<VkBufferAlloc> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let dup = fd.try_clone_to_owned().context("dup dmabuf fd")?;
    let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    // SAFETY: caller contract for `device`/`ext_fd`; every create info is a local that outlives
    // the synchronous call reading it, and each failure destroys what this call created.
    unsafe {
        let buffer = device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(usage)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut ext),
                None,
            )
            .context("create dmabuf buffer")?;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // Borrow-only; a failure leaves no fd types, and the buffer requirements decide.
        let _ = ext_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            dup.as_raw_fd(),
            &mut fd_props,
        );
        let reqs = device.get_buffer_memory_requirements(buffer);
        let bits = match reqs.memory_type_bits & fd_props.memory_type_bits {
            0 => reqs.memory_type_bits,
            bits => bits,
        };
        let mem_type = match memory_type(mem_props, bits, vk::MemoryPropertyFlags::empty()) {
            Ok(t) => t,
            Err(e) => {
                device.destroy_buffer(buffer, None);
                return Err(e); // `dup` drops: nothing imported it
            }
        };
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let memory = match device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size.max(size))
                .memory_type_index(mem_type)
                .push_next(&mut import)
                .push_next(&mut dedicated),
            None,
        ) {
            Ok(m) => {
                let _ = dup.into_raw_fd(); // Vulkan owns the dup now
                m
            }
            Err(e) => {
                device.destroy_buffer(buffer, None);
                return Err(e).context("import dmabuf memory (buffer)"); // `dup` closes once
            }
        };
        let alloc = VkBufferAlloc { buffer, memory };
        if let Err(e) = device.bind_buffer_memory(buffer, memory, 0) {
            alloc.destroy(device); // the memory owns the imported fd
            return Err(e).context("bind dmabuf buffer memory");
        }
        Ok(alloc)
    }
}

/// Import `src` as an image bound to its own dedicated memory. The memory type comes from the
/// fd's properties intersected with the image's requirements, or the requirements alone when
/// the driver reports no fd types. `profile_list` chains a video profile into the image.
///
/// The modifier, its plane count and the usage are checked against `pd` first: `vkCreateImage`
/// with a layout the format does not list is invalid usage, not a clean failure.
///
/// The import takes a dup of `src.fd`: Vulkan owns the dup only once memory allocation
/// succeeds, and `vkFreeMemory` then closes it. On failure every handle this call created is
/// destroyed, and the caller's fd stays theirs. The returned alloc has no view; the caller
/// destroys it.
///
/// # Safety
/// `device` and `ext_fd` belong to the live device `mem_props` describes, with
/// `VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier` enabled;
/// `instance`/`pd` are the instance and physical device it was created on.
pub unsafe fn import_dmabuf_image(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    ext_fd: &ash::khr::external_memory_fd::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    src: &DmabufImage,
    profile_list: Option<&mut vk::VideoProfileListInfoKHR>,
) -> Result<VkImageAlloc> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    // SAFETY: caller contract for `instance`/`pd`; the queries read locals and `profile_list`.
    let supported = unsafe {
        modifier_plane_count(instance, pd, src.format, src.modifier)
            == Some(src.planes.len() as u32)
            && modifier_importable(
                instance,
                pd,
                src.format,
                src.modifier,
                src.usage,
                profile_list.as_deref(),
            )
    };
    ensure!(
        supported,
        "import dmabuf image: modifier {:#x} unsupported for {:?}",
        src.modifier,
        src.format
    );
    let dup = src.fd.try_clone_to_owned().context("dup dmabuf fd")?;
    let mut drm = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
        .drm_format_modifier(src.modifier)
        .plane_layouts(src.planes);
    let mut ext = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(src.format)
        .extent(vk::Extent3D {
            width: src.width,
            height: src.height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(src.usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        // External memory starts UNDEFINED; the first acquire barrier names that layout.
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut ext)
        .push_next(&mut drm);
    if let Some(pl) = profile_list {
        ci = ci.push_next(pl);
    }
    // SAFETY: caller contract for `device`/`ext_fd`; every create info is a local that outlives
    // the synchronous call reading it, and each failure destroys what this call created.
    unsafe {
        let image = device
            .create_image(&ci, None)
            .context("create dmabuf image (modifier)")?;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        // Borrow-only; a failure leaves no fd types, and the image requirements decide.
        let _ = ext_fd.get_memory_fd_properties(
            vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
            dup.as_raw_fd(),
            &mut fd_props,
        );
        let reqs = device.get_image_memory_requirements(image);
        let bits = match reqs.memory_type_bits & fd_props.memory_type_bits {
            0 => reqs.memory_type_bits,
            bits => bits,
        };
        let mem_type = match memory_type(mem_props, bits, vk::MemoryPropertyFlags::empty()) {
            Ok(t) => t,
            Err(e) => {
                device.destroy_image(image, None);
                return Err(e); // `dup` drops: nothing imported it
            }
        };
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(dup.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let memory = match device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(mem_type)
                .push_next(&mut import)
                .push_next(&mut dedicated),
            None,
        ) {
            Ok(m) => {
                let _ = dup.into_raw_fd(); // Vulkan owns the dup now
                m
            }
            Err(e) => {
                device.destroy_image(image, None);
                return Err(e).context("import dmabuf memory (image)"); // `dup` closes once
            }
        };
        let alloc = VkImageAlloc {
            image,
            memory,
            view: vk::ImageView::null(),
        };
        if let Err(e) = device.bind_image_memory(image, memory, 0) {
            alloc.destroy(device); // frees the memory, which closes the imported fd
            return Err(e).context("bind dmabuf image");
        }
        Ok(alloc)
    }
}

/// A `TYPE_2D` colour view of one layer of `image`.
///
/// # Safety
/// `image` is a live image of `device` in `format`.
pub unsafe fn color_view(
    device: &ash::Device,
    image: vk::Image,
    format: vk::Format,
    layer: u32,
) -> Result<vk::ImageView> {
    // SAFETY: caller contract; the create info is a local that outlives the call.
    unsafe {
        device
            .create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(format)
                    .subresource_range(
                        vk::ImageSubresourceRange::default()
                            .aspect_mask(vk::ImageAspectFlags::COLOR)
                            .level_count(1)
                            .base_array_layer(layer)
                            .layer_count(1),
                    ),
                None,
            )
            .context("create dmabuf image view")
    }
}

#[cfg(test)]
mod tests {
    use super::{pick_cuda_device, priority_refused, NVIDIA};
    use ash::vk;

    /// A refused class walks the ladder down; anything else must fail the open.
    #[test]
    fn only_refusals_walk_the_ladder_down() {
        assert!(priority_refused(vk::Result::ERROR_NOT_PERMITTED_KHR));
        assert!(priority_refused(vk::Result::ERROR_INITIALIZATION_FAILED));
        assert!(!priority_refused(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY));
        assert!(!priority_refused(vk::Result::ERROR_EXTENSION_NOT_PRESENT));
        assert!(!priority_refused(vk::Result::SUCCESS));
    }

    /// A dual-NVIDIA box: CUDA's device wins over enumeration order; no UUID keeps the old
    /// first-NVIDIA pick.
    #[test]
    fn the_cuda_device_wins_over_enumeration_order() {
        let devices = [
            (0, 0x8086, [1; 16]),
            (1, NVIDIA, [2; 16]),
            (2, NVIDIA, [3; 16]),
        ];
        assert_eq!(pick_cuda_device(&devices, Some([3; 16])), Some(2));
        assert_eq!(pick_cuda_device(&devices, None), Some(1));
        assert_eq!(pick_cuda_device(&devices, Some([9; 16])), Some(1));
        assert_eq!(pick_cuda_device(&devices[..1], None), None);
    }
}
