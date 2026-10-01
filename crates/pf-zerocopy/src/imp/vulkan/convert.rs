//! The fused convert on the bridge's device: the compositor's dmabuf sampled as an image
//! (any modifier), the cursor blended, the NVENC input slot written — one dispatch per
//! frame, no staging. The slot is the host's OPAQUE_FD memory NVENC registered
//! (`vkslot.rs`), imported here once per slot; the dmabuf image is cached per fd.
//!
//! ```text
//!   dmabuf fd ──DRM-modifier image──▶ texelFetch ─┐
//!   cursor RGBA8 (host-visible SSBO) ─────────────┼─▶ convert_img.comp ─▶ slot SSBO ─▶ NVENC
//!   host slot OPAQUE_FD ──imported VkBuffer ──────┘
//! ```
//!
//! Per frame: an acquire barrier from the external producer, one dispatch, a signal on the
//! exported timeline. The host waits that value on its CUDA stream before NVENC reads the
//! slot, so no CPU sits between the pass and the encode. `FRAMES` command buffers rotate;
//! reusing one waits its own value first.

use super::VkBridge;
use crate::imp::proto::{ConvertOut, ConvertSrc, CursorRect};
use crate::imp::vkdev;
use anyhow::{anyhow, bail, Context, Result};
use ash::vk;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};

const CONVERT_SPV: &[u8] = include_bytes!("../convert_img.spv");
/// Push constants of `convert_img.comp`: nine 32-bit words.
const PUSH_BYTES: u32 = 36;
/// Cursor bitmaps larger than this (in bytes) are refused; 256² RGBA8 is the capture cap.
const CURSOR_MAX_BYTES: u64 = 256 * 256 * 4;
/// Passes in flight before a command buffer's reuse waits. The host holds at most two
/// frames ahead of the encoder.
const FRAMES: usize = 4;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// DRM fourcc → the VkFormat whose channel order matches, so `texelFetch` returns RGB
/// regardless of the producer's packing. Little-endian packing: XR30 has R in bits 20..29,
/// which is Vulkan's `A2R10G10B10`; XB30 has R in bits 0..9, `A2B10G10R10`.
fn vk_format(drm: u32) -> Option<vk::Format> {
    Some(match drm {
        x if x == fourcc(b'X', b'R', b'2', b'4') || x == fourcc(b'A', b'R', b'2', b'4') => {
            vk::Format::B8G8R8A8_UNORM
        }
        x if x == fourcc(b'X', b'B', b'2', b'4') || x == fourcc(b'A', b'B', b'2', b'4') => {
            vk::Format::R8G8B8A8_UNORM
        }
        x if x == fourcc(b'X', b'R', b'3', b'0') || x == fourcc(b'A', b'R', b'3', b'0') => {
            vk::Format::A2R10G10B10_UNORM_PACK32
        }
        x if x == fourcc(b'X', b'B', b'3', b'0') || x == fourcc(b'A', b'B', b'3', b'0') => {
            vk::Format::A2B10G10R10_UNORM_PACK32
        }
        _ => return None,
    })
}

struct SrcImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    /// Geometry the image was created for; a change re-imports.
    shape: (u32, u64, u32, u32, u32, u32),
    /// First acquire transitions from PREINITIALIZED; later ones from the read layout.
    acquired: bool,
}

struct SlotBuf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
}

struct CursorBuf {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    cap: u64,
    map: *mut u8,
}

/// One rotating pass: its command buffer, descriptor set, and the timeline value its last
/// submit signals (0: never used).
struct Frame {
    cmd: vk::CommandBuffer,
    dset: vk::DescriptorSet,
    ticket: u64,
}

pub(super) struct ConvertState {
    module: vk::ShaderModule,
    dset_layout: vk::DescriptorSetLayout,
    playout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    dpool: vk::DescriptorPool,
    frames: Vec<Frame>,
    next: usize,
    /// Exportable timeline every pass signals; `ticket` is the last value handed out.
    timeline: vk::Semaphore,
    ts: ash::khr::timeline_semaphore::Device,
    sem_fd: ash::khr::external_semaphore_fd::Device,
    ticket: u64,
    sampler: vk::Sampler,
    srcs: HashMap<i32, SrcImage>,
    slots: HashMap<u32, SlotBuf>,
    cursor: Option<CursorBuf>,
    /// `(width, height)` of the bitmap in `cursor`; `None` until one is uploaded.
    cursor_dims: Option<(u32, u32)>,
    cursor_serial: u64,
}

impl ConvertState {
    /// Every handle, CUDA-free; the caller has idled the device.
    pub(super) unsafe fn destroy(&mut self, d: &ash::Device, pool: vk::CommandPool) {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            for (_, s) in self.srcs.drain() {
                d.destroy_image_view(s.view, None);
                d.destroy_image(s.image, None);
                d.free_memory(s.memory, None);
            }
            for (_, s) in self.slots.drain() {
                d.destroy_buffer(s.buffer, None);
                d.free_memory(s.memory, None);
            }
            if let Some(c) = self.cursor.take() {
                d.unmap_memory(c.memory);
                d.destroy_buffer(c.buffer, None);
                d.free_memory(c.memory, None);
            }
            let cmds: Vec<vk::CommandBuffer> = self.frames.drain(..).map(|f| f.cmd).collect();
            if !cmds.is_empty() {
                d.free_command_buffers(pool, &cmds);
            }
            d.destroy_semaphore(self.timeline, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.playout, None);
            d.destroy_descriptor_pool(self.dpool, None);
            d.destroy_descriptor_set_layout(self.dset_layout, None);
            d.destroy_shader_module(self.module, None);
        }
    }
}

