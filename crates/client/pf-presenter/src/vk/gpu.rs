//! Low-level GPU helpers: memory allocation, the pass objects, image barriers, geometry.

use super::Presenter;
use anyhow::{Context as _, Result};
use ash::vk;

/// What a fullscreen-triangle pass owns and frees: its render pass, pipeline and layouts, the
/// pool its sets come from, the immutable sampler. A pass with more (a second pipeline, its
/// sets, an intermediate image) keeps that beside its `Pass`.
#[derive(Default)]
pub(crate) struct Pass {
    pub(crate) render_pass: vk::RenderPass,
    pub(crate) pipeline: vk::Pipeline,
    pub(crate) pipeline_layout: vk::PipelineLayout,
    pub(crate) set_layout: vk::DescriptorSetLayout,
    pub(crate) desc_pool: vk::DescriptorPool,
    pub(crate) sampler: vk::Sampler,
}

impl Pass {
    /// Only once the GPU is done with every command buffer that recorded the pass. Null
    /// handles are skipped, so a part-built pass unwinds here; every handle is null after.
    pub(crate) fn destroy(&mut self, device: &ash::Device) {
        let p = std::mem::take(self);
        // SAFETY: DESTROY per the crate contract: the caller's GPU-idle proof covers these.
        // The pool frees the sets allocated from it.
        unsafe {
            device.destroy_pipeline(p.pipeline, None);
            device.destroy_pipeline_layout(p.pipeline_layout, None);
            device.destroy_descriptor_pool(p.desc_pool, None);
            device.destroy_descriptor_set_layout(p.set_layout, None);
            device.destroy_sampler(p.sampler, None);
            device.destroy_render_pass(p.render_pass, None);
        }
    }
}

impl Presenter {
    /// Wait our in-flight fence, not `vkDeviceWaitIdle`. The pump thread submits
    /// decode work on other queues; wait-idle's external-sync over every queue
    /// would race it.
    pub(super) fn quiesce_own(&mut self) -> Result<()> {
        // SAFETY: per the Vulkan contract above - the Vulkan handles used here are owned by this
        // type and live for the call, and every builder struct is a local that outlives it.
        unsafe {
            if self.state.submitted {
                self.device.wait_for_fences(&[self.fence], true, u64::MAX)?;
                self.state.submitted = false;
            }
        }
        Ok(())
    }

    /// Idle our own work mid-session: the in-flight fence, then the graphics queue, which
    /// carries the overlay's submits. Where the decode lane shares that queue, the queue lock
    /// keeps its submits off it. Unlike `vkDeviceWaitIdle`, this never touches the decode
    /// queue, so the pump may keep decoding.
    pub(crate) fn quiesce_queue(&mut self) {
        // A lost device reads nothing more, so a failed wait still lets go.
        self.quiesce_own().ok();
        let _q = self.queue_lock.guard();
        // SAFETY: `queue` is owned here; `queue_lock` is held so no concurrent submit.
        unsafe { self.device.queue_wait_idle(self.queue) }.ok();
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
pub(crate) fn allocate(
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
            // Never recorded or submitted; `memory` may still be null.
            destroy_image_with_memory(device, (image, memory, vk::ImageView::null()));
            Err(e)
        }
    }
}

/// Frees what [`image_with_memory`] made: the view, the image, then its memory. Null handles
/// are skipped. Only once the GPU is done with every command buffer that used them.
pub(crate) fn destroy_image_with_memory(
    device: &ash::Device,
    (image, memory, view): (vk::Image, vk::DeviceMemory, vk::ImageView),
) {
    // SAFETY: DESTROY per the crate contract: the caller's GPU-idle proof covers these, and
    // each goes before what it was made on.
    unsafe {
        device.destroy_image_view(view, None);
        device.destroy_image(image, None);
        device.free_memory(memory, None);
    }
}

