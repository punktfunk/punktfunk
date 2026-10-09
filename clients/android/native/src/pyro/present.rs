//! Swapchain + planar CSC present for the PyroWave lane: the decoder's three Y′CbCr plane
//! views sampled into the `SurfaceView`'s own Vulkan swapchain image.
//!
//! The shader pair is the desktop presenter's, byte for byte
//! (`pf_client_video::video_csc_spv`) — a second copy of the colour maths is a second place
//! for it to be wrong, and the coefficients come from the one tested `csc_rows`.
//!
//! Aspect is preserved by the viewport rather than by resizing the window: the surface is
//! the whole view and the stream rarely matches it exactly, so the render pass clears to
//! black and the triangle is drawn into the fitted rectangle.

use anyhow::{anyhow, Result};
use ash::vk::{self, Handle as _};
use pf_client_video::video_color::{csc_rows, ColorDesc};
use pf_client_video::video_vk::QueueLock;
use std::marker::PhantomData;
use std::sync::Arc;

/// Frames the CPU may record ahead of the GPU. Two is the whole budget this lane wants:
/// decode is synchronous (the shared decoder fence-waits before handing the planes over),
/// so a deeper pipeline would only park finished pictures in front of the panel.
const FRAMES: usize = 2;

/// How long to wait for a free swapchain image before giving up on this picture.
///
/// Not `u64::MAX`: under FIFO with the app backgrounded, nothing drains the queue, and an
/// unbounded acquire would park this thread inside a call Kotlin's `surfaceDestroyed` is
/// waiting to join. A skipped frame is invisible; a blocked join is an ANR.
const ACQUIRE_TIMEOUT_NS: u64 = 250_000_000;

/// Bytes of push constants: three CSC rows plus the params vec4 (`planar_csc.frag`).
const PUSH_BYTES: u32 = 16 * 4;

/// Everything rebuilt when the swapchain is: the images, their views and framebuffers, and
/// the per-image "rendering done" semaphores.
///
/// Per-IMAGE, not per-frame-in-flight: a present waits on the semaphore its own submit
/// signalled, and a semaphore reused while an older present still waits on it is the
/// classic acquire/present race.
struct SwapRes {
    swapchain: vk::SwapchainKHR,
    views: Vec<vk::ImageView>,
    framebuffers: Vec<vk::Framebuffer>,
    done: Vec<vk::Semaphore>,
    extent: vk::Extent2D,
}

/// The lane's present half: owns the surface-side Vulkan objects and draws one decoded
/// frame per call. Borrows the device it was built on, so it drops first.
pub(super) struct Present<'d> {
    device: ash::Device,
    queue: vk::Queue,
    queue_lock: Arc<QueueLock>,
    surface_i: ash::khr::surface::Instance,
    surface: vk::SurfaceKHR,
    swap_d: ash::khr::swapchain::Device,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
    present_mode: vk::PresentModeKHR,
    render_pass: vk::RenderPass,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    desc_pool: vk::DescriptorPool,
    sets: Vec<vk::DescriptorSet>,
    cmd_pool: vk::CommandPool,
    cmds: Vec<vk::CommandBuffer>,
    acquire: Vec<vk::Semaphore>,
    fences: Vec<vk::Fence>,
    swap: SwapRes,
    frame: usize,
    _dev: PhantomData<&'d super::device::PyroDevice>,
}

