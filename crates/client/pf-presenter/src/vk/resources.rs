//! Video-image / staging-buffer rebuild and retired-frame destruction.

use super::gpu::image_with_memory;
use super::{CpuPlanes, Presenter, Retired, Staging, VideoImage};
use anyhow::{Context as _, Result};
use ash::vk;
use pf_client_core::video::CpuPlanarFrame;

impl Retired {
    pub(super) fn destroy(self, device: &ash::Device) {
        // Only the dmabuf lane owns Vulkan objects here; D3D11 imports retire in their cache.
        #[cfg(not(target_os = "linux"))]
        let _ = device;
        match self {
            #[cfg(target_os = "linux")]
            Retired::Dmabuf(f) => f.destroy(device),
            // Image and plane views belong to the decoder's pools — nothing
            // of ours to destroy. Drop sends the release token; the caller
            // reaches here only after the sampling fence (GPU reads done).
            Retired::NativeVk(frame) => drop(frame),
            // The planes go with the frame's hold, or stay with the decoder.
            #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
            Retired::Pyro(frame) => drop(frame),
        }
    }
}

/// Each plane starts on a 16-byte boundary so `bufferOffset` is a multiple
/// of 4 at any picture size — a 1-byte-per-texel odd width would otherwise
/// land a later plane on an odd offset.
fn plane_staging_offsets(f: &CpuPlanarFrame) -> ([usize; 3], usize) {
    let mut offsets = [0usize; 3];
    let mut at = 0usize;
    for (i, off) in offsets.iter_mut().enumerate() {
        let (w, h) = f.plane_dims(i);
        *off = at;
        at += (w as usize * h as usize).next_multiple_of(16);
    }
    (offsets, at)
}

impl CpuPlanes {
    /// Null handles are a no-op, so [`Presenter::rebuild_cpu_planes`] can
    /// unwind a build that failed part-way.
    pub(super) fn destroy(self, device: &ash::Device) {
        // SAFETY: handles owned by `self`. GPU idle: fence/queue-wait on this
        // path, or the swapchain already retired.
        unsafe {
            for i in 0..3 {
                device.destroy_image_view(self.views[i], None);
                device.destroy_image(self.images[i], None);
                device.free_memory(self.memory[i], None);
            }
        }
    }
}

impl VideoImage {
    /// A null framebuffer is a no-op, so a build that failed at it unwinds here.
    pub(super) fn destroy(self, device: &ash::Device) {
        // SAFETY: handles owned by `self`. GPU idle: fence/queue-wait on this
        // path, or never submitted.
        unsafe {
            device.destroy_framebuffer(self.framebuffer, None);
            device.destroy_image_view(self.view, None);
            device.destroy_image(self.image, None);
            device.free_memory(self.memory, None);
        }
    }
}

