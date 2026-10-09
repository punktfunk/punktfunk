//! Vulkan-allocated NVENC input slots and the SPIR-V compute cursor blend.
//!
//! ```text
//!   exportable VkBuffer ──vkGetMemoryFdKHR(OPAQUE_FD)──▶ cuImportExternalMemory
//!        ▲                                                     │ NVENC encodes
//!        └── cursor_blend.comp (cursor rect) ◀── CUDA copies in ┘
//! ```
//!
//! Ring slots are Vulkan external memory both APIs address (`InputSurface`
//! layouts via [`VkSlotBlend::alloc_slot`]). Pin the shader with
//! `glslangValidator -V cursor_blend.comp -o cursor_blend.spv` (CI gates
//! drift). SPIR-V, not PTX: a vendored PTX blob JIT-fails on older drivers.
//!
//! Cursor frames: [`VkSlotBlend::blend_ref_ordered`] (timeline exported to
//! CUDA, copy→blend→encode on-device) or [`VkSlotBlend::blend_ref`] (CPU
//! fence-wait, same as [`super::vulkan::VkBridge`]). No cursor → no Vulkan.
//! Bring-up failure → plain CUDA surfaces and no cursor (warned once); the
//! session still starts.

use super::cuda::{self, CUdeviceptr, PlaneLayout};
use super::vkdev;
use anyhow::{anyhow, ensure, Context as _, Result};
use ash::vk;
use std::os::fd::{FromRawFd as _, OwnedFd};

/// Bitmap edge clamp (px); same value as [`cuda::CURSOR_MAX`] and the capture side.
pub const CURSOR_MAX: u32 = cuda::CURSOR_MAX;

/// `cursor_blend.comp` MODE count — one pipeline each, indexed by [`SlotFormat::mode`]. Bump with the shader MODE list.
const PIPELINE_MODES: u32 = 5;

/// Vendored `cursor_blend.comp` SPIR-V. Rebuild: `glslangValidator -V cursor_blend.comp -o cursor_blend.spv`.
const CURSOR_SPV: &[u8] = include_bytes!("cursor_blend.spv");
/// Vendored `reframe_buf.comp` SPIR-V. Rebuild: `glslangValidator -V reframe_buf.comp -o reframe_buf.spv`.
const REFRAME_SPV: &[u8] = include_bytes!("reframe_buf.spv");

/// NVENC input layout: shader MODE spec-constant and allocation arithmetic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotFormat {
    /// Packed 4-byte ARGB, NVENC byte order B,G,R,A. Size: `pitch × height`.
    Argb,
    /// NV12: Y `[0, H)` then interleaved UV `[H, 3H/2)` under one pitch.
    Nv12,
    /// Planar YUV444: three full-res planes at `pitch × height` intervals.
    Yuv444,
    /// Packed 10-bit `x:R:G:B` 2:10:10:10 LE (NVENC `ARGB10`). Same geometry as [`Argb`](Self::Argb); own MODE because the blend unpacks 10-bit channels.
    X2Rgb10,
    /// Packed 10-bit `x:B:G:R` 2:10:10:10 LE (NVENC `ABGR10`). [`X2Rgb10`](Self::X2Rgb10) with R/B swapped.
    X2Bgr10,
    /// P010: 16-bit Y `[0, H)` then interleaved 16-bit UV `[H, 3H/2)` under one pitch, NV12's
    /// geometry at twice the row bytes. A producer's own P010 is copied in; no blend, no reframe.
    P010,
}

impl SlotFormat {
    /// The layout's number in `cursor_blend.comp` and `convert_img.comp`.
    pub fn mode(self) -> u32 {
        match self {
            SlotFormat::Argb => 0,
            SlotFormat::Nv12 => 1,
            SlotFormat::Yuv444 => 2,
            SlotFormat::X2Rgb10 => 3,
            SlotFormat::X2Bgr10 => 4,
            SlotFormat::P010 => 5,
        }
    }
    /// One 32-bit word per pixel: same slot geometry and one-invocation-per-pixel dispatch.
    fn is_packed32(self) -> bool {
        self.layout() == PlaneLayout::Packed32
    }
    /// `reframe_buf.comp` LAYOUT of the first plane: packed 8-bit, the two 10-bit orders, or
    /// one byte per pixel.
    fn reframe_layout(self) -> u32 {
        match self {
            SlotFormat::Argb => 0,
            SlotFormat::X2Rgb10 => 1,
            SlotFormat::X2Bgr10 => 2,
            SlotFormat::Nv12 | SlotFormat::Yuv444 | SlotFormat::P010 => 3,
        }
    }
    /// The CUDA plane layout this slot holds, under one pitch.
    pub fn layout(self) -> PlaneLayout {
        match self {
            SlotFormat::Nv12 | SlotFormat::P010 => PlaneLayout::Nv12,
            SlotFormat::Yuv444 => PlaneLayout::Yuv444,
            SlotFormat::Argb | SlotFormat::X2Rgb10 | SlotFormat::X2Bgr10 => PlaneLayout::Packed32,
        }
    }
    // A plane's row bytes never depend on the height, nor its rows on the width. P010 is NV12
    // at two bytes a sample.
    fn row_bytes(self, width: u32) -> u64 {
        let width = if self == SlotFormat::P010 {
            width * 2
        } else {
            width
        };
        self.layout().stacked(width, 1).0 as u64
    }
    /// Rows the layout holds for `height` luma rows (NV12 adds its chroma rows, YUV444 its
    /// two extra planes).
    pub fn rows(self, height: u32) -> u64 {
        self.layout().stacked(1, height).1 as u64
    }
}

/// Encoder-held view of one ring slot: the CUDA pointer NVENC registers and the id for
/// [`VkSlotBlend::blend_ref`] / [`blend_ref_ordered`](VkSlotBlend::blend_ref_ordered). Vulkan
/// objects live in [`VkSlotBlend`], so this is `Copy`.
#[derive(Clone, Copy)]
pub struct VkSlotRef {
    /// CUDA mapping of the Vulkan memory — what NVENC registers.
    pub ptr: CUdeviceptr,
    /// Row stride in bytes (row bytes rounded up to 256).
    pub pitch: usize,
    /// Luma rows — the plane-stride multiplier, as in `InputSurface`.
    pub height: u32,
    /// Index into [`VkSlotBlend`]'s slot table.
    pub id: usize,
}

/// Per-slot Vulkan objects, freed CUDA-mapping-first. Own command buffer + descriptor set so
/// ordered blends can be in flight on several slots (a shared set raced the next recording
/// against a still-executing submit). Bindings are written once: slot buffer is immutable,
/// cursor staging is shared.
struct SlotAlloc {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Allocation size, what a second OPAQUE_FD import ([`VkSlotBlend::slot_fd`]) must cover.
    size: u64,
    /// CUDA import of the exported OPAQUE_FD — drop before the Vulkan memory is freed.
    cuda: cuda::ExternalDmabuf,
    cmd: vk::CommandBuffer,
    desc: vk::DescriptorSet,
}

/// Shared Vulkan timeline, exported as OPAQUE_FD and imported into CUDA. `None` → CPU-synced
/// blends only ([`VkSlotBlend::blend_ref`]).
struct Timeline {
    sem: vk::Semaphore,
    /// `vkWaitSemaphoresKHR` (1.1 device: `VK_KHR_timeline_semaphore`). CPU quiesce for bitmap
    /// upload and teardown.
    ts: ash::khr::timeline_semaphore::Device,
    /// CUDA import; `signal`/`wait` enqueue on the encode thread's copy stream.
    cuda: cuda::ExternalSemaphore,
    /// Last value handed out. Monotonic, never reused; each ordered blend takes two (copy-done,
    /// then blend-done).
    ticket: u64,
    /// Last blend-done value an accepted `vkQueueSubmit` will signal. Upload/teardown wait this.
    /// Advanced only on submit success — a failed submit's value would never signal and a wait
    /// would time out.
    last_blend: u64,
}

