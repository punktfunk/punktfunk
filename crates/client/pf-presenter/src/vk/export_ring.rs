//! Images copied into exportable dma-bufs for the native Wayland lane.
//!
//! Drivers decode only into their own optimal tiling, which no compositor imports. A ring
//! holds a few images on a DRM modifier the compositor listed, each exported as one dma-buf:
//! NV12 or P010 for Vulkan Video and PyroWave pictures, the overlay's RGBA for the HUD
//! surface. A Vulkan Video copy waits the picture's timeline value, restores its layout and
//! signals `value + 1`: the write-back the decoder waits before it reuses a sampled picture.
//! PyroWave's three planes need a compute pass to interleave Cb and Cr. Under explicit sync
//! the copy signals the buffer's acquire point; otherwise its fence is waited before the
//! commit, so the compositor never reads a half-written buffer.

use super::sync_timeline::{TimelineMaker, Timelines};
use anyhow::{bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use pf_client_core::video::{NativeVkFrame, NativeVkLayout, QueueLock, RawVkFormat};
use std::cell::Cell;
use std::os::fd::{AsFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::rc::Rc;

pub(crate) const DRM_FORMAT_NV12: u32 = 0x3231_564e;
pub(crate) const DRM_FORMAT_P010: u32 = 0x3031_3050;
/// One buffer on screen, one queued in the compositor, one being written. A fourth only
/// lets a compositing compositor queue a frame deeper (KWin: +4.5 ms at 60 Hz).
const SLOTS: usize = 3;
/// Bit 63 marks a picture ring's key in the lane, never a VAAPI pool key.
pub(crate) const KEY_PICTURES: u64 = 1 << 63;
/// Bit 62 marks an overlay ring's key.
pub(crate) const KEY_OVERLAY: u64 = 1 << 62;
/// A copy of a 4K picture takes well under a millisecond; this bounds a wedged queue.
const COPY_WAIT_NS: u64 = 100_000_000;

/// DRM fourcc and Vulkan format of a picture the ring can copy.
pub(crate) fn fourcc_for(format: RawVkFormat) -> Option<(u32, vk::Format)> {
    let f = vk::Format::from_raw(format.0);
    match f {
        vk::Format::G8_B8R8_2PLANE_420_UNORM => Some((DRM_FORMAT_NV12, f)),
        vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 => Some((DRM_FORMAT_P010, f)),
        _ => None,
    }
}

/// DRM fourcc of an overlay image format; its alpha is premultiplied, as Wayland expects.
pub(crate) fn overlay_fourcc(format: vk::Format) -> Option<u32> {
    let code = |s: &[u8; 4]| u32::from_le_bytes(*s);
    match format {
        vk::Format::B8G8R8A8_UNORM => Some(code(b"AR24")),
        vk::Format::R8G8B8A8_UNORM => Some(code(b"AB24")),
        vk::Format::A2R10G10B10_UNORM_PACK32 => Some(code(b"AR30")),
        vk::Format::A2B10G10R10_UNORM_PACK32 => Some(code(b"AB30")),
        vk::Format::R16G16B16A16_SFLOAT => Some(code(b"AB4H")),
        _ => None,
    }
}

/// Memory planes an export image of `format` has with no auxiliary planes.
fn planes_of(format: vk::Format) -> u32 {
    match format {
        vk::Format::G8_B8R8_2PLANE_420_UNORM
        | vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 => 2,
        _ => 1,
    }
}

/// Kept by the lane while the compositor holds a slot's buffer; dropping it frees the slot.
pub(crate) struct RingHold(Rc<Cell<bool>>);

impl Drop for RingHold {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

struct Slot {
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: OwnedFd,
    /// (offset, pitch) per memory plane.
    planes: Vec<(u32, u32)>,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    /// Submitted, fence not yet waited.
    in_flight: bool,
    /// The compositor holds the buffer.
    busy: Rc<Cell<bool>>,
    /// Acquire and release timelines under explicit sync; `None` for implicit.
    sync: Option<Timelines>,
}

/// What a copy left for the commit.
pub(crate) enum Copied {
    /// Committable: under explicit sync once its acquire point (given) signals, else now.
    Ready(Option<u64>),
    /// Submitted, but it did not finish in time: nothing to show.
    Late,
}

pub(crate) struct ExportRing {
    device: ash::Device,
    pool: vk::CommandPool,
    /// The family the pool and the copy submits live on.
    qfi: u32,
    slots: Vec<Slot>,
    pub(crate) fourcc: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) modifier: u64,
    /// The lane's feedback generation the modifier was chosen under.
    pub(crate) feedback_gen: u64,
    /// The image format; the overlay ring rebuilds when the overlay's changes.
    pub(crate) format: vk::Format,
    /// [`KEY_PICTURES`] or [`KEY_OVERLAY`] plus the ring generation: keys no other ring uses.
    key_base: u64,
    /// The chroma pass of a ring that takes three-plane pictures.
    interleave: Option<Interleave>,
}

/// Cb and Cr interleaved into the chroma plane: a compute pass per copy writes them into a
/// two-channel scratch image of the slot, which the copy then moves into plane 1.
#[derive(Default)]
struct Interleave {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    scratch: Vec<Scratch>,
}

#[derive(Clone, Copy, Default)]
struct Scratch {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    set: vk::DescriptorSet,
}

impl Interleave {
    /// # Safety
    ///
    /// `device` is live and `mem_props` are its physical device's. A failure leaves the
    /// objects made so far for [`Self::destroy`].
    unsafe fn build(
        &mut self,
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        (chroma, spv): (vk::Format, &[u8]),
        (width, height): (u32, u32),
        slots: usize,
    ) -> Result<()> {
        let binding = |b| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(b)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
        };
        let bindings = [binding(0), binding(1), binding(2)];
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spv))?;
        // SAFETY: fn contract; every create-info roots locals that outlive its call, and each
        // handle lands in `self` as it is made.
        unsafe {
            self.set_layout = device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .context("vkCreateDescriptorSetLayout (chroma)")?;
            let set_layouts = [self.set_layout];
            self.layout = device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default().set_layouts(&set_layouts),
                    None,
                )
                .context("vkCreatePipelineLayout (chroma)")?;
            let module = device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                .context("chroma shader module")?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let ci = vk::ComputePipelineCreateInfo::default()
                .stage(stage)
                .layout(self.layout);
            let made = device.create_compute_pipelines(vk::PipelineCache::null(), &[ci], None);
            device.destroy_shader_module(module, None);
            self.pipeline = made
                .map_err(|(_, e)| e)
                .context("vkCreateComputePipelines (chroma)")?[0];
            let sizes = [vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_IMAGE,
                descriptor_count: 3 * slots as u32,
            }];
            self.pool = device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(slots as u32)
                        .pool_sizes(&sizes),
                    None,
                )
                .context("vkCreateDescriptorPool (chroma)")?;
            for _ in 0..slots {
                self.scratch.push(Scratch::default());
                let s = self.scratch.last_mut().expect("just pushed");
                let ci = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(chroma)
                    .extent(extent(width / 2, height / 2))
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED);
                s.image = device
                    .create_image(&ci, None)
                    .context("vkCreateImage (chroma)")?;
                let req = device.get_image_memory_requirements(s.image);
                let Some(type_index) = device_local(mem_props, req.memory_type_bits) else {
                    bail!("no device-local memory type for the chroma image");
                };
                s.memory = device
                    .allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(req.size)
                            .memory_type_index(type_index),
                        None,
                    )
                    .context("vkAllocateMemory (chroma)")?;
                device.bind_image_memory(s.image, s.memory, 0)?;
                s.view = device
                    .create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(s.image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(chroma)
                            .subresource_range(subresource(0)),
                        None,
                    )
                    .context("vkCreateImageView (chroma)")?;
                s.set = device
                    .allocate_descriptor_sets(
                        &vk::DescriptorSetAllocateInfo::default()
                            .descriptor_pool(self.pool)
                            .set_layouts(&set_layouts),
                    )
                    .context("vkAllocateDescriptorSets (chroma)")?[0];
            }
        }
        Ok(())
    }

    /// # Safety
    ///
    /// No submit that uses these objects is pending; null handles are skipped by Vulkan.
    unsafe fn destroy(&mut self, device: &ash::Device) {
        // SAFETY: fn contract; the sets go with their pool.
        unsafe {
            for s in self.scratch.drain(..) {
                device.destroy_image_view(s.view, None);
                device.destroy_image(s.image, None);
                device.free_memory(s.memory, None);
            }
            device.destroy_descriptor_pool(self.pool, None);
            device.destroy_pipeline(self.pipeline, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}

/// The first device-local memory type among `bits`.
fn device_local(mem_props: &vk::PhysicalDeviceMemoryProperties, bits: u32) -> Option<u32> {
    (0..mem_props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0
            && mem_props.memory_types[i as usize]
                .property_flags
                .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
    })
}

/// The chroma plane's format of an NV12 or P010 ring, as a storage image, with its pass.
fn chroma_stage(format: vk::Format) -> Option<(vk::Format, &'static [u8])> {
    use pf_client_core::video_csc_spv::{CHROMA_RG16_COMP, CHROMA_RG8_COMP};
    match format {
        vk::Format::G8_B8R8_2PLANE_420_UNORM => Some((vk::Format::R8G8_UNORM, CHROMA_RG8_COMP)),
        vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 => {
            Some((vk::Format::R16G16_UNORM, CHROMA_RG16_COMP))
        }
        _ => None,
    }
}

/// `(modifier, memory planes)` the driver can create `format` with as a copy target.
///
/// # Safety
///
/// `instance` and `pdev` are live and paired.
unsafe fn driver_modifiers(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
) -> Vec<(u64, u32)> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: fn contract; `list`/`fp2` outlive the call.
    unsafe { instance.get_physical_device_format_properties2(pdev, format, &mut fp2) };
    let mut props = vec![
        vk::DrmFormatModifierPropertiesEXT::default();
        list.drm_format_modifier_count as usize
    ];
    list.p_drm_format_modifier_properties = props.as_mut_ptr();
    let mut fp2 = vk::FormatProperties2::default().push_next(&mut list);
    // SAFETY: fn contract; `props` holds exactly the reported count and outlives the call.
    unsafe { instance.get_physical_device_format_properties2(pdev, format, &mut fp2) };
    props.truncate(list.drm_format_modifier_count as usize);
    props
        .iter()
        .filter(|p| {
            p.drm_format_modifier_tiling_features
                .contains(vk::FormatFeatureFlags::TRANSFER_DST)
        })
        .map(|p| (p.drm_format_modifier, p.drm_format_modifier_plane_count))
        .collect()
}

