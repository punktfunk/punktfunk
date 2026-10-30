//! NV12→RGBA color-space conversion: CICP coefficient rows (the GL presenter's
//! `yuv_to_rgb`, folded for the shader's push constants) and the pipeline that
//! renders imported planes into the presenter's video image.
//!
//! The colour maths is never `VK_KHR_sampler_ycbcr_conversion`: the ext cannot express
//! the PQ path, and range handling varies by driver. A device with no plane views takes
//! its chroma reconstruction alone ([`CscPass::new_ycbcr`]). Coefficients live in
//! `pf_client_core::video::csc_rows` (shared with D3D11 and the Apple port).

use crate::vk::gpu::Pass;
use anyhow::{Context as _, Result};
use ash::vk;

pub use pf_client_core::video::csc_rows;

/// Pass objects except the per-video-size framebuffer (that lives with the video
/// image). No device handle is stored; [`CscPass::destroy`] runs from the presenter's `Drop`.
#[derive(Default)]
pub struct CscPass {
    pub pass: Pass,
    pub desc_set: vk::DescriptorSet,
    /// The immutable sampler's conversion, on the [`Self::new_ycbcr`] pipe only.
    ycbcr: vk::SamplerYcbcrConversion,
}

/// Which pass samples a frame.
#[derive(Clone, Copy, PartialEq)]
pub enum CscPipe {
    /// Two plane views: dmabuf, Vulkan Video, D3D11 slots.
    Nv12,
    /// Three plane views: PyroWave, software I420.
    Planar,
    /// One view through a sampler Y'CbCr conversion: Vulkan Video with no plane views.
    Ycbcr,
}

impl CscPass {
    /// `attachment_format` is the video image: R8G8B8A8 for SDR, 10-bit for PQ
    /// (8 bits band the PQ curve).
    pub fn new(device: &ash::Device, attachment_format: vk::Format) -> Result<CscPass> {
        Self::build(
            device,
            attachment_format,
            2,
            pf_client_core::video_csc_spv::NV12_CSC_FRAG,
            None,
        )
    }

    /// Planar 3-plane variant (separate Cb/Cr R8). Same push-constant contract.
    /// Not feature- or probe-gated: the CPU I420 rung is the ladder's last step
    /// and must exist on every device.
    pub fn new_planar(device: &ash::Device, attachment_format: vk::Format) -> Result<CscPass> {
        Self::build(
            device,
            attachment_format,
            3,
            pf_client_core::video_csc_spv::PLANAR_CSC_FRAG,
            None,
        )
    }

    /// One binding, read through an immutable sampler holding `conversion`, which must be
    /// defined identically to the conversion on the bound view. `descriptors` is the
    /// driver's `combinedImageSamplerDescriptorCount` for the picture format.
    pub fn new_ycbcr(
        device: &ash::Device,
        attachment_format: vk::Format,
        conversion: &vk::SamplerYcbcrConversionCreateInfo<'_>,
        descriptors: u32,
    ) -> Result<CscPass> {
        Self::build(
            device,
            attachment_format,
            1,
            pf_client_core::video_csc_spv::YCBCR_CSC_FRAG,
            Some((conversion, descriptors)),
        )
    }