impl VkBridge {
    /// Build the convert pipeline once. Needs the modifier-import extensions the device
    /// advertised at bring-up; without them the convert lane is unavailable and the caller
    /// falls back to the import path.
    unsafe fn ensure_convert(&mut self) -> Result<()> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            if self.conv.is_some() {
                return Ok(());
            }
            if !self.modifier_import {
                bail!("VK_EXT_image_drm_format_modifier unavailable — no fused convert");
            }
            if !self.timeline_export {
                bail!("timeline semaphore export unavailable — no fused convert");
            }
            let d = &self.device;
            let words: Vec<u32> = CONVERT_SPV
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().expect("4 bytes")))
                .collect();
            let module = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .context("create convert shader module")?;
            let mut st = ConvertState {
                module,
                dset_layout: vk::DescriptorSetLayout::null(),
                playout: vk::PipelineLayout::null(),
                pipeline: vk::Pipeline::null(),
                dpool: vk::DescriptorPool::null(),
                frames: Vec::new(),
                next: 0,
                timeline: vk::Semaphore::null(),
                ts: ash::khr::timeline_semaphore::Device::new(&self.instance, &self.device),
                sem_fd: ash::khr::external_semaphore_fd::Device::new(&self.instance, &self.device),
                ticket: 0,
                sampler: vk::Sampler::null(),
                srcs: HashMap::new(),
                slots: HashMap::new(),
                cursor: None,
                cursor_dims: None,
                cursor_serial: u64::MAX,
            };
            if let Err(e) = self.build_convert(&mut st) {
                st.destroy(&self.device, self.cmd_pool);
                return Err(e);
            }
            self.conv = Some(st);
            tracing::info!("Vulkan-bridge fused convert ready (dmabuf image → NVENC slot)");
            Ok(())
        }
    }

    unsafe fn build_convert(&self, st: &mut ConvertState) -> Result<()> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            let d = &self.device;
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            st.dset_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .context("create convert dset layout")?;
            let pc = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(PUSH_BYTES)];
            let layouts = [st.dset_layout];
            st.playout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&pc),
                    None,
                )
                .context("create convert pipeline layout")?;
            let entry = c"main";
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(st.module)
                .name(entry);
            st.pipeline = d
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(st.playout)],
                    None,
                )
                .map_err(|(_, e)| e)
                .context("create convert pipeline")?[0];
            let n = FRAMES as u32;
            let sizes = [
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(n),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(2 * n),
            ];
            st.dpool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(n)
                        .pool_sizes(&sizes),
                    None,
                )
                .context("create convert descriptor pool")?;
            let per_frame = [st.dset_layout; FRAMES];
            let dsets = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(st.dpool)
                        .set_layouts(&per_frame),
                )
                .context("allocate convert descriptor sets")?;
            let cmds = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.cmd_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(n),
                )
                .context("allocate convert command buffers")?;
            st.frames = cmds
                .into_iter()
                .zip(dsets)
                .map(|(cmd, dset)| Frame {
                    cmd,
                    dset,
                    ticket: 0,
                })
                .collect();
            let mut type_ci = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
            st.timeline = d
                .create_semaphore(
                    &vk::SemaphoreCreateInfo::default()
                        .push_next(&mut type_ci)
                        .push_next(&mut export),
                    None,
                )
                .context("create convert timeline")?;
            st.sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::NEAREST)
                        .min_filter(vk::Filter::NEAREST)
                        .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                        .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                    None,
                )
                .context("create convert sampler")?;
            Ok(())
        }
    }

    /// Import the host's NVENC slot (an OPAQUE_FD export of its Vulkan memory) as a storage
    /// buffer. Replaces an earlier import under the same id. Takes the fd.
    pub fn register_slot(&mut self, id: u32, fd: OwnedFd, size: u64) -> Result<()> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            self.ensure_convert()?;
            let d = &self.device;
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                        .push_next(&mut ext),
                    None,
                )
                .context("create slot import buffer")?;
            let reqs = d.get_buffer_memory_requirements(buffer);
            let mem_type = match self
                .memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
            {
                Ok(t) => t,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD)
                .fd(fd.as_raw_fd());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let memory = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(m) => {
                    // Vulkan owns the descriptor now; a failed import drops `fd` itself.
                    let _ = fd.into_raw_fd();
                    m
                }
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("import slot OPAQUE_FD");
                }
            };
            if let Err(e) = d.bind_buffer_memory(buffer, memory, 0) {
                d.free_memory(memory, None);
                d.destroy_buffer(buffer, None);
                return Err(e).context("bind slot import");
            }
            let st = self.conv.as_mut().expect("ensured above");
            if let Some(old) = st.slots.insert(
                id,
                SlotBuf {
                    buffer,
                    memory,
                    size,
                },
            ) {
                let _ = d.device_wait_idle();
                d.destroy_buffer(old.buffer, None);
                d.free_memory(old.memory, None);
            }
            Ok(())
        }
    }

    /// Drop every slot import: the host rebuilt its ring.
    pub fn forget_slots(&mut self) {
        if let Some(st) = self.conv.as_mut() {
            // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
            // outlives the call, each fallible step destroys what it created, and the fence wait
            // retires the work before any handle it used is freed.
            unsafe {
                let _ = self.device.device_wait_idle();
                for (_, s) in st.slots.drain() {
                    self.device.destroy_buffer(s.buffer, None);
                    self.device.free_memory(s.memory, None);
                }
            }
        }
    }

    /// Upload a straight-alpha RGBA8 cursor bitmap (tight rows). Kept until the next serial.
    pub fn set_cursor(&mut self, serial: u64, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            self.ensure_convert()?;
            let need = u64::from(width) * u64::from(height) * 4;
            if need == 0 || need > CURSOR_MAX_BYTES || (rgba.len() as u64) < need {
                bail!(
                    "cursor bitmap {width}x{height} ({} bytes) out of bounds",
                    rgba.len()
                );
            }
            self.ensure_cursor_capacity(need)?;
            self.quiesce_convert()?;
            let st = self.conv.as_mut().expect("ensured above");
            let c = st.cursor.as_ref().expect("capacity ensured");
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), c.map, need as usize);
            st.cursor_dims = Some((width, height));
            st.cursor_serial = serial;
            Ok(())
        }
    }

    /// Wait until every pass submitted so far retired: the cursor buffer is about to change
    /// under them.
    unsafe fn quiesce_convert(&self) -> Result<()> {
        // SAFETY: raw Vulkan on this bridge's own device; the wait-info locals outlive the
        // synchronous call and the timeline is live while `conv` is.
        unsafe {
            let st = self.conv.as_ref().expect("convert state");
            if st.ticket == 0 {
                return Ok(());
            }
            let sems = [st.timeline];
            let values = [st.ticket];
            st.ts
                .wait_semaphores(
                    &vk::SemaphoreWaitInfo::default()
                        .semaphores(&sems)
                        .values(&values),
                    1_000_000_000,
                )
                .context("wait convert timeline")
        }
    }

    /// A fresh OPAQUE_FD of the convert timeline, for the host's CUDA import.
    pub fn convert_timeline_fd(&mut self) -> Result<OwnedFd> {
        // SAFETY: raw Vulkan on this bridge's own device; the info local outlives the call
        // and the descriptor it returns is fresh, so `OwnedFd` is its only owner.
        unsafe {
            self.ensure_convert()?;
            let st = self.conv.as_ref().expect("ensured above");
            let fd = st
                .sem_fd
                .get_semaphore_fd(
                    &vk::SemaphoreGetFdInfoKHR::default()
                        .semaphore(st.timeline)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
                )
                .context("vkGetSemaphoreFdKHR(convert timeline)")?;
            Ok(<OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd))
        }
    }

    /// A host-visible, persistently mapped cursor SSBO of at least `need` bytes (16 when none
    /// was ever uploaded: the binding must point at a real buffer).
    unsafe fn ensure_cursor_capacity(&mut self, need: u64) -> Result<()> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            let st = self.conv.as_mut().expect("convert state");
            if st.cursor.as_ref().is_some_and(|c| c.cap >= need) {
                return Ok(());
            }
            let d = &self.device;
            // The bitmap goes with its buffer; a rect is refused until the next upload.
            st.cursor_dims = None;
            if let Some(old) = st.cursor.take() {
                let _ = d.device_wait_idle();
                d.unmap_memory(old.memory);
                d.destroy_buffer(old.buffer, None);
                d.free_memory(old.memory, None);
            }
            let cap = need.max(16);
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(cap)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                    None,
                )
                .context("create cursor buffer")?;
            let reqs = d.get_buffer_memory_requirements(buffer);
            let mem_type = self
                .memory_type(
                    reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .inspect_err(|_| d.destroy_buffer(buffer, None))?;
            let memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(mem_type),
                    None,
                )
                .inspect_err(|_| d.destroy_buffer(buffer, None))
                .context("allocate cursor memory")?;
            if let Err(e) = d.bind_buffer_memory(buffer, memory, 0) {
                d.free_memory(memory, None);
                d.destroy_buffer(buffer, None);
                return Err(e).context("bind cursor memory");
            }
            let map = match d.map_memory(memory, 0, cap, vk::MemoryMapFlags::empty()) {
                Ok(p) => p.cast::<u8>(),
                Err(e) => {
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("map cursor memory");
                }
            };
            std::ptr::write_bytes(map, 0, cap as usize);
            let st = self.conv.as_mut().expect("convert state");
            st.cursor = Some(CursorBuf {
                buffer,
                memory,
                cap,
                map,
            });
            Ok(())
        }
    }

    /// Import (or reuse) the dmabuf as a sampled image with its explicit modifier layout.
    unsafe fn src_image(&mut self, s: &ConvertSrc) -> Result<(vk::Image, vk::ImageView, bool)> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            let shape = (s.fourcc, s.modifier, s.offset, s.stride, s.width, s.height);
            let st = self.conv.as_mut().expect("convert state");
            if let Some(img) = st.srcs.get_mut(&s.fd) {
                if img.shape == shape {
                    let first = !img.acquired;
                    img.acquired = true;
                    return Ok((img.image, img.view, first));
                }
                let old = st.srcs.remove(&s.fd).expect("checked");
                let _ = self.device.device_wait_idle();
                self.device.destroy_image_view(old.view, None);
                self.device.destroy_image(old.image, None);
                self.device.free_memory(old.memory, None);
            }
            let fmt = vk_format(s.fourcc)
                .ok_or_else(|| anyhow!("no VkFormat for dmabuf fourcc {:#x}", s.fourcc))?;
            // SAFETY: `s.fd` is the worker's cached dmabuf fd, open for this synchronous call;
            // the import dups it and keeps no borrow.
            let fd = BorrowedFd::borrow_raw(s.fd);
            let (image, memory) = vkdev::import_dmabuf_image(
                &self.device,
                &self.ext_fd,
                &self.mem_props,
                &vkdev::DmabufImage {
                    fd,
                    format: fmt,
                    width: s.width,
                    height: s.height,
                    modifier: s.modifier,
                    planes: &[vk::SubresourceLayout::default()
                        .offset(u64::from(s.offset))
                        .row_pitch(u64::from(s.stride))],
                    usage: vk::ImageUsageFlags::SAMPLED,
                    // The first acquire barrier transitions from PREINITIALIZED.
                    initial_layout: vk::ImageLayout::PREINITIALIZED,
                },
                None,
            )?;
            let view = match vkdev::color_view(&self.device, image, fmt, 0) {
                Ok(v) => v,
                Err(e) => {
                    self.device.destroy_image(image, None);
                    self.device.free_memory(memory, None);
                    return Err(e);
                }
            };
            let st = self.conv.as_mut().expect("convert state");
            st.srcs.insert(
                s.fd,
                SrcImage {
                    image,
                    memory,
                    view,
                    shape,
                    acquired: true,
                },
            );
            Ok((image, view, true))
        }
    }

    /// Drop the cached image for a dmabuf fd the producer retired.
    pub fn forget_src_image(&mut self, fd: i32) {
        if let Some(st) = self.conv.as_mut() {
            if let Some(old) = st.srcs.remove(&fd) {
                // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
                // outlives the call, each fallible step destroys what it created, and the fence wait
                // retires the work before any handle it used is freed.
                unsafe {
                    let _ = self.device.device_wait_idle();
                    self.device.destroy_image_view(old.view, None);
                    self.device.destroy_image(old.image, None);
                    self.device.free_memory(old.memory, None);
                }
            }
        }
    }

    /// One fused pass: `src` (with the cursor at `cursor`, when given) into slot `slot` laid
    /// out as `out`. Returns the timeline value the pass signals; the host waits it on its
    /// CUDA stream ([`convert_timeline_fd`](Self::convert_timeline_fd)).
    pub fn convert(
        &mut self,
        src: &ConvertSrc,
        slot: u32,
        out: &ConvertOut,
        cursor: Option<CursorRect>,
    ) -> Result<u64> {
        // SAFETY: raw Vulkan on this bridge's own device: every info struct is a local that
        // outlives the call, each fallible step destroys what it created, and the fence wait
        // retires the work before any handle it used is freed.
        unsafe {
            self.ensure_convert()?;
            if out.width == 0 || out.height == 0 || out.width > src.width || out.height > src.height
            {
                bail!(
                    "convert output {}x{} does not fit the source {}x{}",
                    out.width,
                    out.height,
                    src.width,
                    src.height
                );
            }
            check_layout(out)?;
            let need = slot_bytes(out);
            if src.plane1.is_some() {
                return self.copy_planar(src, slot, out, need);
            }
            if let Some(c) = cursor {
                match self.conv.as_ref().expect("state").cursor_dims {
                    None => bail!("cursor rect without an uploaded bitmap"),
                    // The shader indexes the bitmap with `c.w` as its row length.
                    Some(dims) if dims != (c.w, c.h) => bail!(
                        "cursor rect {}x{} does not match the uploaded {}x{} bitmap",
                        c.w,
                        c.h,
                        dims.0,
                        dims.1
                    ),
                    Some(_) => {}
                }
            }
            let (image, view, first) = self.src_image(src)?;
            if self.conv.as_ref().expect("state").cursor.is_none() {
                self.ensure_cursor_capacity(16)?;
            }
            let qf = self.qf;
            let (idx, cmd, dset, value) = self.next_pass()?;
            let d = &self.device;
            let st = self.conv.as_ref().expect("state");
            let slot_buf = st
                .slots
                .get(&slot)
                .ok_or_else(|| anyhow!("slot {slot} not registered"))?;
            if slot_buf.size < need {
                bail!(
                    "slot {slot} holds {} bytes, layout needs {need}",
                    slot_buf.size
                );
            }
            let cur = st.cursor.as_ref().expect("ensured");
            let img_info = [vk::DescriptorImageInfo::default()
                .sampler(st.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let dst_info = [vk::DescriptorBufferInfo::default()
                .buffer(slot_buf.buffer)
                .range(vk::WHOLE_SIZE)];
            let cur_info = [vk::DescriptorBufferInfo::default()
                .buffer(cur.buffer)
                .range(vk::WHOLE_SIZE)];
            d.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(dset)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&img_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(dset)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&dst_info),
                    vk::WriteDescriptorSet::default()
                        .dst_set(dset)
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&cur_info),
                ],
                &[],
            );
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .context("begin convert cmd")?;
            // Acquire the producer's writes: the compositor is the external owner of this
            // memory, so every frame is an ownership acquire, not a plain visibility barrier.
            let acquire = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(if first {
                    vk::ImageLayout::PREINITIALIZED
                } else {
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                })
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(qf)
                .image(image)
                .subresource_range(
                    vk::ImageSubresourceRange::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .level_count(1)
                        .layer_count(1),
                );
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, st.pipeline);
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                st.playout,
                0,
                &[dset],
                &[],
            );
            let (cw, ch, cx, cy) = match cursor {
                Some(c) => (c.w, c.h, c.x, c.y),
                None => (0, 0, 0, 0),
            };
            let push: [u32; 9] = [
                out.mode,
                out.width,
                out.height,
                out.pitch_w,
                out.plane_rows,
                cw,
                ch,
                cx as u32,
                cy as u32,
            ];
            let bytes: Vec<u8> = push.iter().flat_map(|w| w.to_ne_bytes()).collect();
            d.cmd_push_constants(cmd, st.playout, vk::ShaderStageFlags::COMPUTE, 0, &bytes);
            d.cmd_dispatch(
                cmd,
                out.width.div_ceil(4).div_ceil(8),
                out.height.div_ceil(2).div_ceil(8),
                1,
            );
            d.end_command_buffer(cmd).context("end convert cmd")?;
            self.submit_pass(idx, cmd, value)
        }
    }

    /// The next rotating pass: its index, command buffer, descriptor set, and the timeline value
    /// it will signal. Reusing a command buffer waits its own last pass; with `FRAMES` rotating
    /// the host consumed that slot long ago.
    unsafe fn next_pass(&mut self) -> Result<(usize, vk::CommandBuffer, vk::DescriptorSet, u64)> {
        let st = self.conv.as_ref().expect("state");
        let idx = st.next;
        let frame = &st.frames[idx];
        if frame.ticket > 0 {
            let sems = [st.timeline];
            let values = [frame.ticket];
            // SAFETY: the timeline belongs to this bridge's live device; the wait info's arrays
            // are locals that outlive the call.
            unsafe {
                st.ts.wait_semaphores(
                    &vk::SemaphoreWaitInfo::default()
                        .semaphores(&sems)
                        .values(&values),
                    1_000_000_000,
                )
            }
            .context("wait convert timeline (reuse)")?;
        }
        Ok((idx, frame.cmd, frame.dset, st.ticket + 1))
    }

    /// Submit the recorded `cmd` signalling `value` on the exported timeline, and rotate.
    unsafe fn submit_pass(
        &mut self,
        idx: usize,
        cmd: vk::CommandBuffer,
        value: u64,
    ) -> Result<u64> {
        let st = self.conv.as_ref().expect("state");
        let cmds = [cmd];
        let sems = [st.timeline];
        let values = [value];
        let mut tsi = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
        // SAFETY: `cmd` was recorded on this bridge's device and is not pending (`next_pass`
        // waited its last value); every info array is a local that outlives the call.
        unsafe {
            if let Err(e) = self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&cmds)
                    .signal_semaphores(&sems)
                    .push_next(&mut tsi)],
                vk::Fence::null(),
            ) {
                let _ = self.device.device_wait_idle();
                return Err(e).context("submit convert");
            }
        }
        let st = self.conv.as_mut().expect("state");
        st.frames[idx].ticket = value;
        st.ticket = value;
        st.next = (idx + 1) % FRAMES;
        Ok(value)
    }

    /// A producer NV12 needs no conversion: copy its two LINEAR planes into the slot, luma at
    /// the slot pitch, chroma `plane_rows` below. The cursor is the host's to blend afterwards.
    unsafe fn copy_planar(
        &mut self,
        src: &ConvertSrc,
        slot: u32,
        out: &ConvertOut,
        need: u64,
    ) -> Result<u64> {
        if src.modifier != 0 {
            bail!(
                "a producer NV12 must be LINEAR to copy, got modifier {:#x}",
                src.modifier
            );
        }
        let regions = planar_regions(src, out)?;
        let span = regions
            .iter()
            .map(|r| r.src_offset + r.size)
            .max()
            .unwrap_or(0);
        // SAFETY: raw Vulkan on this bridge's own device. `src.fd` is the worker's cached dmabuf
        // fd, open for this call (`import_src` dups it). The cached import's size is re-checked
        // against this frame's span, and the slot's against `need`, so every region is in range.
        unsafe {
            if !self.src_cache.contains_key(&src.fd) {
                let size = libc::lseek(src.fd, 0, libc::SEEK_END);
                anyhow::ensure!(size > 0, "lseek(dmabuf)");
                self.import_src(src.fd, size as u64)?;
            }
            let (src_buffer, src_size) = {
                let s = &self.src_cache[&src.fd];
                (s.buffer, s.size)
            };
            anyhow::ensure!(src_size >= span, "dmabuf smaller than the NV12 planes");
            let (idx, cmd, _, value) = self.next_pass()?;
            let qf = self.qf;
            let st = self.conv.as_ref().expect("state");
            let slot_buf = st
                .slots
                .get(&slot)
                .ok_or_else(|| anyhow!("slot {slot} not registered"))?;
            if slot_buf.size < need {
                bail!(
                    "slot {slot} holds {} bytes, layout needs {need}",
                    slot_buf.size
                );
            }
            let d = &self.device;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .context("begin copy cmd")?;
            // The compositor owns this memory: acquire its writes, as the image path does.
            let acquire = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(qf)
                .buffer(src_buffer)
                .size(vk::WHOLE_SIZE);
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[acquire],
                &[],
            );
            d.cmd_copy_buffer(cmd, src_buffer, slot_buf.buffer, &regions);
            d.end_command_buffer(cmd).context("end copy cmd")?;
            self.submit_pass(idx, cmd, value)
        }
    }
}