/// Whether `format` on `modifier` can be created as a TRANSFER_DST image and exported as a
/// dma-buf.
///
/// # Safety
///
/// `instance` and `pdev` are live and paired.
unsafe fn exportable(
    instance: &ash::Instance,
    pdev: vk::PhysicalDevice,
    format: vk::Format,
    modifier: u64,
) -> bool {
    let mut mi = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let mut ext = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(vk::ImageUsageFlags::TRANSFER_DST)
        .push_next(&mut mi)
        .push_next(&mut ext);
    let mut ep = vk::ExternalImageFormatProperties::default();
    let mut props = vk::ImageFormatProperties2::default().push_next(&mut ep);
    // SAFETY: fn contract; the chains root locals that outlive the call.
    unsafe {
        instance
            .get_physical_device_image_format_properties2(pdev, &info, &mut props)
            .is_ok()
            && ep
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
    }
}

impl ExportRing {
    /// A ring of `SLOTS` images on the first of `wanted` (compositor order) the driver can
    /// create as an exportable copy target with no auxiliary planes.
    ///
    /// # Safety
    ///
    /// Handles are live and paired; `device` enabled external_memory_fd,
    /// external_memory_dma_buf, image_drm_format_modifier and queue_family_foreign.
    #[allow(clippy::too_many_arguments)]
    pub(crate) unsafe fn new(
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        device: &ash::Device,
        ext_mem_fd: &ash::khr::external_memory_fd::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        qfi: u32,
        (fourcc, format): (u32, vk::Format),
        (width, height): (u32, u32),
        wanted: &[u64],
        feedback_gen: u64,
        key_base: u64,
        timelines: Option<&TimelineMaker>,
    ) -> Result<Self> {
        // SAFETY: fn contract.
        let driver = unsafe { driver_modifiers(instance, pdev, format) };
        let planes = planes_of(format);
        let candidates: Vec<u64> = wanted
            .iter()
            .copied()
            .filter(|m| driver.iter().any(|&(dm, p)| dm == *m && p == planes))
            // SAFETY: fn contract.
            .filter(|&m| unsafe { exportable(instance, pdev, format, m) })
            .collect();
        let Some(&modifier) = candidates.first() else {
            bail!(
                "no modifier the compositor lists for {fourcc:#010x} is an exportable copy \
                 target here (compositor {wanted:x?}, driver {driver:x?})"
            );
        };
        let pci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(qfi)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: fn contract (live device); `pci` outlives the call.
        let pool =
            unsafe { device.create_command_pool(&pci, None) }.context("vkCreateCommandPool")?;
        let mut ring = Self {
            device: device.clone(),
            pool,
            qfi,
            slots: Vec::new(),
            fourcc,
            width,
            height,
            modifier,
            feedback_gen,
            format,
            key_base,
            interleave: None,
        };
        let image_mod = ash::ext::image_drm_format_modifier::Device::new(instance, device);
        for _ in 0..SLOTS {
            // SAFETY: fn contract; each handle lands in `ring` as it is made, so a failure
            // unwinds through Drop.
            unsafe {
                ring.add_slot(
                    ext_mem_fd,
                    &image_mod,
                    mem_props,
                    format,
                    (width, height),
                    modifier,
                )?
            };
            if let (Some(maker), Some(slot)) = (timelines, ring.slots.last_mut()) {
                // SAFETY: fn contract: the maker was made for this device.
                slot.sync = Some(unsafe { maker.make(device)? });
            }
        }
        Ok(ring)
    }

