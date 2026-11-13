//! The RGB→YUV CSC front end PyroWave and Vulkan Video share: the per-slot cursor plane, the
//! RGB binding, and the layout every `rgb2yuv*.comp` shader reads. Each backend picks its own
//! shader.
//!
//! A source is sampled as an image, except a LINEAR dmabuf on NVIDIA ([`WordsCache`]): that
//! driver reads a LINEAR image at a row pitch rounded up to 32 bytes, so a 1690-wide frame at
//! 6760 bytes shears. The shaders read such a source as words at the producer's own stride.
// `unsafe_op_in_unsafe_fn` off, as in `vk_util.rs`: every call is a plain ash call.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::cursor_cache::{copy_rows, CursorCache, Key, Verdict};
use crate::vk_util::{color_range, make_host_buffer, make_plain_image};
use anyhow::{ensure, Context, Result};
use ash::vk;
use pf_zerocopy::vkdev::{MappedMemory, VkBufferAlloc, VkImageAlloc};
use std::collections::HashMap;
use std::os::fd::AsFd;

/// A LINEAR packed-RGB dmabuf as the CSC shaders read it: the buffer over its bytes, the word
/// layout (`lin.x` of the shaders), the first pixel's word and the row stride in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinearSrc {
    pub(crate) buffer: vk::Buffer,
    pub(crate) fmt: u32,
    pub(crate) off_w: u32,
    pub(crate) pitch_w: u32,
}

/// How a shader reads `d` as words: the layout, the first pixel's word, the row stride in
/// words. `None` samples the image: a tiled buffer, a fourcc the shaders do not unpack, or a
/// layout that is not word-aligned.
pub(crate) fn linear_words(d: &pf_frame::DmabufFrame, width: u32) -> Option<(u32, u32, u32)> {
    use pf_zerocopy::drm;
    let fmt = match d.fourcc {
        drm::XR24 | drm::AR24 => 1,
        drm::XB24 | drm::AB24 => 2,
        drm::XR30 | drm::AR30 => 3,
        drm::XB30 | drm::AB30 => 4,
        _ => return None,
    };
    let aligned = d.offset % 4 == 0 && d.stride % 4 == 0;
    let fits = u64::from(d.stride) >= u64::from(width) * 4;
    (d.modifier == 0 && aligned && fits).then_some((fmt, d.offset / 4, d.stride / 4))
}

/// Buffer imports of the LINEAR dmabufs a session reads as words, keyed like the image cache
/// (`(st_dev, st_ino)`), and the buffer a sampled source binds in their place. Off where the
/// driver reads a LINEAR image at the pitch it is given. [`Default`] is off and all-null.
#[derive(Default)]
pub(crate) struct WordsCache {
    on: bool,
    idle: VkBufferAlloc,
    bufs: HashMap<(u64, u64), (VkBufferAlloc, u64)>,
}

/// A producer pool is eight buffers deep; twice that never evicts in a steady session.
const WORDS_MAX: usize = 16;
const VENDOR_NVIDIA: u32 = 0x10DE;

impl WordsCache {
    /// The idle buffer, and the lane's switch for this device. On failure nothing is left to
    /// destroy.
    pub(crate) unsafe fn new(
        dev: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        vendor_id: u32,
    ) -> Result<Self> {
        let idle = make_host_buffer(dev, mem_props, 16, vk::BufferUsageFlags::STORAGE_BUFFER)?;
        Ok(Self {
            on: vendor_id == VENDOR_NVIDIA,
            idle,
            bufs: HashMap::new(),
        })
    }

    /// The buffer a sampled source binds: the shaders never read it.
    pub(crate) fn idle(&self) -> vk::Buffer {
        self.idle.buffer
    }

    /// `d` as words when this device needs it, importing its buffer on first sight. The import
    /// is checked against this frame's span: a cached one is not proof of it. A full cache idles
    /// the device and starts over.
    pub(crate) unsafe fn source(
        &mut self,
        dev: &ash::Device,
        ext_fd: &ash::khr::external_memory_fd::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        key: (u64, u64),
        d: &pf_frame::DmabufFrame,
        (width, height): (u32, u32),
    ) -> Result<Option<LinearSrc>> {
        if !self.on {
            return Ok(None);
        }
        let Some((fmt, off_w, pitch_w)) = linear_words(d, width) else {
            return Ok(None);
        };
        if !self.bufs.contains_key(&key) {
            if self.bufs.len() >= WORDS_MAX {
                let _ = dev.device_wait_idle();
                for (_, (b, _)) in self.bufs.drain() {
                    b.destroy(dev);
                }
            }
            let size = rustix::fs::seek(d.fd.as_fd(), rustix::fs::SeekFrom::End(0))
                .context("size the dmabuf")?;
            let buf = pf_zerocopy::vkdev::import_dmabuf_buffer(
                dev,
                ext_fd,
                mem_props,
                d.fd.as_fd(),
                size,
                vk::BufferUsageFlags::STORAGE_BUFFER,
            )?;
            self.bufs.insert(key, (buf, size));
        }
        let (buf, size) = self.bufs[&key];
        let span = u64::from(d.offset)
            + u64::from(d.stride) * u64::from(height.saturating_sub(1))
            + u64::from(width) * 4;
        ensure!(size >= span, "dmabuf smaller than its {span}-byte frame");
        Ok(Some(LinearSrc {
            buffer: buf.buffer,
            fmt,
            off_w,
            pitch_w,
        }))
    }