/// The copies that place a producer NV12 in a slot laid out as `out`: luma rows at the slot
/// pitch, chroma rows from `pitch * plane_rows`. A plane whose stride already equals the slot
/// pitch is one region; any other is a region per row. The last row copies only `width` bytes,
/// so a tightly sized buffer is never read past its end. A frame and slot of different sizes
/// (a resize in flight) copy their common top-left corner.
fn planar_regions(src: &ConvertSrc, out: &ConvertOut) -> Result<Vec<vk::BufferCopy>> {
    let (off1, stride1) = src
        .plane1
        .ok_or_else(|| anyhow!("a planar source without its chroma plane"))?;
    let pitch = u64::from(out.pitch_w) * 4;
    // NV12: a byte per luma sample, and a chroma row is the same width (U and V interleaved).
    let (width, height) = (src.width.min(out.width), src.height.min(out.height));
    let row = u64::from(width);
    if u64::from(src.stride) < row || u64::from(stride1) < row {
        bail!(
            "NV12 strides {}/{stride1} are short of a {row}-byte row",
            src.stride
        );
    }
    let mut regions = Vec::new();
    let mut plane = |src_off: u64, stride: u64, dst_off: u64, rows: u64| {
        if rows == 0 {
            return;
        }
        if stride == pitch {
            regions.push(vk::BufferCopy {
                src_offset: src_off,
                dst_offset: dst_off,
                size: stride * (rows - 1) + row,
            });
        } else {
            regions.extend((0..rows).map(|r| vk::BufferCopy {
                src_offset: src_off + r * stride,
                dst_offset: dst_off + r * pitch,
                size: row,
            }));
        }
    };
    plane(
        u64::from(src.offset),
        u64::from(src.stride),
        0,
        u64::from(height),
    );
    plane(
        u64::from(off1),
        u64::from(stride1),
        pitch * u64::from(out.plane_rows),
        u64::from(height.div_ceil(2)),
    );
    Ok(regions)
}