    /// Give an NV12 or P010 ring the chroma pass three-plane pictures need.
    ///
    /// # Safety
    ///
    /// As [`Self::new`], with the same `instance`, `pdev` and `mem_props`.
    pub(crate) unsafe fn add_interleave(
        &mut self,
        instance: &ash::Instance,
        pdev: vk::PhysicalDevice,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
    ) -> Result<()> {
        let Some((chroma, spv)) = chroma_stage(self.format) else {
            bail!("no chroma pass for {:?}", self.format);
        };
        let needs = vk::FormatFeatureFlags::STORAGE_IMAGE | vk::FormatFeatureFlags::TRANSFER_SRC;
        // SAFETY: fn contract.
        let props = unsafe { instance.get_physical_device_format_properties(pdev, chroma) };
        if !props.optimal_tiling_features.contains(needs) {
            bail!("{chroma:?} is no storage image here");
        }
        let mut il = Interleave::default();
        let slots = self.slots.len();
        let size = (self.width, self.height);
        // SAFETY: fn contract; nothing of `il` was submitted when a failure destroys it.
        unsafe {
            if let Err(e) = il.build(&self.device, mem_props, (chroma, spv), size, slots) {
                il.destroy(&self.device);
                return Err(e);
            }
        }
        self.interleave = Some(il);
        Ok(())
    }

