//! A two-plane picture on a DRM modifier, painted by the Vulkan device that owns a render
//! node: the buffer a compositor on that GPU hands the capture. The VA lane proves a tiled
//! planar layout against it, and the encoder tests feed it to the direct lane.

use ash::vk;
use pf_zerocopy::vkdev::{memory_type, MappedMemory};
use std::os::fd::{FromRawFd, OwnedFd};

use super::vk_util::{device_owns_node, node_pci_address, node_rdev};

/// The exported picture. The fd keeps the buffer after its Vulkan device is gone.
pub(crate) struct PlanarDmabuf {
    pub fd: OwnedFd,
    /// `(offset, pitch)` of the luma plane, then of the chroma plane.
    pub planes: [(u32, u32); 2],
}

/// What [`paint`] made on the device, destroyed in one place on every path.
#[derive(Default)]
struct Objects {
    image: vk::Image,
    memory: vk::DeviceMemory,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    pool: vk::CommandPool,
}

/// `tight` is the luma rows then the chroma rows with no padding, NV12 or P010. It is
/// painted into an image of exactly `modifier` on the device that owns `node`. `None` when
/// no Vulkan device owns the node or that driver has no such image.
pub(crate) fn paint(
    node: &std::path::Path,
    modifier: u64,
    ten_bit: bool,
    size: (u32, u32),
    tight: &[u8],
) -> Option<PlanarDmabuf> {
    let (rdev, pci) = (node_rdev(node), node_pci_address(node));
    let exts = [
        ash::khr::external_memory_fd::NAME.as_ptr(),
        ash::ext::external_memory_dma_buf::NAME.as_ptr(),
        ash::ext::image_drm_format_modifier::NAME.as_ptr(),
    ];
    let prio = [1.0f32];
    let queues = [vk::DeviceQueueCreateInfo::default()
        .queue_family_index(0)
        .queue_priorities(&prio)];
    // SAFETY: a fresh instance and device this call owns. Every create-info outlives the call
    // that reads it, and each handle is destroyed below before its parent.
    unsafe {
        let entry = ash::Entry::load().ok()?;
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
        let instance = entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
            .ok()?;
        let owner = instance
            .enumerate_physical_devices()
            .unwrap_or_default()
            .into_iter()
            .find(|&pd| device_owns_node(&instance, pd, rdev, pci));
        let device = owner.and_then(|pd| {
            let info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&exts);
            Some((pd, instance.create_device(pd, &info, None).ok()?))
        });
        let mut made = None;
        if let Some((pd, device)) = device {
            let mut objects = Objects::default();
            made = paint_on(
                &instance,
                pd,
                &device,
                &mut objects,
                modifier,
                ten_bit,
                size,
                tight,
            );
            let _ = device.device_wait_idle();
            device.destroy_command_pool(objects.pool, None);
            device.destroy_buffer(objects.staging, None);
            device.free_memory(objects.staging_memory, None);
            device.destroy_image(objects.image, None);
            device.free_memory(objects.memory, None);
            device.destroy_device(None);
        }
        instance.destroy_instance(None);
        made
    }
}

