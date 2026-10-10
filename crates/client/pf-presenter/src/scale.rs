//! The video scale: a placed frame drawn into the swapchain with the filter ladder
//! `punktfunk_core::video_fit` names, in two passes through `scale.frag`.
//!
//! Pass one filters x from the frame into an RGBA16F intermediate `dst_w × frame_h`
//! (float keeps Lanczos overshoot for the second pass). Pass two filters y into the
//! destination rect and clears the bars. Whole-number scales on both axes never get
//! here: a NEAREST blit is exact and costs nothing extra.

use crate::csc::{build_fullscreen_pipeline, color_pass};
use crate::vk::gpu::image_with_memory;
use anyhow::{Context as _, Result};
use ash::vk;
use punktfunk_core::video_fit::{Kernel, Placement};

const MID_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

/// `scale.frag` push constants, field for field.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    origin: f32,
    step: f32,
    dst_offset: f32,
    other_offset: f32,
    axis: i32,
    kernel: i32,
    size: i32,
    other_size: i32,
}

impl Params {
    /// The push-constant bytes, in declaration order.
    fn to_ne_bytes(self) -> [[u8; 4]; 8] {
        [
            self.origin.to_ne_bytes(),
            self.step.to_ne_bytes(),
            self.dst_offset.to_ne_bytes(),
            self.other_offset.to_ne_bytes(),
            self.axis.to_ne_bytes(),
            self.kernel.to_ne_bytes(),
            self.size.to_ne_bytes(),
            self.other_size.to_ne_bytes(),
        ]
    }
}

struct Mid {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    width: u32,
    height: u32,
}