    /// Null handles first, filled in order; a failed step destroys what was made.
    fn build(
        device: &ash::Device,
        attachment_format: vk::Format,
        plane_bindings: u32,
        frag_spv: &[u8],
        ycbcr: Option<(&vk::SamplerYcbcrConversionCreateInfo<'_>, u32)>,
    ) -> Result<CscPass> {
        let fill =
            |p: &mut CscPass| p.fill(device, attachment_format, plane_bindings, frag_spv, ycbcr);
        fill_or_destroy(device, fill, Self::destroy)
    }

    fn fill(
        &mut self,
        device: &ash::Device,
        attachment_format: vk::Format,
        plane_bindings: u32,
        frag_spv: &[u8],
        ycbcr: Option<(&vk::SamplerYcbcrConversionCreateInfo<'_>, u32)>,
    ) -> Result<()> {
        // Fully overwritten (DONT_CARE load). Ends TRANSFER_SRC so the letterbox blit
        // needs no extra barrier.
        let attachment = [vk::AttachmentDescription::default()
            .format(attachment_format)
            .samples(vk::SampleCountFlags::TYPE_1)
            .load_op(vk::AttachmentLoadOp::DONT_CARE)
            .store_op(vk::AttachmentStoreOp::STORE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)];
        let color_ref = [vk::AttachmentReference::default()
            .attachment(0)
            .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
        let subpass = [vk::SubpassDescription::default()
            .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
            .color_attachments(&color_ref)];
        // Conservative scopes; matches the presenter's per-frame barrier granularity.
        let deps = [
            vk::SubpassDependency::default()
                .src_subpass(vk::SUBPASS_EXTERNAL)
                .dst_subpass(0)
                .src_stage_mask(vk::PipelineStageFlags::ALL_COMMANDS)
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
            vk::SubpassDependency::default()
                .src_subpass(0)
                .dst_subpass(vk::SUBPASS_EXTERNAL)
                .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
        ];
        // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
        self.pass.render_pass = unsafe {
            device.create_render_pass(
                &vk::RenderPassCreateInfo::default()
                    .attachments(&attachment)
                    .subpasses(&subpass)
                    .dependencies(&deps),
                None,
            )
        }
        .context("CSC render pass")?;

        // A conversion's sampler filters as its chroma does.
        let filter = ycbcr.map_or(vk::Filter::LINEAR, |(info, _)| info.chroma_filter);
        let mut sampler = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let mut converted = vk::SamplerYcbcrConversionInfo::default();
        if let Some((info, _)) = ycbcr {
            // SAFETY: handles owned by this type live for the call; `info` is the caller's.
            self.ycbcr = unsafe { device.create_sampler_ycbcr_conversion(info, None) }
                .context("Y'CbCr conversion")?;
            converted = converted.conversion(self.ycbcr);
            sampler = sampler.push_next(&mut converted);
        }
        // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
        self.pass.sampler = unsafe { device.create_sampler(&sampler, None) }?;

        let samplers = [self.pass.sampler];
        let bindings: Vec<vk::DescriptorSetLayoutBinding> = (0..plane_bindings)
            .map(|b| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(b)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT)
                    .immutable_samplers(&samplers)
            })
            .collect();
        // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
        self.pass.set_layout = unsafe {
            device.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                None,
            )
        }?;
        let set_layouts = [self.pass.set_layout];
        let push = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .size(64)]; // three vec4 rows + a params vec4 (mode, tonemap peak)
                        // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
        self.pass.pipeline_layout = unsafe {
            device.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&set_layouts)
                    .push_constant_ranges(&push),
                None,
            )
        }?;

        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(plane_bindings * ycbcr.map_or(1, |(_, descriptors)| descriptors))];
        // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
        self.pass.desc_pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }?;
        // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
        self.desc_set = unsafe {
            device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pass.desc_pool)
                    .set_layouts(&set_layouts),
            )
        }?[0];

        self.pass.pipeline = build_fullscreen_pipeline(
            device,
            self.pass.render_pass,
            self.pass.pipeline_layout,
            frag_spv,
            false, // opaque: CSC output is the video
        )?;

        Ok(())
    }

    /// Only while no submitted command buffer references the set — the
    /// presenter's in-flight fence is waited before every record.
    pub fn bind_planes(&self, device: &ash::Device, luma: vk::ImageView, chroma: vk::ImageView) {
        let infos = [luma, chroma].map(|view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        });
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[0]),
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[1]),
        ];
        // SAFETY: no submitted command buffer references this set (in-flight fence
        // waited before every record); views and sampler outlive the write.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    /// One view made with the conversion this pass was built on. Same fence-wait contract
    /// as [`bind_planes`](Self::bind_planes).
    pub fn bind_ycbcr(&self, device: &ash::Device, view: vk::ImageView) {
        let info = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let write = vk::WriteDescriptorSet::default()
            .dst_set(self.desc_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&info);
        // SAFETY: no submitted command buffer references this set (in-flight fence
        // waited before every record); the view and sampler outlive the write.
        unsafe { device.update_descriptor_sets(&[write], &[]) };
    }

    /// Three single-component plane views in the producer's layout: GENERAL for
    /// pyrowave decode, `SHADER_READ_ONLY_OPTIMAL` for the software upload. Same
    /// fence-wait contract as [`bind_planes`](Self::bind_planes).
    pub fn bind_planes_planar(
        &self,
        device: &ash::Device,
        planes: [vk::ImageView; 3],
        layout: vk::ImageLayout,
    ) {
        let infos = planes.map(|view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(layout)]
        });
        let writes = [0u32, 1, 2].map(|b| {
            vk::WriteDescriptorSet::default()
                .dst_set(self.desc_set)
                .dst_binding(b)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&infos[b as usize])
        });
        // SAFETY: no submitted command buffer references this set (in-flight fence
        // waited before every record); views and sampler outlive the write.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
    }

    /// [`Pass::destroy`]: the set goes with the pool. The conversion outlives its sampler.
    pub fn destroy(&mut self, device: &ash::Device) {
        self.pass.destroy(device);
        // SAFETY: DESTROY per the crate contract; a null handle is skipped by Vulkan.
        unsafe { device.destroy_sampler_ycbcr_conversion(self.ycbcr, None) };
        self.ycbcr = vk::SamplerYcbcrConversion::null();
    }
}