    /// Drop every import and keep the idle buffer. The device must be idle.
    pub(crate) unsafe fn clear(&mut self, dev: &ash::Device) {
        for (_, (b, _)) in self.bufs.drain() {
            b.destroy(dev);
        }
    }

    /// Destroy every handle. The device must be idle.
    pub(crate) unsafe fn destroy(&mut self, dev: &ash::Device) {
        self.clear(dev);
        std::mem::take(&mut self.idle).destroy(dev);
    }
}

/// Cursor overlay cap (px). The CSC shader bounds sampling by push constant, so one allocation
/// fits every pointer bitmap; a larger one uploads its top-left corner.
pub(crate) const CURSOR_MAX: u32 = 256;

/// One slot's cursor overlay: a `CURSOR_MAX`² RGBA8 image at binding 3 plus its host staging.
/// Per slot: a shared image races the previous frame's sampled read, and a shared cache leaves
/// the other slot showing the previous pointer. [`Default`] is all-null.
#[derive(Default)]
pub(crate) struct CursorPlane {
    pub(crate) img: VkImageAlloc,
    pub(crate) stage: VkBufferAlloc,
    /// The bitmap the image holds.
    cache: CursorCache,
    /// The image has left UNDEFINED.
    ready: bool,
}

impl CursorPlane {
    /// The image and its staging. On failure nothing is left to destroy.
    pub(crate) unsafe fn new(
        dev: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<Self> {
        let img = make_plain_image(
            dev,
            mem_props,
            vk::Format::R8G8B8A8_UNORM,
            CURSOR_MAX,
            CURSOR_MAX,
            vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
        )?;
        let mut me = Self {
            img,
            ..Self::default()
        };
        match make_host_buffer(
            dev,
            mem_props,
            (CURSOR_MAX * CURSOR_MAX * 4) as u64,
            vk::BufferUsageFlags::TRANSFER_SRC,
        ) {
            Ok(stage) => {
                me.stage = stage;
                Ok(me)
            }
            Err(e) => {
                me.destroy(dev);
                Err(e)
            }
        }
    }

    /// Bring the image up to date in `cmd` and return the push constant
    /// `[origin_x, origin_y, size_w, size_h]` (size 0 ⇒ the CSC skips the blend). Uploads when
    /// the cache says so; the caller forgets on any failure from here to the submit. First use
    /// always leaves UNDEFINED, so binding 3 is in a valid layout with no cursor. A `pq` session
    /// blends the cursor re-encoded as PQ.
    pub(crate) unsafe fn prep(
        &mut self,
        dev: &ash::Device,
        cmd: vk::CommandBuffer,
        cursor: Option<&pf_frame::CursorOverlay>,
        pq: bool,
    ) -> Result<[i32; 4]> {
        let img = self.img.image;
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
                if self.cache.decide(Key {
                    serial: c.serial,
                    pq,
                }) == Verdict::Upload
                {
                    let px = if pq { c.pq_rgba() } else { c.rgba.clone() };
                    let (row, src_row) = (cw as usize * 4, c.w as usize * 4);
                    anyhow::ensure!(row > 0, "cursor overlay {}x{} has no width", c.w, c.h);
                    // The plane's staging memory: host-visible, `CURSOR_MAX`² × 4 bytes, mapped
                    // only here.
                    let mut stage = MappedMemory::map(dev, self.stage.memory, row * ch as usize)?;
                    copy_rows(&mut stage, &px, row, src_row);
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
                        self.stage.buffer,
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

    /// Drop the upload a reset command buffer discarded. The image may still be UNDEFINED.
    pub(crate) fn forget(&mut self) {
        self.cache.forget();
        self.ready = false;
    }

    /// Destroy every handle. A null one, from a partly built slot, is a no-op.
    pub(crate) unsafe fn destroy(self, dev: &ash::Device) {
        self.img.destroy(dev);
        self.stage.destroy(dev);
    }
}

/// Point a CSC set's binding 0 at this frame's RGB view and binding 4 at `words`: a LINEAR
/// source's buffer, or [`WordsCache::idle`]. The set must not be bound by a PENDING command
/// buffer (VUID-vkUpdateDescriptorSets-None-03047).
pub(crate) unsafe fn bind_rgb(
    dev: &ash::Device,
    sampler: vk::Sampler,
    set: vk::DescriptorSet,
    view: vk::ImageView,
    words: vk::Buffer,
) {
    let ii = [vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let bi = [vk::DescriptorBufferInfo::default()
        .buffer(words)
        .range(vk::WHOLE_SIZE)];
    dev.update_descriptor_sets(
        &[
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&bi),
        ],
        &[],
    );
}

/// Bytes of the CSC push constant: the cursor's `{ivec2 origin, ivec2 size}`, then
/// `uvec3 lin` (layout, first word, row stride in words; layout 0 samples the image).
pub(crate) const CSC_PUSH_BYTES: usize = 28;

/// The CSC push constant for a cursor blend and a source.
pub(crate) fn csc_push(cursor_pc: [i32; 4], lin: Option<LinearSrc>) -> [u8; CSC_PUSH_BYTES] {
    let (fmt, off_w, pitch_w) = lin.map_or((0, 0, 0), |l| (l.fmt, l.off_w, l.pitch_w));
    let mut bytes = [0u8; CSC_PUSH_BYTES];
    for (i, v) in cursor_pc.iter().enumerate() {
        bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
    }
    for (i, v) in [fmt, off_w, pitch_w].iter().enumerate() {
        bytes[16 + i * 4..20 + i * 4].copy_from_slice(&v.to_ne_bytes());
    }
    bytes
}

/// The layout every `rgb2yuv*.comp` shader reads: bindings 0 (RGB) and 3 (cursor) sampled, 1–2
/// the storage planes, 4 a LINEAR source's words, and the [`csc_push`] constant, whose
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
        sb(4, vk::DescriptorType::STORAGE_BUFFER),
    ];
    let dsl = dev.create_descriptor_set_layout(
        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
        None,
    )?;
    let dsls = [dsl];
    let pc_ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size(CSC_PUSH_BYTES as u32)];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dmabuf(fourcc: &[u8; 4], modifier: u64, offset: u32, stride: u32) -> pf_frame::DmabufFrame {
        pf_frame::DmabufFrame {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            fourcc: u32::from_le_bytes(*fourcc),
            modifier,
            plane1: None,
            offset,
            stride,
            hold: None,
            health: pf_zerocopy::zero_copy_health(0x5003),
            rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// A LINEAR packed frame reads as words at its own stride, whatever the stride; a tiled
    /// one, a planar one, or a layout off a word is sampled.
    #[test]
    fn a_linear_source_is_read_as_words() {
        assert_eq!(
            linear_words(&dmabuf(b"XR24", 0, 0, 6760), 1690),
            Some((1, 0, 1690))
        );
        assert_eq!(
            linear_words(&dmabuf(b"XB30", 0, 64, 6912), 1690),
            Some((4, 16, 1728))
        );
        let tiled = 0x0300_0000_0060_6010;
        assert_eq!(linear_words(&dmabuf(b"XR24", tiled, 0, 6784), 1690), None);
        assert_eq!(linear_words(&dmabuf(b"NV12", 0, 0, 1728), 1690), None);
        assert_eq!(linear_words(&dmabuf(b"XR24", 0, 2, 6760), 1690), None);
        assert_eq!(linear_words(&dmabuf(b"XR24", 0, 0, 6756), 1690), None);
    }

    /// The push constant is the cursor rect, then `lin` at byte 16; a sampled source leaves it
    /// zero.
    #[test]
    fn the_push_constant_carries_the_source() {
        let lin = LinearSrc {
            buffer: vk::Buffer::null(),
            fmt: 1,
            off_w: 16,
            pitch_w: 1690,
        };
        let word = |b: &[u8; CSC_PUSH_BYTES], i: usize| {
            u32::from_ne_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
        };
        let b = csc_push([3, 4, 24, 24], Some(lin));
        assert_eq!(
            [3, 4, 24, 24, 1, 16, 1690],
            std::array::from_fn(|i| word(&b, i))
        );
        let b = csc_push([3, 4, 24, 24], None);
        assert_eq!([0, 0, 0], [word(&b, 4), word(&b, 5), word(&b, 6)]);
    }
}