/// 28-byte push-constant block; must match `cursor_blend.comp`'s `Push`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Push {
    pitch: u32,
    surf_w: u32,
    surf_h: u32,
    cur_w: u32,
    cur_h: u32,
    ox: i32,
    oy: i32,
}

/// 56-byte push-constant block; must match `reframe_buf.comp`'s `Push`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct ReframePush {
    lay: u32,
    axis: u32,
    src_off: u32,
    src_pitch: u32,
    dst_off: u32,
    dst_pitch: u32,
    crop_x: i32,
    crop_y: i32,
    crop_w: i32,
    crop_h: i32,
    out_w: u32,
    out_h: u32,
    step: f32,
    words: u32,
}

/// Every dispatch of one reframe, in order: per plane the x pass then the y pass, each with its
/// group counts. Crop and output halve on NV12's chroma plane (both are even by construction).
fn reframe_passes(
    fmt: SlotFormat,
    src: &VkSlotRef,
    dst: &VkSlotRef,
    crop: [u32; 4],
    out: (u32, u32),
) -> Vec<(ReframePush, u32, u32)> {
    let (sp, dp) = (src.pitch as u32, dst.pitch as u32);
    let (sh, dh) = (src.height, dst.height);
    let mut passes = Vec::new();
    let mut plane = |lay: u32, src_off: u32, dst_off: u32, div: u32| {
        let [x, y, w, h] = crop.map(|v| v / div);
        let (ow, oh) = (out.0 / div, out.1 / div);
        let words = match lay {
            3 => ow.div_ceil(4),
            4 => ow.div_ceil(2),
            _ => ow,
        };
        let base = ReframePush {
            lay,
            axis: 0,
            src_off,
            src_pitch: sp,
            dst_off,
            dst_pitch: dp,
            crop_x: x as i32,
            crop_y: y as i32,
            crop_w: w as i32,
            crop_h: h as i32,
            out_w: ow,
            out_h: oh,
            step: w as f32 / ow as f32,
            words,
        };
        passes.push((base, ow.div_ceil(16), h.div_ceil(16)));
        let ypass = ReframePush {
            axis: 1,
            step: h as f32 / oh as f32,
            ..base
        };
        passes.push((ypass, words.div_ceil(16), oh.div_ceil(16)));
    };
    match fmt {
        SlotFormat::Nv12 => {
            plane(3, 0, 0, 1);
            plane(4, sp * sh, dp * dh, 2);
        }
        SlotFormat::Yuv444 => {
            for k in 0..3 {
                plane(3, k * sp * sh, k * dp * dh, 1);
            }
        }
        packed => plane(packed.reframe_layout(), 0, 0, 1),
    }
    passes
}

/// [`VkSlotBlend::reframe`]'s objects, built on first use.
struct ReframeStage {
    shader: vk::ShaderModule,
    desc_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    /// Float intermediate, `4` floats per texel of the widest plane's x pass.
    tmp: vk::Buffer,
    tmp_mem: vk::DeviceMemory,
    tmp_bytes: u64,
    /// One set per (staging slot id, NVENC slot id) pair. Freed with the slots.
    sets: Vec<(usize, usize, vk::DescriptorSet)>,
}

pub struct VkSlotBlend {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    fence: vk::Fence,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    shader: vk::ShaderModule,
    desc_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    desc_pool: vk::DescriptorPool,
    /// One pipeline per [`SlotFormat`], indexed by `mode()` (spec constant).
    pipelines: [vk::Pipeline; PIPELINE_MODES as usize],
    /// Host-visible cursor bitmap (CURSOR_MAX²·4, tight rows), persistently mapped.
    cur_buf: vk::Buffer,
    cur_mem: vk::DeviceMemory,
    cur_map: *mut u8,
    slots: Vec<SlotAlloc>,
    /// Stream-ordered blend (`None` = CPU-synced only). See [`Timeline`].
    timeline: Option<Timeline>,
    /// Crop-and-scale pipeline ([`VkSlotBlend::reframe`]); `None` until a session reframes.
    reframe: Option<ReframeStage>,
}

// SAFETY: Vulkan handles + a persistently-mapped pointer, uniquely owned and
// destroyed once in `Drop`. Encoder-thread `Send` (not `Sync`): moving opaque
// handles cannot dangle.
unsafe impl Send for VkSlotBlend {}

impl VkSlotBlend {
    /// Create the device and blend pipelines. The encoder's CUDA shared context must already be
    /// current; the physical device is NVIDIA (NVENC).
    pub fn new() -> Result<VkSlotBlend> {
        let nv = vkdev::NvComputeDevice::open(vkdev::DeviceWants::default(), &[])?;
        let (instance, device, qf) = (nv.instance, nv.device, nv.queue_family);
        let want_timeline = nv.timeline_export;
        // SAFETY: `instance` and `device` are the live handles just opened; every
        // CreateInfo/AllocateInfo below is a local that outlives the synchronous call.
        // Single-threaded.
        unsafe {
            // From here Drop tears down; leftover null handles are no-ops.
            let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
            let queue = device.get_device_queue(qf, 0);
            let mut me = VkSlotBlend {
                _entry: nv.entry,
                instance,
                device,
                ext_fd,
                queue,
                cmd_pool: vk::CommandPool::null(),
                fence: vk::Fence::null(),
                mem_props: nv.mem_props,
                shader: vk::ShaderModule::null(),
                desc_layout: vk::DescriptorSetLayout::null(),
                pipe_layout: vk::PipelineLayout::null(),
                desc_pool: vk::DescriptorPool::null(),
                pipelines: [vk::Pipeline::null(); PIPELINE_MODES as usize],
                cur_buf: vk::Buffer::null(),
                cur_mem: vk::DeviceMemory::null(),
                cur_map: std::ptr::null_mut(),
                slots: Vec::new(),
                timeline: None,
                reframe: None,
            };
            me.init_objects(qf).inspect_err(|_| {
                // Drop tears down; null handles from a partial init are no-ops.
            })?;
            if want_timeline {
                // Non-fatal: export/import failure leaves `timeline` None (CPU-synced
                // blends). Bring-up still succeeds.
                if let Err(e) = me.init_timeline() {
                    tracing::info!(
                        error = %format!("{e:#}"),
                        "cursor blend timeline export unavailable — blends stay CPU-synced"
                    );
                }
            }
            tracing::info!(
                stream_ordered = me.timeline.is_some(),
                "Vulkan slot blend ready (exportable NVENC inputs + SPIR-V cursor blend)"
            );
            Ok(me)
        }
    }

