//! The RGB→YUV CSC front end PyroWave and Vulkan Video share: the per-slot cursor plane, the
//! RGB binding, and the layout every `rgb2yuv*.comp` shader reads. Each backend picks its own
//! shader.
// `unsafe_op_in_unsafe_fn` off, as in `vk_util.rs`: every call is a plain ash call.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::vk_util::{color_range, make_host_buffer, make_plain_image};
use anyhow::Result;
use ash::vk;

/// Cursor overlay cap (px). The CSC shader bounds sampling by push constant, so one allocation
/// fits every pointer bitmap; a larger one uploads its top-left corner.
pub(crate) const CURSOR_MAX: u32 = 256;

/// One slot's cursor overlay: a `CURSOR_MAX`² RGBA8 image at binding 3 plus its host staging.
/// Per slot: a shared image races the previous frame's sampled read, and a shared serial leaves
/// the other slot showing the previous pointer. [`Default`] is all-null.
pub(crate) struct CursorPlane {
    pub(crate) img: vk::Image,
    mem: vk::DeviceMemory,
    pub(crate) view: vk::ImageView,
    pub(crate) stage: vk::Buffer,
    stage_mem: vk::DeviceMemory,
    /// Serial of the uploaded bitmap. `u64::MAX` before the first: a real serial may be 0.
    pub(crate) serial: u64,
    /// The image has left UNDEFINED.
    pub(crate) ready: bool,
}

impl Default for CursorPlane {
    fn default() -> Self {
        Self {
            img: vk::Image::null(),
            mem: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            stage: vk::Buffer::null(),
            stage_mem: vk::DeviceMemory::null(),
            serial: u64::MAX,
            ready: false,
        }
    }
}

impl CursorPlane {
    /// The image and its staging. On failure nothing is left to destroy.
    pub(crate) unsafe fn new(
        dev: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<Self> {
        let (img, mem, view) = make_plain_image(
            dev,
            mem_props,
            vk::Format::R8G8B8A8_UNORM,
            CURSOR_MAX,
            CURSOR_MAX,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        )?;
        let mut me = Self {
            img,
            mem,
            view,
            ..Self::default()
        };
        match make_host_buffer(
            dev,
            mem_props,
            (CURSOR_MAX * CURSOR_MAX * 4) as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
        ) {
            Ok((stage, stage_mem)) => {
                (me.stage, me.stage_mem) = (stage, stage_mem);
                Ok(me)
            }
            Err(e) => {
                me.destroy(dev);
                Err(e)
            }
        }
    }

    /// Bring the image up to date in `cmd` and return the push constant
    /// `[origin_x, origin_y, size_w, size_h]` (size 0 ⇒ the CSC skips the blend). Uploads only
    /// when the serial changed. First use always leaves UNDEFINED, so binding 3 is in a valid
    /// layout with no cursor. A `pq` session blends the cursor re-encoded as PQ.
    pub(crate) unsafe fn prep(
        &mut self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        cursor: Option<&pf_frame::CursorOverlay>,
        pq: bool,
    ) -> Result<[i32; 4]> {
        let img = self.img;
        let barrier = |old: vk::ImageLayout, new: vk::ImageLayout, ss, sa, ds, da| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(ss)
                .src_access_mask(sa)
                .dst_stage_mask(ds)
                .dst_access_mask(da)
                .old_layout(old)
                .new_layout(new)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        match cursor {
            Some(c) if !c.rgba.is_empty() => {
                let cw = c.w.min(CURSOR_MAX);
                let ch = c.h.min(CURSOR_MAX);
                if self.serial != c.serial {
                    let px = if pq { c.pq_rgba() } else { c.rgba.clone() };
                    let bytes = (cw as usize) * (ch as usize) * 4;
                    let ptr = dev.map_memory(
                        self.stage_mem,
                        0,
                        bytes as u64,
                        vk::MemoryMapFlags::empty(),
                    )?;
                    std::ptr::copy_nonoverlapping(px.as_ptr(), ptr as *mut u8, bytes.min(px.len()));
                    dev.unmap_memory(self.stage_mem);
                    let old = if self.ready {
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                    } else {
                        vk::ImageLayout::UNDEFINED
                    };
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            old,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                        )]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        cmd,
                        self.stage,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            .image_extent(vk::Extent3D {
                                width: cw,
                                height: ch,
                                depth: 1,
                            })],
                    );
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::ALL_TRANSFER,
                            vk::AccessFlags2::TRANSFER_WRITE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.serial = c.serial;
                    self.ready = true;
                }
                Ok([c.x, c.y, cw as i32, ch as i32])
            }
            _ => {
                if !self.ready {
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::PipelineStageFlags2::NONE,
                            vk::AccessFlags2::NONE,
                            vk::PipelineStageFlags2::COMPUTE_SHADER,
                            vk::AccessFlags2::SHADER_READ,
                        )]),
                    );
                    self.ready = true;
                }
                Ok([0, 0, 0, 0])
            }
        }
    }

    /// Destroy every handle. A null one, from a partly built slot, is a no-op.
    pub(crate) unsafe fn destroy(self, dev: &ash::Device) {
        dev.destroy_image_view(self.view, None);
        dev.destroy_image(self.img, None);
        dev.free_memory(self.mem, None);
        dev.destroy_buffer(self.stage, None);
        dev.free_memory(self.stage_mem, None);
    }
}

/// Point a CSC set's binding 0 at this frame's RGB view. The set must not be bound by a
/// PENDING command buffer (VUID-vkUpdateDescriptorSets-None-03047).
pub(crate) unsafe fn bind_rgb(
    dev: &ash::Device,
    sampler: vk::Sampler,
    set: vk::DescriptorSet,
    view: vk::ImageView,
) {
    let ii = [vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    dev.update_descriptor_sets(
        &[vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&ii)],
        &[],
    );
}

/// The layout every `rgb2yuv*.comp` shader reads: bindings 0 (RGB) and 3 (cursor) sampled, 1–2
/// the storage planes, and a 16-byte push constant `{ivec2 origin, ivec2 size}` whose
/// `size.x <= 0` disables the blend. On failure nothing is left to destroy.
pub(crate) unsafe fn csc_layout(
    dev: &ash::Device,
) -> Result<(vk::DescriptorSetLayout, vk::PipelineLayout)> {
    let sb = |b: u32, t: vk::DescriptorType| {
        vk::DescriptorSetLayoutBinding::default()
            .binding(b)
            .descriptor_type(t)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
    };
    let bindings = [
        sb(0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
        sb(1, vk::DescriptorType::STORAGE_IMAGE),
        sb(2, vk::DescriptorType::STORAGE_IMAGE),
        sb(3, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
    ];
    let dsl = dev.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
        None,
    )?;
    let dsls = [dsl];
    let pc_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size(16)];
    match dev.create_pipeline_layout(
        &vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&dsls)
            .push_constant_ranges(&pc_ranges),
        None,
    ) {
        Ok(layout) => Ok((dsl, layout)),
        Err(e) => {
            dev.destroy_descriptor_set_layout(dsl, None);
            Err(e.into())
        }
    }
}
