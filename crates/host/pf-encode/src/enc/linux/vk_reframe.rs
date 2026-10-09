//! The host's reframe on the compute queue (`reframe.comp`): the crop of each captured picture,
//! scaled to the session's size in two separable passes, feeding the CSC. One set of images
//! per ring slot, so a frame still in flight never shares them.
// `unsafe_op_in_unsafe_fn` off, as in `vk_util.rs`: every body here is raw ash calls.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::vk_util::{color_range, make_plain_image};
use anyhow::Result;
use ash::vk;

// Source `reframe.comp`; regenerate with `glslangValidator -V reframe.comp -o reframe.spv`.
const REFRAME_SPV: &[u8] = include_bytes!("reframe.spv");
/// `{ivec2 origin, ivec2 extent, float step, int axis, ivec2 cur_origin, ivec2 cur_size}`.
const PUSH_BYTES: usize = 40;
const FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;

type Plain = (vk::Image, vk::DeviceMemory, vk::ImageView);

#[derive(Default)]
struct Slot {
    /// `out_w × crop_h`: the x pass.
    tmp: Plain,
    /// `out_w × out_h`: the y pass, the CSC's source.
    dst: Plain,
    set_x: vk::DescriptorSet,
    set_y: vk::DescriptorSet,
}

#[derive(Default)]
pub(super) struct Reframe {
    crop: [u32; 4],
    out: (u32, u32),
    pipe: vk::Pipeline,
    layout: vk::PipelineLayout,
    dsl: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    slots: Vec<Slot>,
}

impl Reframe {
    /// Build the stage for `slots` ring slots. On error everything built so far is released.
    pub(super) unsafe fn new(
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        crop: [u32; 4],
        out: (u32, u32),
        slots: usize,
    ) -> Result<Reframe> {
        let mut me = Reframe {
            crop,
            out,
            ..Default::default()
        };
        match me.build(device, mem_props, slots) {
            Ok(()) => Ok(me),
            Err(e) => {
                me.destroy(device);
                Err(e)
            }
        }
    }