/// Create, paint and export. Each handle lands in `o` as it is made.
///
/// # Safety
/// `device` is a live device of `pd` with dma-buf export and DRM modifiers enabled, and the
/// caller destroys what `o` holds.
#[allow(clippy::too_many_arguments)]
unsafe fn paint_on(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    device: &ash::Device,
    o: &mut Objects,
    modifier: u64,
    ten_bit: bool,
    (w, h): (u32, u32),
    tight: &[u8],
) -> Option<PlanarDmabuf> {
    let dma = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
    let format = if ten_bit {
        vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16
    } else {
        vk::Format::G8_B8R8_2PLANE_420_UNORM
    };
    let extent = |width, height| vk::Extent3D {
        width,
        height,
        depth: 1,
    };
    // SAFETY: the caller's contract; every create-info is a local that outlives its call.
    unsafe {
        let mut mod_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier);
        let mut ext_info = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(dma);
        let query = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .push_next(&mut mod_info)
            .push_next(&mut ext_info);
        let mut props = vk::ImageFormatProperties2::default();
        instance
            .get_physical_device_image_format_properties2(pd, &query, &mut props)
            .ok()?;

        let mods = [modifier];
        let mut mod_list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
        let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(dma);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(extent(w, h))
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .push_next(&mut mod_list)
            .push_next(&mut external);
        o.image = device.create_image(&image_info, None).ok()?;
        let mp = instance.get_physical_device_memory_properties(pd);
        let req = device.get_image_memory_requirements(o.image);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(o.image);
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(dma);
        let device_local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(memory_type(&mp, req.memory_type_bits, device_local).ok()?)
            .push_next(&mut dedicated)
            .push_next(&mut export);
        o.memory = device.allocate_memory(&alloc, None).ok()?;
        device.bind_image_memory(o.image, o.memory, 0).ok()?;

        let staging_info = vk::BufferCreateInfo::default()
            .size(tight.len() as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC);
        o.staging = device.create_buffer(&staging_info, None).ok()?;
        let req = device.get_buffer_memory_requirements(o.staging);
        let host = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(memory_type(&mp, req.memory_type_bits, host).ok()?);
        o.staging_memory = device.allocate_memory(&alloc, None).ok()?;
        device
            .bind_buffer_memory(o.staging, o.staging_memory, 0)
            .ok()?;
        MappedMemory::map(device, o.staging_memory, tight.len())
            .ok()?
            .copy_from_slice(tight);

        o.pool = device
            .create_command_pool(&vk::CommandPoolCreateInfo::default(), None)
            .ok()?;
        let cmd_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(o.pool)
            .command_buffer_count(1);
        let cmd = device.allocate_command_buffers(&cmd_info).ok()?[0];
        device
            .begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
            .ok()?;
        let whole = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let to_dst = vk::ImageMemoryBarrier::default()
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(o.image)
            .subresource_range(whole);
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_dst],
        );
        let luma_bytes = u64::from(w * h) * if ten_bit { 2 } else { 1 };
        let plane = |aspect, at, size| {
            let layers = vk::ImageSubresourceLayers::default()
                .aspect_mask(aspect)
                .layer_count(1);
            vk::BufferImageCopy::default()
                .buffer_offset(at)
                .image_subresource(layers)
                .image_extent(size)
        };
        device.cmd_copy_buffer_to_image(
            cmd,
            o.staging,
            o.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[
                plane(vk::ImageAspectFlags::PLANE_0, 0, extent(w, h)),
                plane(
                    vk::ImageAspectFlags::PLANE_1,
                    luma_bytes,
                    extent(w / 2, h / 2),
                ),
            ],
        );
        device.end_command_buffer(cmd).ok()?;
        let cmds = [cmd];
        let queue = device.get_device_queue(0, 0);
        let submit = vk::SubmitInfo::default().command_buffers(&cmds);
        device
            .queue_submit(queue, &[submit], vk::Fence::null())
            .ok()?;
        device.queue_wait_idle(queue).ok()?;

        let layout = |aspect| {
            let at = vk::ImageSubresource::default().aspect_mask(aspect);
            let l = device.get_image_subresource_layout(o.image, at);
            (l.offset as u32, l.row_pitch as u32)
        };
        let planes = [
            layout(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT),
            layout(vk::ImageAspectFlags::MEMORY_PLANE_1_EXT),
        ];
        let get_fd = vk::MemoryGetFdInfoKHR::default()
            .memory(o.memory)
            .handle_type(dma);
        let fd = ash::khr::external_memory_fd::Device::new(instance, device)
            .get_memory_fd(&get_fd)
            .ok()?;
        Some(PlanarDmabuf {
            fd: OwnedFd::from_raw_fd(fd),
            planes,
        })
    }
}