    /// The ring takes three-plane pictures.
    pub(crate) fn planar(&self) -> bool {
        self.interleave.is_some()
    }

    /// `slot`'s (acquire, release) timeline fds, under explicit sync.
    pub(crate) fn sync_fds(&self, slot: usize) -> Option<(BorrowedFd<'_>, BorrowedFd<'_>)> {
        self.slots[slot].sync.as_ref().map(Timelines::fds)
    }

    /// A commit of `slot` that did not happen: its release point is not owed.
    pub(crate) fn uncommit(&mut self, slot: usize) {
        if let Some(t) = &mut self.slots[slot].sync {
            t.uncommit();
        }
    }

    /// Keys whose release point passed since the last poll, under explicit sync.
    pub(crate) fn poll_releases(&mut self) -> Vec<u64> {
        let mut out = Vec::new();
        for i in 0..self.slots.len() {
            let key = self.key(i);
            let d = &self.device;
            if let Some(t) = &mut self.slots[i].sync {
                // SAFETY: the ring's device owns the timelines.
                if t.is_committed() && !unsafe { t.poll_held(d) } {
                    out.push(key);
                }
            }
        }
        out
    }

    /// # Safety
    ///
    /// As [`Self::new`].
    unsafe fn add_slot(
        &mut self,
        ext_mem_fd: &ash::khr::external_memory_fd::Device,
        image_mod: &ash::ext::image_drm_format_modifier::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        format: vk::Format,
        (width, height): (u32, u32),
        modifier: u64,
    ) -> Result<()> {
        let d = &self.device;
        let mods = [modifier];
        let mut list =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&mods);
        let mut ext = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut list)
            .push_next(&mut ext);
        // SAFETY: live device; `ci` roots locals that outlive the call.
        let image = unsafe { d.create_image(&ci, None) }.context("vkCreateImage (export)")?;
        // SAFETY: `image` was just created on this device.
        let req = unsafe { d.get_image_memory_requirements(image) };
        let Some(type_index) = device_local(mem_props, req.memory_type_bits) else {
            // SAFETY: the never-bound image created above.
            unsafe { d.destroy_image(image, None) };
            bail!("no device-local memory type for the export image");
        };
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let ai = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(type_index)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        // SAFETY: live device; the chain roots locals that outlive the call.
        let memory = match unsafe { d.allocate_memory(&ai, None) } {
            Ok(m) => m,
            Err(e) => {
                // SAFETY: the never-bound image created above.
                unsafe { d.destroy_image(image, None) };
                return Err(e).context("vkAllocateMemory (exportable)");
            }
        };
        let gi = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // SAFETY: fresh image and memory of the required size; the export names live
        // memory allocated exportable.
        let raw = unsafe {
            d.bind_image_memory(image, memory, 0)
                .and_then(|()| ext_mem_fd.get_memory_fd(&gi))
        };
        let raw = match raw {
            Ok(fd) => fd,
            Err(e) => {
                // SAFETY: unwinding the two objects created above; nothing else uses them.
                unsafe {
                    d.destroy_image(image, None);
                    d.free_memory(memory, None);
                }
                return Err(e).context("bind + vkGetMemoryFdKHR");
            }
        };
        // SAFETY: the driver hands us a fresh fd we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        // SAFETY: `image` is live and was created with modifier tiling.
        let chosen =
            unsafe { image_mod.get_image_drm_format_modifier_properties(image, &mut props) }
                .map(|()| props.drm_format_modifier);
        let planes: Vec<(u32, u32)> = [
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
        ][..planes_of(format) as usize]
            .iter()
            .map(|&aspect| {
                let sub = vk::ImageSubresource {
                    aspect_mask: aspect,
                    mip_level: 0,
                    array_layer: 0,
                };
                // SAFETY: `image` is live; memory-plane aspects are legal on a modifier image.
                let l = unsafe { d.get_image_subresource_layout(image, sub) };
                (l.offset as u32, l.row_pitch as u32)
            })
            .collect();
        let cbi = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: live pool on this device.
        let cmd = unsafe { d.allocate_command_buffers(&cbi) }.map(|v| v[0]);
        // SAFETY: live device.
        let fence = unsafe { d.create_fence(&vk::FenceCreateInfo::default(), None) };
        // Park before judging, so Drop unwinds whatever did get made.
        self.slots.push(Slot {
            image,
            memory,
            fd,
            planes,
            cmd: *cmd.as_ref().unwrap_or(&vk::CommandBuffer::null()),
            fence: *fence.as_ref().unwrap_or(&vk::Fence::null()),
            in_flight: false,
            busy: Rc::new(Cell::new(false)),
            sync: None,
        });
        cmd.context("vkAllocateCommandBuffers")?;
        fence.context("vkCreateFence")?;
        if chosen.context("vkGetImageDrmFormatModifierPropertiesEXT")? != modifier {
            bail!("the driver created the export image on another modifier");
        }
        Ok(())
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn key(&self, slot: usize) -> u64 {
        self.key_base | slot as u64
    }

    /// `(fd, offset, pitch)` per memory plane of `slot`'s dma-buf, for the lane's import.
    pub(crate) fn planes(&self, slot: usize) -> Vec<(BorrowedFd<'_>, u32, u32)> {
        let s = &self.slots[slot];
        s.planes
            .iter()
            .map(|&(offset, pitch)| (s.fd.as_fd(), offset, pitch))
            .collect()
    }

    /// A slot the compositor does not hold and the lane reports usable.
    pub(crate) fn free_slot(&self, usable: impl Fn(u64) -> bool) -> Option<usize> {
        (0..self.slots.len()).find(|&i| !self.slots[i].busy.get() && usable(self.key(i)))
    }

    /// Mark `slot` held; the lane keeps the returned hold until the compositor releases it.
    pub(crate) fn hold(&self, slot: usize) -> RingHold {
        let busy = &self.slots[slot].busy;
        busy.set(true);
        RingHold(busy.clone())
    }

    /// Copy `frame`'s visible picture into `slot`. `Err` means nothing was submitted.
    /// [`Copied::Ready`]: committable (under explicit sync at the given point, the copy still
    /// running). [`Copied::Late`]: the waited copy did not finish in time; the slot stays in
    /// flight and the frame is not shown. On any `Ok` the submit carries the frame's
    /// `value + 1` signal.
    ///
    /// # Safety
    ///
    /// `frame`'s handles are live on this device and its guard is held for the call; `queue`
    /// is this device's queue of the ring's family, externally synchronised by `lock`.
    pub(crate) unsafe fn copy(
        &mut self,
        slot: usize,
        frame: &NativeVkFrame,
        queue: vk::Queue,
        lock: &QueueLock,
    ) -> Result<Copied> {
        let src = vk::Image::from_raw(frame.image);
        let decode_layout = match frame.layout {
            NativeVkLayout::DecodeDst => vk::ImageLayout::VIDEO_DECODE_DST_KHR,
            NativeVkLayout::DecodeDpb => vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        };
        let range = subresource(frame.layer);
        let layers = |aspect, layer| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: layer,
            layer_count: 1,
        };
        let (w, h) = (self.width, self.height);
        let regions = [
            vk::ImageCopy {
                src_subresource: layers(vk::ImageAspectFlags::PLANE_0, frame.layer),
                src_offset: offset(frame.crop_x, frame.crop_y),
                dst_subresource: layers(vk::ImageAspectFlags::PLANE_0, 0),
                dst_offset: vk::Offset3D::default(),
                extent: extent(w, h),
            },
            vk::ImageCopy {
                src_subresource: layers(vk::ImageAspectFlags::PLANE_1, frame.layer),
                src_offset: offset(frame.crop_x / 2, frame.crop_y / 2),
                dst_subresource: layers(vk::ImageAspectFlags::PLANE_1, 0),
                dst_offset: vk::Offset3D::default(),
                extent: extent(w / 2, h / 2),
            },
        ];
        let timeline = (
            vk::Semaphore::from_raw(frame.semaphore),
            frame.semaphore_value,
        );
        let record = |d: &ash::Device, cmd: vk::CommandBuffer, dst: vk::Image| {
            {
                // The timeline wait sits at TRANSFER, so the chain starts there.
                let to_src = layout_barrier(
                    src,
                    range,
                    decode_layout,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_READ,
                );
                let back = layout_barrier(
                    src,
                    range,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    decode_layout,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::empty(),
                );
                // SAFETY: `cmd` is recording; `src` and `dst` are live per the contract.
                unsafe {
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_src],
                    );
                    d.cmd_copy_image(
                        cmd,
                        src,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        dst,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &regions,
                    );
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[back],
                    );
                }
            }
        };
        // SAFETY: fn contract.
        unsafe { self.run(slot, Some(timeline), queue, lock, record) }
    }

    /// Copy the overlay image (Skia's, left in SHADER_READ_ONLY_OPTIMAL on this queue) into
    /// `slot` and wait for the copy; the image returns to that layout. Results as [`Self::copy`].
    ///
    /// # Safety
    ///
    /// `src` is live on this device, `self.width`×`self.height` in the ring's format, and
    /// its last writer was submitted on `queue`; `queue` as in [`Self::copy`].
    pub(crate) unsafe fn copy_overlay(
        &mut self,
        slot: usize,
        src: vk::Image,
        queue: vk::Queue,
        lock: &QueueLock,
    ) -> Result<Copied> {
        let range = subresource(0);
        let region = vk::ImageCopy {
            src_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            src_offset: vk::Offset3D::default(),
            dst_subresource: vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            dst_offset: vk::Offset3D::default(),
            extent: extent(self.width, self.height),
        };
        let record = |d: &ash::Device, cmd: vk::CommandBuffer, dst: vk::Image| {
            {
                let to_src = layout_barrier(
                    src,
                    range,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                );
                let back = layout_barrier(
                    src,
                    range,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::SHADER_READ,
                );
                // SAFETY: `cmd` is recording; `src` and `dst` are live per the contract.
                unsafe {
                    // Skia's draw on this queue precedes the copy: its colour writes chain in.
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[to_src],
                    );
                    d.cmd_copy_image(
                        cmd,
                        src,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        dst,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[region],
                    );
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::FRAGMENT_SHADER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[back],
                    );
                }
            }
        };
        // SAFETY: fn contract.
        unsafe { self.run(slot, None, queue, lock, record) }
    }

    /// Copy a three-plane picture into `slot`: Y straight into plane 0, Cb and Cr through the
    /// chroma pass into plane 1. Results as [`Self::copy`].
    ///
    /// # Safety
    ///
    /// The ring has its interleave. `luma` (the ring's size, R8 for NV12 or R16 for P010)
    /// and the `cb`/`cr` views (half size each way, same depth) are live on this device in
    /// GENERAL, and their writer was submitted on `queue` before this call. Their next writer
    /// orders itself after this submit's compute and copy stages. `queue` as in [`Self::copy`].
    #[cfg(feature = "pyrowave")]
    pub(crate) unsafe fn copy_planar(
        &mut self,
        slot: usize,
        luma: vk::Image,
        [cb, cr]: [vk::ImageView; 2],
        queue: vk::Queue,
        lock: &QueueLock,
    ) -> Result<Copied> {
        let Some(il) = self.interleave.as_ref() else {
            bail!("the ring has no chroma pass");
        };
        let (s, pipeline, layout) = (il.scratch[slot], il.pipeline, il.layout);
        let (w, h) = (self.width, self.height);
        let plane = |aspect| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        };
        let region = |dst_aspect, (w, h)| vk::ImageCopy {
            src_subresource: plane(vk::ImageAspectFlags::COLOR),
            src_offset: vk::Offset3D::default(),
            dst_subresource: plane(dst_aspect),
            dst_offset: vk::Offset3D::default(),
            extent: extent(w, h),
        };
        let luma_region = region(vk::ImageAspectFlags::PLANE_0, (w, h));
        let chroma_region = region(vk::ImageAspectFlags::PLANE_1, (w / 2, h / 2));
        let info = |view| {
            [vk::DescriptorImageInfo {
                sampler: vk::Sampler::null(),
                image_view: view,
                image_layout: vk::ImageLayout::GENERAL,
            }]
        };
        let (cb_info, cr_info, out_info) = (info(cb), info(cr), info(s.view));
        // The decode's storage writes, made visible to the pass and the luma copy.
        let decoded = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_READ);
        let to_write = layout_barrier(
            s.image,
            subresource(0),
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::SHADER_WRITE,
        );
        let to_src = layout_barrier(
            s.image,
            subresource(0),
            vk::ImageLayout::GENERAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::SHADER_WRITE,
            vk::AccessFlags::TRANSFER_READ,
        );
        let record = |d: &ash::Device, cmd: vk::CommandBuffer, dst: vk::Image| {
            let writes = [(0, &cb_info), (1, &cr_info), (2, &out_info)].map(|(binding, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(s.set)
                    .dst_binding(binding)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(info)
            });
            // SAFETY: `cmd` is recording and the slot's last submit finished, so its set is
            // idle; every handle is live per the contract.
            unsafe {
                d.update_descriptor_sets(&writes, &[]);
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[decoded],
                    &[],
                    &[to_write],
                );
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    layout,
                    0,
                    &[s.set],
                    &[],
                );
                d.cmd_dispatch(cmd, (w / 2).div_ceil(8), (h / 2).div_ceil(8), 1);
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[to_src],
                );
                d.cmd_copy_image(
                    cmd,
                    luma,
                    vk::ImageLayout::GENERAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[luma_region],
                );
                d.cmd_copy_image(
                    cmd,
                    s.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[chroma_region],
                );
            }
        };
        // SAFETY: fn contract.
        unsafe { self.run(slot, None, queue, lock, record) }
    }

    /// One copy into `slot`: take the export image back from the compositor, let `record`
    /// copy into it, hand it back, submit (waiting `timeline`'s value at TRANSFER and
    /// signalling value + 1). Under explicit sync the submit also signals the slot's next
    /// acquire point and returns at once; otherwise the fence is waited. Results as
    /// [`Self::copy`].
    ///
    /// # Safety
    ///
    /// Whatever `record` names is live on this device; `queue` as in [`Self::copy`].
    unsafe fn run(
        &mut self,
        slot: usize,
        timeline: Option<(vk::Semaphore, u64)>,
        queue: vk::Queue,
        lock: &QueueLock,
        record: impl FnOnce(&ash::Device, vk::CommandBuffer, vk::Image),
    ) -> Result<Copied> {
        let d = &self.device;
        let own = self.qfi;
        let s = &mut self.slots[slot];
        if s.in_flight {
            // SAFETY: the fence belongs to this slot's last submit.
            unsafe { d.wait_for_fences(&[s.fence], true, COPY_WAIT_NS) }
                .context("an earlier copy never finished")?;
            s.in_flight = false;
        }
        // SAFETY: the fence is idle: its submit, if any, completed above.
        unsafe { d.reset_fences(&[s.fence]) }.context("vkResetFences")?;
        let foreign = vk::QUEUE_FAMILY_FOREIGN_EXT;
        // Back from the compositor; the old contents are overwritten.
        let acquire = layout_barrier(
            s.image,
            subresource(0),
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        )
        .src_queue_family_index(foreign)
        .dst_queue_family_index(own);
        let release = layout_barrier(
            s.image,
            subresource(0),
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
        )
        .src_queue_family_index(own)
        .dst_queue_family_index(foreign);
        // SAFETY: the slot's command buffer is idle (fence waited above); every handle it
        // names is live per the fn contract; builders are locals outliving each call.
        unsafe {
            d.begin_command_buffer(
                s.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            d.cmd_pipeline_barrier(
                s.cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[acquire],
            );
            record(d, s.cmd, s.image);
            d.cmd_pipeline_barrier(
                s.cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[release],
            );
            d.end_command_buffer(s.cmd)?;
        }
        let cmds = [s.cmd];
        let wait_sems: Vec<vk::Semaphore> = timeline.iter().map(|t| t.0).collect();
        let wait_values: Vec<u64> = timeline.iter().map(|t| t.1).collect();
        let stages: Vec<vk::PipelineStageFlags> = timeline
            .iter()
            .map(|_| vk::PipelineStageFlags::TRANSFER)
            .collect();
        let mut signal_sems = wait_sems.clone();
        let mut signal_values: Vec<u64> = timeline.iter().map(|t| t.1 + 1).collect();
        // The acquire point the commit will carry; recorded as owed until its release.
        let point = s.sync.as_mut().map(|t| {
            let p = t.next_point();
            signal_sems.push(t.acquire);
            signal_values.push(p);
            p
        });
        let mut timeline_info = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wait_values)
            .signal_semaphore_values(&signal_values);
        let mut submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_sems)
            .wait_dst_stage_mask(&stages)
            .command_buffers(&cmds)
            .signal_semaphores(&signal_sems);
        if !signal_sems.is_empty() {
            submit = submit.push_next(&mut timeline_info);
        }
        let submitted = {
            let _q = lock.guard();
            // SAFETY: fn contract (queue external sync held by `_q`); the submit's arrays
            // are locals that outlive the call.
            unsafe { d.queue_submit(queue, &[submit], s.fence) }
        };
        if let Err(e) = submitted {
            if let Some(t) = &mut s.sync {
                t.uncommit();
            }
            return Err(e).context("vkQueueSubmit (copy)");
        }
        s.in_flight = true;
        // The compositor waits the acquire point itself.
        if point.is_some() {
            return Ok(Copied::Ready(point));
        }
        // SAFETY: the fence belongs to the submit above.
        match unsafe { d.wait_for_fences(&[s.fence], true, COPY_WAIT_NS) } {
            Ok(()) => {
                s.in_flight = false;
                Ok(Copied::Ready(None))
            }
            Err(vk::Result::TIMEOUT) => Ok(Copied::Late),
            Err(e) => Err(e).context("vkWaitForFences (copy)"),
        }
    }
}

