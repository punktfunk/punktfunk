//! PyroWave client decode on the presenter's VkDevice: decode + CSC + present on one
//! device, no interop (design/pyrowave-codec-plan.md). One AU is one self-delimiting
//! packet: `push_packet` → ready → `decode_gpu_buffer` into our command buffer,
//! submitted under [`QueueLock`], fence-waited.
//!
//! Output is three STORAGE planes (Y full-res; Cb/Cr half-res, or full-res on 4:4:4;
//! R8, or R16 UNORM at 10-bit). Encoder RG packing is invalid here (`pyrowave.h`).
//! Colour comes from negotiated `ColorInfo`; the bitstream has no VUI. A plane-set
//! ring keeps decode from overwriting a set the presenter still samples.
//!
//! pyrowave 0.4.0 needs instance/device create-infos alive for the shared device.
//! The presenter does not pin its originals, so [`Hold`] reconstructs content-equivalent
//! ones from [`VulkanDecodeDevice`]. Pointer identity is not required.
//!
//! The decoder object is fixed-size. [`au_dims`] sniffs each frame's sequence header
//! and [`PyroWaveDecoder::reconfigure`] rebuilds decoder + plane ring in place; device,
//! command pool and pinned create-infos survive. A frame holds its ring ([`PlaneRing`]);
//! a superseded ring is also retired for consumers that sample without holding the frame
//! (see [`RETIRE_HANDOVERS`]).
//!
//! Decoder invariants every method's SAFETY leans on: `device` is the presenter's live
//! device, which outlives the decoder; `pw_dev`, `pw_dec`, the rings, `cmd` and `fence`
//! are this decoder's own; the shared queue is touched only under `queue_lock`.

use crate::video_color::ColorDesc;
use crate::video_vk::{QueueLock, VulkanDecodeDevice};
use anyhow::{bail, Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use pyrowave_sys as pw;
use std::ffi::{c_char, c_void, CString};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Decode writes slot N while the presenter may still sample N-1/N-2. Same-queue FIFO
/// already serializes writes vs. reads per slot; 4 covers ≤ 1–2 frames of present latency.
const RING: usize = 4;

/// Handovers after a resize before a retired ring's images may be destroyed. The
/// pump→presenter channel (depth 2, newest-wins) can still hold an old-ring frame, and
/// the presenter binds views only inside `present`. Eight new-ring frames displace every
/// old one; `queue_wait_idle` then covers submitted GPU work.
const RETIRE_HANDOVERS: u32 = 8;
/// Floor while `present` is stalled on an occluded swapchain acquire and frames still flow.
const RETIRE_MIN_AGE: Duration = Duration::from_millis(250);

fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

/// Sequence header at the start of `bytes` (pyrowave_common.hpp): 8 bytes, two LE u32s —
/// word 0 = `width_minus_1:14 | height_minus_1:14 | sequence:3 | extended:1`, word 1 =
/// `total_blocks:24 | code:2 | …`. The `extended` bit distinguishes a START-OF-FRAME
/// header from a regular `BitstreamHeader` (a wavelet block).
fn seq_header_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 8 {
        return None;
    }
    let w0 = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let w1 = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if w0 >> 31 == 0 {
        return None;
    }
    if (w1 >> 24) & 0x3 != 0 {
        return None; // not START_OF_FRAME
    }
    Some(((w0 & 0x3FFF) + 1, ((w0 >> 14) & 0x3FFF) + 1))
}

/// Frame dimensions this AU announces, or `None` when the sequence header rode a lost
/// shard. One header per frame, at bitstream byte 0: start of an unaligned AU, or start
/// of the first window body in a chunk-aligned AU (4-byte prefix `used:u16 | kind:u16`;
/// PACKED and FRAG_FIRST both begin with the frame's first packet).
fn au_dims(au: &[u8], aligned: bool, wire_window: usize) -> Option<(u32, u32)> {
    if !aligned {
        return seq_header_dims(au);
    }
    let win = &au[..au.len().min(wire_window)];
    if win.len() < 4 {
        return None;
    }
    let used = u16::from_le_bytes([win[0], win[1]]) as usize;
    let kind = u16::from_le_bytes([win[2], win[3]]);
    if used == 0 || 4 + used > win.len() {
        return None;
    }
    // PACKED (0) and FRAG_FIRST (1) start at the frame's first packet; CONT/LAST here
    // means the first window was lost.
    if kind > 1 {
        return None;
    }
    seq_header_dims(&win[4..4 + used])
}

/// Content-equivalent reconstruction of the presenter device's create-infos, pinned for
/// the `pyrowave_device` lifetime. Heap boxes: moving `Hold` moves only pointers.
struct Hold {
    _inst_ext_names: Vec<CString>,
    _inst_ext_ptrs: Vec<*const c_char>,
    _dev_ext_names: Vec<CString>,
    _dev_ext_ptrs: Vec<*const c_char>,
    _app_info: Box<vk::ApplicationInfo<'static>>,
    instance_ci: Box<vk::InstanceCreateInfo<'static>>,
    _queue_prio: Box<[f32; 1]>,
    _queue_cis: Vec<vk::DeviceQueueCreateInfo<'static>>,
    _feat2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v11: Box<vk::PhysicalDeviceVulkan11Features<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

impl Hold {
    fn build(vkd: &VulkanDecodeDevice) -> Hold {
        let inst_ext_names = vkd.instance_extensions.clone();
        let inst_ext_ptrs: Vec<*const c_char> = inst_ext_names.iter().map(|c| c.as_ptr()).collect();
        let dev_ext_names = vkd.device_extensions.clone();
        let dev_ext_ptrs: Vec<*const c_char> = dev_ext_names.iter().map(|c| c.as_ptr()).collect();

        let mut app_info =
            Box::new(vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3));
        let mut instance_ci = Box::new(vk::InstanceCreateInfo::default());
        instance_ci.p_application_info = &mut *app_info;
        instance_ci.enabled_extension_count = inst_ext_ptrs.len() as u32;
        instance_ci.pp_enabled_extension_names = if inst_ext_ptrs.is_empty() {
            std::ptr::null()
        } else {
            inst_ext_ptrs.as_ptr()
        };