/// CSC straight into a swapchain image, taken when the whole picture lands on the surface at
/// an exact scale. Skips the video image and its blit: at 4K that is two writes and a read of
/// 33 MB per frame, most of an iGPU's present cost. Pipelines reuse the two CSC layouts, so
/// their descriptor sets and push constants bind unchanged; the passes fit the overlay's
/// per-image framebuffers. `clear` paints the letterbox first, `keep` skips that fill.
#[derive(Default)]
pub struct DirectPass {
    pub clear: vk::RenderPass,
    pub keep: vk::RenderPass,
    pub nv12: vk::Pipeline,
    pub planar: vk::Pipeline,
}

impl DirectPass {
    /// `format` is the swapchain's. Rebuilt with the overlay pipe when that changes.
    pub fn new(
        device: &ash::Device,
        format: vk::Format,
        nv12_layout: vk::PipelineLayout,
        planar_layout: vk::PipelineLayout,
    ) -> Result<DirectPass> {
        let (from, out) = (
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        );
        let clear = color_pass(device, format, vk::AttachmentLoadOp::CLEAR, from, out)?;
        let mut pass = DirectPass {
            clear,
            keep: vk::RenderPass::null(),
            nv12: vk::Pipeline::null(),
            planar: vk::Pipeline::null(),
        };
        let built = (|| {
            pass.keep = color_pass(device, format, vk::AttachmentLoadOp::DONT_CARE, from, out)?;
            let frag = pf_client_core::video_csc_spv::NV12_CSC_FRAG;
            pass.nv12 = build_fullscreen_pipeline(device, pass.keep, nv12_layout, frag, false)?;
            let frag = pf_client_core::video_csc_spv::PLANAR_CSC_FRAG;
            pass.planar = build_fullscreen_pipeline(device, pass.keep, planar_layout, frag, false)?;
            Ok::<(), anyhow::Error>(())
        })();
        match built {
            Ok(()) => Ok(pass),
            Err(e) => {
                pass.destroy(device);
                Err(e).context("direct CSC pass")
            }
        }
    }

    /// GPU idle on the last submit that used these.
    pub fn destroy(&self, device: &ash::Device) {
        // SAFETY: DESTROY per the crate contract; null handles are skipped by Vulkan.
        unsafe {
            device.destroy_pipeline(self.planar, None);
            device.destroy_pipeline(self.nv12, None);
            device.destroy_render_pass(self.keep, None);
            device.destroy_render_pass(self.clear, None);
        }
    }
}