impl Mid {
    /// A null framebuffer is a no-op, so a build that failed at it unwinds here.
    fn destroy(self, device: &ash::Device) {
        // SAFETY: DESTROY — never submitted, or the fence contract of `prepare`/`destroy`.
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

pub struct ScalePass {
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    sampler: vk::Sampler,
    desc_pool: vk::DescriptorPool,
    set_src: vk::DescriptorSet,
    set_mid: vk::DescriptorSet,
    mid_pass: vk::RenderPass,
    mid_pipeline: vk::Pipeline,
    out_pass: vk::RenderPass,
    out_pipeline: vk::Pipeline,
    mid: Option<Mid>,
}

/// Whether `p` needs the shader: any fractional axis. Whole-number axes blit exactly.
pub fn needs_filter(p: &Placement) -> bool {
    let fractional = |k| matches!(k, Kernel::CatmullRom | Kernel::Lanczos);
    !p.is_empty() && (fractional(p.kernel_x()) || fractional(p.kernel_y()))
}

fn kernel_id(k: Kernel) -> i32 {
    match k {
        Kernel::Copy | Kernel::Nearest => 0,
        Kernel::CatmullRom => 1,
        Kernel::Lanczos => 2,
    }
}

impl ScalePass {
    /// `out_format` is the swapchain's. The output pass comes from `csc::color_pass`, so the
    /// overlay's per-image framebuffers serve it.
    pub fn new(device: &ash::Device, out_format: vk::Format) -> Result<ScalePass> {
        // SAFETY: CREATE per the crate contract.
        let sampler = unsafe {
            device.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::NEAREST)
                    .min_filter(vk::Filter::NEAREST)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )
        }?;
        let samplers = [sampler];
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .immutable_samplers(&samplers)];
        // SAFETY: CREATE per the crate contract.
        let set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        let set_layouts = [set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .size(std::mem::size_of::<Params>() as u32)];
        // SAFETY: CREATE per the crate contract.
        let pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )
        }?;
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(2)];
        // SAFETY: CREATE per the crate contract.
        let desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(2)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        let two_layouts = [set_layout, set_layout];
        // SAFETY: ALLOCATE per the crate contract; the pool holds two sets.
        let sets = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(desc_pool)
                    .set_layouts(&two_layouts),
            )
        }?;

        let mid_pass = color_pass(
            device,
            MID_FORMAT,
            vk::AttachmentLoadOp::DONT_CARE,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        )?;
        let out_pass = color_pass(
            device,
            out_format,
            vk::AttachmentLoadOp::CLEAR,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        )?;
        let frag = pf_client_core::video_csc_spv::SCALE_FRAG;
        let mid_pipeline =
            build_fullscreen_pipeline(device, mid_pass, pipeline_layout, frag, false)?;
        let out_pipeline =
            build_fullscreen_pipeline(device, out_pass, pipeline_layout, frag, false)?;
        Ok(ScalePass {
            set_layout,
            pipeline_layout,
            sampler,
            desc_pool,
            set_src: sets[0],
            set_mid: sets[1],
            mid_pass,
            mid_pipeline,
            out_pass,
            out_pipeline,
            mid: None,
        })
    }

    /// Size the intermediate for `p` (device-local memory from `mem_props`) and bind `src`
    /// (a view in SHADER_READ_ONLY_OPTIMAL at record time). Only while no submitted command
    /// buffer references this pass.
    pub fn prepare(
        &mut self,
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        frame_h: u32,
        p: &Placement,
        src: vk::ImageView,
    ) -> Result<()> {
        if self
            .mid
            .as_ref()
            .is_none_or(|m| (m.width, m.height) != (p.dst_w, frame_h))
        {
            self.destroy_mid(device);
            self.mid = Some(self.build_mid(device, mem_props, p.dst_w, frame_h)?);
        }
        let mid_view = self.mid.as_ref().map_or(vk::ImageView::null(), |m| m.view);
        let infos = [src, mid_view].map(|view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        });
        let writes = [(self.set_src, &infos[0]), (self.set_mid, &infos[1])].map(|(set, info)| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(info)
        });
        // SAFETY: no submitted command buffer references these sets (caller's fence
        // contract); the views outlive the write.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(())
    }

    /// Record both passes after [`ScalePass::prepare`] for the same placement. `target` is a
    /// framebuffer compatible with the output pass (an overlay framebuffer) over `extent`;
    /// it ends in COLOR_ATTACHMENT_OPTIMAL with the bars cleared black.
    ///
    /// # Safety
    /// `cmd` is recording, the source is in SHADER_READ_ONLY_OPTIMAL, and `target` is live.
    pub unsafe fn record(
        &self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        frame: (u32, u32),
        p: &Placement,
        target: vk::Framebuffer,
        extent: vk::Extent2D,
    ) {
        let Some(mid) = &self.mid else { return };
        let mid_rect = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent: vk::Extent2D {
                width: mid.width,
                height: mid.height,
            },
        };
        let horizontal = Params {
            origin: p.src_x as f32,
            step: (1.0 / p.scale_x) as f32,
            dst_offset: 0.0,
            other_offset: 0.0,
            axis: 0,
            kernel: kernel_id(p.kernel_x()),
            size: frame.0 as i32,
            other_size: frame.1 as i32,
        };
        // SAFETY: the caller's recording contract; the intermediate is live (`prepare`).
        unsafe {
            self.draw(
                device,
                cmd,
                (self.mid_pass, self.mid_pipeline, mid.framebuffer),
                mid_rect,
                mid_rect,
                self.set_src,
                horizontal,
            )
        };
        // The vertical pass samples what the horizontal one wrote. ALL_COMMANDS also waits
        // out the pass's final layout move, which its implicit external dependency leaves open.
        let written = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(mid.image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        // SAFETY: the caller's recording contract; the intermediate is live (`prepare`).
        unsafe {
            device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[written],
            )
        };
        let dst = vk::Rect2D {
            offset: vk::Offset2D {
                x: p.dst_x as i32,
                y: p.dst_y as i32,
            },
            extent: vk::Extent2D {
                width: p.dst_w,
                height: p.dst_h,
            },
        };
        let vertical = Params {
            origin: p.src_y as f32,
            step: (1.0 / p.scale_y) as f32,
            dst_offset: p.dst_y as f32,
            other_offset: p.dst_x as f32,
            axis: 1,
            kernel: kernel_id(p.kernel_y()),
            size: frame.1 as i32,
            other_size: mid.width as i32,
        };
        let full = vk::Rect2D {
            offset: vk::Offset2D::default(),
            extent,
        };
        // SAFETY: the caller's recording contract; `target` is live.
        unsafe {
            self.draw(
                device,
                cmd,
                (self.out_pass, self.out_pipeline, target),
                full,
                dst,
                self.set_mid,
                vertical,
            )
        };
    }

    /// One fullscreen-triangle pass: `area` is the render (and clear) area, `rect` the
    /// viewport and scissor.
    ///
    /// # Safety
    /// Same as [`ScalePass::record`].
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw(
        &self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        (pass, pipeline, framebuffer): (vk::RenderPass, vk::Pipeline, vk::Framebuffer),
        area: vk::Rect2D,
        rect: vk::Rect2D,
        set: vk::DescriptorSet,
        params: Params,
    ) {
        let words = params.to_ne_bytes();
        // SAFETY: RECORD per the crate contract (`record`'s caller).
        unsafe {
            let clear = [vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: [0.0, 0.0, 0.0, 1.0],
                },
            }];
            device.cmd_begin_render_pass(
                cmd,
                &vk::RenderPassBeginInfo::default()
                    .render_pass(pass)
                    .framebuffer(framebuffer)
                    .render_area(area)
                    .clear_values(&clear),
                vk::SubpassContents::INLINE,
            );
            device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_set_viewport(
                cmd,
                0,
                &[vk::Viewport {
                    x: rect.offset.x as f32,
                    y: rect.offset.y as f32,
                    width: rect.extent.width as f32,
                    height: rect.extent.height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            device.cmd_set_scissor(cmd, 0, &[rect]);
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[set],
                &[],
            );
            device.cmd_push_constants(
                cmd,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                words.as_flattened(),
            );
            device.cmd_draw(cmd, 3, 1, 0, 0);
            device.cmd_end_render_pass(cmd);
        }
    }

    fn build_mid(
        &self,
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        width: u32,
        height: u32,
    ) -> Result<Mid> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(MID_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let (image, memory, view) = image_with_memory(
            device,
            mem_props,
            &info,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )
        .context("scale intermediate image")?;
        let mid = Mid {
            image,
            memory,
            view,
            framebuffer: vk::Framebuffer::null(),
            width,
            height,
        };
        let attachments = [view];
        // SAFETY: CREATE per the crate contract.
        let framebuffer = unsafe {
            device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(self.mid_pass)
                    .attachments(&attachments)
                    .width(width)
                    .height(height)
                    .layers(1),
                None,
            )
        };
        match framebuffer {
            Ok(framebuffer) => Ok(Mid { framebuffer, ..mid }),
            Err(e) => {
                mid.destroy(device);
                Err(e).context("scale intermediate framebuffer")
            }
        }
    }

    fn destroy_mid(&mut self, device: &ash::Device) {
        if let Some(m) = self.mid.take() {
            m.destroy(device);
        }
    }

    /// Only after the GPU is done with every command buffer that recorded this pass.
    pub fn destroy(&mut self, device: &ash::Device) {
        self.destroy_mid(device);
        // SAFETY: DESTROY — the caller's GPU-idle proof covers these.
        unsafe {
            device.destroy_pipeline(self.out_pipeline, None);
            device.destroy_pipeline(self.mid_pipeline, None);
            device.destroy_render_pass(self.out_pass, None);
            device.destroy_render_pass(self.mid_pass, None);
            device.destroy_pipeline_layout(self.pipeline_layout, None);
            device.destroy_descriptor_pool(self.desc_pool, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
            device.destroy_sampler(self.sampler, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::video_fit::{place, VideoFit};

    type Px = [f32; 4];

    /// `scale.frag`'s arithmetic on the CPU, pass for pass. Pixel centres sit at +0.5, as
    /// `gl_FragCoord` does.
    fn reference(src: &[Px], frame: (u32, u32), view: (u32, u32), p: &Placement) -> Vec<Px> {
        fn axis(kernel: i32, s: f32, step: f32, size: i32, fetch: impl Fn(i32) -> Px) -> Px {
            if kernel == 0 {
                return fetch((s.floor() as i32).clamp(0, size - 1));
            }
            let stretch = if kernel == 2 {
                step.clamp(1.0, 10.0)
            } else {
                1.0
            };
            let support = if kernel == 2 { 3.0 } else { 2.0 } * stretch;
            let first = (s - support - 0.5).floor() as i32 + 1;
            let last = (((s + support - 0.5).ceil() as i32) - 1).min(first + 63);
            let (mut acc, mut total) = ([0f32; 4], 0f32);
            for i in first..=last {
                let x = ((i as f32 + 0.5 - s) / stretch).abs();
                let w = if kernel == 2 {
                    if x < 1e-5 {
                        1.0
                    } else if x >= 3.0 {
                        0.0
                    } else {
                        let px = std::f32::consts::PI * x;
                        3.0 * px.sin() * (px / 3.0).sin() / (px * px)
                    }
                } else if x < 1.0 {
                    (1.5 * x - 2.5) * x * x + 1.0
                } else if x < 2.0 {
                    ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
                } else {
                    0.0
                };
                let t = fetch(i.clamp(0, size - 1));
                for c in 0..4 {
                    acc[c] += w * t[c];
                }
                total += w;
            }
            [acc[0] / total, acc[1] / total, acc[2] / total, 1.0]
        }
        let (fw, fh) = (frame.0 as i32, frame.1 as i32);
        let mid_w = p.dst_w as i32;
        let (kx, ky) = (kernel_id(p.kernel_x()), kernel_id(p.kernel_y()));
        let mut mid = vec![[0f32; 4]; (mid_w * fh) as usize];
        for my in 0..fh {
            for mx in 0..mid_w {
                let s = p.src_x as f32 + (mx as f32 + 0.5) * (1.0 / p.scale_x) as f32;
                mid[(my * mid_w + mx) as usize] = axis(kx, s, (1.0 / p.scale_x) as f32, fw, |i| {
                    src[(my * fw + i) as usize]
                });
            }
        }
        let mut out = vec![[0.0, 0.0, 0.0, 1.0]; (view.0 * view.1) as usize];
        for y in p.dst_y..p.dst_y + p.dst_h {
            for x in p.dst_x..p.dst_x + p.dst_w {
                let d = y as f32 + 0.5 - p.dst_y as f32;
                let s = p.src_y as f32 + d * (1.0 / p.scale_y) as f32;
                let col = (x - p.dst_x) as i32;
                out[(y * view.0 + x) as usize] = axis(ky, s, (1.0 / p.scale_y) as f32, fh, |i| {
                    mid[(i * mid_w + col) as usize]
                });
            }
        }
        out
    }

    fn pattern(frame: (u32, u32)) -> Vec<[u8; 4]> {
        (0..frame.0 * frame.1)
            .map(|i| {
                let h = i.wrapping_mul(2_654_435_761);
                [(h >> 24) as u8, (h >> 16) as u8, (h >> 8) as u8, 255]
            })
            .collect()
    }

    fn unorm(px: &[[u8; 4]]) -> Vec<Px> {
        px.iter().map(|p| p.map(|c| f32::from(c) / 255.0)).collect()
    }

    #[test]
    fn a_constant_frame_stays_constant_under_every_kernel() {
        let frame = (40, 30);
        let src = vec![[0.25, 0.5, 0.75, 1.0]; 1200];
        for view in [(52, 39), (30, 22), (7, 5), (100, 31)] {
            for fit in [VideoFit::Fit, VideoFit::Crop, VideoFit::Stretch] {
                let p = place(fit, view, frame);
                let out = reference(&src, frame, view, &p);
                let px = out[((p.dst_y + p.dst_h / 2) * view.0 + p.dst_x + p.dst_w / 2) as usize];
                for c in 0..3 {
                    assert!((px[c] - src[0][c]).abs() < 1e-4, "{fit:?} {view:?} {px:?}");
                }
            }
        }
    }

    #[test]
    fn lanczos_greys_out_a_pixel_checkerboard_instead_of_aliasing() {
        let frame = (64, 64);
        let src: Vec<Px> = (0..64 * 64)
            .map(|i| {
                let v = ((i % 64 + i / 64) % 2) as f32;
                [v, v, v, 1.0]
            })
            .collect();
        let view = (32, 32);
        let p = place(VideoFit::Fit, view, frame);
        assert_eq!(p.kernel_x(), Kernel::Lanczos);
        let out = reference(&src, frame, view, &p);
        for px in &out[(8 * 32)..(24 * 32)] {
            assert!((px[0] - 0.5).abs() < 0.02, "{px:?}");
        }
    }

    #[test]
    fn one_to_one_copies_the_frame() {
        let frame = (16, 9);
        let src = unorm(&pattern(frame));
        let p = place(VideoFit::Fit, frame, frame);
        // Copy on both axes never reaches the shader, but the pass must still be exact.
        assert!(!needs_filter(&p));
        assert_eq!(reference(&src, frame, frame, &p), src);
    }

    /// The shader on a real Vulkan device against [`reference`]. Needs an ICD; lavapipe
    /// does: `apt install mesa-vulkan-drivers`, then `cargo test -p pf-presenter scale --
    /// --ignored`.
    #[test]
    #[ignore = "needs a Vulkan device"]
    fn the_shader_matches_the_reference_on_a_device() {
        let frame = (64, 36);
        let cases = [
            (VideoFit::Fit, (96, 64)),      // 1.5× Catmull-Rom, letterboxed
            (VideoFit::Crop, (100, 40)),    // Catmull-Rom with a fractional crop origin
            (VideoFit::Fit, (48, 27)),      // 0.75× Lanczos
            (VideoFit::Stretch, (128, 45)), // nearest x, Catmull-Rom y
            (VideoFit::Fit, (20, 12)),      // 0.31× Lanczos, 20-tap footprint
        ];
        let src = pattern(frame);
        let gpu = gpu::Gpu::new().expect("a Vulkan device");
        for (fit, view) in cases {
            let p = place(fit, view, frame);
            assert!(
                needs_filter(&p),
                "{fit:?} {view:?} must exercise the shader"
            );
            let got = gpu.run(&src, frame, view, &p).expect("scale pass ran");
            let want = reference(&unorm(&src), frame, view, &p);
            for (i, (g, w)) in got.iter().zip(&want).enumerate() {
                for c in 0..3 {
                    let w8 = (w[c].clamp(0.0, 1.0) * 255.0).round() as i32;
                    assert!(
                        (i32::from(g[c]) - w8).abs() <= 1,
                        "{fit:?} {view:?} pixel {} channel {c}: gpu {} reference {w8}",
                        i,
                        g[c]
                    );
                }
            }
        }
    }

    /// A headless device, one frame at a time. Test-only; nothing here is reused by the
    /// presenter.
    mod gpu {
        use super::super::ScalePass;
        use anyhow::{Context as _, Result};
        use ash::vk;
        use punktfunk_core::video_fit::Placement;

        const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

        pub struct Gpu {
            _entry: ash::Entry,
            instance: ash::Instance,
            device: ash::Device,
            mem_props: vk::PhysicalDeviceMemoryProperties,
            queue: vk::Queue,
            pool: vk::CommandPool,
        }

        impl Gpu {
            pub fn new() -> Result<Gpu> {
                // SAFETY: test harness; every handle is owned by `Gpu` and destroyed in Drop.
                unsafe {
                    let entry = ash::Entry::load()?;
                    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
                    let instance = entry.create_instance(
                        &vk::InstanceCreateInfo::default().application_info(&app),
                        None,
                    )?;
                    let (pdev, qfi) = instance
                        .enumerate_physical_devices()?
                        .into_iter()
                        .find_map(|pd| {
                            instance
                                .get_physical_device_queue_family_properties(pd)
                                .iter()
                                .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                                .map(|i| (pd, i as u32))
                        })
                        .context("no graphics device")?;
                    let priorities = [1.0];
                    let queues = [vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(qfi)
                        .queue_priorities(&priorities)];
                    let device = instance.create_device(
                        pdev,
                        &vk::DeviceCreateInfo::default().queue_create_infos(&queues),
                        None,
                    )?;
                    let pool = device.create_command_pool(
                        &vk::CommandPoolCreateInfo::default().queue_family_index(qfi),
                        None,
                    )?;
                    Ok(Gpu {
                        mem_props: instance.get_physical_device_memory_properties(pdev),
                        queue: device.get_device_queue(qfi, 0),
                        _entry: entry,
                        instance,
                        device,
                        pool,
                    })
                }
            }

            fn memory(
                &self,
                reqs: vk::MemoryRequirements,
                flags: vk::MemoryPropertyFlags,
            ) -> Result<vk::DeviceMemory> {
                let index = (0..self.mem_props.memory_type_count)
                    .find(|&i| {
                        reqs.memory_type_bits & (1 << i) != 0
                            && self.mem_props.memory_types[i as usize]
                                .property_flags
                                .contains(flags)
                    })
                    .context("memory type")?;
                // SAFETY: test harness; freed by the caller.
                Ok(unsafe {
                    self.device.allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(reqs.size)
                            .memory_type_index(index),
                        None,
                    )?
                })
            }

            fn image(
                &self,
                (w, h): (u32, u32),
                usage: vk::ImageUsageFlags,
            ) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
                // SAFETY: test harness; the caller destroys all three.
                unsafe {
                    let image = self.device.create_image(
                        &vk::ImageCreateInfo::default()
                            .image_type(vk::ImageType::TYPE_2D)
                            .format(FORMAT)
                            .extent(vk::Extent3D {
                                width: w,
                                height: h,
                                depth: 1,
                            })
                            .mip_levels(1)
                            .array_layers(1)
                            .samples(vk::SampleCountFlags::TYPE_1)
                            .usage(usage),
                        None,
                    )?;
                    let memory = self.memory(
                        self.device.get_image_memory_requirements(image),
                        vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    )?;
                    self.device.bind_image_memory(image, memory, 0)?;
                    let view = self.device.create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(FORMAT)
                            .subresource_range(range()),
                        None,
                    )?;
                    Ok((image, memory, view))
                }
            }

            fn buffer(
                &self,
                bytes: usize,
                usage: vk::BufferUsageFlags,
            ) -> Result<(vk::Buffer, vk::DeviceMemory)> {
                // SAFETY: test harness; the caller destroys both.
                unsafe {
                    let buffer = self.device.create_buffer(
                        &vk::BufferCreateInfo::default()
                            .size(bytes as u64)
                            .usage(usage),
                        None,
                    )?;
                    let memory = self.memory(
                        self.device.get_buffer_memory_requirements(buffer),
                        vk::MemoryPropertyFlags::HOST_VISIBLE
                            | vk::MemoryPropertyFlags::HOST_COHERENT,
                    )?;
                    self.device.bind_buffer_memory(buffer, memory, 0)?;
                    Ok((buffer, memory))
                }
            }

            /// Upload `src`, run [`ScalePass`] into a `view`-sized RGBA8 target, read it back.
            pub fn run(
                &self,
                src: &[[u8; 4]],
                frame: (u32, u32),
                view: (u32, u32),
                p: &Placement,
            ) -> Result<Vec<[u8; 4]>> {
                let d = &self.device;
                let mut scale = ScalePass::new(d, FORMAT)?;
                let (src_img, src_mem, src_view) = self.image(
                    frame,
                    vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
                )?;
                let (dst_img, dst_mem, dst_view) = self.image(
                    view,
                    vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                )?;
                let (up, up_mem) =
                    self.buffer(src.len() * 4, vk::BufferUsageFlags::TRANSFER_SRC)?;
                let (down, down_mem) = self.buffer(
                    (view.0 * view.1 * 4) as usize,
                    vk::BufferUsageFlags::TRANSFER_DST,
                )?;
                scale.prepare(d, &self.mem_props, frame.1, p, src_view)?;
                // SAFETY: test harness; one command buffer, waited to completion before any
                // destroy below.
                unsafe {
                    let ptr =
                        d.map_memory(up_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
                    std::ptr::copy_nonoverlapping(
                        src.as_ptr().cast::<u8>(),
                        ptr.cast(),
                        src.len() * 4,
                    );
                    d.unmap_memory(up_mem);
                    let fb = d.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(scale.out_pass)
                            .attachments(&[dst_view])
                            .width(view.0)
                            .height(view.1)
                            .layers(1),
                        None,
                    )?;
                    let cmd = d.allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default()
                            .command_pool(self.pool)
                            .command_buffer_count(1),
                    )?[0];
                    d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())?;
                    let layers = vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1);
                    transition(
                        d,
                        cmd,
                        src_img,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    );
                    d.cmd_copy_buffer_to_image(
                        cmd,
                        up,
                        src_img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(layers)
                            .image_extent(vk::Extent3D {
                                width: frame.0,
                                height: frame.1,
                                depth: 1,
                            })],
                    );
                    transition(
                        d,
                        cmd,
                        src_img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                    scale.record(
                        d,
                        cmd,
                        frame,
                        p,
                        fb,
                        vk::Extent2D {
                            width: view.0,
                            height: view.1,
                        },
                    );
                    transition(
                        d,
                        cmd,
                        dst_img,
                        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    );
                    d.cmd_copy_image_to_buffer(
                        cmd,
                        dst_img,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        down,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(layers)
                            .image_extent(vk::Extent3D {
                                width: view.0,
                                height: view.1,
                                depth: 1,
                            })],
                    );
                    d.end_command_buffer(cmd)?;
                    d.queue_submit(
                        self.queue,
                        &[vk::SubmitInfo::default().command_buffers(&[cmd])],
                        vk::Fence::null(),
                    )?;
                    d.queue_wait_idle(self.queue)?;
                    let ptr =
                        d.map_memory(down_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
                    let mut out = vec![[0u8; 4]; (view.0 * view.1) as usize];
                    std::ptr::copy_nonoverlapping(
                        ptr.cast::<u8>(),
                        out.as_mut_ptr().cast(),
                        out.len() * 4,
                    );
                    d.unmap_memory(down_mem);
                    d.free_command_buffers(self.pool, &[cmd]);
                    d.destroy_framebuffer(fb, None);
                    for (b, m) in [(up, up_mem), (down, down_mem)] {
                        d.destroy_buffer(b, None);
                        d.free_memory(m, None);
                    }
                    for (i, m, v) in [(src_img, src_mem, src_view), (dst_img, dst_mem, dst_view)] {
                        d.destroy_image_view(v, None);
                        d.destroy_image(i, None);
                        d.free_memory(m, None);
                    }
                    scale.destroy(d);
                    Ok(out)
                }
            }
        }

        impl Drop for Gpu {
            fn drop(&mut self) {
                // SAFETY: test harness; `run` waited the queue idle before returning.
                unsafe {
                    self.device.destroy_command_pool(self.pool, None);
                    self.device.destroy_device(None);
                    self.instance.destroy_instance(None);
                }
            }
        }

        fn range() -> vk::ImageSubresourceRange {
            vk::ImageSubresourceRange::default()
                .aspect_mask(vk::ImageAspectFlags::COLOR)
                .level_count(1)
                .layer_count(1)
        }

        /// # Safety
        /// `cmd` is recording and `image` is live.
        unsafe fn transition(
            d: &ash::Device,
            cmd: vk::CommandBuffer,
            image: vk::Image,
            from: vk::ImageLayout,
            to: vk::ImageLayout,
        ) {
            let b = vk::ImageMemoryBarrier::default()
                .old_layout(from)
                .new_layout(to)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(range());
            // SAFETY: per this fn's contract.
            unsafe {
                d.cmd_pipeline_barrier(
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
    }
}