    /// Export a timeline semaphore as OPAQUE_FD and import it into CUDA. Caller enabled the
    /// timeline extensions on this device.
    fn init_timeline(&mut self) -> Result<()> {
        // SAFETY: ash calls on the live device; CreateInfo locals outlive each
        // synchronous call. The semaphore is destroyed on every post-create
        // failure. The exported fd is fresh, so `OwnedFd` is its only owner.
        // CUDA context is current (encoder thread).
        unsafe {
            let mut type_ci = vk::SemaphoreTypeCreateInfo::default()
                .semaphore_type(vk::SemaphoreType::TIMELINE)
                .initial_value(0);
            let mut export = vk::ExportSemaphoreCreateInfo::default()
                .handle_types(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD);
            let sem = self
                .device
                .create_semaphore(
                    &vk::SemaphoreCreateInfo::default()
                        .push_next(&mut type_ci)
                        .push_next(&mut export),
                    None,
                )
                .context("create timeline semaphore")?;
            let sem_fd = ash::khr::external_semaphore_fd::Device::new(&self.instance, &self.device);
            let fd = match sem_fd.get_semaphore_fd(
                &vk::SemaphoreGetFdInfoKHR::default()
                    .semaphore(sem)
                    .handle_type(vk::ExternalSemaphoreHandleTypeFlags::OPAQUE_FD),
            ) {
                Ok(f) => OwnedFd::from_raw_fd(f),
                Err(e) => {
                    self.device.destroy_semaphore(sem, None);
                    return Err(e).context("vkGetSemaphoreFdKHR(timeline)");
                }
            };
            let cuda_sem = match cuda::ExternalSemaphore::import_timeline_fd(fd) {
                Ok(c) => c,
                Err(e) => {
                    self.device.destroy_semaphore(sem, None);
                    return Err(e);
                }
            };
            self.timeline = Some(Timeline {
                sem,
                ts: ash::khr::timeline_semaphore::Device::new(&self.instance, &self.device),
                cuda: cuda_sem,
                ticket: 0,
                last_blend: 0,
            });
            Ok(())
        }
    }