/// The driver's `combinedImageSamplerDescriptorCount` for `format`: a descriptor over a
/// conversion can take more than one, and [`CscPass::new_ycbcr`] sizes its pool by this.
pub(crate) fn ycbcr_descriptors(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
) -> Result<u32> {
    let mut limits = vk::SamplerYcbcrConversionImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut limits);
    let query = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED);
    // SAFETY: `pdev` is `instance`'s; `query` and `props` are locals outliving the call.
    unsafe { instance.get_physical_device_image_format_properties2(pdev, &query, &mut props) }
        .context("query the Y'CbCr descriptor count")?;
    Ok(limits.combined_image_sampler_descriptor_count.max(1))
}

/// A pass from null handles: `fill` makes its objects in order, and on a failed step
/// `destroy` frees what it made (null handles are no-ops), so a constructor leaves nothing.
pub(crate) fn fill_or_destroy<T: Default>(
    device: &ash::Device,
    fill: impl FnOnce(&mut T) -> Result<()>,
    destroy: impl FnOnce(&mut T, &ash::Device),
) -> Result<T> {
    let mut pass = T::default();
    match fill(&mut pass) {
        Ok(()) => Ok(pass),
        Err(e) => {
            destroy(&mut pass, device);
            Err(e)
        }
    }
}

/// One colour attachment behind the one subpass dependency every composite pass shares.
/// Passes that differ only in load op and layouts are compatible, so a framebuffer made
/// for any of them (the overlay's per-image ones) fits the scale and direct passes too.
pub(crate) fn color_pass(
    device: &ash::Device,
    format: vk::Format,
    load: vk::AttachmentLoadOp,
    initial: vk::ImageLayout,
    final_layout: vk::ImageLayout,
) -> Result<vk::RenderPass> {
    let attachment = [vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(load)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(initial)
        .final_layout(final_layout)];
    let color_ref = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref)];
    let deps = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::ALL_COMMANDS)
        .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(
            vk::AccessFlags::COLOR_ATTACHMENT_READ | vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        )];
    // SAFETY: CREATE per the crate contract.
    unsafe {
        device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attachment)
                .subpasses(&subpass)
                .dependencies(&deps),
            None,
        )
    }
    .context("composite render pass")
}

/// Bufferless fullscreen triangle (`fullscreen.vert` + `frag_spv`), dynamic
/// viewport/scissor. `blend` is premultiplied-alpha over the destination
/// (overlay); `false` is opaque write (CSC).
pub(crate) fn build_fullscreen_pipeline(
    device: &ash::Device,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    frag_spv: &[u8],
    blend: bool,
) -> Result<vk::Pipeline> {
    // The blobs are `include_bytes!` in pf-client-core, whose alignment is unspecified;
    // read_spv copies into an aligned Vec<u32>.
    let vert = ash::util::read_spv(&mut std::io::Cursor::new(
        pf_client_core::video_csc_spv::FULLSCREEN_VERT,
    ))?;
    let frag = ash::util::read_spv(&mut std::io::Cursor::new(frag_spv))?;
    // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
    let vert_mod = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&vert), None)
    }?;
    // SAFETY: handles owned by this type live for the call; builder structs are locals that outlive it.
    let frag_mod = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&frag), None)
    };
    let frag_mod = match frag_mod {
        Ok(m) => m,
        Err(e) => {
            // SAFETY: vert_mod is owned here and was never submitted.
            unsafe { device.destroy_shader_module(vert_mod, None) };
            return Err(e).context("fragment shader module");
        }
    };

    let entry = c"main";
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vert_mod)
            .name(entry),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(frag_mod)
            .name(entry),
    ];
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default(); // bufferless
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    // Video size changes with stream mode; do not bake viewport/scissor into the pipeline.
    let viewport = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    let blend_attachment = [if blend {
        // Premultiplied alpha (Skia surfaces are premultiplied).
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
    } else {
        vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA)
    }];
    let blend = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);

    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewport)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .render_pass(render_pass);
    let pipeline =
        // SAFETY: live device; builder structs are locals that outlive the call; returned handle is owned here.
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None) }
            .map_err(|(_, e)| e)
            .context("CSC pipeline");
    // SAFETY: modules were used only to create the pipeline and were never submitted.
    unsafe {
        device.destroy_shader_module(vert_mod, None);
        device.destroy_shader_module(frag_mod, None);
    }
    Ok(pipeline?[0])
}

#[cfg(test)]
mod tests;