/// Refuse a layout whose rows `convert_img.comp` would write past: a row pitch shorter than
/// the row, fewer plane rows than picture rows, or a mode the shader does not know.
fn check_layout(out: &ConvertOut) -> Result<()> {
    let row_bytes = match out.mode {
        0 | 3 | 4 => u64::from(out.width) * 4,
        1 | 2 => u64::from(out.width),
        m => bail!("unknown convert mode {m}"),
    };
    if u64::from(out.pitch_w) * 4 < row_bytes {
        bail!(
            "slot pitch of {} words is short of a {row_bytes}-byte row",
            out.pitch_w
        );
    }
    if matches!(out.mode, 1 | 2) && out.plane_rows < out.height {
        bail!(
            "{} plane rows cannot hold a {}-row picture",
            out.plane_rows,
            out.height
        );
    }
    Ok(())
}

/// Bytes the slot must hold for `out`: packed modes one word per pixel per row, NV12 its
/// luma rows plus half as many chroma rows, YUV444 three planes.
fn slot_bytes(out: &ConvertOut) -> u64 {
    let rows = match out.mode {
        1 => u64::from(out.plane_rows) + u64::from(out.plane_rows.div_ceil(2)),
        2 => 3 * u64::from(out.plane_rows),
        _ => u64::from(out.height),
    };
    u64::from(out.pitch_w) * 4 * rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot size the pass needs follows the mode: packed rows, NV12 1.5×, YUV444 3×.
    #[test]
    fn slot_bytes_follow_the_layout() {
        let out = |mode| ConvertOut {
            mode,
            width: 100,
            height: 50,
            pitch_w: 64,
            plane_rows: 50,
        };
        assert_eq!(slot_bytes(&out(0)), 64 * 4 * 50);
        assert_eq!(slot_bytes(&out(1)), 64 * 4 * 75);
        assert_eq!(slot_bytes(&out(2)), 64 * 4 * 150);
        assert_eq!(slot_bytes(&out(3)), 64 * 4 * 50);
        assert_eq!(
            vk_format(fourcc(b'X', b'R', b'2', b'4')),
            Some(vk::Format::B8G8R8A8_UNORM)
        );
        assert_eq!(
            vk_format(fourcc(b'X', b'B', b'3', b'0')),
            Some(vk::Format::A2B10G10R10_UNORM_PACK32)
        );
        assert_eq!(vk_format(0), None);
    }

    /// A producer NV12 lands at the slot's layout: one region per plane when the strides match
    /// the slot pitch, a region per row otherwise, never a byte past the last row.
    #[test]
    fn planar_regions_place_both_planes_in_the_slot() {
        let src = |stride: u32| ConvertSrc {
            fd: 0,
            fourcc: fourcc(b'N', b'V', b'1', b'2'),
            modifier: 0,
            offset: 64,
            stride,
            width: 100,
            height: 5,
            plane1: Some((64 + stride * 5, stride)),
        };
        let out = ConvertOut {
            mode: 1,
            width: 100,
            height: 5,
            pitch_w: 32,
            plane_rows: 6,
        };
        // Matching strides: luma 5 rows, chroma 3 rows, each ending at its last row's width.
        let r = planar_regions(&src(128), &out).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(
            (r[0].src_offset, r[0].dst_offset, r[0].size),
            (64, 0, 128 * 4 + 100)
        );
        assert_eq!(
            (r[1].src_offset, r[1].dst_offset, r[1].size),
            (64 + 128 * 5, 128 * 6, 128 * 2 + 100)
        );
        // A tighter producer stride: one region per row, each at the slot pitch.
        let r = planar_regions(&src(112), &out).unwrap();
        assert_eq!(r.len(), 5 + 3);
        assert_eq!(
            (r[1].src_offset, r[1].dst_offset, r[1].size),
            (64 + 112, 128, 100)
        );
        assert_eq!(r[5].dst_offset, 128 * 6);
        assert_eq!(r[7].src_offset, 64 + 112 * 5 + 112 * 2);
        // A smaller frame copies its own rows, never past its planes.
        let small = ConvertSrc {
            width: 50,
            height: 3,
            ..src(112)
        };
        let r = planar_regions(&small, &out).unwrap();
        assert_eq!(r.len(), 3 + 2);
        assert!(r.iter().all(|r| r.size == 50));
        // A stride short of a row, or no chroma plane, is refused.
        assert!(planar_regions(&src(99), &out).is_err());
        assert!(planar_regions(
            &ConvertSrc {
                plane1: None,
                ..src(128)
            },
            &out
        )
        .is_err());
    }

    /// Layouts the shader would write past are refused before any GPU work.
    #[test]
    fn check_layout_bounds_the_shader_writes() {
        let out = |mode, pitch_w, plane_rows| ConvertOut {
            mode,
            width: 100,
            height: 50,
            pitch_w,
            plane_rows,
        };
        assert!(check_layout(&out(0, 100, 50)).is_ok());
        assert!(
            check_layout(&out(0, 99, 50)).is_err(),
            "packed row is 100 words"
        );
        assert!(check_layout(&out(1, 25, 50)).is_ok());
        assert!(
            check_layout(&out(1, 24, 50)).is_err(),
            "NV12 luma row is 25 words"
        );
        assert!(
            check_layout(&out(2, 25, 49)).is_err(),
            "plane rows short of the height"
        );
        assert!(check_layout(&out(5, 1000, 1000)).is_err(), "unknown mode");
    }

    /// The BT.709 limited-range bytes `convert_img.comp` writes, in f32 like the shader.
    fn yuv(c: [u8; 3]) -> (u8, u8, u8) {
        let [r, g, b] = c.map(|v| v as f32 / 255.0);
        let q = |v: f32| v.clamp(0.0, 255.0) as u8;
        (
            q(16.0 + 255.0 * (0.1826 * r + 0.6142 * g + 0.0620 * b) + 0.5),
            q(128.0 + 255.0 * (-0.1006 * r - 0.3386 * g + 0.4392 * b) + 0.5),
            q(128.0 + 255.0 * (0.4392 * r - 0.3989 * g - 0.0403 * b) + 0.5),
        )
    }

    /// Hardware: a tiled dmabuf with a known pattern and a solid cursor goes through the
    /// fused pass into a real NVENC slot (Vulkan-allocated, CUDA-mapped); the slot read back
    /// through CUDA matches the CPU reference for NV12 and packed ARGB within one LSB.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run on the RTX box (.21)"]
    fn fused_convert_matches_the_cpu_reference() {
        use crate::imp::tiled_spike::TiledPattern;
        use crate::imp::vkslot::{SlotFormat, VkSlotBlend};
        use crate::imp::{cuda, proto};
        const W: u32 = 128;
        const H: u32 = 64;
        cuda::make_current().expect("shared CUDA context current");
        let mut slots = VkSlotBlend::new().expect("Vulkan slot device");
        let mut bridge = VkBridge::new().expect("Vulkan bridge");
        let src_img = TiledPattern::new(W, H).expect("tiled pattern");
        let (cx, cy, cw, ch) = (10i32, 10i32, 8u32, 8u32);
        let cursor: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], (cw * ch) as usize)
            .flatten()
            .collect();
        bridge
            .set_cursor(7, cw, ch, &cursor)
            .expect("cursor upload");
        let sem = cuda::ExternalSemaphore::import_timeline_fd(
            bridge.convert_timeline_fd().expect("timeline fd"),
        )
        .expect("convert timeline into CUDA");
        let reference = |x: u32, y: u32| -> [u8; 3] {
            let inside = (x as i32) >= cx
                && (y as i32) >= cy
                && (x as i32) < cx + cw as i32
                && (y as i32) < cy + ch as i32;
            if inside {
                [255, 0, 0]
            } else {
                TiledPattern::rgb(x, y)
            }
        };
        let src = ConvertSrc {
            fd: src_img.fd.as_raw_fd(),
            fourcc: fourcc(b'X', b'R', b'2', b'4'),
            modifier: src_img.modifier,
            offset: src_img.offset,
            stride: src_img.stride,
            width: W,
            height: H,
            plane1: None,
        };
        let rect = Some(proto::CursorRect {
            x: cx,
            y: cy,
            w: cw,
            h: ch,
        });
        for (fmt, mode) in [(SlotFormat::Nv12, 1u32), (SlotFormat::Argb, 0u32)] {
            let slot = slots.alloc_slot(fmt, W, H).expect("slot");
            let (fd, size) = slots.slot_fd(slot.id).expect("slot fd");
            bridge
                .register_slot(slot.id as u32, fd, size)
                .expect("register slot");
            let out = ConvertOut {
                mode,
                width: W,
                height: H,
                pitch_w: (slot.pitch / 4) as u32,
                plane_rows: slot.height,
            };
            // Twice: the second pass exercises the cached image's re-acquire.
            for _ in 0..2 {
                let value = bridge
                    .convert(&src, slot.id as u32, &out, rect)
                    .expect("fused convert");
                sem.wait(value).expect("wait the pass on the copy stream");
            }
            let mut worst = 0u8;
            if mode == 1 {
                let y_plane =
                    cuda::read_plane_to_host(slot.ptr, slot.pitch, W as usize, H as usize)
                        .expect("read Y");
                let uv = cuda::read_plane_to_host(
                    slot.ptr + (slot.pitch * H as usize) as u64,
                    slot.pitch,
                    W as usize,
                    (H / 2) as usize,
                )
                .expect("read UV");
                for y in 0..H {
                    for x in 0..W {
                        let (ly, _, _) = yuv(reference(x, y));
                        let got = y_plane[(y * W + x) as usize];
                        worst = worst.max(ly.abs_diff(got));
                    }
                }
                for by in 0..H / 2 {
                    for bx in 0..W / 2 {
                        let (x0, y0) = (bx * 2, by * 2);
                        let mut acc = [0f32; 3];
                        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let c = reference(x0 + dx, y0 + dy);
                            for i in 0..3 {
                                acc[i] += c[i] as f32 / 255.0 * 0.25;
                            }
                        }
                        let q = |v: f32| v.clamp(0.0, 255.0) as u8;
                        let [r, g, b] = acc;
                        let u = q(128.0 + 255.0 * (-0.1006 * r - 0.3386 * g + 0.4392 * b) + 0.5);
                        let v = q(128.0 + 255.0 * (0.4392 * r - 0.3989 * g - 0.0403 * b) + 0.5);
                        let off = (by * W + bx * 2) as usize;
                        worst = worst.max(u.abs_diff(uv[off])).max(v.abs_diff(uv[off + 1]));
                    }
                }
            } else {
                let argb =
                    cuda::read_plane_to_host(slot.ptr, slot.pitch, (W * 4) as usize, H as usize)
                        .expect("read ARGB");
                for y in 0..H {
                    for x in 0..W {
                        let [r, g, b] = reference(x, y);
                        let off = ((y * W + x) * 4) as usize;
                        let got = &argb[off..off + 4];
                        worst = worst
                            .max(b.abs_diff(got[0]))
                            .max(g.abs_diff(got[1]))
                            .max(r.abs_diff(got[2]))
                            .max(255u8.abs_diff(got[3]));
                    }
                }
            }
            println!("fused convert {fmt:?}: worst LSB delta {worst}");
            assert!(worst <= 1, "{fmt:?}: worst delta {worst} exceeds one LSB");
        }
        slots.free_slots();
    }
}