    fn init_objects(&mut self, qf: u32) -> Result<()> {
        // SAFETY: ash calls on the live device; CreateInfo locals outlive each
        // synchronous call. Created handles go into `self` immediately so Drop
        // frees them if a later step fails.
        unsafe {
            let d = &self.device;
            self.cmd_pool = d
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(qf)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .context("create command pool")?;
            self.fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .context("create fence")?;

            let cur_size = (CURSOR_MAX * CURSOR_MAX * 4) as u64;
            self.cur_buf = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(cur_size)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                    None,
                )
                .context("create cursor buffer")?;
            let reqs = d.get_buffer_memory_requirements(self.cur_buf);
            let mem_type = self
                .memory_type(
                    reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .context("cursor buffer memory type")?;
            self.cur_mem = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(mem_type),
                    None,
                )
                .context("allocate cursor memory")?;
            d.bind_buffer_memory(self.cur_buf, self.cur_mem, 0)
                .context("bind cursor memory")?;
            self.cur_map = d
                .map_memory(self.cur_mem, 0, cur_size, vk::MemoryMapFlags::empty())
                .context("map cursor memory")? as *mut u8;

            // Binding 0 = surface SSBO, 1 = cursor SSBO (written once in `alloc_slot`).
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            self.desc_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .context("create descriptor layout")?;
            let pc = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(std::mem::size_of::<Push>() as u32)];
            let dl = [self.desc_layout];
            self.pipe_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&dl)
                        .push_constant_ranges(&pc),
                    None,
                )
                .context("create pipeline layout")?;
            // Per-slot sets, freed on ring rebuild (`FREE_DESCRIPTOR_SET`).
            // 64 is well above encoder POOL (8). 128 descriptors = 2 bindings × 64 sets.
            let pool_sizes = [vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(128)];
            self.desc_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                        .max_sets(64)
                        .pool_sizes(&pool_sizes),
                    None,
                )
                .context("create descriptor pool")?;

            // One pipeline per MODE; spec constant 0.
            if CURSOR_SPV.len() % 4 != 0 {
                anyhow::bail!("cursor_blend.spv is not word-aligned");
            }
            let words: Vec<u32> = CURSOR_SPV
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            self.shader = d
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .context("create blend shader module")?;
            for mode in 0u32..PIPELINE_MODES {
                let entries = [vk::SpecializationMapEntry::default()
                    .constant_id(0)
                    .offset(0)
                    .size(4)];
                let data = mode.to_le_bytes();
                let spec = vk::SpecializationInfo::default()
                    .map_entries(&entries)
                    .data(&data);
                let stage = vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::COMPUTE)
                    .module(self.shader)
                    .name(c"main")
                    .specialization_info(&spec);
                let info = [vk::ComputePipelineCreateInfo::default()
                    .stage(stage)
                    .layout(self.pipe_layout)];
                let p = d
                    .create_compute_pipelines(vk::PipelineCache::null(), &info, None)
                    .map_err(|(_, e)| e)
                    .context("create blend pipeline")?[0];
                self.pipelines[mode as usize] = p;
            }
        }
        Ok(())
    }

    fn memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        vkdev::memory_type(&self.mem_props, type_bits, flags)
    }

    /// Allocate one NVENC input as exportable Vulkan memory mapped into CUDA. Layout matches
    /// `InputSurface` (contiguous planes, one pitch); pitch = row bytes rounded to 256.
    pub fn alloc_slot(&mut self, fmt: SlotFormat, width: u32, height: u32) -> Result<VkSlotRef> {
        let pitch = (fmt.row_bytes(width) + 255) & !255;
        let size = pitch * fmt.rows(height);
        // SAFETY: `ExternalMemoryBufferCreateInfo`/`ExportMemoryAllocateInfo`
        // declare OPAQUE_FD; `MemoryDedicatedAllocateInfo` ties memory to the
        // buffer. Infos are locals outliving each call. Failure paths destroy
        // created objects once. The exported fd is fresh, so `OwnedFd` is its
        // only owner.
        unsafe {
            let d = &self.device;
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(size)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER)
                        .push_next(&mut ext_info),
                    None,
                )
                .context("create slot buffer")?;
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
            let mut export = vk::ExportMemoryAllocateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let memory = match d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(mem_type)
                    .push_next(&mut export)
                    .push_next(&mut dedicated),
                None,
            ) {
                Ok(m) => m,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("allocate exportable slot memory");
                }
            };
            if let Err(e) = d.bind_buffer_memory(buffer, memory, 0) {
                d.free_memory(memory, None);
                d.destroy_buffer(buffer, None);
                return Err(e).context("bind slot memory");
            }
            let fd = match self.ext_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            ) {
                Ok(f) => OwnedFd::from_raw_fd(f),
                Err(e) => {
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("vkGetMemoryFdKHR(slot)");
                }
            };
            let ext = match cuda::ExternalDmabuf::import_owned_fd(fd, reqs.size) {
                Ok(c) => c,
                Err(e) => {
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("cuImportExternalMemory(slot OPAQUE_FD)");
                }
            };
            // Per-slot descriptor set + command buffer (see `SlotAlloc`).
            // Binding 0 = this slot's buffer (immutable); 1 = shared cursor staging.
            let dls = [self.desc_layout];
            let desc = match d.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.desc_pool)
                    .set_layouts(&dls),
            ) {
                Ok(s) => s[0],
                Err(e) => {
                    drop(ext); // CUDA mapping first
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("allocate slot descriptor set");
                }
            };
            let surf_info = [vk::DescriptorBufferInfo::default()
                .buffer(buffer)
                .offset(0)
                .range(size)];
            let cur_info = [vk::DescriptorBufferInfo::default()
                .buffer(self.cur_buf)
                .offset(0)
                .range((CURSOR_MAX * CURSOR_MAX * 4) as u64)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(desc)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&surf_info),
                vk::WriteDescriptorSet::default()
                    .dst_set(desc)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&cur_info),
            ];
            d.update_descriptor_sets(&writes, &[]);
            let cmd = match d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(self.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            ) {
                Ok(c) => c[0],
                Err(e) => {
                    let _ = d.free_descriptor_sets(self.desc_pool, &[desc]);
                    drop(ext); // CUDA mapping first
                    d.free_memory(memory, None);
                    d.destroy_buffer(buffer, None);
                    return Err(e).context("allocate slot command buffer");
                }
            };
            let r = VkSlotRef {
                ptr: ext.ptr,
                pitch: pitch as usize,
                height,
                id: self.slots.len(),
            };
            self.slots.push(SlotAlloc {
                buffer,
                memory,
                size: reqs.size,
                cuda: ext,
                cmd,
                desc,
            });
            Ok(r)
        }
    }

    /// Free every slot (encoder teardown). CUDA mappings drop first — `SlotAlloc.cuda`'s `Drop`
    /// runs before the Vulkan objects are freed below.
    /// A fresh OPAQUE_FD of slot `id`'s memory plus its size, for the zero-copy worker to
    /// import: its fused convert then writes the very bytes NVENC encodes.
    pub fn slot_fd(&self, id: usize) -> Result<(std::os::fd::OwnedFd, u64)> {
        let s = self
            .slots
            .get(id)
            .ok_or_else(|| anyhow!("bad slot id {id}"))?;
        // SAFETY: `s.memory` is a live exportable allocation of this device.
        let fd = unsafe {
            self.ext_fd.get_memory_fd(
                &vk::MemoryGetFdInfoKHR::default()
                    .memory(s.memory)
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD),
            )
        }
        .context("vkGetMemoryFdKHR(slot, worker)")?;
        // SAFETY: a fresh descriptor the call just returned; nothing else owns it.
        Ok((
            unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) },
            s.size,
        ))
    }

    pub fn free_slots(&mut self) {
        // Ordered blends (and fence-wait timeouts) can still be on the queue.
        // `device_wait_idle`, not a timeline wait: a submit whose CUDA copy-done
        // never fired is still covered.
        if !self.slots.is_empty() {
            // SAFETY: single-threaded owner; no other thread touches this device or its queue.
            unsafe {
                let _ = self.device.device_wait_idle();
            }
        }
        if let Some(stage) = &mut self.reframe {
            for (_, _, set) in stage.sets.drain(..) {
                // SAFETY: allocated from `stage.pool` in `reframe_set`, freed once; the queue
                // is idle (above) and the sets name the slot buffers freed below.
                unsafe {
                    let _ = self.device.free_descriptor_sets(stage.pool, &[set]);
                }
            }
        }
        for s in self.slots.drain(..) {
            drop(s.cuda); // CUDA mapping first
                          // SAFETY: uniquely owned by the drained `SlotAlloc`, created in
                          // `alloc_slot`, destroyed once. Queue is idle (`device_wait_idle`).
            unsafe {
                let _ = self.device.free_descriptor_sets(self.desc_pool, &[s.desc]);
                self.device.free_command_buffers(self.cmd_pool, &[s.cmd]);
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
        }
    }

    /// Upload cursor RGBA (`cw*ch*4`, tight rows) into the mapped staging buffer. Call only when
    /// the bitmap changes (position is a push constant). Quiesces any in-flight ordered blend first.
    pub fn upload_cursor(&mut self, rgba: &[u8], cw: u32, ch: u32) {
        if let Some(t) = &self.timeline {
            if t.last_blend > 0 {
                let sems = [t.sem];
                let values = [t.last_blend];
                // SAFETY: `t.sem` is live; wait-info arrays outlive the call.
                // `last_blend` is only a value an accepted submit will signal.
                // Timeout: proceed (one torn bitmap); the staging buffer lives.
                let r = unsafe {
                    t.ts.wait_semaphores(
                        &vk::SemaphoreWaitInfo::default()
                            .semaphores(&sems)
                            .values(&values),
                        1_000_000_000,
                    )
                };
                if let Err(e) = r {
                    tracing::warn!(
                        error = ?e,
                        "cursor upload quiesce failed — proceeding (a torn cursor bitmap for \
                         one frame at worst)"
                    );
                }
            }
        }
        let cw = cw.min(CURSOR_MAX);
        let ch = ch.min(CURSOR_MAX);
        let len = (cw * ch * 4) as usize;
        let len = len.min(rgba.len());
        // SAFETY: `cur_map` is the live CURSOR_MAX²·4 mapping (unmapped in
        // `Drop`); `len` is clamped to the source and the buffer. No blend
        // reads race: CPU-synced blends fence-wait; ordered blends quiesced.
        unsafe {
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), self.cur_map, len);
        }
    }

    /// Timeline was exported to CUDA, so [`blend_ref_ordered`](Self::blend_ref_ordered) can order
    /// copy→blend→encode on-device.
    pub fn ordered_ready(&self) -> bool {
        self.timeline.is_some()
    }

    /// Last timeline value handed out (`0` = none). Test hook: did the ordered path run.
    pub fn ordered_ticket(&self) -> u64 {
        self.timeline.as_ref().map_or(0, |t| t.ticket)
    }

    /// Push constants + dispatch groups for one blend (must match `cursor_blend.comp`). Packed
    /// 32-bit: one invocation per cursor pixel. NV12/YUV444: word-aligned 4-px spans × (2-row
    /// blocks | rows). `None` if the clamped rect is empty.
    fn blend_geometry(
        slot: &VkSlotRef,
        fmt: SlotFormat,
        surf_w: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Option<(Push, u32, u32)> {
        let cw = cw.min(CURSOR_MAX);
        let ch = ch.min(CURSOR_MAX);
        if cw == 0 || ch == 0 {
            return None;
        }
        let push = Push {
            pitch: slot.pitch as u32,
            surf_w,
            surf_h: slot.height,
            cur_w: cw,
            cur_h: ch,
            ox,
            oy,
        };
        // Packed 32-bit (ARGB and both 10-bit HDR layouts): one invocation per pixel.
        // Everything else is the word-aligned-span arm.
        let (gx, gy) = if fmt.is_packed32() {
            // One invocation per cursor pixel = one exclusively-owned 32-bit word.
            (cw.div_ceil(8), ch.div_ceil(8))
        } else {
            let x0 = (ox >> 2) << 2;
            let spans = ((ox + cw as i32) - x0 + 3).div_euclid(4).max(1) as u32;
            let rows = match fmt {
                SlotFormat::Nv12 => {
                    // 2-row blocks on the SURFACE chroma grid (shader uses the
                    // same y0). Odd `oy` covers one extra block.
                    let first = oy.div_euclid(2);
                    let last = (oy + ch as i32 - 1).div_euclid(2);
                    (last - first + 1) as u32
                }
                _ => ch,
            };
            (spans.div_ceil(8), rows.div_ceil(8))
        };
        Some((push, gx, gy))
    }

    /// Record barriers + bind + push + dispatch into `id`'s command buffer.
    ///
    /// # Safety
    /// The slot's previous submit must have completed: sync blends fence-wait;
    /// ordered blends reuse a slot only after its encode (GPU-ordered after
    /// the blend) was polled.
    unsafe fn record_blend(
        &self,
        id: usize,
        fmt: SlotFormat,
        push: &Push,
        gx: u32,
        gy: u32,
    ) -> Result<vk::CommandBuffer> {
        // SAFETY: caller contract (`# Safety`): previous submit completed, so
        // the command buffer is re-recordable. Single-thread owner.
        unsafe {
            let alloc = self
                .slots
                .get(id)
                .ok_or_else(|| anyhow!("bad slot id {id}"))?;
            let d = &self.device;
            let cmd = alloc.cmd;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .context("begin blend cmd")?;
            // CUDA wrote this memory outside Vulkan's view — acquire for the shader.
            let acquire = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &acquire,
                &[],
                &[],
            );
            d.cmd_bind_pipeline(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipelines[fmt.mode() as usize],
            );
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipe_layout,
                0,
                &[alloc.desc],
                &[],
            );
            d.cmd_push_constants(
                cmd,
                self.pipe_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                bytemuck::bytes_of(push),
            );
            d.cmd_dispatch(cmd, gx.max(1), gy.max(1), 1);
            // Release shader writes for the downstream CUDA/NVENC read.
            let release = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &release,
                &[],
                &[],
            );
            d.end_command_buffer(cmd).context("end blend cmd")?;
            Ok(cmd)
        }
    }

    /// Blend the uploaded cursor into `slot` at `(ox, oy)`: record, submit, fence-wait.
    /// Caller has CPU-synced the CUDA copy; the fence makes shader writes visible
    /// to the following NVENC encode (NVIDIA fence-ordered access, no QF transfer).
    #[allow(clippy::too_many_arguments)] // surface geometry + cursor rect — unpacked kernel args
    pub fn blend_ref(
        &mut self,
        slot: &VkSlotRef,
        fmt: SlotFormat,
        surf_w: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        ensure!(fmt != SlotFormat::P010, "a P010 slot has no cursor blend");
        let Some((push, gx, gy)) = Self::blend_geometry(slot, fmt, surf_w, cw, ch, ox, oy) else {
            return Ok(());
        };
        // SAFETY: single-thread owner. Previous submit completed (`record_blend`
        // contract: this path fence-waits; an earlier ordered blend finished
        // before ring reuse). Submit-info arrays outlive the call. Shader
        // accesses stay in-bounds via `push` vs the `alloc_slot` geometry.
        unsafe {
            let d = &self.device;
            let cmd = self.record_blend(slot.id, fmt, &push, gx, gy)?;
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            d.queue_submit(self.queue, &submit, self.fence)
                .context("submit blend")?;
            let r = d.wait_for_fences(&[self.fence], true, 1_000_000_000);
            if r.is_err() {
                // Wait failed: the submit may still be running. Do not reset
                // the fence (pending signal) — drain first, then reset.
                let _ = d.device_wait_idle();
            }
            d.reset_fences(&[self.fence]).ok();
            r.context("blend fence wait")?;
        }
        Ok(())
    }

    /// Stream-ordered blend: no CPU sync. Caller has enqueued (not synced) the
    /// CUDA copy on this thread's copy stream. Then: CUDA-signal `copy_done`,
    /// Vulkan-submit waiting it and signaling `blend_done`, CUDA-wait
    /// `blend_done` so later stream work (the encode) sees the writes.
    ///
    /// Fresh timeline values, never reused. CUDA wait is enqueued only after
    /// submit is accepted. An orphaned CUDA signal is legal (later larger
    /// signal satisfies waiters).
    #[allow(clippy::too_many_arguments)] // same unpacked kernel args as `blend_ref`
    pub fn blend_ref_ordered(
        &mut self,
        slot: &VkSlotRef,
        fmt: SlotFormat,
        surf_w: u32,
        cw: u32,
        ch: u32,
        ox: i32,
        oy: i32,
    ) -> Result<()> {
        ensure!(fmt != SlotFormat::P010, "a P010 slot has no cursor blend");
        let Some((push, gx, gy)) = Self::blend_geometry(slot, fmt, surf_w, cw, ch, ox, oy) else {
            return Ok(());
        };
        let (copy_done, blend_done, sem) = {
            let t = self
                .timeline
                .as_mut()
                .ok_or_else(|| anyhow!("ordered blend without timeline support"))?;
            t.ticket += 2;
            (t.ticket - 1, t.ticket, t.sem)
        };
        // Signal first: if this fails, nothing was submitted and the fresh
        // values are skipped. Reverse order wedges the queue (blend waiting
        // a copy-done that was never enqueued).
        self.timeline
            .as_ref()
            .expect("checked above")
            .cuda
            .signal(copy_done)
            .context("cuSignalExternalSemaphoresAsync (copy done)")?;
        // SAFETY: single-thread owner. Previous submit completed (`record_blend`
        // / ring-reuse). Submit-info and timeline chain are locals outliving
        // `queue_submit`. Completion is the timeline, not a fence.
        unsafe {
            let cmd = self.record_blend(slot.id, fmt, &push, gx, gy)?;
            let cmds = [cmd];
            let wait_sems = [sem];
            let wait_vals = [copy_done];
            let wait_stages = [vk::PipelineStageFlags::COMPUTE_SHADER];
            let sig_sems = [sem];
            let sig_vals = [blend_done];
            let mut tsi = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&wait_vals)
                .signal_semaphore_values(&sig_vals);
            let submit = [vk::SubmitInfo::default()
                .command_buffers(&cmds)
                .wait_semaphores(&wait_sems)
                .wait_dst_stage_mask(&wait_stages)
                .signal_semaphores(&sig_sems)
                .push_next(&mut tsi)];
            self.device
                .queue_submit(self.queue, &submit, vk::Fence::null())
                .context("submit ordered blend")?;
        }
        let t = self.timeline.as_mut().expect("checked above");
        // `blend_done` will be signaled; upload/teardown may wait it.
        t.last_blend = blend_done;
        if let Err(e) = t.cuda.wait(blend_done) {
            // Blend is in flight but encode would no longer wait it — restore
            // ordering with one bounded CPU wait.
            let sems = [t.sem];
            let values = [blend_done];
            // SAFETY: live timeline; wait-info arrays outlive the call.
            // `blend_done` was accepted for signaling, so the wait terminates
            // (timeout backstops a wedged queue).
            let r = unsafe {
                t.ts.wait_semaphores(
                    &vk::SemaphoreWaitInfo::default()
                        .semaphores(&sems)
                        .values(&values),
                    1_000_000_000,
                )
            };
            tracing::warn!(
                error = %format!("{e:#}"),
                cpu_wait = ?r,
                "ordered cursor blend: CUDA-side wait enqueue failed — ordering restored with \
                 a CPU wait this frame"
            );
        }
        Ok(())
    }
}