/// A device-local colour image, its view and a framebuffer over it: one pass's target.
pub(crate) struct RenderImage {
    pub(crate) image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    pub(crate) view: vk::ImageView,
    pub(crate) framebuffer: vk::Framebuffer,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl RenderImage {
    /// `info`'s image with a framebuffer of `render_pass` over it; a failure frees what it made.
    pub(crate) fn build(
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        info: &vk::ImageCreateInfo<'_>,
        render_pass: vk::RenderPass,
    ) -> Result<Self> {
        let local = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        let (image, memory, view) = image_with_memory(device, mem_props, info, local)?;
        let vk::Extent3D { width, height, .. } = info.extent;
        let attachments = [view];
        let fb = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(&attachments)
            .width(width)
            .height(height)
            .layers(1);
        // SAFETY: CREATE per the crate contract.
        let framebuffer = unsafe { device.create_framebuffer(&fb, None) }
            .inspect_err(|_| destroy_image_with_memory(device, (image, memory, view)))
            .context("vkCreateFramebuffer")?;
        Ok(Self {
            image,
            memory,
            view,
            framebuffer,
            width,
            height,
        })
    }

    /// Only once the GPU is done with every command buffer that used it.
    pub(crate) fn destroy(self, device: &ash::Device) {
        // SAFETY: DESTROY per the crate contract: the caller's GPU-idle proof covers it.
        unsafe { device.destroy_framebuffer(self.framebuffer, None) };
        destroy_image_with_memory(device, (self.image, self.memory, self.view));
    }
}

pub(super) fn subresource_layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .layer_count(1)
}

pub(crate) fn subresource_range() -> vk::ImageSubresourceRange {
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
    let fragment = vk::PipelineStageFlags::FRAGMENT_SHADER;
    Barrier {
        image,
        range: subresource_range().base_array_layer(layer),
        layouts: (from, to),
        stages: (fragment, fragment),
        access: (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ),
        queues: (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED),
    }
    .record(device, cmd);
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
    Barrier {
        image,
        range: subresource_range(),
        layouts: (vk::ImageLayout::UNDEFINED, layout),
        stages: (vk::PipelineStageFlags::TOP_OF_PIPE, stage),
        access: (vk::AccessFlags::empty(), access),
        queues: (vk::QUEUE_FAMILY_EXTERNAL, qfi),
    }
    .record(device, cmd);
}

/// UNDEFINED old-layout still preserves contents on externally-bound memory
/// (VAAPI dmabuf). The hop is FOREIGN → ours, not a discard. Both stages are
/// FRAGMENT_SHADER as in [`native_layer_barrier`]: the submit waits the decode
/// sync file there, and a TOP_OF_PIPE hop would run while the decoder still writes.
#[cfg(target_os = "linux")]
pub(super) fn foreign_acquire_barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    qfi: u32,
) {
    let fragment = vk::PipelineStageFlags::FRAGMENT_SHADER;
    Barrier {
        image,
        range: subresource_range(),
        layouts: (
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        ),
        stages: (fragment, fragment),
        access: (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ),
        queues: (vk::QUEUE_FAMILY_FOREIGN_EXT, qfi),
    }
    .record(device, cmd);
}

/// ALL_COMMANDS both sides: this transfer pipeline is per-frame, not per-stage.
pub(crate) fn barrier(
    device: &ash::Device,
    cmd: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
) {
    let all = vk::PipelineStageFlags::ALL_COMMANDS;
    Barrier {
        image,
        range: subresource_range(),
        layouts: (from, to),
        stages: (all, all),
        access: (
            vk::AccessFlags::MEMORY_WRITE,
            vk::AccessFlags::MEMORY_READ | vk::AccessFlags::MEMORY_WRITE,
        ),
        queues: (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED),
    }
    .record(device, cmd);
}

/// One image memory barrier; each pair is (before, after). Its callers say why their stages.
pub(crate) struct Barrier {
    pub(crate) image: vk::Image,
    pub(crate) range: vk::ImageSubresourceRange,
    pub(crate) layouts: (vk::ImageLayout, vk::ImageLayout),
    pub(crate) stages: (vk::PipelineStageFlags, vk::PipelineStageFlags),
    pub(crate) access: (vk::AccessFlags, vk::AccessFlags),
    pub(crate) queues: (u32, u32),
}

impl Barrier {
    /// Into `cmd`, which the caller has begun and owns, as it owns `image`.
    pub(crate) fn record(self, device: &ash::Device, cmd: vk::CommandBuffer) {
        let b = vk::ImageMemoryBarrier::default()
            .src_access_mask(self.access.0)
            .dst_access_mask(self.access.1)
            .old_layout(self.layouts.0)
            .new_layout(self.layouts.1)
            .src_queue_family_index(self.queues.0)
            .dst_queue_family_index(self.queues.1)
            .image(self.image)
            .subresource_range(self.range);
        // SAFETY: RECORD per the crate contract: into a command buffer the caller owns and has
        // begun, naming an image it owns; nothing runs until the submit.
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                self.stages.0,
                self.stages.1,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[b],
            );
        }
    }
}