/// One colour subresource at array `layer`: the whole picture, every plane.
fn subresource(layer: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .base_array_layer(layer)
        .layer_count(1)
}

fn offset(x: u32, y: u32) -> vk::Offset3D {
    vk::Offset3D {
        x: x as i32,
        y: y as i32,
        z: 0,
    }
}

fn extent(width: u32, height: u32) -> vk::Extent3D {
    vk::Extent3D {
        width,
        height,
        depth: 1,
    }
}

/// A layout transition with no queue-family transfer.
fn layout_barrier(
    image: vk::Image,
    range: vk::ImageSubresourceRange,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .image(image)
        .subresource_range(range)
        .old_layout(from)
        .new_layout(to)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
}

impl Drop for ExportRing {
    fn drop(&mut self) {
        let d = &self.device;
        // SAFETY: each in-flight fence is waited before its image and command buffer go;
        // the compositor keeps its own reference to a dma-buf it still shows.
        unsafe {
            for s in &self.slots {
                if s.in_flight && s.fence != vk::Fence::null() {
                    let _ = d.wait_for_fences(&[s.fence], true, COPY_WAIT_NS);
                }
            }
            if let Some(mut il) = self.interleave.take() {
                il.destroy(d);
            }
            for s in self.slots.drain(..) {
                if s.fence != vk::Fence::null() {
                    d.destroy_fence(s.fence, None);
                }
                if let Some(t) = s.sync {
                    t.destroy(d);
                }
                d.destroy_image(s.image, None);
                d.free_memory(s.memory, None);
            }
            d.destroy_command_pool(self.pool, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the two 4:2:0 layouts the compositor can take map to a fourcc.
    #[test]
    fn the_ring_copies_nv12_and_p010_only() {
        let nv12 = RawVkFormat(vk::Format::G8_B8R8_2PLANE_420_UNORM.as_raw());
        let p010 = RawVkFormat(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16.as_raw());
        let yuv444 = RawVkFormat(vk::Format::G8_B8_R8_3PLANE_444_UNORM.as_raw());
        assert_eq!(fourcc_for(nv12).map(|f| f.0), Some(DRM_FORMAT_NV12));
        assert_eq!(fourcc_for(p010).map(|f| f.0), Some(DRM_FORMAT_P010));
        assert_eq!(fourcc_for(yuv444), None);
    }

    /// Ring keys never collide with a VAAPI pool key (high half = pool generation), with the
    /// other kind of ring, or across ring generations (`key_base` = kind | generation << 8).
    #[test]
    fn ring_keys_live_in_their_own_namespace() {
        let key = |kind: u64, generation: u64, slot: u64| kind | (generation << 8) | slot;
        assert_ne!(key(KEY_PICTURES, 1, 0), key(KEY_PICTURES, 2, 0));
        assert_ne!(key(KEY_PICTURES, 1, 0), key(KEY_PICTURES, 1, 1));
        assert_ne!(key(KEY_PICTURES, 1, 0), key(KEY_OVERLAY, 1, 0));
        assert_eq!(
            (7u64 << 32 | 3) & (KEY_PICTURES | KEY_OVERLAY),
            0,
            "a VAAPI pool key never sets bit 63 or 62"
        );
    }

    /// The chroma pass writes the chroma plane's two-channel format at the ring's depth, from
    /// SPIR-V that parses.
    #[test]
    fn the_chroma_pass_matches_the_ring_depth() {
        let stage = |f| chroma_stage(f).map(|(c, spv)| (c, spv.len() % 4 == 0 && spv.len() > 20));
        assert_eq!(
            stage(vk::Format::G8_B8R8_2PLANE_420_UNORM),
            Some((vk::Format::R8G8_UNORM, true))
        );
        assert_eq!(
            stage(vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16),
            Some((vk::Format::R16G16_UNORM, true))
        );
        assert_eq!(stage(vk::Format::B8G8R8A8_UNORM), None);
        for (_, spv) in [
            vk::Format::G8_B8R8_2PLANE_420_UNORM,
            vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
        ]
        .map(|f| chroma_stage(f).expect("mapped"))
        {
            assert!(ash::util::read_spv(&mut std::io::Cursor::new(spv)).is_ok());
        }
    }

    /// The overlay's formats map to the DRM codes whose byte order they share.
    #[test]
    fn overlay_formats_name_their_drm_codes() {
        assert_eq!(
            overlay_fourcc(vk::Format::B8G8R8A8_UNORM),
            Some(0x3432_5241)
        );
        assert_eq!(
            overlay_fourcc(vk::Format::R8G8B8A8_UNORM),
            Some(0x3432_4241)
        );
        assert_eq!(
            overlay_fourcc(vk::Format::R16G16B16A16_SFLOAT),
            Some(0x4834_4241)
        );
        assert_eq!(overlay_fourcc(vk::Format::R8_UNORM), None);
        assert_eq!(planes_of(vk::Format::B8G8R8A8_UNORM), 1);
        assert_eq!(planes_of(vk::Format::G8_B8R8_2PLANE_420_UNORM), 2);
    }
}