impl VkSlotBlend {
    /// Scale the `crop` (`x, y, w, h` in source pixels) of staging slot `src` into NVENC slot
    /// `dst` at `out`, both in layout `fmt`: record, submit, fence-wait. The caller has
    /// CPU-synced the CUDA copy into `src`; the fence makes the writes visible to the encode.
    pub fn reframe(
        &mut self,
        src: &VkSlotRef,
        dst: &VkSlotRef,
        fmt: SlotFormat,
        crop: [u32; 4],
        out: (u32, u32),
    ) -> Result<()> {
        ensure!(fmt != SlotFormat::P010, "a P010 slot has no reframe");
        let passes = reframe_passes(fmt, src, dst, crop, out);
        let need = passes
            .iter()
            .map(|(p, _, _)| u64::from(p.out_w) * p.crop_h as u64 * 16)
            .max()
            .unwrap_or(16);
        self.ensure_reframe(need)?;
        let set = self.reframe_set(src.id, dst.id)?;
        let stage = self.reframe.as_ref().expect("ensure_reframe built it");
        let cmd = self
            .slots
            .get(dst.id)
            .ok_or_else(|| anyhow!("bad slot id {}", dst.id))?
            .cmd;
        // SAFETY: single-thread owner. `dst`'s previous submit completed (fence-waited blend
        // or reframe; reframing sessions never submit ordered). Every info and slice is a
        // local outliving its synchronous call.
        // Shader reads and writes stay inside the slots by `reframe_passes`' geometry.
        unsafe {
            let d = &self.device;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .context("begin reframe cmd")?;
            let acquire = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &acquire,
                &[],
                &[],
            );
            d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, stage.pipeline);
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                stage.pipe_layout,
                0,
                &[set],
                &[],
            );
            for (push, gx, gy) in &passes {
                d.cmd_push_constants(
                    cmd,
                    stage.pipe_layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    bytemuck::bytes_of(push),
                );
                d.cmd_dispatch(cmd, (*gx).max(1), (*gy).max(1), 1);
                // Each pass reads what the one before wrote. Without a barrier between them
                // the NVIDIA driver ran the y pass before the x pass it reads.
                let between = [vk::MemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)];
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &between,
                    &[],
                    &[],
                );
            }
            let release = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::MEMORY_READ)];
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &release,
                &[],
                &[],
            );
            d.end_command_buffer(cmd).context("end reframe cmd")?;
            let cmds = [cmd];
            let submit = [vk::SubmitInfo::default().command_buffers(&cmds)];
            d.queue_submit(self.queue, &submit, self.fence)
                .context("submit reframe")?;
            let r = d.wait_for_fences(&[self.fence], true, 1_000_000_000);
            if r.is_err() {
                // Same as `blend_ref`: drain before resetting a fence that may still signal.
                let _ = d.device_wait_idle();
            }
            d.reset_fences(&[self.fence]).ok();
            r.context("reframe fence wait")?;
        }
        Ok(())
    }

    /// Build the reframe pipeline on first use and grow the intermediate to `tmp_bytes`.
    fn ensure_reframe(&mut self, tmp_bytes: u64) -> Result<()> {
        if self.reframe.is_none() {
            let stage = self.build_reframe_stage()?;
            self.reframe = Some(stage);
        }
        let stage = self.reframe.as_mut().expect("built above");
        if stage.tmp_bytes >= tmp_bytes {
            return Ok(());
        }
        // SAFETY: single-thread owner. The queue is drained before the old intermediate and
        // the sets naming it are released; the new buffer is bound before any set names it.
        unsafe {
            let d = &self.device;
            let _ = d.device_wait_idle();
            for (_, _, set) in stage.sets.drain(..) {
                let _ = d.free_descriptor_sets(stage.pool, &[set]);
            }
            d.destroy_buffer(stage.tmp, None);
            d.free_memory(stage.tmp_mem, None);
            stage.tmp = vk::Buffer::null();
            stage.tmp_mem = vk::DeviceMemory::null();
            stage.tmp_bytes = 0;
            let tmp = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(tmp_bytes)
                        .usage(vk::BufferUsageFlags::STORAGE_BUFFER),
                    None,
                )
                .context("create reframe intermediate")?;
            stage.tmp = tmp;
            let reqs = d.get_buffer_memory_requirements(tmp);
            let mem_type = vkdev::memory_type(
                &self.mem_props,
                reqs.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )
            .context("reframe intermediate memory")?;
            stage.tmp_mem = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(reqs.size)
                        .memory_type_index(mem_type),
                    None,
                )
                .context("allocate reframe intermediate")?;
            d.bind_buffer_memory(tmp, stage.tmp_mem, 0)
                .context("bind reframe intermediate")?;
            stage.tmp_bytes = tmp_bytes;
        }
        Ok(())
    }

    fn build_reframe_stage(&self) -> Result<ReframeStage> {
        let mut stage = ReframeStage {
            shader: vk::ShaderModule::null(),
            desc_layout: vk::DescriptorSetLayout::null(),
            pipe_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            pool: vk::DescriptorPool::null(),
            tmp: vk::Buffer::null(),
            tmp_mem: vk::DeviceMemory::null(),
            tmp_bytes: 0,
            sets: Vec::new(),
        };
        // SAFETY: ash calls on the live device; create-infos are locals outliving each call.
        // On any failure every handle made so far is destroyed once (null ones are no-ops).
        unsafe {
            let d = &self.device;
            let built = (|| -> Result<()> {
                let binding = |b| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(b)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                };
                let bindings = [binding(0), binding(1), binding(2)];
                stage.desc_layout = d
                    .create_descriptor_set_layout(
                        &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                        None,
                    )
                    .context("create reframe descriptor layout")?;
                let pc = [vk::PushConstantRange::default()
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
                    .size(std::mem::size_of::<ReframePush>() as u32)];
                let dl = [stage.desc_layout];
                stage.pipe_layout = d
                    .create_pipeline_layout(
                        &vk::PipelineLayoutCreateInfo::default()
                            .set_layouts(&dl)
                            .push_constant_ranges(&pc),
                        None,
                    )
                    .context("create reframe pipeline layout")?;
                if REFRAME_SPV.len() % 4 != 0 {
                    anyhow::bail!("reframe_buf.spv is not word-aligned");
                }
                let words: Vec<u32> = REFRAME_SPV
                    .chunks_exact(4)
                    .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                stage.shader = d
                    .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                    .context("create reframe shader module")?;
                let info = [vk::ComputePipelineCreateInfo::default()
                    .stage(
                        vk::PipelineShaderStageCreateInfo::default()
                            .stage(vk::ShaderStageFlags::COMPUTE)
                            .module(stage.shader)
                            .name(c"main"),
                    )
                    .layout(stage.pipe_layout)];
                stage.pipeline = d
                    .create_compute_pipelines(vk::PipelineCache::null(), &info, None)
                    .map_err(|(_, e)| e)
                    .context("create reframe pipeline")?[0];
                // 64 sets: far above POOL (8) staging × NVENC pairings. 3 buffers each.
                let sizes = [vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(192)];
                stage.pool = d
                    .create_descriptor_pool(
                        &vk::DescriptorPoolCreateInfo::default()
                            .flags(vk::DescriptorPoolCreateFlags::FREE_DESCRIPTOR_SET)
                            .max_sets(64)
                            .pool_sizes(&sizes),
                        None,
                    )
                    .context("create reframe descriptor pool")?;
                Ok(())
            })();
            if let Err(e) = built {
                d.destroy_pipeline(stage.pipeline, None);
                d.destroy_shader_module(stage.shader, None);
                d.destroy_descriptor_pool(stage.pool, None);
                d.destroy_pipeline_layout(stage.pipe_layout, None);
                d.destroy_descriptor_set_layout(stage.desc_layout, None);
                return Err(e);
            }
        }
        Ok(stage)
    }

    /// The descriptor set binding staging slot `src`, the intermediate and NVENC slot `dst`.
    fn reframe_set(&mut self, src: usize, dst: usize) -> Result<vk::DescriptorSet> {
        let (src_buf, dst_buf) = match (self.slots.get(src), self.slots.get(dst)) {
            (Some(s), Some(d)) => (s.buffer, d.buffer),
            _ => anyhow::bail!("bad reframe slot ids {src} → {dst}"),
        };
        let stage = self.reframe.as_mut().expect("ensure_reframe ran");
        if let Some(&(_, _, set)) = stage.sets.iter().find(|(s, d, _)| (*s, *d) == (src, dst)) {
            return Ok(set);
        }
        // SAFETY: live device and pool; the buffers are live slot allocations and the
        // intermediate built by `ensure_reframe`. Infos outlive the synchronous calls.
        unsafe {
            let dls = [stage.desc_layout];
            let set = self
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(stage.pool)
                        .set_layouts(&dls),
                )
                .context("allocate reframe descriptor set")?[0];
            let info = |buffer| {
                [vk::DescriptorBufferInfo::default()
                    .buffer(buffer)
                    .offset(0)
                    .range(vk::WHOLE_SIZE)]
            };
            let (a, b, c) = (info(src_buf), info(stage.tmp), info(dst_buf));
            fn write<'a>(
                set: vk::DescriptorSet,
                binding: u32,
                info: &'a [vk::DescriptorBufferInfo; 1],
            ) -> vk::WriteDescriptorSet<'a> {
                vk::WriteDescriptorSet::default()
                    .dst_set(set)
                    .dst_binding(binding)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(info)
            }
            self.device.update_descriptor_sets(
                &[write(set, 0, &a), write(set, 1, &b), write(set, 2, &c)],
                &[],
            );
            stage.sets.push((src, dst, set));
            Ok(set)
        }
    }
}

