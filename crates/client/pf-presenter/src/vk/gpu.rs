//! Low-level GPU helpers: memory allocation, decode-image barriers, geometry.

use super::Presenter;
use anyhow::{Context as _, Result};
use ash::vk;

impl Presenter {
    /// Wait our in-flight fence, not `vkDeviceWaitIdle`. The pump thread submits
    /// decode work on other queues; wait-idle's external-sync over every queue
    /// would race it.
    pub(super) fn quiesce_own(&mut self) -> Result<()> {
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe {
            if self.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.submitted = false;
            }
        }
        Ok(())
    }

    /// An empty batch that waits `acquire_sem`. A discarded image leaves the semaphore
    /// signalled, and the next acquire needs it unsignalled with no wait pending: that holds
    /// once the queue drains after this.
    ///
    /// # Safety
    /// The caller holds `queue_lock`.
    pub(super) unsafe fn retire_acquire_sem(&self) -> ash::prelude::VkResult<()> {
        let sems = [self.acquire_sem];
        let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let batch = vk::SubmitInfo::default()
            .wait_semaphores(&sems)
            .wait_dst_stage_mask(&stages);
        // SAFETY: `queue` external sync is the caller's `queue_lock`. `acquire_sem` carries
        // an acquire's signal that no batch waits yet; `batch` and its arrays are locals.
        unsafe {
            self.device
                .queue_submit(self.queue, &[batch], vk::Fence::null())
        }
    }

    pub(super) fn allocate(
        &self,
        reqs: vk::MemoryRequirements,
        flags: vk::MemoryPropertyFlags,
    ) -> Result<vk::DeviceMemory> {
        allocate(&self.device, &self.mem_props, reqs, flags)
    }
}

/// Memory of the first type in `mem_props` that `reqs` accepts with `flags`.
pub(super) fn allocate(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    reqs: vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
) -> Result<vk::DeviceMemory> {
    let type_index = (0..mem_props.memory_type_count)
        .find(|&i| {
            reqs.memory_type_bits & (1 << i) != 0
                && mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(flags)
        })
        .with_context(|| format!("no memory type for {flags:?}"))?;
    // SAFETY: per the Vulkan contract above - a create/allocate call on the live device, over
    // builder structs that are locals outliving the call; the handle it returns is owned by
    // the caller.
    unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size)
                .memory_type_index(type_index),
            None,
        )
    }
    .context("vkAllocateMemory")
}

/// A 2D colour image from `info`, bound to fresh `flags` memory, with a whole-image view in
/// the image's format. A failure destroys whatever was created before the error returns.
pub(crate) fn image_with_memory(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    info: &vk::ImageCreateInfo<'_>,
    flags: vk::MemoryPropertyFlags,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    // SAFETY: CREATE per the crate contract.
    let image = unsafe { device.create_image(info, None) }.context("vkCreateImage")?;
    let mut memory = vk::DeviceMemory::null();
    let view = (|| {
        // SAFETY: `image` was created above and is owned here.
        let reqs = unsafe { device.get_image_memory_requirements(image) };
        memory = allocate(device, mem_props, reqs, flags)?;
        // SAFETY: `image` and `memory` were created above; neither is bound yet.
        unsafe { device.bind_image_memory(image, memory, 0) }.context("vkBindImageMemory")?;
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(info.format)
            .subresource_range(subresource_range());
        // SAFETY: CREATE per the crate contract; `image` is bound above.
        unsafe { device.create_image_view(&view_info, None) }.context("vkCreateImageView")
    })();
    match view {
        Ok(view) => Ok((image, memory, view)),
        Err(e) => {
            // SAFETY: neither was recorded or submitted; freeing a null `memory` is a no-op.
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            }
            Err(e)
        }
    }
}

pub(super) fn subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

pub(super) fn subresource_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

/// Layer-scoped: the pool is an image array; other layers are live DPB.
/// No queue-family transfer: the pool is CONCURRENT across graphics+decode.
///
/// Both stages are FRAGMENT_SHADER: the submit waits the decode-complete
/// timeline with `wait_dst_stage_mask = FRAGMENT_SHADER`, and a semaphore wait
/// only orders work whose first sync scope intersects that mask. TOP_OF_PIPE
/// would form no chain and could run while decode is still writing the image.
pub(super) fn native_layer_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    layer: u32,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .base_array_layer(layer)
                .layer_count(1),
        );
    // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns and
    // has begun, referencing handles it also owns; nothing is submitted until the recording is
    // ended.
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

/// The keyed mutex on the submit is the cross-API order. UNDEFINED old-layout
/// on externally-bound memory preserves contents (unlike ordinary images);
/// this is the layout/ownership hop only, into whatever the reader needs.
#[cfg(windows)]
pub(super) fn external_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    qfi: u32,
    layout: vk::ImageLayout,
    stage: vk::PipelineStageFlags,
    access: vk::AccessFlags,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(access)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(layout)
        .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
        .dst_queue_family_index(qfi)
        .image(image)
        .subresource_range(subresource_range());
    // SAFETY: per the Vulkan contract in lib.rs - recorded into a command buffer this code owns
    // and has begun, referencing handles it also owns; nothing runs until submit.
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            stage,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

/// UNDEFINED old-layout still preserves contents on externally-bound memory
/// (VAAPI dmabuf). The hop is FOREIGN → ours, not a discard.
#[cfg(target_os = "linux")]
pub(super) fn foreign_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    qfi: u32,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        .dst_queue_family_index(qfi)
        .image(image)
        .subresource_range(subresource_range());
    // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns and
    // has begun, referencing handles it also owns; nothing is submitted until the recording is
    // ended.
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}

/// ALL_COMMANDS both sides: this transfer pipeline is per-frame, not per-stage.
pub(super) fn barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let b = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_access_mask(vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE)
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(subresource_range());
    // SAFETY: per the Vulkan contract above - recorded into a command buffer this code owns and
    // has begun, referencing handles it also owns; nothing is submitted until the recording is
    // ended.
    unsafe {
        device.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[b],
        );
    }
}