    unsafe fn build(
        &mut self,
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        slots: usize,
    ) -> Result<()> {
        let binding = |b: u32, t: vk::DescriptorType| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(t)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [
            binding(0, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
            binding(1, vk::DescriptorType::STORAGE_IMAGE),
            binding(2, vk::DescriptorType::COMBINED_IMAGE_SAMPLER),
        ];
        self.dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        let dsls = [self.dsl];
        let ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(PUSH_BYTES as u32)];
        self.layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&dsls)
                .push_constant_ranges(&ranges),
            None,
        )?;
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(REFRAME_SPV))?;
        let shader =
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
        let pipe = device.create_compute_pipelines(
            vk::PipelineCache::null(),
            &[vk::ComputePipelineCreateInfo::default()
                .layout(self.layout)
                .stage(
                    vk::PipelineShaderStageCreateInfo::default()
                        .stage(vk::ShaderStageFlags::COMPUTE)
                        .module(shader)
                        .name(c"main"),
                )],
            None,
        );
        device.destroy_shader_module(shader, None);
        self.pipe = pipe.map_err(|(_, e)| e)?[0];

        let n = slots as u32;
        let sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(4 * n),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * n),
        ];
        self.pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(2 * n)
                .pool_sizes(&sizes),
            None,
        )?;
        let usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED;
        for _ in 0..slots {
            self.slots.push(Slot::default());
            let slot = self.slots.last_mut().expect("just pushed");
            slot.tmp =
                make_plain_image(device, mem_props, FORMAT, self.out.0, self.crop[3], usage)?;
            slot.dst = make_plain_image(device, mem_props, FORMAT, self.out.0, self.out.1, usage)?;
            let layouts = [self.dsl, self.dsl];
            let sets = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&layouts),
            )?;
            (slot.set_x, slot.set_y) = (sets[0], sets[1]);
        }
        Ok(())
    }

    /// Whether a `width × height` capture holds the crop.
    pub(super) fn covers(&self, width: u32, height: u32) -> bool {
        let [x, y, w, h] = self.crop;
        x + w <= width && y + h <= height
    }

    /// Record both passes for `slot` into `cmd`, reading `src` and `cursor` (both
    /// `SHADER_READ_ONLY_OPTIMAL`, sampled through `sampler`) with the cursor at
    /// `[x, y, w, h]` in source pixels. Returns the slot's picture, left
    /// `SHADER_READ_ONLY_OPTIMAL` for the CSC.
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn record(
        &self,
        device: &ash::Device,
        cmd: vk::CommandBuffer,
        slot: usize,
        sampler: vk::Sampler,
        src: vk::ImageView,
        cursor: vk::ImageView,
        cursor_rect: [i32; 4],
    ) -> vk::ImageView {
        let s = &self.slots[slot];
        let [cx, cy, cw, ch] = self.crop.map(|v| v as i32);
        let (ow, oh) = self.out;
        let sampled = |view| {
            [vk::DescriptorImageInfo::default()
                .sampler(sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
        };
        let storage = |view| {
            [vk::DescriptorImageInfo::default()
                .image_view(view)
                .image_layout(vk::ImageLayout::GENERAL)]
        };
        let (src_x, out_x, cur) = (sampled(src), storage(s.tmp.2), sampled(cursor));
        let (src_y, out_y) = (sampled(s.tmp.2), storage(s.dst.2));
        fn write<'a>(
            set: vk::DescriptorSet,
            binding: u32,
            ty: vk::DescriptorType,
            info: &'a [vk::DescriptorImageInfo],
        ) -> vk::WriteDescriptorSet<'a> {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(binding)
                .descriptor_type(ty)
                .image_info(info)
        }
        let sampler_ty = vk::DescriptorType::COMBINED_IMAGE_SAMPLER;
        let storage_ty = vk::DescriptorType::STORAGE_IMAGE;
        device.update_descriptor_sets(
            &[
                write(s.set_x, 0, sampler_ty, &src_x),
                write(s.set_x, 1, storage_ty, &out_x),
                write(s.set_x, 2, sampler_ty, &cur),
                write(s.set_y, 0, sampler_ty, &src_y),
                write(s.set_y, 1, storage_ty, &out_y),
                write(s.set_y, 2, sampler_ty, &cur),
            ],
            &[],
        );
        // `src_access`: a pass's `imageStore`s must be made available before the next reads.
        let barrier = |img, old, new, src_access, dst_access| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(src_access)
                .dst_access_mask(dst_access)
                .old_layout(old)
                .new_layout(new)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(img)
                .subresource_range(color_range(0))
        };
        let (undefined, general, read) = (
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        );
        let none = vk::AccessFlags2::NONE;
        let write_access = vk::AccessFlags2::SHADER_WRITE;
        let read_access = vk::AccessFlags2::SHADER_READ;
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[
                barrier(s.tmp.0, undefined, general, none, write_access),
                barrier(s.dst.0, undefined, general, none, write_access),
            ]),
        );
        device.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipe);

        let pass = |set,
                    origin: [i32; 2],
                    extent: [i32; 2],
                    step: f32,
                    axis: i32,
                    cursor: [i32; 4],
                    groups: (u32, u32)| {
            device.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[set],
                &[],
            );
            let words: [i32; 10] = [
                origin[0],
                origin[1],
                extent[0],
                extent[1],
                i32::from_ne_bytes(step.to_ne_bytes()),
                axis,
                cursor[0],
                cursor[1],
                cursor[2],
                cursor[3],
            ];
            let mut bytes = [0u8; PUSH_BYTES];
            for (i, w) in words.iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes());
            }
            device.cmd_push_constants(cmd, self.layout, vk::ShaderStageFlags::COMPUTE, 0, &bytes);
            device.cmd_dispatch(cmd, groups.0.div_ceil(16), groups.1.div_ceil(16), 1);
        };
        pass(
            s.set_x,
            [cx, cy],
            [cw, ch],
            cw as f32 / ow as f32,
            0,
            cursor_rect,
            (ow, ch as u32),
        );
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                s.tmp.0,
                general,
                read,
                write_access,
                read_access,
            )]),
        );
        pass(
            s.set_y,
            [0, 0],
            [ow as i32, ch],
            ch as f32 / oh as f32,
            1,
            [0; 4],
            (ow, oh),
        );
        device.cmd_pipeline_barrier2(
            cmd,
            &vk::DependencyInfo::default().image_memory_barriers(&[barrier(
                s.dst.0,
                general,
                read,
                write_access,
                read_access,
            )]),
        );
        s.dst.2
    }

    /// Release everything. The device must be idle for this stage; null handles are skipped.
    pub(super) unsafe fn destroy(&mut self, device: &ash::Device) {
        for s in self.slots.drain(..) {
            for (img, mem, view) in [s.tmp, s.dst] {
                device.destroy_image_view(view, None);
                device.destroy_image(img, None);
                device.free_memory(mem, None);
            }
        }
        device.destroy_descriptor_pool(self.pool, None);
        device.destroy_pipeline(self.pipe, None);
        device.destroy_pipeline_layout(self.layout, None);
        device.destroy_descriptor_set_layout(self.dsl, None);
        *self = Reframe::default();
    }
}