impl Drop for VkSlotBlend {
    fn drop(&mut self) {
        self.free_slots();
        if let Some(stage) = self.reframe.take() {
            // SAFETY: built in `ensure_reframe`, uniquely owned, destroyed once before the
            // device. `free_slots` waited the queue idle and freed the sets.
            unsafe {
                let d = &self.device;
                d.destroy_buffer(stage.tmp, None);
                d.free_memory(stage.tmp_mem, None);
                d.destroy_pipeline(stage.pipeline, None);
                d.destroy_pipeline_layout(stage.pipe_layout, None);
                d.destroy_descriptor_pool(stage.pool, None);
                d.destroy_descriptor_set_layout(stage.desc_layout, None);
                d.destroy_shader_module(stage.shader, None);
            }
        }
        if let Some(t) = self.timeline.take() {
            drop(t.cuda); // CUDA import of the semaphore before the Vulkan object
                          // SAFETY: created in `init_timeline`, uniquely owned, destroyed
                          // once. `free_slots` already `device_wait_idle`'d in-flight work.
            unsafe {
                self.device.destroy_semaphore(t.sem, None);
            }
        }
        // SAFETY: created in `new`/`init_objects` (or null from a partial init
        // — Vulkan destroy is a no-op on null). Uniquely owned, destroyed once,
        // pipelines/layouts/pools before the device, device before instance.
        // No work in flight (`free_slots` drained; CPU blends fence-wait).
        unsafe {
            let d = &self.device;
            for p in self.pipelines {
                if p != vk::Pipeline::null() {
                    d.destroy_pipeline(p, None);
                }
            }
            if self.shader != vk::ShaderModule::null() {
                d.destroy_shader_module(self.shader, None);
            }
            if self.desc_pool != vk::DescriptorPool::null() {
                d.destroy_descriptor_pool(self.desc_pool, None);
            }
            if self.pipe_layout != vk::PipelineLayout::null() {
                d.destroy_pipeline_layout(self.pipe_layout, None);
            }
            if self.desc_layout != vk::DescriptorSetLayout::null() {
                d.destroy_descriptor_set_layout(self.desc_layout, None);
            }
            if !self.cur_map.is_null() {
                d.unmap_memory(self.cur_mem);
            }
            if self.cur_buf != vk::Buffer::null() {
                d.destroy_buffer(self.cur_buf, None);
            }
            if self.cur_mem != vk::DeviceMemory::null() {
                d.free_memory(self.cur_mem, None);
            }
            if self.fence != vk::Fence::null() {
                d.destroy_fence(self.fence, None);
            }
            if self.cmd_pool != vk::CommandPool::null() {
                d.destroy_command_pool(self.cmd_pool, None);
            }
            d.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> VkSlotRef {
        VkSlotRef {
            ptr: 0,
            pitch: 2048,
            height: 1080,
            id: 0,
        }
    }

    fn geo(fmt: SlotFormat, cw: u32, ch: u32, ox: i32, oy: i32) -> Option<(Push, u32, u32)> {
        VkSlotBlend::blend_geometry(&slot(), fmt, 1920, cw, ch, ox, oy)
    }

    #[test]
    fn empty_rect_is_none() {
        assert!(geo(SlotFormat::Argb, 0, 32, 10, 10).is_none());
        assert!(geo(SlotFormat::Nv12, 32, 0, 10, 10).is_none());
    }

    /// NV12 reframes Y at full size, then UV at half the crop and output, from the plane that
    /// starts `pitch × height` into each slot.
    #[test]
    fn nv12_reframe_halves_the_chroma_plane() {
        let src = VkSlotRef {
            ptr: 0,
            pitch: 2048,
            height: 1080,
            id: 0,
        };
        let dst = VkSlotRef {
            ptr: 0,
            pitch: 1024,
            height: 720,
            id: 1,
        };
        let passes = reframe_passes(
            SlotFormat::Nv12,
            &src,
            &dst,
            [240, 0, 1440, 1080],
            (960, 720),
        );
        assert_eq!(passes.len(), 4, "x and y for each of two planes");
        let (y_x, gx, gy) = passes[0];
        assert_eq!(
            (y_x.lay, y_x.axis, y_x.crop_x, y_x.crop_w),
            (3, 0, 240, 1440)
        );
        assert_eq!((y_x.step, gx, gy), (1.5, 60, 68));
        let (y_y, gx, _) = passes[1];
        assert_eq!(
            (y_y.axis, y_y.words, gx),
            (1, 240, 15),
            "four luma bytes per word"
        );
        let (uv_x, _, _) = passes[2];
        assert_eq!(
            (uv_x.lay, uv_x.src_off, uv_x.dst_off),
            (4, 2048 * 1080, 1024 * 720)
        );
        assert_eq!(
            (uv_x.crop_x, uv_x.crop_w, uv_x.out_w, uv_x.out_h),
            (120, 720, 480, 360)
        );
        assert_eq!(passes[3].0.words, 240, "two chroma pairs per word");
        assert_eq!(
            std::mem::size_of::<ReframePush>(),
            56,
            "the shader's push block is 56 bytes"
        );
    }

    /// Hardware: every slot layout, a 2x reframe of a crop with a different value outside it.
    /// Both edges of the output must hold the crop's own values. Reads the NVENC slot back
    /// through CUDA, so it checks the exact bytes NVENC would encode.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run on the RTX box (.21)"]
    fn reframe_keeps_only_the_crop_in_every_layout() {
        cuda::make_current().expect("shared CUDA context current");
        let mut vk = VkSlotBlend::new().expect("Vulkan slot device");
        let (sw, sh, crop, out) = (96u32, 64u32, [16u32, 8, 64, 48], (32u32, 24u32));
        for fmt in [
            SlotFormat::Argb,
            SlotFormat::X2Rgb10,
            SlotFormat::X2Bgr10,
            SlotFormat::Nv12,
            SlotFormat::Yuv444,
        ] {
            let src = vk.alloc_slot(fmt, sw, sh).expect("staging slot");
            let dst = vk.alloc_slot(fmt, out.0, out.1).expect("NVENC slot");
            // Value at a source pixel in plane units: 40 outside the crop, 90 left, 200 right.
            let value = |x: u32, y: u32, div: u32| -> u32 {
                let [cx, cy, cw, ch] = crop.map(|v| v / div);
                if x < cx || y < cy || x >= cx + cw || y >= cy + ch {
                    40
                } else if x < cx + cw / 2 {
                    90
                } else {
                    200
                }
            };
            let rows = fmt.rows(sh) as usize;
            let mut bytes = vec![0u8; src.pitch * rows];
            let put = |bytes: &mut Vec<u8>, off: usize, word: u32| {
                bytes[off..off + 4].copy_from_slice(&word.to_le_bytes());
            };
            for y in 0..sh {
                for x in 0..sw {
                    let v = value(x, y, 1);
                    let row = y as usize * src.pitch;
                    match fmt {
                        SlotFormat::Argb => put(
                            &mut bytes,
                            row + x as usize * 4,
                            0xFF00_0000 | (v << 16) | ((v / 2) << 8) | (v / 3),
                        ),
                        SlotFormat::X2Rgb10 => put(
                            &mut bytes,
                            row + x as usize * 4,
                            0xC000_0000 | ((v * 4) << 20) | ((v * 2) << 10) | v,
                        ),
                        SlotFormat::X2Bgr10 => put(
                            &mut bytes,
                            row + x as usize * 4,
                            0xC000_0000 | (v << 20) | ((v * 2) << 10) | (v * 4),
                        ),
                        SlotFormat::Nv12 | SlotFormat::Yuv444 => bytes[row + x as usize] = v as u8,
                        SlotFormat::P010 => unreachable!("a P010 slot has no reframe"),
                    }
                }
            }
            let plane = src.pitch * sh as usize;
            if fmt == SlotFormat::Nv12 {
                for y in 0..sh / 2 {
                    for x in 0..sw / 2 {
                        let off = plane + y as usize * src.pitch + x as usize * 2;
                        bytes[off] = value(x, y, 2) as u8;
                        bytes[off + 1] = 255 - value(x, y, 2) as u8;
                    }
                }
            }
            if fmt == SlotFormat::Yuv444 {
                let (y_plane, rest) = bytes.split_at_mut(plane);
                rest[..plane].copy_from_slice(y_plane);
                rest[plane..2 * plane].copy_from_slice(y_plane);
            }
            // SAFETY: `src` is the live staging slot allocated above, `rows` rows of `pitch`;
            // the test made the shared context current.
            unsafe { cuda::write_plane_from_host(src.ptr, src.pitch, &bytes, src.pitch, rows) }
                .expect("upload the staging slot");
            vk.reframe(&src, &dst, fmt, crop, out).expect("reframe");
            let got =
                cuda::read_plane_to_host(dst.ptr, dst.pitch, dst.pitch, fmt.rows(out.1) as usize)
                    .expect("read the NVENC slot");
            let word = |off: usize| u32::from_le_bytes(got[off..off + 4].try_into().unwrap());
            let near = |got: u32, want: u32, what: &str| {
                assert!(
                    got.abs_diff(want) <= 3,
                    "{fmt:?} {what}: got {got}, want {want}"
                );
            };
            for (x, want) in [(0u32, 90u32), (out.0 - 1, 200)] {
                for y in [0u32, out.1 - 1] {
                    let off = y as usize * dst.pitch;
                    match fmt {
                        SlotFormat::Argb => {
                            let w = word(off + x as usize * 4);
                            near((w >> 16) & 0xFF, want, "red");
                            near((w >> 8) & 0xFF, want / 2, "green");
                            near(w & 0xFF, want / 3, "blue");
                        }
                        SlotFormat::X2Rgb10 => {
                            let w = word(off + x as usize * 4);
                            near((w >> 20) & 0x3FF, want * 4, "red");
                            near(w & 0x3FF, want, "blue");
                        }
                        SlotFormat::X2Bgr10 => {
                            let w = word(off + x as usize * 4);
                            near(w & 0x3FF, want * 4, "red");
                            near((w >> 20) & 0x3FF, want, "blue");
                        }
                        SlotFormat::Nv12 | SlotFormat::Yuv444 => {
                            near(u32::from(got[off + x as usize]), want, "luma");
                        }
                        SlotFormat::P010 => unreachable!("a P010 slot has no reframe"),
                    }
                }
            }
            if fmt == SlotFormat::Nv12 {
                let base = dst.pitch * out.1 as usize;
                let off =
                    base + (out.1 as usize / 2 - 1) * dst.pitch + (out.0 as usize / 2 - 1) * 2;
                near(u32::from(got[base]), 90, "chroma U, left");
                near(u32::from(got[off + 1]), 55, "chroma V, bottom right");
            }
            if fmt == SlotFormat::Yuv444 {
                let off = 2 * dst.pitch * out.1 as usize + out.0 as usize - 1;
                near(u32::from(got[off]), 200, "third plane, right");
            }
            vk.free_slots();
        }
    }

    /// Clamp to `CURSOR_MAX` so push constants match the staging buffer.
    #[test]
    fn cursor_dims_clamp_to_max() {
        let (push, _, _) = geo(SlotFormat::Argb, CURSOR_MAX + 100, CURSOR_MAX + 1, 0, 0).unwrap();
        assert_eq!(push.cur_w, CURSOR_MAX);
        assert_eq!(push.cur_h, CURSOR_MAX);
    }

    /// Packed 32-bit: per cursor pixel. NV12/YUV444: word-aligned 4-px spans;
    /// NV12 walks 2-row blocks, YUV444 rows. 32×32 at ox=13: spans cover
    /// aligned x∈[12,48) → 9 spans → 2 groups of 8.
    #[test]
    fn group_counts_per_format() {
        let (_, gx, gy) = geo(SlotFormat::Argb, 32, 32, 13, 0).unwrap();
        assert_eq!((gx, gy), (4, 4)); // 32/8 in both axes

        let (_, gx, gy) = geo(SlotFormat::Nv12, 32, 32, 13, 0).unwrap();
        assert_eq!((gx, gy), (2, 2)); // 9 spans → 2 groups; 16 2-row blocks → 2 groups

        let (_, gx, gy) = geo(SlotFormat::Yuv444, 32, 32, 13, 0).unwrap();
        assert_eq!((gx, gy), (2, 4)); // 9 spans → 2 groups; 32 rows → 4 groups
    }

    /// Negative `ox` anchors spans with floor alignment (`>>` on signed),
    /// not truncating division: ox=-5 starts at -8 (9 spans for 32 px).
    /// Truncation would start at -4 and drop the right edge.
    #[test]
    fn negative_ox_floor_aligns_the_span_origin() {
        let (push, gx, _) = geo(SlotFormat::Nv12, 32, 32, -5, 0).unwrap();
        assert_eq!(push.ox, -5, "push constants carry the true origin");
        assert_eq!(gx, 2, "9 spans from the floor-aligned start → 2 groups");
    }

    /// NV12 blocks sit on the SURFACE chroma grid: odd `oy` needs one extra
    /// block for the last luma row. Negative odd `oy` uses `div_euclid`.
    #[test]
    fn odd_oy_nv12_adds_the_straddle_block() {
        let (_, _, gy) = geo(SlotFormat::Nv12, 32, 32, 0, 8).unwrap();
        assert_eq!(gy, 2, "even oy: 16 chroma-grid blocks → 2 groups");
        let (_, _, gy) = geo(SlotFormat::Nv12, 32, 32, 0, 7).unwrap();
        assert_eq!(gy, 3, "odd oy: 17 blocks cover luma rows 7..39 → 3 groups");
        let (push, _, gy) = geo(SlotFormat::Nv12, 32, 32, 0, -3).unwrap();
        assert_eq!(push.oy, -3, "push constants carry the true origin");
        assert_eq!(gy, 3, "blocks -4..30 in steps of 2 → 17 → 3 groups");
    }
}