        let queue_prio = Box::new([1.0f32]);
        let mut queue_cis: Vec<vk::DeviceQueueCreateInfo<'static>> = vkd
            .queue_families
            .iter()
            .map(|&fam| {
                let mut ci = vk::DeviceQueueCreateInfo::default().queue_family_index(fam);
                ci.queue_count = 1;
                ci
            })
            .collect();
        for ci in &mut queue_cis {
            ci.p_queue_priorities = queue_prio.as_ptr();
        }

        // Facts the presenter enabled at device create; pyrowave keys its paths off these.
        let mut feat2 = Box::new(vk::PhysicalDeviceFeatures2::default());
        feat2.features.shader_int16 = vkd.f_shader_int16 as u32;
        let mut v11 = Box::new(
            vk::PhysicalDeviceVulkan11Features::default()
                .sampler_ycbcr_conversion(vkd.f_sampler_ycbcr),
        );
        let mut v12 = Box::new(
            vk::PhysicalDeviceVulkan12Features::default()
                .timeline_semaphore(vkd.f_timeline_semaphore)
                .storage_buffer8_bit_access(vkd.f_storage_buffer8)
                .shader_float16(vkd.f_shader_float16),
        );
        let mut v13 = Box::new(
            vk::PhysicalDeviceVulkan13Features::default()
                .synchronization2(vkd.f_synchronization2)
                .subgroup_size_control(vkd.f_subgroup_size_control)
                .compute_full_subgroups(vkd.f_compute_full_subgroups),
        );
        feat2.p_next = &mut *v11 as *mut _ as *mut c_void;
        v11.p_next = &mut *v12 as *mut _ as *mut c_void;
        v12.p_next = &mut *v13 as *mut _ as *mut c_void;

        let mut device_ci = Box::new(vk::DeviceCreateInfo::default());
        device_ci.p_next = &*feat2 as *const _ as *const c_void;
        device_ci.queue_create_info_count = queue_cis.len() as u32;
        device_ci.p_queue_create_infos = queue_cis.as_ptr();
        device_ci.enabled_extension_count = dev_ext_ptrs.len() as u32;
        device_ci.pp_enabled_extension_names = dev_ext_ptrs.as_ptr();

        Hold {
            _inst_ext_names: inst_ext_names,
            _inst_ext_ptrs: inst_ext_ptrs,
            _dev_ext_names: dev_ext_names,
            _dev_ext_ptrs: dev_ext_ptrs,
            _app_info: app_info,
            instance_ci,
            _queue_prio: queue_prio,
            _queue_cis: queue_cis,
            _feat2: feat2,
            _v11: v11,
            _v12: v12,
            _v13: v13,
            device_ci,
        }
    }
}

/// Trampolines pyrowave calls around internal queue use. `userdata` is a raw pointer
/// to the [`QueueLock`] the decoder's Arc keeps alive.
unsafe extern "C" fn queue_lock_cb(ud: *mut c_void) {
    // SAFETY: `ud` is the QueueLock the decoder's Arc pins; pyrowave only calls this
    // while the decoder (and thus the Arc) lives.
    unsafe { (*(ud as *const QueueLock)).lock() }
}
unsafe extern "C" fn queue_unlock_cb(ud: *mut c_void) {
    // SAFETY: as above.
    unsafe { (*(ud as *const QueueLock)).unlock() }
}

/// Fence-waited decode output on the presenter's device, GENERAL layout.
pub struct PyroWavePlanarFrame {
    /// Keeps the planes alive past a resize or the decoder's end. A consumer that may
    /// sample the frame again holds the frame.
    pub hold: Arc<PlaneRing>,
    /// Raw `VkImageView`s (Y, Cb, Cr) for planar CSC sampling.
    pub views: [u64; 3],
    /// Raw `VkImage` of the Y plane, for the native lane's copy.
    pub luma: u64,
    /// Cb/Cr at full resolution rather than half.
    pub chroma444: bool,
    pub width: u32,
    pub height: u32,
    pub color: ColorDesc,
    /// R16 planes of MSB-packed 10-bit codes, else R8. Follows the session's depth, not its
    /// transfer: a Linux host sends 10-bit SDR.
    pub ten_bit: bool,
    /// Independently decodable — always a clean re-anchor.
    pub keyframe: bool,
}

struct PlaneSet {
    imgs: [vk::Image; 3],
    mems: [vk::DeviceMemory; 3],
    views: [vk::ImageView; 3],
}

/// A plane ring's images, views and memory. The decoder holds one and so does every
/// frame cut from it; the last holder frees them, so each holder must outlive its own
/// GPU use of the planes.
pub struct PlaneRing {
    device: ash::Device,
    sets: Vec<PlaneSet>,
}