impl<'d> Present<'d> {
    /// Build the render pass, pipeline and first swapchain for `dev`'s surface.
    ///
    /// `smooth` picks FIFO over MAILBOX: the same "presentation intent" the MediaCodec
    /// backends take from the settings, expressed in the only actuator a swapchain has.
    /// MAILBOX shows the newest picture and discards what it overtook — the latency intent
    /// — where FIFO queues every one onto the panel's own cadence.
    pub(super) fn new(dev: &'d super::device::PyroDevice, smooth: bool) -> Result<Present<'d>> {
        let surface = dev.base.surface;
        let surface_i = ash::khr::surface::Instance::new(&dev.base.entry, &dev.base.instance);
        let swap_d = ash::khr::swapchain::Device::new(&dev.base.instance, &dev.device);
        // SAFETY: physical device and surface are live for the lane's lifetime.
        let formats = unsafe { surface_i.get_physical_device_surface_formats(dev.pdev, surface) }?;
        // Whatever the compositor offers first in 8-bit UNORM; the CSC writes non-linear
        // sRGB values, so an _SRGB swapchain format would apply the transfer twice.
        let format = formats
            .iter()
            .find(|f| {
                matches!(
                    f.format,
                    vk::Format::R8G8B8A8_UNORM | vk::Format::B8G8R8A8_UNORM
                )
            })
            .or_else(|| formats.first())
            .ok_or_else(|| anyhow!("surface offers no formats"))?;
        let (format, color_space) = (format.format, format.color_space);
        // SAFETY: as above.
        let modes =
            unsafe { surface_i.get_physical_device_surface_present_modes(dev.pdev, surface) }?;
        let present_mode = if !smooth && modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO // always supported
        };

        let render_pass = build_render_pass(&dev.device, format)?;
        let sampler = build_sampler(&dev.device)?;
        let (set_layout, pipeline_layout, pipeline) =
            build_pipeline(&dev.device, render_pass, sampler)?;

        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(3 * FRAMES as u32)];
        // SAFETY: builders are locals outliving the call; the pool is owned by this struct.
        let desc_pool = unsafe {
            dev.device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(FRAMES as u32)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        let layouts = vec![set_layout; FRAMES];
        // SAFETY: pool and layouts are live; the sets are freed with the pool.
        let sets = unsafe {
            dev.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&layouts),
            )
        }?;

        // SAFETY: `qf` is the family the device was created with.
        let cmd_pool = unsafe {
            dev.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(dev.qf)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        // SAFETY: pool is live; buffers are freed with it.
        let cmds = unsafe {
            dev.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(FRAMES as u32),
            )
        }?;

        let mut acquire = Vec::with_capacity(FRAMES);
        let mut fences = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            // SAFETY: device live; both are owned by this struct and destroyed in `destroy`.
            acquire.push(unsafe {
                dev.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
            // Signalled: the first pass through the loop waits on it before recording.
            // SAFETY: device live; the fence is owned by this struct and destroyed in `destroy`.
            fences.push(unsafe {
                dev.device.create_fence(
                    &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                    None,
                )
            }?);
        }

        let mut p = Present {
            device: dev.device.clone(),
            queue: dev.queue,
            queue_lock: dev.vkd.queue_lock.clone(),
            surface_i,
            surface,
            swap_d,
            pdev: dev.pdev,
            format,
            present_mode,
            render_pass,
            sampler,
            set_layout,
            pipeline_layout,
            pipeline,
            desc_pool,
            sets,
            cmd_pool,
            cmds,
            acquire,
            fences,
            swap: SwapRes {
                swapchain: vk::SwapchainKHR::null(),
                views: Vec::new(),
                framebuffers: Vec::new(),
                done: Vec::new(),
                extent: vk::Extent2D::default(),
            },
            frame: 0,
            _dev: PhantomData,
        };
        p.rebuild_swapchain(color_space)?;
        log::info!(
            "pyro: swapchain {}x{} {:?} {:?}",
            p.swap.extent.width,
            p.swap.extent.height,
            p.format,
            p.present_mode
        );
        Ok(p)
    }

    /// (Re)create the swapchain and everything sized by it, keeping the old one as the
    /// `oldSwapchain` handover so the compositor never sees a gap. On an error, `swap` names
    /// only live objects, which `Drop` destroys.
    fn rebuild_swapchain(&mut self, color_space: vk::ColorSpaceKHR) -> Result<()> {
        // SAFETY: physical device and surface are live.
        let caps = unsafe {
            self.surface_i
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }?;
        // `current_extent` is the window's real size on Android (never the 0xFFFFFFFF
        // "you choose" sentinel), so it is also the resize signal.
        let extent = caps.current_extent;
        if extent.width == 0 || extent.height == 0 {
            return Err(anyhow!("surface has no extent yet"));
        }
        let mut count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            count = count.min(caps.max_image_count);
        }
        // IDENTITY where the surface offers it, NOT `current_transform`. Passing the
        // compositor's transform back means promising to have rotated the picture ourselves,
        // and this pipeline draws one unrotated fullscreen triangle — honouring it would put
        // the stream sideways on any device reporting a rotation. IDENTITY hands the rotation
        // back to SurfaceFlinger, which is doing it for every other codec on this client too.
        let pre_transform = if caps
            .supported_transforms
            .contains(vk::SurfaceTransformFlagsKHR::IDENTITY)
        {
            vk::SurfaceTransformFlagsKHR::IDENTITY
        } else {
            caps.current_transform
        };
        let old = self.swap.swapchain;
        let ci = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(count)
            .image_format(self.format)
            .image_color_space(color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(pre_transform)
            .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
            .present_mode(self.present_mode)
            .clipped(true)
            .old_swapchain(old);
        // SAFETY: `ci` outlives the call; the new swapchain is owned here.
        let swapchain = unsafe { self.swap_d.create_swapchain(&ci, None) }?;
        // SAFETY: destroys only what this struct built; the queue is idle at every call
        // site (see `present`) and the retired swapchain was handed over above.
        unsafe { self.destroy_swap_res(old) };
        self.swap.swapchain = swapchain;
        self.swap.extent = extent;

        // SAFETY: the swapchain is live; the images belong to it.
        let images = unsafe { self.swap_d.get_swapchain_images(swapchain) }?;
        for image in images {
            // SAFETY: builders are locals; the view is owned by this struct.
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            level_count: 1,
                            layer_count: 1,
                            ..Default::default()
                        }),
                    None,
                )
            }?;
            self.swap.views.push(view);
            let attachments = [view];
            // SAFETY: as above; the framebuffer borrows `view`, which outlives it here.
            let fb = unsafe {
                self.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render_pass)
                        .attachments(&attachments)
                        .width(extent.width)
                        .height(extent.height)
                        .layers(1),
                    None,
                )
            }?;
            self.swap.framebuffers.push(fb);
            // SAFETY: device live.
            let sem = unsafe {
                self.device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?;
            self.swap.done.push(sem);
        }
        Ok(())
    }

    /// Draw one decoded frame and present it.
    ///
    /// `Ok(false)` means the swapchain went out of date and was rebuilt — the picture was
    /// dropped, and the caller simply decodes the next one. Only a real device error is an
    /// `Err`, and that ends the lane.
    pub(super) fn show(
        &mut self,
        views: [u64; 3],
        crop: [f32; 4],
        color: ColorDesc,
        depth: u8,
        msb_packed: bool,
    ) -> Result<bool> {
        let frame = self.frame;
        let fence = self.fences[frame];
        // SAFETY: the fence belongs to this struct; waiting is read-only on the device.
        unsafe {
            self.device
                .wait_for_fences(&[fence], true, u64::MAX)
                .map_err(|e| anyhow!("wait_for_fences: {e}"))?;
        }
        // SAFETY: the semaphore is unsignalled — its previous acquire was consumed by the
        // submit whose fence we just waited on.
        let acquired = unsafe {
            self.swap_d.acquire_next_image(
                self.swap.swapchain,
                ACQUIRE_TIMEOUT_NS,
                self.acquire[frame],
                vk::Fence::null(),
            )
        };
        let (index, suboptimal) = match acquired {
            Ok(v) => v,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate()?;
                return Ok(false);
            }
            // Timed out: nothing is draining the queue (backgrounded, or a stalled
            // compositor). The semaphore is untouched, so drop this picture and come back.
            Err(vk::Result::TIMEOUT | vk::Result::NOT_READY) => return Ok(false),
            Err(e) => return Err(anyhow!("acquire_next_image: {e}")),
        };

        // Bind this frame's planes. Safe to rewrite the set: the fence above proves the
        // submit that last read it has completed. GENERAL is the layout the decoder leaves
        // its planes in — sampling from it needs no transition, which is the point.
        let set = self.sets[frame];
        let infos: Vec<vk::DescriptorImageInfo> = views
            .iter()
            .map(|&v| {
                vk::DescriptorImageInfo::default()
                    .sampler(self.sampler)
                    .image_view(vk::ImageView::from_raw(v))
                    .image_layout(vk::ImageLayout::GENERAL)
            })
            .collect();
        let writes: Vec<vk::WriteDescriptorSet> = (0..3)
            .map(|i| {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(i as u32)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(&infos[i]))
            })
            .collect();
        // SAFETY: `writes` borrows `infos`, both live for the call; the set is idle.
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };

        let cmd = self.cmds[frame];
        let fb = self.swap.framebuffers[index as usize];
        // SAFETY: the buffer is not in flight (fence waited) and the pool allows reset.
        unsafe {
            self.device
                .reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            self.record(cmd, fb, crop, color, depth, msb_packed);
            self.device.end_command_buffer(cmd)?;
        }

        let wait = [self.acquire[frame]];
        let signal = [self.swap.done[index as usize]];
        let stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let cmds = [cmd];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal);
        let swapchains = [self.swap.swapchain];
        let indices = [index];
        let present = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal)
            .swapchains(&swapchains)
            .image_indices(&indices);
        // The decoder submits to this same queue from this same thread, so the lock is not
        // guarding against us — it is the contract pyrowave's own internal submits take.
        let outcome = {
            let _q = self.queue_lock.guard();
            // SAFETY: the fence is unsignalled (waited and reset below), every handle is
            // owned here, and the submit's borrows outlive the call.
            unsafe {
                self.device.reset_fences(&[fence])?;
                self.device.queue_submit(self.queue, &[submit], fence)?;
                self.swap_d.queue_present(self.queue, &present)
            }
        };
        self.frame = (frame + 1) % FRAMES;
        match outcome {
            Ok(false) if !suboptimal => Ok(true),
            // Suboptimal is not an error — the picture was shown. Android reports it on every
            // present whose IDENTITY transform differs from the panel's rotation, so only a
            // size change (the bars retracting) earns a rebuild.
            Ok(_) => {
                if self.extent_changed()? {
                    self.recreate()?;
                }
                Ok(true)
            }
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.recreate()?;
                Ok(false)
            }
            Err(e) => Err(anyhow!("queue_present: {e}")),
        }
    }

    /// The render pass: clear the whole image, draw the fitted triangle.
    ///
    /// # Safety
    /// `cmd` is recording and `fb` belongs to the current swapchain.
    unsafe fn record(
        &self,
        cmd: vk::CommandBuffer,
        fb: vk::Framebuffer,
        crop: [f32; 4],
        color: ColorDesc,
        depth: u8,
        msb_packed: bool,
    ) {
        let extent = self.swap.extent;
        let clear = [vk::ClearValue {
            color: vk::ClearColorValue {
                float32: [0.0, 0.0, 0.0, 1.0],
            },
        }];
        let rows = csc_rows(color, depth, msb_packed);
        let mut pc = [0f32; 16];
        pc[..12].copy_from_slice(rows.as_flattened());
        // Mode 1 = PQ->SDR tonemap. This lane never asks for an HDR swapchain, so a PQ
        // stream is always tone-mapped here rather than passed through.
        pc[12] = if color.is_pq() { 1.0 } else { 0.0 };
        pc[13] = 4.9; // ~1000 nits over the 203-nit reference, the presenter's default
        let words = pc.map(f32::to_ne_bytes);
        // SAFETY: the caller holds `cmd` recording; every handle is owned by this struct and
        // the borrowed builders are locals that outlive the calls.
        unsafe {
            self.device.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(self.render_pass)
                    .framebuffer(fb)
                    .render_area(vk::Rect2D {
                        offset: vk::Offset2D { x: 0, y: 0 },
                        extent,
                    })
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
            self.device
                .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            self.device
                .cmd_set_viewport(cmd, 0, &[crop_viewport(crop, extent)]);
            self.device.cmd_set_scissor(
                cmd,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            self.device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[self.sets[self.frame]],
                &[],
            );
            self.device.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                words.as_flattened(),
            );
            self.device.cmd_draw(cmd, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(cmd);
        }
    }

    /// Whether the surface's size differs from the swapchain's.
    fn extent_changed(&self) -> Result<bool> {
        // SAFETY: physical device and surface are live.
        let caps = unsafe {
            self.surface_i
                .get_physical_device_surface_capabilities(self.pdev, self.surface)
        }?;
        Ok(caps.current_extent != self.swap.extent)
    }

    /// Idle the device, then rebuild the swapchain against the surface's current size.
    fn recreate(&mut self) -> Result<()> {
        {
            let _q = self.queue_lock.guard();
            // SAFETY: idling is the precondition for destroying the in-flight swapchain
            // resources `rebuild_swapchain` retires.
            unsafe { self.device.device_wait_idle() }?;
        }
        // SAFETY: physical device and surface are live.
        let formats = unsafe {
            self.surface_i
                .get_physical_device_surface_formats(self.pdev, self.surface)
        }?;
        let color_space = formats
            .iter()
            .find(|f| f.format == self.format)
            .map(|f| f.color_space)
            .unwrap_or(vk::ColorSpaceKHR::SRGB_NONLINEAR);
        self.rebuild_swapchain(color_space)
    }

    /// Destroy the per-swapchain resources and `old`.
    ///
    /// # Safety
    /// The device must be idle and `old` must no longer be presented from.
    unsafe fn destroy_swap_res(&mut self, old: vk::SwapchainKHR) {
        // SAFETY: the caller guarantees the device is idle and `old` is retired.
        unsafe {
            for fb in self.swap.framebuffers.drain(..) {
                self.device.destroy_framebuffer(fb, None);
            }
            for view in self.swap.views.drain(..) {
                self.device.destroy_image_view(view, None);
            }
            for sem in self.swap.done.drain(..) {
                self.device.destroy_semaphore(sem, None);
            }
            if old != vk::SwapchainKHR::null() {
                self.swap_d.destroy_swapchain(old, None);
            }
        }
    }
}

