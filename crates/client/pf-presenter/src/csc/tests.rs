//! The conversion pipe against the plane-view pipe on a real device: one picture through both
//! must draw the same, on a device with plane views where both routes exist.

use super::{csc_rows, ycbcr_descriptors, CscPass};
use crate::vk::gpu::{allocate, RenderImage};
use anyhow::{Context as _, Result};
use ash::vk;
use pf_client_core::video::ColorDesc;
use pf_vkdecode::caps::device_ycbcr_conversion;
use pf_vkdecode::{plane_formats, NV12, P010};

const SIZE: (u32, u32) = (64, 32);
const TARGET: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;
/// Rounding of the 10-bit target.
const ROUNDING: i32 = 1;
/// A half-luma-pixel chroma shift moves the pattern's hard Cb edges by far more than rounding,
/// and by far less than a swapped channel would.
const SHIFTED: std::ops::Range<i32> = 40..300;

/// BT.709 limited: what an SDR stream signals.
const COLOR: ColorDesc = ColorDesc {
    primaries: 1,
    transfer: 1,
    matrix: 1,
    full_range: false,
};

/// Needs an ICD; lavapipe does. `cargo test -p pf-presenter --lib csc -- --ignored`.
#[test]
#[ignore = "needs a Vulkan device"]
fn the_conversion_pipe_draws_what_the_plane_pipe_draws() {
    let gpu = Gpu::new().expect("a Vulkan device");
    for format in [NV12, P010] {
        // SAFETY: `pdev` is `instance`'s.
        let info = unsafe { device_ycbcr_conversion(&gpu.instance, gpu.pdev, format) };
        let info = info.expect("a conversion for the picture format");
        let planes = gpu.through_planes(format).expect("plane pipe drew");
        let converted = gpu.through_conversion(format, &info).expect("pipe drew");
        let worst = worst_channel(&planes, &converted);
        let (x, y) = (info.x_chroma_offset, info.y_chroma_offset);
        eprintln!(
            "{format:?}: x {x:?} y {y:?} {:?}: {worst} codes apart",
            info.chroma_filter
        );
        if (x, y)
            != (
                vk::ChromaLocation::COSITED_EVEN,
                vk::ChromaLocation::MIDPOINT,
            )
        {
            // A driver with one siting draws the picture half a luma pixel off, and no more.
            assert!(worst < SHIFTED.end, "{format:?}: {worst} codes apart");
            continue;
        }
        assert!(worst <= ROUNDING, "{format:?}: {worst} codes apart");

        // The check sees siting: the plane shader's chroma is left-cosited, this one is not.
        let centred = info.x_chroma_offset(vk::ChromaLocation::MIDPOINT);
        let shifted = gpu.through_conversion(format, &centred).expect("pipe drew");
        let worst = worst_channel(&planes, &shifted);
        assert!(SHIFTED.contains(&worst), "{format:?}: {worst} codes apart");
    }
}

/// The largest difference of any colour channel at any pixel, in 10-bit codes.
fn worst_channel(a: &[u32], b: &[u32]) -> i32 {
    assert_eq!(a.len(), b.len());
    let channel = |px: u32, shift: u32| ((px >> shift) & 0x3ff) as i32;
    a.iter()
        .zip(b)
        .flat_map(|(a, b)| [0, 10, 20].map(|s| (channel(*a, s) - channel(*b, s)).abs()))
        .max()
        .unwrap_or(0)
}

/// Luma ramps, and Cb that flips every three chroma texels so a siting error shows. Codes are
/// 8-bit, widened and MSB-packed for P010.
fn picture_bytes(format: vk::Format) -> (Vec<u8>, u64) {
    let (w, h) = SIZE;
    let mut codes = Vec::new();
    for y in 0..h {
        codes.extend((0..w).map(|x| 16 + (x * 7 + y * 13) % 220));
    }
    let luma = codes.len() as u64;
    for y in 0..h / 2 {
        for x in 0..w / 2 {
            codes.push(if x % 6 < 3 { 64 } else { 192 });
            codes.push(128 + (y * 16) % 64);
        }
    }
    if format == P010 {
        let bytes = codes.iter().flat_map(|c| ((c << 8) as u16).to_le_bytes());
        (bytes.collect(), luma * 2)
    } else {
        (codes.iter().map(|c| *c as u8).collect(), luma)
    }
}

/// A headless device, one draw at a time. Test-only; nothing here is reused by the presenter.
struct Gpu {
    _entry: ash::Entry,
    instance: ash::Instance,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    queue: vk::Queue,
    pool: vk::CommandPool,
}