impl Drop for PlaneRing {
    fn drop(&mut self) {
        // SAFETY: made on the live `device`; no holder is left to use them.
        unsafe { destroy_sets(&self.device, &self.sets) };
    }
}

/// Plane ring superseded by a mid-stream resize, awaiting release
/// (see [`RETIRE_HANDOVERS`]).
struct RetiredRing {
    _ring: Arc<PlaneRing>,
    handed_over: u32,
    retired_at: Instant,
}

/// One decode-output plane: STORAGE (decode writes) + SAMPLED (presenter CSC), in
/// device-local memory. No such memory type is an error.
///
/// # Safety
///
/// `device` is live, and `mem_props` are its physical device's.
unsafe fn make_plane(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    w: u32,
    h: u32,
    fmt: vk::Format,
) -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
    // SAFETY: fn contract; the create-info is a local that outlives the call.
    let img = unsafe {
        device.create_image(
            &vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(fmt)
                .extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED)
                .initial_layout(vk::ImageLayout::UNDEFINED),
            None,
        )
    }?;
    let mut mem = vk::DeviceMemory::null();
    let view = (|| -> Result<vk::ImageView> {
        // SAFETY: `img` was created above on the live `device`.
        let req = unsafe { device.get_image_memory_requirements(img) };
        let ti = (0..mem_props.memory_type_count)
            .find(|&i| {
                (req.memory_type_bits & (1 << i)) != 0
                    && mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .context("no device-local memory type for a PyroWave plane")?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(ti);
        // SAFETY: fn contract; `alloc` is a local.
        mem = unsafe { device.allocate_memory(&alloc, None) }?;
        // SAFETY: `img` and `mem` were created above; neither is bound yet.
        unsafe { device.bind_image_memory(img, mem, 0) }?;
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let info = vk::ImageViewCreateInfo::default()
            .image(img)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(fmt)
            .subresource_range(range);
        // SAFETY: fn contract; `img` is bound above; `info` is a local.
        Ok(unsafe { device.create_image_view(&info, None) }?)
    })();
    if view.is_err() {
        // SAFETY: both were created above and never used; freeing a null `mem` is a no-op.
        unsafe {
            device.destroy_image(img, None);
            device.free_memory(mem, None);
        }
    }
    Ok((img, mem, view?))
}

/// Null handles are a no-op, so a partly built set unwinds here too.
///
/// # Safety
///
/// Every handle in `sets` is null or was created on the live `device`, and the GPU is
/// done with all of them.
unsafe fn destroy_sets(device: &ash::Device, sets: &[PlaneSet]) {
    // SAFETY: fn contract.
    unsafe {
        for set in sets {
            for v in set.views {
                device.destroy_image_view(v, None);
            }
            for i in set.imgs {
                device.destroy_image(i, None);
            }
            for m in set.mems {
                device.free_memory(m, None);
            }
        }
    }
}

/// Fresh [`RING`]-deep plane ring. Cleans up a partial ring on failure; the caller
/// keeps whatever it was using.
///
/// # Safety
///
/// As [`make_plane`].
unsafe fn build_ring(
    device: &ash::Device,
    mem_props: &vk::PhysicalDeviceMemoryProperties,
    width: u32,
    height: u32,
    chroma444: bool,
    fmt: vk::Format,
) -> Result<Arc<PlaneRing>> {
    // Presenter planar CSC samples with normalized UVs; chroma resolution is transparent.
    let (cw, ch) = if chroma444 {
        (width, height)
    } else {
        (width / 2, height / 2)
    };
    let mut ring: Vec<PlaneSet> = Vec::with_capacity(RING);
    for _ in 0..RING {
        // Null until built, so `destroy_sets` unwinds a partial set with the rest.
        ring.push(PlaneSet {
            imgs: [vk::Image::null(); 3],
            mems: [vk::DeviceMemory::null(); 3],
            views: [vk::ImageView::null(); 3],
        });
        let set = ring.last_mut().expect("pushed above");
        for (i, (w, h)) in [(width, height), (cw, ch), (cw, ch)]
            .into_iter()
            .enumerate()
        {
            // SAFETY: fn contract.
            match unsafe { make_plane(device, mem_props, w, h, fmt) } {
                Ok(p) => (set.imgs[i], set.mems[i], set.views[i]) = p,
                Err(e) => {
                    // SAFETY: every handle in `ring` is null or made above, never used.
                    unsafe { destroy_sets(device, &ring) };
                    return Err(e);
                }
            }
        }
    }
    Ok(Arc::new(PlaneRing {
        device: device.clone(),
        sets: ring,
    }))
}

pub struct PyroWaveDecoder {
    // ash wrappers over the presenter's raw handles (not owned). Drop destroys only
    // what this struct created; the presenter outlives the decoder.
    device: ash::Device,
    queue: vk::Queue,
    _hold: Box<Hold>,
    queue_lock: Arc<QueueLock>,
    pw_dev: pw::pyrowave_device,
    pw_dec: pw::pyrowave_decoder,
    ring: Arc<PlaneRing>,
    /// Per slot: written once. First use transitions from UNDEFINED, later ones
    /// GENERAL→GENERAL.
    initialized: [bool; RING],
    retired: Vec<RetiredRing>,
    next: usize,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    width: u32,
    height: u32,
    /// Session-fixed ([`Welcome::chroma_format`]). A seq-header mismatch fails upstream,
    /// never silently.
    chroma444: bool,
    /// Negotiated [`Welcome::color`]. The wavelet bitstream has no VUI.
    color: ColorDesc,
    /// Depth ≥10: `R16_UNORM` planes carry P010-style studio codes; the presenter
    /// samples them as depth-10 MSB-packed rows.
    hdr16: bool,
    /// Shard payload: parse-window size for chunk-aligned AUs. Each window holds whole
    /// self-delimiting packets, zero-padded.
    wire_window: usize,
    split: DecodeSplit,
}

/// Where the last decode's wall time went, in microseconds.
#[derive(Clone, Copy, Default, Debug)]
pub struct DecodeSplit {
    /// Packets into the decoder (CPU).
    pub parse_us: u32,
    /// Command recording, payload upload included (CPU).
    pub record_us: u32,
    /// Submit to fence: the GPU's own time plus whatever queued ahead of it.
    pub gpu_us: u32,
}

// SAFETY: used only from the single decode thread; the shared-queue accesses go through
// QueueLock, matching every other Vulkan backend's threading contract.
unsafe impl Send for PyroWaveDecoder {}

impl PyroWaveDecoder {
    pub fn new(
        vkd: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
        chroma444: bool,
        color: ColorDesc,
        hdr16: bool,
    ) -> Result<PyroWaveDecoder> {
        if !vkd.pyrowave_decode {
            bail!("presenter device lacks the PyroWave compute feature set");
        }
        if !chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // SAFETY: `vkd` handles are the presenter's live instance/device (it outlives
        // the decoder). `Hold` pins reconstructed create-infos for the pyrowave device.
        unsafe { Self::new_inner(vkd, width, height, shard_payload, chroma444, color, hdr16) }
    }

    /// # Safety
    ///
    /// `vkd`'s handles are the presenter's live instance and device, and they outlive
    /// the decoder.
    unsafe fn new_inner(
        vkd: &VulkanDecodeDevice,
        width: u32,
        height: u32,
        shard_payload: usize,
        chroma444: bool,
        color: ColorDesc,
        hdr16: bool,
    ) -> Result<PyroWaveDecoder> {
        // SAFETY: fn contract: `get_instance_proc_addr` is the loader's entry point for the
        // live instance and device, so each table loads from live handles.
        let (static_fn, instance, device) = unsafe {
            let static_fn = ash::StaticFn {
                get_instance_proc_addr: std::mem::transmute::<usize, vk::PFN_vkGetInstanceProcAddr>(
                    vkd.get_instance_proc_addr,
                ),
            };
            let instance_h = vk::Instance::from_raw(vkd.instance as u64);
            let device_h = vk::Device::from_raw(vkd.device as u64);
            let instance = ash::Instance::load(&static_fn, instance_h);
            let device = ash::Device::load(instance.fp_v1_0(), device_h);
            (static_fn, instance, device)
        };
        // SAFETY: fn contract; the presenter created queue 0 of its graphics family.
        let queue = unsafe { device.get_device_queue(vkd.graphics_qf, 0) };

        let hold = Box::new(Hold::build(vkd));
        let queue_lock = vkd.queue_lock.clone();
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: queue.as_raw() as usize as pw::VkQueue,
            familyIndex: vkd.graphics_qf,
            index: 0,
        };
        let create = pw::pyrowave_device_create_info {
            // SAFETY: re-labels the loader entry point between ash's and bindgen's
            // identical C function-pointer types.
            GetInstanceProcAddr: Some(unsafe {
                std::mem::transmute::<
                    vk::PFN_vkGetInstanceProcAddr,
                    unsafe extern "C" fn(pw::VkInstance, *const c_char) -> pw::PFN_vkVoidFunction,
                >(static_fn.get_instance_proc_addr)
            }),
            instance: vkd.instance as pw::VkInstance,
            physical_device: vkd.physical_device as pw::VkPhysicalDevice,
            device: vkd.device as pw::VkDevice,
            instance_create_info: &*hold.instance_ci as *const vk::InstanceCreateInfo
                as *const pw::VkInstanceCreateInfo,
            device_create_info: &*hold.device_ci as *const vk::DeviceCreateInfo
                as *const pw::VkDeviceCreateInfo,
            queue_info: &mut queue_info,
            queue_info_count: 1,
            // Presenter, Skia and every decode lane serialize on this same lock.
            queue_lock_callback: Some(queue_lock_cb),
            queue_unlock_callback: Some(queue_unlock_cb),
            userdata: Arc::as_ptr(&queue_lock) as *mut c_void,
        };
        let mut pw_dev: pw::pyrowave_device = std::ptr::null_mut();
        // SAFETY: fn contract for the handles; `create` points at `hold`'s pinned
        // create-infos, `queue_info` and the `queue_lock` the decoder keeps alive.
        let created = unsafe { pw::pyrowave_create_device(&create, &mut pw_dev) };
        pw_check(created, "create_device (shared presenter device)")?;
        // SAFETY: `pw_dev` was created above.
        let _ = unsafe {
            pw::pyrowave_device_set_queue_type(pw_dev, pw::VkQueueFlagBits_VK_QUEUE_COMPUTE_BIT)
        };

        let dinfo = pw::pyrowave_decoder_create_info {
            device: pw_dev,
            width: width as i32,
            height: height as i32,
            chroma: if chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
            // fragment-iDWT is Mali/Adreno only.
            fragment_path: false,
        };
        let mut pw_dec: pw::pyrowave_decoder = std::ptr::null_mut();
        // SAFETY: `pw_dev` was created above; `dinfo` and `pw_dec` are locals.
        let created = unsafe { pw::pyrowave_decoder_create(&dinfo, &mut pw_dec) };
        if let Err(e) = pw_check(created, "decoder_create") {
            // SAFETY: `pw_dev` has no decoder and no work.
            unsafe { pw::pyrowave_device_destroy(pw_dev) };
            return Err(e);
        }
        // Only the pyrowave objects exist yet: an early error destroys both.
        let destroy_pw = || {
            // SAFETY: `pw_dec` and `pw_dev` were created above and have recorded nothing.
            unsafe {
                pw::pyrowave_decoder_destroy(pw_dec);
                pw::pyrowave_device_destroy(pw_dev);
            }
        };

        let pdev = vk::PhysicalDevice::from_raw(vkd.physical_device as u64);
        // SAFETY: fn contract: `pdev` is the live instance's physical device.
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        // R16_UNORM STORAGE_IMAGE is optional in Vulkan. Probe so a miss is a clear
        // error instead of a validation failure.
        let plane_fmt = if hdr16 {
            // SAFETY: as above.
            let props = unsafe {
                instance.get_physical_device_format_properties(pdev, vk::Format::R16_UNORM)
            };
            if !props
                .optimal_tiling_features
                .contains(vk::FormatFeatureFlags::STORAGE_IMAGE)
            {
                destroy_pw();
                bail!("this GPU lacks R16_UNORM STORAGE_IMAGE — cannot decode a 10-bit PyroWave session");
            }
            vk::Format::R16_UNORM
        } else {
            vk::Format::R8_UNORM
        };
        // SAFETY: fn contract; `mem_props` are the device's physical device's.
        let ring =
            match unsafe { build_ring(&device, &mem_props, width, height, chroma444, plane_fmt) } {
                Ok(r) => r,
                Err(e) => {
                    destroy_pw();
                    return Err(e);
                }
            };

        // SAFETY: fn contract; every create-info is a local that outlives its call.
        let (cmd_pool, cmd, fence) = unsafe {
            let cmd_pool = device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(vkd.graphics_qf)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let cmd = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
            (cmd_pool, cmd, fence)
        };

        tracing::info!(
            width,
            height,
            "PyroWave decoder open on the presenter's device (compute iDWT, BT.709 limited)"
        );
        Ok(PyroWaveDecoder {
            device,
            queue,
            _hold: hold,
            queue_lock,
            pw_dev,
            pw_dec,
            ring,
            initialized: [false; RING],
            retired: Vec::new(),
            next: 0,
            cmd_pool,
            cmd,
            fence,
            mem_props,
            width,
            height,
            chroma444,
            color,
            hdr16,
            wire_window: shard_payload.max(64),
            split: DecodeSplit::default(),
        })
    }

    /// Rebuild decoder + plane ring at new dimensions. Device, command pool, fence and
    /// pinned create-infos are dimension-independent and survive. Build-new-before-drop-old:
    /// failure leaves the current decoder untouched. The old ring is retired, not
    /// destroyed (see [`RETIRE_HANDOVERS`]).
    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        if !self.chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (resize to {width}x{height})");
        }
        let dinfo = pw::pyrowave_decoder_create_info {
            device: self.pw_dev,
            width: width as i32,
            height: height as i32,
            // Chroma is session-fixed; a resize never changes it.
            chroma: if self.chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
            fragment_path: false,
        };
        let mut new_dec: pw::pyrowave_decoder = std::ptr::null_mut();
        // SAFETY: `pw_dev` is this decoder's live device; `dinfo`/`new_dec` are locals.
        let created = unsafe { pw::pyrowave_decoder_create(&dinfo, &mut new_dec) };
        pw_check(created, "decoder_create (mid-stream resize)")?;
        let fmt = if self.hdr16 {
            vk::Format::R16_UNORM
        } else {
            vk::Format::R8_UNORM
        };
        // SAFETY: `device` is the presenter's live device; `mem_props` are its GPU's.
        let new_ring = match unsafe {
            build_ring(
                &self.device,
                &self.mem_props,
                width,
                height,
                self.chroma444,
                fmt,
            )
        } {
            Ok(r) => r,
            Err(e) => {
                // SAFETY: `new_dec` was created above and has decoded nothing.
                unsafe { pw::pyrowave_decoder_destroy(new_dec) };
                return Err(e).context("plane ring (mid-stream resize)");
            }
        };
        // SAFETY: decode is fence-synchronous, so no work of the old decoder is in
        // flight and it can go now; only the plane images wait (retired).
        unsafe { pw::pyrowave_decoder_destroy(self.pw_dec) };
        self.pw_dec = new_dec;
        let old = std::mem::replace(&mut self.ring, new_ring);
        self.retired.push(RetiredRing {
            _ring: old,
            handed_over: 0,
            retired_at: Instant::now(),
        });
        self.initialized = [false; RING];
        self.next = 0;
        tracing::info!(
            from = %format_args!("{}x{}", self.width, self.height),
            to = %format_args!("{width}x{height}"),
            "PyroWave decoder rebuilt for mid-stream resize"
        );
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// Let go of retired rings that have enough new-ring handovers and have aged past
    /// [`RETIRE_MIN_AGE`]. Queue idle bounds any still-submitted sampling by a consumer
    /// that holds no frame; one that does keeps its ring.
    fn reap_retired(&mut self) {
        let ripe = |r: &RetiredRing| {
            r.handed_over >= RETIRE_HANDOVERS && r.retired_at.elapsed() >= RETIRE_MIN_AGE
        };
        if !self.retired.iter().any(ripe) {
            return;
        }
        {
            let _guard = self.queue_lock.guard();
            // SAFETY: `queue` external sync is `queue_lock`, held above.
            let _ = unsafe { self.device.queue_wait_idle(self.queue) };
        }
        self.retired.retain(|r| !ripe(r));
    }

    /// Push one packet into the decoder.
    fn push_packet(&mut self, packet: &[u8], what: &str) -> Result<()> {
        // SAFETY: `pw_dec` is this decoder's live decoder; `packet` is a live slice for
        // the call.
        let r = unsafe {
            pw::pyrowave_decoder_push_packet(
                self.pw_dec,
                packet.as_ptr() as *const c_void,
                packet.len(),
            )
        };
        pw_check(r, what)
    }

    /// One framed shard window: 4-byte prefix (`used:u16`, `kind:u16`) then whole packets
    /// (PACKED) or one fragment of an oversized packet (FRAG chain). A lost shard is a
    /// zeroed window (`used = 0`) — skip it and break any fragment chain it interrupts.
    fn push_window(&mut self, win: &[u8], frag: &mut Vec<u8>) -> Result<()> {
        if win.len() < 4 {
            return Ok(());
        }
        let used = u16::from_le_bytes([win[0], win[1]]) as usize;
        let kind = u16::from_le_bytes([win[2], win[3]]);
        if used == 0 || 4 + used > win.len() {
            frag.clear(); // missing / garbage window — drop any chain in progress
            return Ok(());
        }
        let body = &win[4..4 + used];
        match kind {
            0 => {
                frag.clear();
                self.push_packet(body, "push_packet")
            }
            1 => {
                frag.clear();
                frag.extend_from_slice(body);
                Ok(())
            }
            2 => {
                if !frag.is_empty() {
                    frag.extend_from_slice(body);
                }
                Ok(())
            }
            3 => {
                if !frag.is_empty() {
                    frag.extend_from_slice(body);
                    let r = self.push_packet(frag, "push_packet (fragmented)");
                    frag.clear();
                    return r;
                }
                Ok(())
            }
            _ => {
                frag.clear();
                Ok(())
            }
        }
    }

    /// The last [`Self::decode_frame`]'s wall split.
    pub fn last_split(&self) -> DecodeSplit {
        self.split
    }

    /// The vendored decoder's GPU time per stage, one line each. `reset` starts a new window.
    pub fn gpu_stage_report(&self, reset: bool) -> Vec<String> {
        unsafe extern "C" fn collect(ud: *mut c_void, msg: *const c_char) {
            // SAFETY: `ud` is the `Vec` below, alive for the call; `msg` is a C string.
            unsafe {
                (*(ud as *mut Vec<String>)).push(
                    std::ffi::CStr::from_ptr(msg)
                        .to_string_lossy()
                        .trim()
                        .to_owned(),
                );
            }
        }
        let mut lines: Vec<String> = Vec::new();
        // SAFETY: `pw_dev` is this decoder's; the callback only runs inside the call.
        unsafe {
            pw::pyrowave_device_report_performance_stats(
                self.pw_dev,
                Some(collect),
                &mut lines as *mut Vec<String> as *mut c_void,
                reset,
            );
        }
        lines
    }

    /// One AU in → one frame out. `aligned`: shard-window chunked (each `wire_window`
    /// holds whole self-delimiting packets, zero-padded). `complete`: every shard arrived;
    /// a partial still decodes — missing blocks are localized blur for this frame only.
    pub fn decode_frame(
        &mut self,
        au: &[u8],
        aligned: bool,
        complete: bool,
    ) -> Result<Option<PyroWavePlanarFrame>> {
        // The AU's sequence header announces a host mode switch; rebuild first — a size
        // mismatch hard-errors upstream. A lost first shard sniffs `None` and decodes at
        // the current size; the next complete frame carries the header again.
        if let Some(dims) = au_dims(au, aligned, self.wire_window) {
            if dims != (self.width, self.height) {
                self.reconfigure(dims.0, dims.1)?;
            }
        }
        let parse_started = Instant::now();
        let mut push_err: Option<anyhow::Error> = None;
        if aligned {
            let mut frag: Vec<u8> = Vec::new();
            for win in au.chunks(self.wire_window) {
                if let Err(e) = self.push_window(win, &mut frag) {
                    push_err = Some(e);
                    break;
                }
            }
        } else if let Err(e) = self.push_packet(au, "push_packet") {
            push_err = Some(e);
        }
        self.split.parse_us = parse_started.elapsed().as_micros() as u32;
        if let Some(e) = push_err {
            // A partial straddling a resize can carry blocks the (possibly wrong-size)
            // decoder rejects — one lost frame, not a broken session. A complete frame
            // that fails to parse is corruption: propagate.
            if complete {
                return Err(e);
            }
            tracing::debug!(error = %format!("{e:#}"), "partial AU rejected — frame dropped");
            return Ok(None);
        }
        // A complete AU that isn't ready is a stale/duplicate (sequence rewind) — skip.
        // A partial still decodes: missing wavelet blocks reconstruct as zeros (blur
        // for this frame only).
        // SAFETY: `pw_dec` is this decoder's live decoder.
        if complete && !unsafe { pw::pyrowave_decoder_decode_is_ready(self.pw_dec, false) } {
            return Ok(None);
        }

        let slot = self.next;
        self.next = (self.next + 1) % RING;
        if let Err(e) = self.record_and_wait(slot) {
            // The buffer may still be recording or on the GPU, and the pump keeps this decoder:
            // idle the queue, then reset the buffer so the next frame can begin on it.
            let _guard = self.queue_lock.guard();
            // SAFETY: `queue_lock` is held; after the idle `cmd` (from a RESET_COMMAND_BUFFER
            // pool) is not pending, so it can be reset.
            unsafe {
                let _ = self.device.queue_wait_idle(self.queue);
                let _ = self
                    .device
                    .reset_command_buffer(self.cmd, vk::CommandBufferResetFlags::empty());
            }
            return Err(e);
        }
        self.initialized[slot] = true;

        for r in &mut self.retired {
            r.handed_over += 1;
        }
        self.reap_retired();

        let (w, h) = (self.width, self.height);
        let set = &self.ring.sets[slot];
        Ok(Some(PyroWavePlanarFrame {
            hold: self.ring.clone(),
            views: set.views.map(|v| v.as_raw()),
            luma: set.imgs[0].as_raw(),
            chroma444: self.chroma444,
            width: w,
            height: h,
            color: self.color,
            ten_bit: self.hdr16,
            keyframe: true,
        }))
    }

    /// Record `slot`'s decode, submit it, and wait for its fence. On `Err` the command buffer
    /// may be recording or pending; the caller idles the queue before reusing it.
    fn record_and_wait(&mut self, slot: usize) -> Result<()> {
        let record_started = Instant::now();
        let dev = self.device.clone();
        // SAFETY: `cmd` is this decoder's and idle: the last submit's fence was waited, or
        // a failed decode idled the queue and reset it.
        unsafe {
            dev.begin_command_buffer(
                self.cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        }?;
        let old_layout = if self.initialized[slot] {
            vk::ImageLayout::GENERAL
        } else {
            vk::ImageLayout::UNDEFINED
        };
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let to_write = |img| {
            vk::ImageMemoryBarrier2::default()
                // Order against the presenter's prior reads of this slot (same queue): CSC
                // sampling, or the native lane's chroma pass and luma copy.
                .src_stage_mask(
                    vk::PipelineStageFlags2::FRAGMENT_SHADER
                        | vk::PipelineStageFlags2::COMPUTE_SHADER
                        | vk::PipelineStageFlags2::COPY,
                )
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(img)
                .subresource_range(range)
        };
        let imgs = self.ring.sets[slot].imgs;
        let pre: Vec<_> = imgs.iter().map(|&i| to_write(i)).collect();
        // SAFETY: `cmd` is recording (begun above); `pre` names this decoder's ring images.
        unsafe {
            dev.cmd_pipeline_barrier2(
                self.cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&pre),
            )
        };

        // Declared format/extent must equal the ring image. pyrowave wraps our VkImage
        // and creates a storage view from `image_format`/`view_format`; R8 over R16_UNORM
        // addresses half the surface. Same for chroma extents: a 4:4:4 ring is full-res.
        let fmt = if self.hdr16 {
            pw::VkFormat_VK_FORMAT_R16_UNORM
        } else {
            pw::VkFormat_VK_FORMAT_R8_UNORM
        };
        let plane = |img: vk::Image, w: u32, h: u32| pw::pyrowave_image_view {
            image: img.as_raw() as usize as pw::VkImage,
            width: w,
            height: h,
            image_format: fmt,
            view_format: fmt,
            mip_level: 0,
            layer: 0,
            aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
            swizzle: pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
            layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
        };
        let (w, h) = (self.width, self.height);
        let (cw, ch) = if self.chroma444 {
            (w, h)
        } else {
            (w / 2, h / 2)
        };
        let buffers = pw::pyrowave_gpu_buffers {
            planes: [
                plane(imgs[0], w, h),
                plane(imgs[1], cw, ch),
                plane(imgs[2], cw, ch),
            ],
        };
        // SAFETY: `pw_dev`/`pw_dec` are this decoder's; `cmd` is recording on this thread
        // only and is unset again before return. `buffers` names the slot's ring images at
        // their created format and extent, in GENERAL after the barrier above.
        let dec_res = unsafe {
            pw::pyrowave_device_set_command_buffer(
                self.pw_dev,
                self.cmd.as_raw() as usize as pw::VkCommandBuffer,
            );
            let r = pw::pyrowave_decoder_decode_gpu_buffer(
                self.pw_dec,
                std::ptr::null(),
                std::ptr::null(),
                &buffers,
            );
            pw::pyrowave_device_set_command_buffer(self.pw_dev, std::ptr::null_mut());
            r
        };
        pw_check(dec_res, "decode_gpu_buffer")?;

        // Storage writes → presenter fragment sampling. Layout stays GENERAL: that is
        // what the planar CSC descriptors use.
        let to_read = |img| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
                .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .image(img)
                .subresource_range(range)
        };
        let post: Vec<_> = imgs.iter().map(|&i| to_read(i)).collect();
        // SAFETY: `cmd` is still recording; `post` names this decoder's ring images. The
        // fence is not pending: the last submit's wait finished, or the queue was idled.
        unsafe {
            dev.cmd_pipeline_barrier2(
                self.cmd,
                &vk::DependencyInfo::default().image_memory_barriers(&post),
            );
            dev.end_command_buffer(self.cmd)?;
            dev.reset_fences(&[self.fence])?;
        }
        self.split.record_us = record_started.elapsed().as_micros() as u32;
        let gpu_started = Instant::now();
        {
            let _guard = self.queue_lock.guard();
            let cmds = [self.cmd];
            // SAFETY: `queue` external sync is `queue_lock`, held above; `cmd` is executable
            // and `fence` unsignalled; the submit info and `cmds` are locals.
            unsafe {
                dev.queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default().command_buffers(&cmds)],
                    self.fence,
                )
            }?;
        }
        // SAFETY: `fence` is this decoder's, named by the submit above.
        unsafe { dev.wait_for_fences(&[self.fence], true, 5_000_000_000) }
            .context("pyrowave decode fence")?;
        self.split.gpu_us = gpu_started.elapsed().as_micros() as u32;
        Ok(())
    }
}