impl Drop for Present<'_> {
    /// Idle the device, then destroy everything this half owns. The device and surface
    /// outlive it (`'d`).
    fn drop(&mut self) {
        {
            let _q = self.queue_lock.guard();
            // SAFETY: idling is the precondition for destroying objects submitted work uses.
            let _ = unsafe { self.device.device_wait_idle() };
        }
        let current = self.swap.swapchain;
        // SAFETY: the device is idle, and every handle here was created on it by this struct.
        unsafe {
            self.destroy_swap_res(current);
            for sem in self.acquire.drain(..) {
                self.device.destroy_semaphore(sem, None);
            }
            for fence in self.fences.drain(..) {
                self.device.destroy_fence(fence, None);
            }
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_descriptor_pool(self.desc_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_render_pass(self.render_pass, None);
        }
    }
}

/// The viewport that shows `crop` (frame fractions) across the whole surface. The SurfaceView is
/// already laid out at the picture's rect, so the cropped-away edges fall outside the surface and
/// the scissor drops them.
fn crop_viewport([left, top, right, bottom]: [f32; 4], surface: vk::Extent2D) -> vk::Viewport {
    let (sw, sh) = (surface.width as f32, surface.height as f32);
    let (w, h) = (sw / (right - left).max(1e-3), sh / (bottom - top).max(1e-3));
    vk::Viewport {
        x: -left * w,
        y: -top * h,
        width: w,
        height: h,
        min_depth: 0.0,
        max_depth: 1.0,
    }
}