impl Gpu {
    fn new() -> Result<Gpu> {
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
            let mut ycbcr = vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default()
                .sampler_ycbcr_conversion(true);
            let device = instance.create_device(
                pdev,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .push_next(&mut ycbcr),
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
                pdev,
                device,
                pool,
            })
        }
    }

    /// A one-shot command buffer, waited.
    fn submit(&self, record: impl FnOnce(vk::CommandBuffer)) -> Result<()> {
        // SAFETY: test harness; the buffer and fence are this call's and waited below.
        unsafe {
            let cmd = self.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            record(cmd);
            self.device.end_command_buffer(cmd)?;
            let fence = self
                .device
                .create_fence(&vk::FenceCreateInfo::default(), None)?;
            let cmds = [cmd];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds);
            self.device.queue_submit(self.queue, &[submit], fence)?;
            self.device.wait_for_fences(&[fence], true, u64::MAX)?;
            self.device.destroy_fence(fence, None);
            self.device.free_command_buffers(self.pool, &cmds);
        }
        Ok(())
    }

    /// A host-visible buffer the caller frees.
    fn buffer(
        &self,
        bytes: u64,
        usage: vk::BufferUsageFlags,
    ) -> Result<(vk::Buffer, vk::DeviceMemory)> {
        // SAFETY: test harness; the caller destroys both.
        unsafe {
            let buffer = self.device.create_buffer(
                &vk::BufferCreateInfo::default().size(bytes).usage(usage),
                None,
            )?;
            let memory = allocate(
                &self.device,
                &self.mem_props,
                self.device.get_buffer_memory_requirements(buffer),
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            self.device.bind_buffer_memory(buffer, memory, 0)?;
            Ok((buffer, memory))
        }
    }

    /// The test picture in SHADER_READ_ONLY_OPTIMAL. `flags` is what the route creates it with.
    fn picture(
        &self,
        format: vk::Format,
        flags: vk::ImageCreateFlags,
    ) -> Result<(vk::Image, vk::DeviceMemory)> {
        let (bytes, luma) = picture_bytes(format);
        let (staging, staged) =
            self.buffer(bytes.len() as u64, vk::BufferUsageFlags::TRANSFER_SRC)?;
        // SAFETY: test harness: fresh handles, freed here or by the caller; the mapping is
        // the size of `bytes`; the fence in `submit` waits every recorded use.
        unsafe {
            let map = self.device.map_memory(
                staged,
                0,
                bytes.len() as u64,
                vk::MemoryMapFlags::empty(),
            )?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), map.cast::<u8>(), bytes.len());
            self.device.unmap_memory(staged);

            let image = self.device.create_image(
                &vk::ImageCreateInfo::default()
                    .flags(flags)
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: SIZE.0,
                        height: SIZE.1,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED),
                None,
            )?;
            let memory = allocate(
                &self.device,
                &self.mem_props,
                self.device.get_image_memory_requirements(image),
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            self.device.bind_image_memory(image, memory, 0)?;

            let hop = |from, to, dst_access| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(from)
                    .new_layout(to)
                    .dst_access_mask(dst_access)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(image)
                    .subresource_range(crate::vk::gpu::subresource_range())
            };
            let plane = |aspect, offset, (width, height)| {
                vk::BufferImageCopy::default()
                    .buffer_offset(offset)
                    .image_subresource(
                        vk::ImageSubresourceLayers::default()
                            .aspect_mask(aspect)
                            .layer_count(1),
                    )
                    .image_extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
            };
            self.submit(|cmd| {
                self.device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[hop(
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::AccessFlags::TRANSFER_WRITE,
                    )],
                );
                self.device.cmd_copy_buffer_to_image(
                    cmd,
                    staging,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[
                        plane(vk::ImageAspectFlags::PLANE_0, 0, SIZE),
                        plane(
                            vk::ImageAspectFlags::PLANE_1,
                            luma,
                            (SIZE.0 / 2, SIZE.1 / 2),
                        ),
                    ],
                );
                self.device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[hop(
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::AccessFlags::SHADER_READ,
                    )],
                );
            })?;
            self.device.destroy_buffer(staging, None);
            self.device.free_memory(staged, None);
            Ok((image, memory))
        }
    }

    fn view(
        &self,
        image: vk::Image,
        format: vk::Format,
        aspect: vk::ImageAspectFlags,
        conversion: vk::SamplerYcbcrConversion,
    ) -> Result<vk::ImageView> {
        let mut converted = vk::SamplerYcbcrConversionInfo::default().conversion(conversion);
        let mut info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(crate::vk::gpu::subresource_range().aspect_mask(aspect));
        if conversion != vk::SamplerYcbcrConversion::null() {
            info = info.push_next(&mut converted);
        }
        // SAFETY: test harness; the caller destroys the view before `image`.
        Ok(unsafe { self.device.create_image_view(&info, None) }?)
    }

    /// Run `pass` over the bound picture, as `record_csc` does, and read the target back.
    fn draw(&self, pass: &CscPass, format: vk::Format, msb_packed: bool) -> Result<Vec<u32>> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(TARGET)
            .extent(vk::Extent3D {
                width: SIZE.0,
                height: SIZE.1,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC);
        let target =
            RenderImage::build(&self.device, &self.mem_props, &info, pass.pass.render_pass)?;
        let bytes = u64::from(SIZE.0 * SIZE.1 * 4);
        let (readback, read_memory) = self.buffer(bytes, vk::BufferUsageFlags::TRANSFER_DST)?;
        let extent = vk::Extent2D {
            width: SIZE.0,
            height: SIZE.1,
        };
        let area = vk::Rect2D::default().extent(extent);
        let depth = if format == P010 { 10 } else { 8 };
        let mut pc = [0f32; 16];
        pc[..12].copy_from_slice(csc_rows(COLOR, depth, msb_packed).as_flattened());
        pc[14] = 1.0;
        pc[15] = 1.0;
        let words = pc.map(f32::to_ne_bytes);
        self.submit(|cmd| {
            // SAFETY: test harness: `cmd` is recording, every handle lives past the wait.
            unsafe {
                self.device.cmd_begin_render_pass(
                    cmd,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(pass.pass.render_pass)
                        .framebuffer(target.framebuffer)
                        .render_area(area),
                    vk::SubpassContents::INLINE,
                );
                self.device.cmd_bind_pipeline(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    pass.pass.pipeline,
                );
                self.device.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: SIZE.0 as f32,
                        height: SIZE.1 as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                self.device.cmd_set_scissor(cmd, 0, &[area]);
                self.device.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    pass.pass.pipeline_layout,
                    0,
                    &[pass.desc_set],
                    &[],
                );
                self.device.cmd_push_constants(
                    cmd,
                    pass.pass.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    words.as_flattened(),
                );
                self.device.cmd_draw(cmd, 3, 1, 0, 0);
                self.device.cmd_end_render_pass(cmd);
                self.device.cmd_copy_image_to_buffer(
                    cmd,
                    target.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback,
                    &[vk::BufferImageCopy::default()
                        .image_subresource(
                            vk::ImageSubresourceLayers::default()
                                .aspect_mask(vk::ImageAspectFlags::COLOR)
                                .layer_count(1),
                        )
                        .image_extent(vk::Extent3D {
                            width: SIZE.0,
                            height: SIZE.1,
                            depth: 1,
                        })],
                );
            }
        })?;
        // SAFETY: test harness: `submit` waited the copy; the mapping is `bytes` long and
        // the buffer's memory is host-coherent; everything freed here is unused after it.
        let pixels = unsafe {
            let map = self
                .device
                .map_memory(read_memory, 0, bytes, vk::MemoryMapFlags::empty())?;
            let pixels =
                std::slice::from_raw_parts(map.cast::<u32>(), (SIZE.0 * SIZE.1) as usize).to_vec();
            self.device.unmap_memory(read_memory);
            self.device.destroy_buffer(readback, None);
            self.device.free_memory(read_memory, None);
            pixels
        };
        target.destroy(&self.device);
        Ok(pixels)
    }

    /// The presenter's plane route: a MUTABLE_FORMAT picture and two plane views.
    fn through_planes(&self, format: vk::Format) -> Result<Vec<u32>> {
        let (image, memory) = self.picture(format, vk::ImageCreateFlags::MUTABLE_FORMAT)?;
        let formats = plane_formats(format).context("plane formats")?;
        let none = vk::SamplerYcbcrConversion::null();
        let luma = self.view(image, formats[0], vk::ImageAspectFlags::PLANE_0, none)?;
        let chroma = self.view(image, formats[1], vk::ImageAspectFlags::PLANE_1, none)?;
        let mut pass = CscPass::new(&self.device, TARGET)?;
        pass.bind_planes(&self.device, luma, chroma);
        let pixels = self.draw(&pass, format, format == P010);
        pass.destroy(&self.device);
        // SAFETY: test harness: `draw` waited its submit, so nothing uses these.
        unsafe {
            self.device.destroy_image_view(luma, None);
            self.device.destroy_image_view(chroma, None);
            self.device.destroy_image(image, None);
            self.device.free_memory(memory, None);
        }
        pixels
    }

    /// The conversion route: no create flags, one view, and a pass that builds its own
    /// identically defined conversion from `info`, as the pool and the presenter do.
    fn through_conversion(
        &self,
        format: vk::Format,
        info: &vk::SamplerYcbcrConversionCreateInfo<'_>,
    ) -> Result<Vec<u32>> {
        let (image, memory) = self.picture(format, vk::ImageCreateFlags::empty())?;
        // SAFETY: test harness; destroyed below, after the view made from it.
        let conversion = unsafe { self.device.create_sampler_ycbcr_conversion(info, None) }?;
        let view = self.view(image, format, vk::ImageAspectFlags::COLOR, conversion)?;
        let descriptors = ycbcr_descriptors(&self.instance, self.pdev, format)?;
        let mut pass = CscPass::new_ycbcr(&self.device, TARGET, info, descriptors)?;
        pass.bind_ycbcr(&self.device, view);
        let pixels = self.draw(&pass, format, false);
        pass.destroy(&self.device);
        // SAFETY: test harness: `draw` waited its submit, so nothing uses these.
        unsafe {
            self.device.destroy_image_view(view, None);
            self.device
                .destroy_sampler_ycbcr_conversion(conversion, None);
            self.device.destroy_image(image, None);
            self.device.free_memory(memory, None);
        }
        pixels
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: test harness: every submit was waited, and each draw freed its own handles.
        unsafe {
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