impl Drop for PyroWaveDecoder {
    fn drop(&mut self) {
        // SAFETY: owned handles on the presenter's device. Decode is fence-synchronous so
        // none of our work is in flight; the presenter may still sample the last slot —
        // idle the shared queue under the lock before destroying plane images.
        unsafe {
            {
                let _guard = self.queue_lock.guard();
                let _ = self.device.queue_wait_idle(self.queue);
            }
            pw::pyrowave_decoder_destroy(self.pw_dec);
            pw::pyrowave_device_destroy(self.pw_dev);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
            // `self.device`/instance are the presenter's — never destroyed here.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{au_dims, seq_header_dims};

    /// LE encoding of `BitstreamSequenceHeader` (pyrowave_common.hpp): word 0 =
    /// width_minus_1:14 | height_minus_1:14 | sequence:3 | extended:1; word 1 =
    /// total_blocks:24 | code:2 | chroma:1 | …
    fn seq_header(w: u32, h: u32, code: u32) -> [u8; 8] {
        let w0 = (w - 1) & 0x3FFF | ((h - 1) & 0x3FFF) << 14 | 1 << 31;
        let w1 = 0x1234 | code << 24; // arbitrary total_blocks
        let mut out = [0u8; 8];
        out[0..4].copy_from_slice(&w0.to_le_bytes());
        out[4..8].copy_from_slice(&w1.to_le_bytes());
        out
    }

    /// A regular `BitstreamHeader` (block packet): extended bit clear.
    fn block_header() -> [u8; 8] {
        let w0 = 0xBEEFu32 | 8 << 16; // ballot | payload_words=8, extended=0
        let w1 = 42u32 << 8; // block_index
        let mut out = [0u8; 8];
        out[0..4].copy_from_slice(&w0.to_le_bytes());
        out[4..8].copy_from_slice(&w1.to_le_bytes());
        out
    }

    fn window(body: &[u8], kind: u16, win: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(win);
        out.extend_from_slice(&(body.len() as u16).to_le_bytes());
        out.extend_from_slice(&kind.to_le_bytes());
        out.extend_from_slice(body);
        out.resize(win, 0);
        out
    }

    #[test]
    fn sniffs_dims_from_a_sequence_header() {
        assert_eq!(
            seq_header_dims(&seq_header(1920, 1080, 0)),
            Some((1920, 1080))
        );
        assert_eq!(
            seq_header_dims(&seq_header(1280, 720, 0)),
            Some((1280, 720))
        );
        // 14-bit fields carry up to 16384.
        assert_eq!(
            seq_header_dims(&seq_header(16384, 16384, 0)),
            Some((16384, 16384))
        );
    }

    #[test]
    fn rejects_non_sequence_headers() {
        assert_eq!(seq_header_dims(&block_header()), None);
        assert_eq!(seq_header_dims(&seq_header(1920, 1080, 1)), None); // not START_OF_FRAME
        assert_eq!(seq_header_dims(&seq_header(1920, 1080, 0)[..7]), None);
        assert_eq!(seq_header_dims(&[]), None);
    }

    #[test]
    fn unaligned_au_sniffs_at_byte_zero() {
        let mut au = seq_header(2560, 1440, 0).to_vec();
        au.extend_from_slice(&block_header());
        assert_eq!(au_dims(&au, false, 1404), Some((2560, 1440)));
    }

    #[test]
    fn aligned_au_sniffs_the_first_window_body() {
        const WIN: usize = 64;
        let mut body = seq_header(1280, 800, 0).to_vec();
        body.extend_from_slice(&block_header());
        let mut au = window(&body, 0, WIN);
        au.extend_from_slice(&window(&block_header(), 0, WIN));
        assert_eq!(au_dims(&au, true, WIN), Some((1280, 800)));
        // FRAG_FIRST also starts at the frame's first byte, so the header is still there.
        let frag = window(&body, 1, WIN);
        assert_eq!(au_dims(&frag, true, WIN), Some((1280, 800)));
    }

    #[test]
    fn lost_first_window_means_unknown_dims() {
        const WIN: usize = 64;
        let mut au = vec![0u8; WIN];
        au.extend_from_slice(&window(&seq_header(1280, 800, 0), 0, WIN));
        assert_eq!(au_dims(&au, true, WIN), None);
        // FRAG_CONT/LAST as the first window: its FIRST was in a lost prior AU.
        let cont = window(&block_header(), 2, WIN);
        assert_eq!(au_dims(&cont, true, WIN), None);
        // Used-length 0xFFFF must not read past the window.
        let mut garbage = vec![0u8; WIN];
        garbage[0] = 0xFF;
        garbage[1] = 0xFF;
        assert_eq!(au_dims(&garbage, true, WIN), None);
    }
}