/// One subpass writing the swapchain image: cleared on load (the letterbox bars), left in
/// PRESENT_SRC so no explicit transition is recorded.
fn build_render_pass(device: &ash::Device, format: vk::Format) -> Result<vk::RenderPass> {
    let attachment = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR)];
    let color_ref = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)];
    // Pairs with the acquire semaphore's COLOR_ATTACHMENT_OUTPUT wait stage.
    let dependency = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    // SAFETY: all builders are locals that outlive the call.
    Ok(unsafe {
        device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attachment)
                .subpasses(&subpass)
                .dependencies(&dependency),
            None,
        )
    }?)
}

/// Linear, clamped: the chroma planes are upsampled by the sampler and the shader's siting
/// correction assumes filtered taps.
fn build_sampler(device: &ash::Device) -> Result<vk::Sampler> {
    // SAFETY: builder is a local that outlives the call.
    Ok(unsafe {
        device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )
    }?)
}

/// The planar CSC pipeline: bufferless fullscreen triangle, three sampled planes, the CSC
/// rows in push constants. Viewport and scissor are dynamic so a resize costs no rebuild.
fn build_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    sampler: vk::Sampler,
) -> Result<(vk::DescriptorSetLayout, vk::PipelineLayout, vk::Pipeline)> {
    let immutable = [sampler];
    let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..3)
        .map(|i| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(i)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                .immutable_samplers(&immutable)
        })
        .collect();
    // SAFETY: builders are locals outliving the call; the objects are returned to the owner.
    let set_layout = unsafe {
        device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )
    }?;
    let ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
        .size(PUSH_BYTES)];
    let set_layouts = [set_layout];
    // SAFETY: as above.
    let pipeline_layout = unsafe {
        device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&set_layouts)
                .push_constant_ranges(&ranges),
            None,
        )
    }?;

    // include_bytes! alignment is unspecified; read_spv copies into aligned words.
    let vert_words = ash::util::read_spv(&mut std::io::Cursor::new(
        pf_client_video::video_csc_spv::FULLSCREEN_VERT,
    ))?;
    let frag_words = ash::util::read_spv(&mut std::io::Cursor::new(
        pf_client_video::video_csc_spv::PLANAR_CSC_FRAG,
    ))?;
    // SAFETY: the word slices outlive the create calls; modules are destroyed below.
    let vert = unsafe {
        device.create_shader_module(
            &vk::ShaderModuleCreateInfo::default().code(&vert_words),
            None,
        )
    }?;
    // SAFETY: as above.
    let frag = unsafe {
        device.create_shader_module(
            &vk::ShaderModuleCreateInfo::default().code(&frag_words),
            None,
        )
    }?;

    let name = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert)
            .name(name),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag)
            .name(name),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = [vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let info = [vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(pipeline_layout)
        .render_pass(render_pass)];
    // SAFETY: every borrowed builder is a local that outlives the call.
    let pipeline =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &info, None) }
            .map_err(|(_, e)| anyhow!("create_graphics_pipelines: {e}"))?[0];
    // SAFETY: the modules are consumed by pipeline creation.
    unsafe {
        device.destroy_shader_module(vert, None);
        device.destroy_shader_module(frag, None);
    }
    Ok((set_layout, pipeline_layout, pipeline))
}