impl Staging {
    /// Null handles and a null `ptr` are a no-op, so a build that failed part-way
    /// unwinds here.
    pub(super) fn destroy(self, device: &ash::Device) {
        // SAFETY: handles owned by `self`. GPU idle: fence/queue-wait on this
        // path, or never submitted. Unmapped before its memory is freed.
        unsafe {
            if !self.ptr.is_null() {
                device.unmap_memory(self.memory);
            }
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

impl Presenter {
    /// Copy `f` into the staging buffer, sizing the plane images and the buffer first.
    /// Touches no queue: a failed rebuild must fail before acquire, same rule as the
    /// hardware imports.
    pub(super) fn stage_frame(&mut self, f: &CpuPlanarFrame) -> Result<[usize; 3]> {
        if self
            .cpu_planes
            .as_ref()
            .is_none_or(|p| p.width != f.width || p.height != f.height)
        {
            self.rebuild_cpu_planes(f.width, f.height)?;
        }
        let (offsets, needed) = plane_staging_offsets(f);
        if self.staging.as_ref().is_none_or(|s| s.capacity < needed) {
            self.rebuild_staging(needed)?;
        }
        let s = self.staging.as_ref().unwrap();
        for (i, off) in offsets.iter().enumerate() {
            let plane = f.plane(i);
            // SAFETY: `s.ptr` maps a HOST_VISIBLE allocation of
            // `s.capacity >= needed` bytes; `plane_staging_offsets` placed
            // `off + plane.len()` inside `needed`; src and dst are distinct.
            unsafe { std::ptr::copy_nonoverlapping(plane.as_ptr(), s.ptr.add(*off), plane.len()) };
        }
        Ok(offsets)
    }

    fn rebuild_cpu_planes(&mut self, width: u32, height: u32) -> Result<()> {
        // Old images are only referenced by our command buffers.
        self.quiesce_own()?;
        if let Some(p) = self.cpu_planes.take() {
            p.destroy(&self.device);
        }
        let (cw, ch) = CpuPlanarFrame::chroma_dims(width, height);
        let dims = [(width, height), (cw, ch), (cw, ch)];
        // Build into the owning value, not loose arrays: three fallible planes,
        // and `destroy` treats `VK_NULL_HANDLE` as a no-op so a prefix unwinds.
        let mut planes = CpuPlanes {
            images: [vk::Image::null(); 3],
            memory: [vk::DeviceMemory::null(); 3],
            views: [vk::ImageView::null(); 3],
            width,
            height,
            initialized: false,
        };
        for (i, dim) in dims.into_iter().enumerate() {
            if let Err(e) = self.build_cpu_plane(&mut planes, i, dim) {
                planes.destroy(&self.device);
                return Err(e);
            }
        }
        tracing::info!(width, height, "software plane images (re)built");
        self.cpu_planes = Some(planes);
        Ok(())
    }

    /// One R8 plane into slot `i` of `planes`: all three handles or none, so a
    /// failure leaves the caller only whole earlier planes to destroy.
    fn build_cpu_plane(&self, planes: &mut CpuPlanes, i: usize, (w, h): (u32, u32)) -> Result<()> {
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(vk::Extent3D {
                width: w,
                height: h,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let (image, memory, view) = image_with_memory(
            &self.device,
            &self.mem_props,
            &info,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        planes.images[i] = image;
        planes.memory[i] = memory;
        planes.views[i] = view;
        Ok(())
    }

    /// Rebuild the video image when the picture size changes. Every lane's CSC target.
    pub(super) fn ensure_video_image(&mut self, width: u32, height: u32) -> Result<()> {
        if self
            .video
            .as_ref()
            .is_none_or(|v| v.width != width || v.height != height)
        {
            self.rebuild_video_image(width, height)?;
            tracing::info!(width, height, "video image (re)built");
        }
        Ok(())
    }

    fn rebuild_video_image(&mut self, width: u32, height: u32) -> Result<()> {
        // Old image is only referenced by our command buffers.
        self.quiesce_own()?;
        if let Some(v) = self.video.take() {
            v.destroy(&self.device); // `quiesce_own` above: GPU idle on it
        }
        // COLOR_ATTACHMENT is the CSC render target; SAMPLED feeds the scale pass.
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(super::VIDEO_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let (image, memory, view) = image_with_memory(
            &self.device,
            &self.mem_props,
            &info,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        )?;
        let video = VideoImage {
            image,
            memory,
            view,
            framebuffer: vk::Framebuffer::null(),
            width,
            height,
        };
        // View + framebuffer always: Vulkan Video needs the CSC pass on every device.
        let attachments = [view];
        // SAFETY: `device` is live; `view`/`csc.render_pass` are owned here;
        // create-info is a local that outlives the call.
        let framebuffer = unsafe {
            self.device.create_framebuffer(
                &vk::FramebufferCreateInfo::default()
                    .render_pass(self.csc.render_pass)
                    .attachments(&attachments)
                    .width(width)
                    .height(height)
                    .layers(1),
                None,
            )
        };
        match framebuffer {
            Ok(framebuffer) => {
                self.video = Some(VideoImage {
                    framebuffer,
                    ..video
                })
            }
            Err(e) => {
                video.destroy(&self.device);
                return Err(e).context("vkCreateFramebuffer (video image)");
            }
        }
        Ok(())
    }

    fn rebuild_staging(&mut self, capacity: usize) -> Result<()> {
        self.quiesce_own()?;
        if let Some(s) = self.staging.take() {
            s.destroy(&self.device); // `quiesce_own` above: GPU idle on it
        }
        // SAFETY: `device` is live; create-info is a local that outlives the call.
        let buffer = unsafe {
            self.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(capacity as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
        }
        .context("vkCreateBuffer (staging)")?;
        let mut memory = vk::DeviceMemory::null();
        let mapped = (|| {
            // SAFETY: `buffer` was created above and is owned here.
            let reqs = unsafe { self.device.get_buffer_memory_requirements(buffer) };
            memory = self.allocate(
                reqs,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            )?;
            // SAFETY: `buffer` and `memory` were created above; neither is bound yet.
            unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }
                .context("vkBindBufferMemory")?;
            // SAFETY: `memory` is HOST_VISIBLE, bound above, and not mapped yet.
            unsafe {
                self.device
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            }
            .context("vkMapMemory")
        })();
        let staging = Staging {
            buffer,
            memory,
            ptr: std::ptr::null_mut(),
            capacity,
        };
        match mapped {
            Ok(ptr) => {
                self.staging = Some(Staging {
                    ptr: ptr.cast(),
                    ..staging
                })
            }
            Err(e) => {
                staging.destroy(&self.device);
                return Err(e);
            }
        }
        Ok(())
    }
}
