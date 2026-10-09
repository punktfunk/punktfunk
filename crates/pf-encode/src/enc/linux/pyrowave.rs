//! PyroWave host encoder: intra-only CDF 9/7 wavelet over a private Vulkan 1.3
//! compute device (`pyrowave-sys`). Every frame is a keyframe, so IDR/RFI recovery
//! is unused. Opt-in via `PUNKTFUNK_ENCODER=pyrowave`; no shipping client decodes
//! this until `CODEC_PYROWAVE` negotiation lands.
//!
//! `pyrowave_create_device` retains the original instance/device create-infos for
//! the device's lifetime — [`DeviceHold`] pins them. Frames enter as capture dmabufs
//! (DRM modifiers, cached per buffer) or CPU RGB; a CSC shader writes luma + interleaved
//! chroma planes that pyrowave samples via R/G view swizzles — `rgb2yuv*.comp`, picked by
//! chroma (4:2:0 / 4:4:4), depth (R8/RG8 or R16/RG16 `code << 6`), and colour (BT.709
//! limited or BT.2020/PQ). Ingest, CSC and encode record into one command buffer
//! (`pyrowave_device_set_command_buffer`).
//!
//! The AU is one pyrowave packet (boundary = buffer size), `keyframe = true`, through
//! the normal FEC/packetizer path. Evidence: `design/pyrowave-codec-plan.md`.
// `unsafe_op_in_unsafe_fn` off: this file is pyrowave-sys + ash calls. Clearing it
// means deleting markers with no caller contract, not wrapping each call.
#![allow(unsafe_op_in_unsafe_fn)]

// Every unsafe block in this module carries a `// SAFETY:` proof (parent module enforces it).

use super::vk_util::{
    color_range, import_failure_feeds_latch, import_rgb_dmabuf, imported_acquire_barrier,
    imported_release_barrier, make_host_buffer, make_plain_image, normalize_cpu_rgb, pixel_to_vk,
    reject_dmabuf, select_physical_device, ImportCache, ImportRejected,
};
use crate::pyrowave_ffi::{packetize, pw_check};
use crate::pyrowave_wire::{AuStream, FrameBudget};
use crate::{EncodedFrame, Encoder, EncoderCaps};
use anyhow::{bail, Context, Result};
use ash::vk;
use ash::vk::Handle as _;
use pf_frame::{CapturedFrame, FramePayload};
use pyrowave_sys as pw;
use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd};
use std::os::raw::c_char;

/// Shared RGB→(Y, interleaved-UV) BT.709-limited CSC, 4:2:0 8-bit. `stamp_color_bits`
/// marks the stream LIMITED + left-sited so VUI-honoring clients don't wash out blacks.
const CSC_SPV: &[u8] = include_bytes!("rgb2yuv.spv");
/// Per-pixel 4:4:4 twin of `CSC_SPV`; same BT.709-limited coefficients.
const CSC444_SPV: &[u8] = include_bytes!("rgb2yuv444.spv");
/// 10-bit 4:2:0 twins of `CSC_SPV`: R16/RG16 planes, `code10 << 6` in the high bits.
/// `CSC10_SPV` is BT.2020 NCL over PQ-coded input (HDR); `CSC10_709_SPV` is BT.709
/// over sRGB input (10-bit SDR — the shader widens 8→10 itself).
const CSC10_SPV: &[u8] = include_bytes!("rgb2yuv10.spv");
const CSC10_709_SPV: &[u8] = include_bytes!("rgb2yuv10_709.spv");
/// Full-res-chroma twins of the 10-bit pair.
const CSC444_10_SPV: &[u8] = include_bytes!("rgb2yuv444_10.spv");
const CSC444_10_709_SPV: &[u8] = include_bytes!("rgb2yuv444_10_709.spv");
/// Cursor overlay cap (px). The CSC shader bounds sampling by push constant, so one
/// allocation fits every pointer bitmap.
const CURSOR_MAX: u32 = 256;
/// Headroom over the per-frame rate budget for block headers + meta; the rate
/// controller itself never exceeds the budget.
const BS_SLACK: usize = 256 * 1024;

/// DRM modifiers this device can import as a SAMPLED packed-RGB image. Advertised
/// to capture instead of VAAPI's LINEAR-only policy — tiled dmabufs import via
/// `VK_EXT_image_drm_format_modifier`. Probed per session (instance + PD only).
pub(crate) fn capture_modifiers(fourcc: u32) -> Vec<u64> {
    // Same selector as `open_inner`: these modifiers are what capture allocates against.
    super::vk_util::sampled_capture_modifiers(fourcc)
}

/// Render node named beside the picked device: `PUNKTFUNK_RENDER_NODE` else
/// `/dev/dri/renderD128`. Log-only — the device pick must not use this.
fn capture_anchor_node() -> std::path::PathBuf {
    pf_gpu::render_node_env().unwrap_or_else(|| std::path::PathBuf::from("/dev/dri/renderD128"))
}

/// `(major, minor)` of a device node in the encoding `VkPhysicalDeviceDrmPropertiesEXT`
/// uses (glibc `gnu_dev_major`/`gnu_dev_minor`).
fn node_rdev(path: &std::path::Path) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let rdev = std::fs::metadata(path).ok()?.rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfffu64);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xffu64);
    Some((major as i64, minor as i64))
}

/// Node PCI `(domain, bus, device, function)` from sysfs — fallback when the
/// driver lacks `VK_EXT_physical_device_drm`.
fn node_pci_address(path: &std::path::Path) -> Option<(u32, u32, u32, u32)> {
    let node = path.file_name()?.to_str()?;
    let dev = std::fs::canonicalize(format!("/sys/class/drm/{node}/device")).ok()?;
    let addr = dev.file_name()?.to_str()?;
    let (rest, func) = addr.rsplit_once('.')?;
    let mut parts = rest.split(':');
    let domain = u32::from_str_radix(parts.next()?, 16).ok()?;
    let bus = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = u32::from_str_radix(parts.next()?, 16).ok()?;
    Some((domain, bus, device, u32::from_str_radix(func, 16).ok()?))
}

/// Features pyrowave.h requires (shaderInt16, storageBuffer8BitAccess, timeline
/// semaphores, subgroup size control). shaderFloat16 is optional. Checked after
/// selection: folding this into the pick can land on a GPU that cannot import the
/// capturer's buffers and trip the process-wide raw-dmabuf latch. A hard `bail!`
/// is latch-free and the session layer renegotiates.
///
/// # Safety
/// `instance` must be live; issues only physical-device feature queries.
unsafe fn missing_features(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Vec<&'static str> {
    let mut have12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut have13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut have2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut have12)
        .push_next(&mut have13);
    instance.get_physical_device_features2(pd, &mut have2);
    [
        (have2.features.shader_int16 == vk::TRUE, "shaderInt16"),
        (
            have12.storage_buffer8_bit_access == vk::TRUE,
            "storageBuffer8BitAccess",
        ),
        (have12.timeline_semaphore == vk::TRUE, "timelineSemaphore"),
        (
            have13.subgroup_size_control == vk::TRUE,
            "subgroupSizeControl",
        ),
        (
            have13.compute_full_subgroups == vk::TRUE,
            "computeFullSubgroups",
        ),
        (have13.synchronization2 == vk::TRUE, "synchronization2"),
    ]
    .iter()
    .filter(|(ok, _)| !ok)
    .map(|(_, n)| *n)
    .collect()
}

/// Whether `pd` owns the anchor render node. DRM render major/minor is primary
/// (disambiguates twin-model GPUs); PCI bus info is the fallback. Neither
/// extension advertised → never matches.
///
/// # Safety
/// `instance` must be live; issues only physical-device property queries.
unsafe fn device_owns_node(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
    rdev: Option<(i64, i64)>,
    pci: Option<(u32, u32, u32, u32)>,
) -> bool {
    let exts = instance
        .enumerate_device_extension_properties(pd)
        .unwrap_or_default();
    let has_ext =
        |name: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
    if let Some((major, minor)) = rdev {
        if has_ext(ash::ext::physical_device_drm::NAME) {
            let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            instance.get_physical_device_properties2(pd, &mut p2);
            return drm.has_render == vk::TRUE
                && drm.render_major == major
                && drm.render_minor == minor;
        }
    }
    if let Some((domain, bus, device, function)) = pci {
        if has_ext(ash::ext::pci_bus_info::NAME) {
            let mut pcip = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
            let mut p2 = vk::PhysicalDeviceProperties2::default().push_next(&mut pcip);
            instance.get_physical_device_properties2(pd, &mut p2);
            return pcip.pci_domain == domain
                && pcip.pci_bus == bus
                && pcip.pci_device == device
                && pcip.pci_function == function;
        }
    }
    false
}

/// Create-infos `pyrowave_create_device` requires to outlive the `pyrowave_device`.
/// Boxes pin heap locations; moving `DeviceHold` moves only the box pointers.
struct DeviceHold {
    _app_info: Box<vk::ApplicationInfo<'static>>,
    instance_ci: Box<vk::InstanceCreateInfo<'static>>,
    _queue_prio: Box<[f32; 1]>,
    _queue_ci: Box<[vk::DeviceQueueCreateInfo<'static>; 1]>,
    /// Global-priority request chained into `_queue_ci[0].p_next`. Boxed because
    /// `pyrowave_create_device` retains `device_ci` and Granite re-reads the chain;
    /// the ladder must write its final state back here (null `p_next` if the
    /// no-priority attempt won). A chain the device was not created with is a lie.
    _queue_gp: Box<[vk::DeviceQueueGlobalPriorityCreateInfoKHR<'static>; 1]>,
    // Vec, not `Box<[_; N]>`: `queue_family_foreign` is pushed conditionally.
    // `as_ptr()` is move-stable like the Boxes.
    _dev_exts: Vec<*const c_char>,
    _feat2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    _v12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _v13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

/// `CLOCK_MONOTONIC` in ns — the domain the GPU split calibrates its timestamps into.
fn mono_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, writable timespec; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// `PUNKTFUNK_PERF` GPU split of one encode: three timestamps per slot (start, after
/// CSC, end) mapped onto `CLOCK_MONOTONIC`, so a slow submit→AU reads as a late GPU
/// start, GPU work, or a late CPU wake. Without calibration only the GPU spans are real.
struct GpuTimer {
    pool: vk::QueryPool,
    period_ns: f64,
    mask: u64,
    calibrate: Option<vk::PFN_vkGetCalibratedTimestampsKHR>,
    /// `cpu_ns − gpu_ns`, refreshed every 2 s (the clocks drift apart slowly).
    offset_ns: Option<i128>,
    calibrated_at: Option<std::time::Instant>,
    /// Per frame (µs): our record, pyrowave record, submit call, GPU start lag, CSC,
    /// wavelet GPU, wake, packetize. Lag and wake are signed (negative = overlap).
    samples: Vec<[i64; 8]>,
}

impl GpuTimer {
    /// # Safety
    /// `instance`/`device` live; `family` is the queue family `device` was created with.
    unsafe fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        pd: vk::PhysicalDevice,
        family: u32,
        calib_ext: Option<&std::ffi::CStr>,
    ) -> Option<Self> {
        let bits = instance
            .get_physical_device_queue_family_properties(pd)
            .get(family as usize)?
            .timestamp_valid_bits;
        if bits == 0 {
            return None;
        }
        let pool = device
            .create_query_pool(
                &vk::QueryPoolCreateInfo::default()
                    .query_type(vk::QueryType::TIMESTAMP)
                    .query_count(3 * SLOTS as u32),
                None,
            )
            .ok()?;
        let calibrate = calib_ext.and_then(|ext| {
            let name = if ext == ash::khr::calibrated_timestamps::NAME {
                c"vkGetCalibratedTimestampsKHR"
            } else {
                c"vkGetCalibratedTimestampsEXT"
            };
            // SAFETY: both entry points share `PFN_vkGetCalibratedTimestampsKHR`'s signature.
            instance
                .get_device_proc_addr(device.handle(), name.as_ptr())
                .map(|f| std::mem::transmute::<_, vk::PFN_vkGetCalibratedTimestampsKHR>(f))
        });
        Some(Self {
            pool,
            period_ns: f64::from(
                instance
                    .get_physical_device_properties(pd)
                    .limits
                    .timestamp_period,
            ),
            mask: if bits >= 64 {
                u64::MAX
            } else {
                (1u64 << bits) - 1
            },
            calibrate,
            offset_ns: None,
            calibrated_at: None,
            samples: Vec::new(),
        })
    }

    /// # Safety
    /// `device` is the device the calibrate entry point was loaded from.
    unsafe fn recalibrate(&mut self, device: &ash::Device) {
        let Some(f) = self.calibrate else { return };
        if self
            .calibrated_at
            .is_some_and(|t| t.elapsed().as_secs() < 2)
        {
            return;
        }
        let infos = [
            vk::CalibratedTimestampInfoKHR::default().time_domain(vk::TimeDomainKHR::DEVICE),
            vk::CalibratedTimestampInfoKHR::default()
                .time_domain(vk::TimeDomainKHR::CLOCK_MONOTONIC),
        ];
        let mut out = [0u64; 2];
        let mut dev = 0u64;
        if f(
            device.handle(),
            2,
            infos.as_ptr(),
            out.as_mut_ptr(),
            &mut dev,
        ) == vk::Result::SUCCESS
        {
            let gpu_ns = ((out[0] & self.mask) as f64 * self.period_ns) as i128;
            self.offset_ns = Some(out[1] as i128 - gpu_ns);
        }
        self.calibrated_at = Some(std::time::Instant::now());
    }

    /// GPU tick → `CLOCK_MONOTONIC` ns, once calibrated.
    fn to_cpu_ns(&self, ticks: u64) -> Option<i128> {
        let gpu_ns = ((ticks & self.mask) as f64 * self.period_ns) as i128;
        self.offset_ns.map(|o| gpu_ns + o)
    }
}

/// Nearest-rank percentile of a sorted sample. The encode spike is a tail event
/// (mean barely moves); a mean-only readout would report "fine".
fn pct(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((sorted.len() as f64) * q).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// PCI vendor id of NVIDIA, whose REALTIME queues pay extra on every submit.
const VENDOR_NVIDIA: u32 = 0x10de;

/// Global-priority classes for `PYROWAVE_QUEUE_PRIORITY`, the grammar of
/// `patches/0005-global-priority-queue.patch`: ASCII-lowercased; `off` → none;
/// `high` → `[HIGH]`; `realtime`, unset and junk → `[REALTIME, HIGH]`.
/// Linux adds one rule: unset or junk on an NVIDIA device → `[HIGH]`. Its REALTIME
/// queue makes every `vkQueueSubmit` cost 0.4–1 ms, more than the preemption saves.
/// The same env var drives Windows (patch live) and Linux (we pass create-infos,
/// Granite takes `inherit_info`). `off` is the only disable spelling; `0` is not.
/// Do not change the grammar without the patch in the same commit.
fn queue_priority_candidates(raw: Option<&str>, vendor_id: u32) -> Vec<vk::QueueGlobalPriorityKHR> {
    let want = raw.map(|s| s.to_ascii_lowercase());
    let ladder = vec![
        vk::QueueGlobalPriorityKHR::REALTIME,
        vk::QueueGlobalPriorityKHR::HIGH,
    ];
    match want.as_deref() {
        Some("off") => Vec::new(),
        Some("high") => vec![vk::QueueGlobalPriorityKHR::HIGH],
        Some("realtime") => ladder,
        _ if vendor_id == VENDOR_NVIDIA => vec![vk::QueueGlobalPriorityKHR::HIGH],
        _ => ladder,
    }
}

/// Independent per-frame resource sets. Two: Granite's device defaults to
/// `init_frame_contexts(2)` and `next_frame_context()` waits the context it rotates
/// into, so frame N may not begin until N-2 completed. A third slot needs a
/// vendored `init_frame_contexts(3)`, which is not exposed. `max_inflight` (still 1)
/// is how many are live; this is capacity only.
const SLOTS: usize = 2;

/// Exclusive resources for one in-flight frame. Overlap on a shared copy is a
/// correctness bug: `csc_set` rewritten while still PENDING
/// (VUID-vkUpdateDescriptorSets-None-03047), CSC of N+1 writing images N is sampling,
/// cursor host-write racing N's sampled read, recording into a PENDING `cmd`, CPU
/// staging written while N's copy is pending. `bitstream` stays shared (packetize is
/// poll-side, one frame at a time). `import_cache` retains `VkImage`/`VkDeviceMemory`
/// per dmabuf inode so dropping a `CapturedFrame` is not a use-after-free.
struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    csc_set: vk::DescriptorSet,
    y_img: vk::Image,
    y_mem: vk::DeviceMemory,
    y_view: vk::ImageView,
    uv_img: vk::Image,
    uv_mem: vk::DeviceMemory,
    uv_view: vk::ImageView,
    cursor_img: vk::Image,
    cursor_mem: vk::DeviceMemory,
    cursor_view: vk::ImageView,
    cursor_stage: vk::Buffer,
    cursor_stage_mem: vk::DeviceMemory,
    /// Per-slot: a bitmap change uploads once per slot. A global serial would leave
    /// the other slot showing the previous pointer.
    cursor_serial: u64,
    cursor_ready: bool,
    /// CPU-input staging, lazily (re)created on format change.
    cpu_img: Option<(vk::Image, vk::DeviceMemory, vk::ImageView, vk::Format)>,
    cpu_stage: Option<(vk::Buffer, vk::DeviceMemory, u64)>,
}

impl Slot {
    /// All-null so `Drop` on a partially-built slot is sound: `vkDestroy*` of
    /// `VK_NULL_HANDLE` is the spec no-op.
    fn null() -> Self {
        Self {
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            csc_set: vk::DescriptorSet::null(),
            y_img: vk::Image::null(),
            y_mem: vk::DeviceMemory::null(),
            y_view: vk::ImageView::null(),
            uv_img: vk::Image::null(),
            uv_mem: vk::DeviceMemory::null(),
            uv_view: vk::ImageView::null(),
            cursor_img: vk::Image::null(),
            cursor_mem: vk::DeviceMemory::null(),
            cursor_view: vk::ImageView::null(),
            cursor_stage: vk::Buffer::null(),
            cursor_stage_mem: vk::DeviceMemory::null(),
            cursor_serial: u64::MAX,
            cursor_ready: false,
            cpu_img: None,
            cpu_stage: None,
        }
    }
}

/// Submitted frame state kept until its fence is waited and packetized.
#[derive(Clone)]
struct InFlight {
    /// Slot whose fence and bitstream belong to this frame.
    slot: usize,
    pts_ns: u64,
    /// Submit-time cap; a later bitrate update cannot change this frame's boundary.
    cap: usize,
    seq: u8,
    /// Submit-time chunking; a later update cannot relabel this frame's AU.
    wire_chunk: Option<usize>,
    t0: std::time::Instant,
    /// `CLOCK_MONOTONIC` at record start, around the pyrowave record, and after
    /// `vkQueueSubmit` returned. Read only by the `PUNKTFUNK_PERF` GPU split.
    cpu_ns: [u64; 4],
    /// Keeps a raw producer buffer stable through the GPU read.
    _src_hold: Option<pf_frame::FrameHold>,
    /// The dmabuf this frame reads, by `import_cache` key.
    src_key: Option<(u64, u64)>,
}

pub struct PyroWaveEncoder {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    ext_fd: ash::khr::external_memory_fd::Device,
    queue: vk::Queue,
    family: u32,
    /// `src` family for the fresh-dmabuf acquire barrier: FOREIGN when the extension
    /// is enabled, else the core EXTERNAL substitute.
    foreign_qfi: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    _hold: DeviceHold,

    // Destroyed before the VkDevice they borrow.
    pw_dev: pw::pyrowave_device,
    /// One `pyrowave_encoder` per [`Slot`]. `Encoder::Impl` owns one wavelet/scratch
    /// set and `Impl::encode` opens by discarding it (`UNDEFINED` old layout + buffer
    /// fills). Two encodes on one handle have no Vulkan execution dependency, so N+1
    /// would overwrite bands N is still packing. Overlap is two handles; within a
    /// handle, encodes stay serialized so patch 0004's scratch-pool stays intact.
    pw_encs: Vec<pw::pyrowave_encoder>,
    /// Wire sequence, kept here not in the encoder objects. Each handle has its own
    /// `sequence_count`, so alternating them emits 1,1,2,2… The decoder restarts a
    /// frame only when the value changes, so a repeat is swallowed as more blocks of
    /// the same frame. `patches/0007-encoder-sequence-override.patch` stamps this
    /// counter regardless of which handle encodes.
    wire_seq: u32,

    // Shared CSC pipeline + sampler: immutable once built, read-only while recording.
    csc_pipe: vk::Pipeline,
    csc_layout: vk::PipelineLayout,
    csc_dsl: vk::DescriptorSetLayout,
    csc_pool: vk::DescriptorPool,
    sampler: vk::Sampler,

    // Per-buffer, not per-slot: it retains the VkImage/VkDeviceMemory per inode, which is
    // what makes two slots sampling the same imported buffer safe.
    import_cache: ImportCache,
    /// 3→4 expansion for 24-bpp CPU payloads. Consumed inside `submit_frame` before
    /// return, so no GPU work reads it.
    cpu_expand: Vec<u8>,

    cmd_pool: vk::CommandPool,
    slots: Vec<Slot>,
    next_slot: usize,
    /// Submitted frames whose fence has not been waited. `reset()` keys its bounded
    /// wait on this being non-empty: a never-submitted fence starts unsignaled and
    /// would read as wedged.
    inflight: VecDeque<InFlight>,
    /// Submitted-but-not-polled cap. Still 1: a pyrowave `Encoder` cannot hold two
    /// frames (single wavelet/scratch; `Impl::encode` discards with UNDEFINED). Never
    /// exceeds `SLOTS`.
    max_inflight: usize,

    width: u32,
    height: u32,
    /// Session chroma: 4:4:4 = full-res chroma + per-pixel CSC + `Chroma444` objects.
    chroma444: bool,
    /// 10-bit session: R16/RG16 planes holding `code10 << 6` (P010-style), matching the
    /// Windows `hdr16` layout — the wavelet reads the high bits either way.
    ten_bit: bool,
    /// BT.2020 PQ session: CSC matrix, the `stamp_color_bits` bits, and a PQ-encoded
    /// cursor upload all follow it. `pq` implies `ten_bit` on every negotiated session.
    pq: bool,
    /// Ladder outcome, reported to the host (the process that owns the log pipeline).
    priority: super::worker::PriorityOutcome,
    /// Opened `deviceName`. On a multi-GPU host the worker's GPU is otherwise invisible.
    device_name: String,
    budget: FrameBudget,
    /// `PUNKTFUNK_PERF` reservoir of submit→AU durations. Other backends already log
    /// a submit split; this is the number the priority lever exists to protect.
    perf_us: Vec<u32>,
    perf_logged_at: Option<std::time::Instant>,
    /// `PUNKTFUNK_PERF` only; `None` when the queue family has no timestamps.
    gpu_timer: Option<GpuTimer>,
    /// Boundary and streamed-AU cursor. Packets pad to the boundary so each shard carries
    /// whole self-delimiting packets, and an AU is complete before its first chunk leaves.
    stream: AuStream,
    bitstream: Vec<u8>,
    pending: VecDeque<EncodedFrame>,
    /// The worker's mapped return buffer: `build_au` writes windows straight into it.
    au_arena: Option<AuArena>,
    /// Bytes the last AU put in `au_arena`, until the worker reports them.
    arena_len: Option<usize>,
    frame_count: u64,
}

/// The worker's AU return buffer, mapped, so the wire windows are laid out once in the
/// memfd the host reads instead of in a `Vec` that is copied there. Grows in 1 MiB steps
/// when a frame's bound outgrows it.
pub(crate) struct AuArena {
    file: std::fs::File,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is process memory with no thread affinity; the encoder that owns it
// moves between threads as a whole.
unsafe impl Send for AuArena {}

impl AuArena {
    pub(crate) fn new(file: std::fs::File) -> Self {
        AuArena {
            file,
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }

    fn unmap(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr`/`len` are exactly what `mmap` returned below.
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
            self.ptr = std::ptr::null_mut();
            self.len = 0;
        }
    }

    fn ensure(&mut self, need: usize) -> std::io::Result<()> {
        if need <= self.len {
            return Ok(());
        }
        let len = need.next_multiple_of(1 << 20);
        self.file.set_len(len as u64)?;
        self.unmap();
        // SAFETY: a fresh shared mapping of the first `len` bytes of our own memfd, which
        // `set_len` just made at least that long; checked against `MAP_FAILED` before use.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        self.ptr = p as *mut u8;
        self.len = len;
        Ok(())
    }

    /// Lay `packets` out as the wire AU in the mapping; its length.
    pub(crate) fn build(
        &mut self,
        packets: &[(usize, usize)],
        bitstream: &[u8],
        wire_chunk: Option<usize>,
    ) -> Result<usize> {
        let bound = crate::pyrowave_wire::au_bound(packets, wire_chunk);
        self.ensure(bound).context("grow the AU return buffer")?;
        // SAFETY: `ptr` maps `len ≥ bound` writable bytes that only this thread touches until
        // the host is told the length, and the host reads the memfd with `pread`, not a
        // mapping of ours.
        let out = unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) };
        crate::pyrowave_wire::build_au_into(packets, bitstream, wire_chunk, out)
            .ok_or_else(|| anyhow::anyhow!("AU larger than its bound {bound}"))
    }
}

impl Drop for AuArena {
    fn drop(&mut self) {
        self.unmap();
    }
}

// SAFETY: encode thread only; Vulkan handles are owned and never shared. Pyrowave
// handles are touched from that thread, and it only submits GPU work inside our API calls.
unsafe impl Send for PyroWaveEncoder {}

impl PyroWaveEncoder {
    /// `PUNKTFUNK_PERF`: record one encode duration and summarise on a slow cadence.
    /// Sample is stamped when `submit` starts and taken when the AU is readable
    /// (CSC + encode + fence wait + packetize). At depth > 1 this grows by about one
    /// loop period: N's AU is not retrieved until after N+1 has been submitted.
    fn note_encode_us(&mut self, us: u32) {
        if !pf_host_config::config().perf {
            return;
        }
        self.perf_us.push(us);
        let now = std::time::Instant::now();
        let since = self.perf_logged_at.map(|t| now.duration_since(t));
        // 2 s matches the other backends' submit-split cadence; 30 samples so p99 means something.
        if self.perf_us.len() < 30 || since.is_some_and(|d| d.as_secs() < 2) {
            if self.perf_logged_at.is_none() {
                self.perf_logged_at = Some(now);
            }
            return;
        }
        self.perf_logged_at = Some(now);
        let mut s = std::mem::take(&mut self.perf_us);
        s.sort_unstable();
        let n = s.len() as u64;
        let mean = s.iter().map(|&v| u64::from(v)).sum::<u64>() / n.max(1);
        tracing::info!(
            frames = n,
            mean_us = mean,
            p50_us = pct(&s, 0.50),
            p99_us = pct(&s, 0.99),
            max_us = *s.last().unwrap_or(&0),
            depth = self.max_inflight,
            "pyrowave encode, submit->AU (CSC + encode + fence wait + packetize). Under a \
             GPU-bound game this is the number the global-priority queue exists to protect — \
             watch p99, not the mean. At depth > 1 it includes one loop period of pipelining"
        );
        let Some(t) = self.gpu_timer.as_mut() else {
            return;
        };
        let rows = std::mem::take(&mut t.samples);
        let col = |i: usize| {
            let mut v: Vec<i64> = rows.iter().map(|r| r[i]).collect();
            v.sort_unstable();
            let at = |q: f64| v[((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1];
            (at(0.50), at(0.99))
        };
        let [rec, pwrec, sub, lag, csc, pw, wake, pack] = [0, 1, 2, 3, 4, 5, 6, 7].map(col);
        tracing::info!(
            frames = rows.len(),
            calibrated = t.offset_ns.is_some(),
            rec_us_p50 = rec.0,
            rec_us_p99 = rec.1,
            pwrec_us_p50 = pwrec.0,
            pwrec_us_p99 = pwrec.1,
            sub_us_p50 = sub.0,
            sub_us_p99 = sub.1,
            lag_us_p50 = lag.0,
            lag_us_p99 = lag.1,
            csc_us_p50 = csc.0,
            csc_us_p99 = csc.1,
            gpu_us_p50 = pw.0,
            gpu_us_p99 = pw.1,
            wake_us_p50 = wake.0,
            wake_us_p99 = wake.1,
            pack_us_p50 = pack.0,
            pack_us_p99 = pack.1,
            "pyrowave encode split: rec=our record (CPU) pwrec=pyrowave record (CPU) \
             sub=vkQueueSubmit call lag=submit returned→GPU start csc=CSC (GPU) \
             gpu=wavelet encode+readback copy (GPU) wake=GPU done→fence returned \
             pack=packetize (CPU)"
        );
    }

    /// Ladder outcome the worker reports so the host can log the grant (or refusal)
    /// once, naming the right binary.
    pub(crate) fn priority_outcome(&self) -> super::worker::PriorityOutcome {
        self.priority
    }

    /// Worker only: lay every AU out in `file` (the return buffer the host reads) instead of
    /// a `Vec`. From then on [`Encoder::poll`] hands out empty frames and
    /// [`take_arena_len`](Self::take_arena_len) says how many bytes the buffer holds.
    pub(crate) fn set_au_arena(&mut self, file: std::fs::File) {
        self.au_arena = Some(AuArena::new(file));
    }

    /// Bytes the last polled AU put in the arena; `None` off the arena.
    pub(crate) fn take_arena_len(&mut self) -> Option<usize> {
        self.arena_len.take()
    }

    pub(crate) fn device_name(&self) -> &str {
        &self.device_name
    }

    /// `bit_depth` is the negotiated stream depth (8 or 10); `hdr` marks a BT.2020 PQ
    /// session (HDR colour volume + `stamp_color_bits`). 10-bit without `hdr` is
    /// 10-bit SDR — the CSC widens the 8-bit capture to BT.709 10-bit itself.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma: crate::ChromaFormat,
        bit_depth: u8,
        hdr: bool,
    ) -> Result<Self> {
        // In-process path reads intent from its own environment and owns the inert warn.
        let intent = std::env::var("PYROWAVE_QUEUE_PRIORITY").ok();
        Self::open_checked(
            width,
            height,
            fps,
            bitrate_bps,
            chroma.is_444(),
            bit_depth,
            hdr,
            intent.as_deref(),
            true,
        )
    }

    /// [`Self::open`] as `punktfunk-encode-worker` runs it. Same open path; two
    /// differences: `intent` comes from the host handshake (the worker strips
    /// `PYROWAVE_QUEUE_PRIORITY` at startup), and the inert warn is left to the host
    /// so it names the worker binary, not the host.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_in_worker(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma444: bool,
        bit_depth: u8,
        hdr: bool,
        intent: Option<&str>,
    ) -> Result<Self> {
        Self::open_checked(
            width,
            height,
            fps,
            bitrate_bps,
            chroma444,
            bit_depth,
            hdr,
            intent,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_checked(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        chroma444: bool,
        bit_depth: u8,
        hdr: bool,
        intent: Option<&str>,
        warn_inert: bool,
    ) -> Result<Self> {
        if !chroma444 && (width % 2 != 0 || height % 2 != 0) {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        // Against the chroma actually being opened, not hardcoded 4:4:4. 4:2:0 block
        // count is still unbounded (8192×6144 4:2:0 = 73728 > u16::MAX), and a 4:4:4 →
        // 4:2:0 downgrade would skip a `chroma.is_444()`-gated check. Wrapping the index
        // lets resolve over-credit and `packetize` overshoot (Release strips the assert).
        if !crate::pyrowave_mode_fits_rdo(width, height, chroma444) {
            bail!(
                "pyrowave {} at {width}x{height} exceeds the rate controller's 16-bit block \
                 index (see pyrowave-sys patches/0002 note) — lower the resolution",
                if chroma444 { "4:4:4" } else { "4:2:0" }
            );
        }
        // SAFETY: `open_inner` only issues Vulkan/pyrowave calls whose preconditions it
        // establishes (valid instance/device, create-infos `DeviceHold` keeps alive);
        // all handles are freshly created and owned by the result.
        unsafe {
            Self::open_inner(
                width,
                height,
                fps.max(1),
                bitrate_bps.max(1_000_000),
                chroma444,
                bit_depth,
                hdr,
                intent,
                warn_inert,
            )
        }
    }

    /// `intent` is the raw `PYROWAVE_QUEUE_PRIORITY` (`None` = the default class), resolved
    /// by the caller. `warn_inert` is whether this process emits the "every class refused"
    /// warning — see [`Self::open_in_worker`]. `bit_depth`/`hdr` are the negotiated stream
    /// depth and the BT.2020 PQ flag — they pick the CSC shader, the plane formats, and the
    /// colour bits stamped into every AU.
    #[allow(clippy::too_many_arguments)]
    unsafe fn open_inner(
        w: u32,
        h: u32,
        fps: u32,
        bitrate: u64,
        chroma444: bool,
        bit_depth: u8,
        hdr: bool,
        intent: Option<&str>,
        warn_inert: bool,
    ) -> Result<Self> {
        let entry = ash::Entry::load().context("load vulkan loader")?;

        let mut hold = DeviceHold {
            _app_info: Box::new(vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3)),
            instance_ci: Box::new(vk::InstanceCreateInfo::default()),
            _queue_prio: Box::new([1.0f32]),
            _queue_ci: Box::new([vk::DeviceQueueCreateInfo::default()]),
            _queue_gp: Box::new([vk::DeviceQueueGlobalPriorityCreateInfoKHR::default()]),
            _dev_exts: vec![
                ash::khr::external_memory_fd::NAME.as_ptr(),
                ash::ext::external_memory_dma_buf::NAME.as_ptr(),
                ash::ext::image_drm_format_modifier::NAME.as_ptr(),
            ],
            _feat2: Box::new(vk::PhysicalDeviceFeatures2::default()),
            _v12: Box::new(vk::PhysicalDeviceVulkan12Features::default()),
            _v13: Box::new(vk::PhysicalDeviceVulkan13Features::default()),
            device_ci: Box::new(vk::DeviceCreateInfo::default()),
        };
        hold.instance_ci.p_application_info = &*hold._app_info;
        let instance = entry
            .create_instance(&hold.instance_ci, None)
            .context("create instance")?;

        // Between `create_instance` and `create_device` the only live resource is the
        // instance: one fallible block, one destroy on error. From the device on, `Drop`
        // is the unwind path.

        // SAFETY: physical-device queries on the live instance; `create_device` create-infos
        // are pinned in `hold`. Retries only change `global_priority` and, on the last
        // attempt, `_queue_ci[0].p_next`. Both live in `hold`'s Boxes, so `device_ci`
        // pointers stay valid; a failed `create_device` does not consume its create-info.
        let selected = (|| unsafe {
            // Same selector as `capture_modifiers` so the two never disagree about the device.
            let picked = select_physical_device(&instance)?;
            let (pd, family) = (picked.pd, picked.family);
            // Log-only: picked device beside the two guesses. A mismatch is not evidence of a
            // wrong pick (loader first-device vs renderD128 disagree on hybrid hosts), so no
            // arm is a WARN.
            let anchor = capture_anchor_node();
            let anchor_owner = {
                let rdev = node_rdev(&anchor);
                let pci = node_pci_address(&anchor);
                if rdev.is_none() && pci.is_none() {
                    "unresolved".to_string()
                } else {
                    instance
                        .enumerate_physical_devices()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|&o| device_owns_node(&instance, o, rdev, pci))
                        .map(|o| {
                            let p = instance.get_physical_device_properties(o);
                            format!("{:04x}:{:04x}", p.vendor_id, p.device_id)
                        })
                        .unwrap_or_else(|| "unmatched".to_string())
                }
            };
            let selected_gpu = pf_gpu::selected_gpu()
                .map(|s| format!("{:04x}:{:04x}", s.info.vendor_id, s.info.device_id))
                .unwrap_or_else(|| "none".to_string());
            tracing::info!(
                vendor_id = format_args!("{:04x}", picked.vendor_id),
                device_id = format_args!("{:04x}", picked.device_id),
                anchor = %anchor.display(),
                anchor_owner = %anchor_owner,
                selected_gpu = %selected_gpu,
                "pyrowave: encoding on the first usable Vulkan GPU (a wrong-device report on a \
                 multi-GPU host needs these fields)"
            );

            // Pyrowave's documented encoder requirements; the re-query below mirrors optionals
            // (shaderFloat16, vulkanMemoryModel, maintenance4) into the create-info.
            let missing = missing_features(&instance, pd);
            if !missing.is_empty() {
                bail!("GPU lacks pyrowave-required Vulkan features: {missing:?}");
            }
            let mut have12 = vk::PhysicalDeviceVulkan12Features::default();
            let mut have13 = vk::PhysicalDeviceVulkan13Features::default();
            let mut have2 = vk::PhysicalDeviceFeatures2::default()
                .push_next(&mut have12)
                .push_next(&mut have13);
            instance.get_physical_device_features2(pd, &mut have2);

            hold._feat2.features.shader_int16 = vk::TRUE;
            hold._v12.storage_buffer8_bit_access = vk::TRUE;
            hold._v12.timeline_semaphore = vk::TRUE;
            hold._v12.shader_float16 = have12.shader_float16;
            hold._v12.vulkan_memory_model = have12.vulkan_memory_model;
            hold._v12.vulkan_memory_model_device_scope = have12.vulkan_memory_model_device_scope;
            hold._v13.subgroup_size_control = vk::TRUE;
            hold._v13.compute_full_subgroups = vk::TRUE;
            hold._v13.synchronization2 = vk::TRUE;
            hold._v13.maintenance4 = have13.maintenance4;
            hold._feat2.p_next = &mut *hold._v12 as *mut _ as *mut std::ffi::c_void;
            hold._v12.p_next = &mut *hold._v13 as *mut _ as *mut std::ffi::c_void;

            // Fresh-import acquire names FOREIGN as src when advertised, else
            // QUEUE_FAMILY_EXTERNAL. Push before the count/as_ptr wiring below.
            let dev_ext_props = instance
                .enumerate_device_extension_properties(pd)
                .unwrap_or_default();
            let foreign_qfi = if crate::vk_util::ext_advertised(
                &dev_ext_props,
                ash::ext::queue_family_foreign::NAME,
            ) {
                hold._dev_exts
                    .push(ash::ext::queue_family_foreign::NAME.as_ptr());
                vk::QUEUE_FAMILY_FOREIGN_EXT
            } else {
                tracing::warn!(
                    "pyrowave: VK_EXT_queue_family_foreign not advertised — dmabuf acquires \
                     use the core QUEUE_FAMILY_EXTERNAL substitute (no fleet hardware takes \
                     this arm; report it)"
                );
                vk::QUEUE_FAMILY_EXTERNAL
            };
            // Encode shares shader cores with the game; process priority only orders
            // submission. The vendored patch is gated `if (!inherit_info)` and Linux
            // passes its own create-infos, so Granite takes the inherit branch.
            let gp_candidates = queue_priority_candidates(intent, picked.vendor_id);
            // KHR is the promoted name; match pf-zerocopy's VkBridge probe so spellings agree.
            let gp_ext =
                if crate::vk_util::ext_advertised(&dev_ext_props, vk::KHR_GLOBAL_PRIORITY_NAME) {
                    Some(vk::KHR_GLOBAL_PRIORITY_NAME)
                } else if crate::vk_util::ext_advertised(
                    &dev_ext_props,
                    vk::EXT_GLOBAL_PRIORITY_NAME,
                ) {
                    Some(vk::EXT_GLOBAL_PRIORITY_NAME)
                } else {
                    None
                };
            let gp = gp_ext.filter(|_| !gp_candidates.is_empty());
            if let Some(name) = gp {
                hold._dev_exts.push(name.as_ptr());
            }
            // `PUNKTFUNK_PERF` only, so the default device stays exactly as before.
            let calib_ext = [
                ash::khr::calibrated_timestamps::NAME,
                ash::ext::calibrated_timestamps::NAME,
            ]
            .into_iter()
            .filter(|_| pf_host_config::config().perf)
            .find(|n| crate::vk_util::ext_advertised(&dev_ext_props, n));
            if let Some(name) = calib_ext {
                hold._dev_exts.push(name.as_ptr());
            }

            hold._queue_ci[0] = vk::DeviceQueueCreateInfo::default().queue_family_index(family);
            hold._queue_ci[0].queue_count = 1;
            hold._queue_ci[0].p_queue_priorities = hold._queue_prio.as_ptr();
            if gp.is_some() {
                hold._queue_ci[0].p_next = &*hold._queue_gp as *const _ as *const std::ffi::c_void;
            }
            hold.device_ci.p_next = &*hold._feat2 as *const _ as *const std::ffi::c_void;
            hold.device_ci.queue_create_info_count = 1;
            hold.device_ci.p_queue_create_infos = hold._queue_ci.as_ptr();
            hold.device_ci.enabled_extension_count = hold._dev_exts.len() as u32;
            hold.device_ci.pp_enabled_extension_names = hold._dev_exts.as_ptr();

            // Try each class; step down only on a refusal; if every class is refused, create
            // with no global priority. A refused class must not fail the open: this path is a
            // negotiated PyroWave session, so a hard error is a dead stream.
            let mut chosen = None;
            let mut device = None;
            for want in &gp_candidates {
                if gp.is_none() {
                    break;
                }
                hold._queue_gp[0].global_priority = *want;
                match instance.create_device(pd, &hold.device_ci, None) {
                    Ok(d) => {
                        chosen = Some(*want);
                        device = Some(d);
                        break;
                    }
                    Err(e) if pf_zerocopy::vkdev::priority_refused(e) => {
                        tracing::debug!(
                            priority = ?want,
                            error = ?e,
                            "pyrowave: global queue priority not permitted — downgrading"
                        );
                    }
                    Err(e) => {
                        return Err(e).context("create device");
                    }
                }
            }
            let device = match device {
                Some(d) => {
                    tracing::info!(
                        priority = ?chosen,
                        ext = ?gp,
                        "pyrowave: elevated global queue priority (the encode dispatch preempts a \
                         GPU-bound game where the driver honors it)"
                    );
                    d
                }
                None => {
                    // Nothing requested, or every class refused. The retained create-info must
                    // describe a device created without a priority chain: Granite re-reads it
                    // via `get_existing_create_info()`. The extension stays enabled (it is on
                    // the device); only the request is dropped.
                    hold._queue_ci[0].p_next = std::ptr::null();
                    if !gp_candidates.is_empty() && gp.is_some() && warn_inert {
                        // Unprivileged hosts are refused every class; `cap_sys_nice+ep` is
                        // granted REALTIME. The worker reports the outcome to its parent, which
                        // logs naming the worker binary — do not `setcap` the host.
                        tracing::warn!(
                            "pyrowave: every global queue priority class was refused — encoding \
                             at default priority. The GPU-preemption lever is INERT without \
                             CAP_SYS_NICE on the host binary (measured on both NVIDIA and RADV); \
                             PYROWAVE_QUEUE_PRIORITY=off silences this"
                        );
                    }
                    instance
                        .create_device(pd, &hold.device_ci, None)
                        .context("create device")?
                }
            };
            // Candidates are only REALTIME or HIGH, so `Some(_)` is High (ash models the
            // class as a newtype, not a Rust enum).
            let priority = match chosen {
                Some(c) if c == vk::QueueGlobalPriorityKHR::REALTIME => {
                    super::worker::PriorityOutcome::Granted(super::worker::GrantedClass::Realtime)
                }
                Some(_) => {
                    super::worker::PriorityOutcome::Granted(super::worker::GrantedClass::High)
                }
                None if !gp_candidates.is_empty() && gp.is_some() => {
                    super::worker::PriorityOutcome::Refused
                }
                None => super::worker::PriorityOutcome::NotRequested,
            };
            let device_name = instance
                .get_physical_device_properties(pd)
                .device_name_as_c_str()
                .ok()
                .and_then(|s| s.to_str().ok())
                .unwrap_or("unknown")
                .to_string();
            Ok((
                pd,
                family,
                device,
                foreign_qfi,
                priority,
                device_name,
                calib_ext,
            ))
        })();
        let (pd, family, device, foreign_qfi, priority, device_name, calib_ext) = match selected {
            Ok(v) => v,
            Err(e) => {
                instance.destroy_instance(None);
                return Err(e);
            }
        };
        let queue = device.get_device_queue(family, 0);
        let ext_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let mem_props = instance.get_physical_device_memory_properties(pd);

        // Construct `Self` now with later resources null and assign as they come up. Any `?`
        // drops `me`; `Drop` tears down the prefix: idle first, null-guard `pw_encs`
        // (`pyrowave_encoder_destroy` dereferences), `pyrowave_device_destroy(null)` is
        // `delete nullptr`, and `vkDestroy*` of VK_NULL_HANDLE is a spec no-op.
        let mut me = Self {
            _entry: entry,
            instance,
            device,
            ext_fd,
            queue,
            family,
            foreign_qfi,
            mem_props,
            _hold: hold,
            pw_dev: std::ptr::null_mut(),
            pw_encs: vec![std::ptr::null_mut(); SLOTS],
            wire_seq: 0,
            csc_pipe: vk::Pipeline::null(),
            csc_layout: vk::PipelineLayout::null(),
            csc_dsl: vk::DescriptorSetLayout::null(),
            csc_pool: vk::DescriptorPool::null(),
            sampler: vk::Sampler::null(),
            import_cache: ImportCache::default(),
            cpu_expand: Vec::new(),
            cmd_pool: vk::CommandPool::null(),
            slots: (0..SLOTS).map(|_| Slot::null()).collect(),
            next_slot: 0,
            inflight: VecDeque::new(),
            max_inflight: 1,
            width: w,
            height: h,
            chroma444,
            // The CSC shaders write `code10 << 6` for 10-bit; PQ labelling only makes
            // sense on the 10-bit stream — an 8-bit `hdr` ask degrades to 709 codes.
            ten_bit: bit_depth >= 10,
            pq: hdr && bit_depth >= 10,
            priority,
            device_name,
            budget: FrameBudget::new(bitrate, fps),
            perf_us: Vec::new(),
            perf_logged_at: None,
            gpu_timer: None,
            stream: AuStream::default(),
            bitstream: Vec::new(),
            pending: VecDeque::new(),
            au_arena: None,
            arena_len: None,
            frame_count: 0,
        };

        // Create-infos stay pinned in `me._hold`: pyrowave retains the pointers for the
        // device's lifetime, and the Boxes' heap data does not move when `Self` does.
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: me.queue.as_raw() as pw::VkQueue,
            familyIndex: family,
            index: 0,
        };
        let create = pw::pyrowave_device_create_info {
            // SAFETY: ash's loader entry and bindgen's PFN are the same C function pointer.
            GetInstanceProcAddr: Some(std::mem::transmute::<
                unsafe extern "system" fn(
                    ash::vk::Instance,
                    *const c_char,
                ) -> Option<unsafe extern "system" fn()>,
                unsafe extern "C" fn(pw::VkInstance, *const c_char) -> pw::PFN_vkVoidFunction,
            >(me._entry.static_fn().get_instance_proc_addr)),
            instance: me.instance.handle().as_raw() as usize as pw::VkInstance,
            physical_device: pd.as_raw() as usize as pw::VkPhysicalDevice,
            device: me.device.handle().as_raw() as usize as pw::VkDevice,
            instance_create_info: &*me._hold.instance_ci as *const vk::InstanceCreateInfo
                as *const pw::VkInstanceCreateInfo,
            device_create_info: &*me._hold.device_ci as *const vk::DeviceCreateInfo
                as *const pw::VkDeviceCreateInfo,
            queue_info: &mut queue_info,
            queue_info_count: 1,
            // Encode thread only; pyrowave submits only inside our API calls — no lock.
            queue_lock_callback: None,
            queue_unlock_callback: None,
            userdata: std::ptr::null_mut(),
        };
        pw_check(
            pw::pyrowave_create_device(&create, &mut me.pw_dev),
            "create_device",
        )?;
        let _ =
            pw::pyrowave_device_set_queue_type(me.pw_dev, pw::VkQueueFlagBits_VK_QUEUE_COMPUTE_BIT);

        let einfo = pw::pyrowave_encoder_create_info {
            device: me.pw_dev,
            width: w as i32,
            height: h as i32,
            chroma: if chroma444 {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
            } else {
                pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
            },
        };
        for i in 0..SLOTS {
            pw_check(
                pw::pyrowave_encoder_create(&einfo, &mut me.pw_encs[i]),
                "encoder_create",
            )?;
        }

        let device = me.device.clone(); // fn-table clone; lets `me.*` assignments interleave
        let (cw, ch) = if chroma444 { (w, h) } else { (w / 2, h / 2) };

        me.sampler = device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::NEAREST)
                .min_filter(vk::Filter::NEAREST)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )?;
        let spv = ash::util::read_spv(&mut std::io::Cursor::new(
            match (chroma444, me.ten_bit, me.pq) {
                (false, false, _) => CSC_SPV,
                (true, false, _) => CSC444_SPV,
                (false, true, false) => CSC10_709_SPV,
                (false, true, true) => CSC10_SPV,
                (true, true, false) => CSC444_10_709_SPV,
                (true, true, true) => CSC444_10_SPV,
            },
        ))?;
        let shader =
            device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&spv), None)?;
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
        me.csc_dsl = device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
            None,
        )?;
        let dsls = [me.csc_dsl];
        // Cursor {ivec2 origin, ivec2 size} = 16 bytes; matches the shared CSC shader.
        let pc_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(16)];
        me.csc_layout = device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&dsls)
                .push_constant_ranges(&pc_ranges),
            None,
        )?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(c"main");
        let pipe_res = device.create_compute_pipelines(
            vk::PipelineCache::null(),
            &[vk::ComputePipelineCreateInfo::default()
                .layout(me.csc_layout)
                .stage(stage)],
            None,
        );
        // Destroy the module before `?`ing: it lives in no field. On failure the batch-of-1
        // out array is all VK_NULL_HANDLE; a multi-entry batch could not assume that.
        device.destroy_shader_module(shader, None);
        me.csc_pipe = pipe_res.map_err(|(_, e)| e)?[0];

        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(2 * SLOTS as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(2 * SLOTS as u32),
        ];
        me.csc_pool = device.create_descriptor_pool(
            &vk::DescriptorPoolCreateInfo::default()
                .max_sets(SLOTS as u32)
                .pool_sizes(&pool_sizes),
            None,
        )?;
        me.cmd_pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
            None,
        )?;

        // 10-bit planes are R16/RG16 (`code10 << 6`, the P010-style layout the Windows
        // `hdr16` path uses) — the wavelet reads UNORM samples, so depth is a view-format
        // fact, not a codec flag.
        let (y_fmt, uv_fmt) = if me.ten_bit {
            (vk::Format::R16_UNORM, vk::Format::R16G16_UNORM)
        } else {
            (vk::Format::R8_UNORM, vk::Format::R8G8_UNORM)
        };
        // One complete `Slot` per iteration; a mid-loop failure leaves earlier slots formed
        // and the rest null — `Drop` handles VK_NULL_HANDLE.
        for i in 0..SLOTS {
            let (y_img, y_mem, y_view) = make_plain_image(
                &device,
                &me.mem_props,
                y_fmt,
                w,
                h,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            )?;
            me.slots[i].y_img = y_img;
            me.slots[i].y_mem = y_mem;
            me.slots[i].y_view = y_view;
            let (uv_img, uv_mem, uv_view) = make_plain_image(
                &device,
                &me.mem_props,
                uv_fmt,
                cw,
                ch,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            )?;
            me.slots[i].uv_img = uv_img;
            me.slots[i].uv_mem = uv_mem;
            me.slots[i].uv_view = uv_view;
            let (cursor_img, cursor_mem, cursor_view) = make_plain_image(
                &device,
                &me.mem_props,
                vk::Format::R8G8B8A8_UNORM,
                CURSOR_MAX,
                CURSOR_MAX,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?;
            me.slots[i].cursor_img = cursor_img;
            me.slots[i].cursor_mem = cursor_mem;
            me.slots[i].cursor_view = cursor_view;
            let (cursor_stage, cursor_stage_mem) = make_host_buffer(
                &device,
                &me.mem_props,
                (CURSOR_MAX * CURSOR_MAX * 4) as u64,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            me.slots[i].cursor_stage = cursor_stage;
            me.slots[i].cursor_stage_mem = cursor_stage_mem;
            let csc_set = device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(me.csc_pool)
                    .set_layouts(&dsls),
            )?[0];
            me.slots[i].csc_set = csc_set;
            // Bindings 1–3 are fixed for the slot's life; only binding 0 is rewritten per
            // frame — that is why each slot needs its own set.
            let yi = [vk::DescriptorImageInfo::default()
                .image_view(y_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let uvi = [vk::DescriptorImageInfo::default()
                .image_view(uv_view)
                .image_layout(vk::ImageLayout::GENERAL)];
            let curi = [vk::DescriptorImageInfo::default()
                .sampler(me.sampler)
                .image_view(cursor_view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            device.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(csc_set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&yi),
                    vk::WriteDescriptorSet::default()
                        .dst_set(csc_set)
                        .dst_binding(2)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&uvi),
                    vk::WriteDescriptorSet::default()
                        .dst_set(csc_set)
                        .dst_binding(3)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&curi),
                ],
                &[],
            );
            me.slots[i].cmd = device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(me.cmd_pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )?[0];
            me.slots[i].fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
        }

        if pf_host_config::config().perf {
            me.gpu_timer = GpuTimer::new(&me.instance, &device, pd, family, calib_ext);
        }

        // Driver-reported slot size (not an estimate). CPU staging is excluded: lazy, software
        // capture only.
        let slot_bytes: u64 = [
            me.slots[0].y_img,
            me.slots[0].uv_img,
            me.slots[0].cursor_img,
        ]
        .iter()
        .map(|&i| device.get_image_memory_requirements(i).size)
        .sum::<u64>()
            + device
                .get_buffer_memory_requirements(me.slots[0].cursor_stage)
                .size;

        let props = me.instance.get_physical_device_properties(pd);
        tracing::info!(
            gpu = %props.device_name_as_c_str().unwrap_or(c"?").to_string_lossy(),
            mode = %format!("{w}x{h}@{fps}"),
            budget_kib = me.budget.bytes / 1024,
            chroma = if chroma444 { "4:4:4" } else { "4:2:0" },
            bit_depth = if me.ten_bit { 10 } else { 8 },
            colour = if me.pq { "BT.2020 PQ" } else { "BT.709" },
            slots = SLOTS,
            slot_kib = slot_bytes / 1024,
            slots_kib = slot_bytes * SLOTS as u64 / 1024,
            "PyroWave encoder open (intra-only wavelet)"
        );

        Ok(me)
    }

    /// Point slot `slot`'s CSC binding 0 at this frame's RGB view.
    /// Writing a set still bound by a PENDING command buffer violates
    /// VUID-vkUpdateDescriptorSets-None-03047. Safe here: this slot's previous frame
    /// was retired before `submit` chose it.
    unsafe fn bind_rgb(&self, slot: usize, rgb_view: vk::ImageView) {
        let ii = [vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(rgb_view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        self.device.update_descriptor_sets(
            &[vk::WriteDescriptorSet::default()
                .dst_set(self.slots[slot].csc_set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&ii)],
            &[],
        );
    }

    /// Bring the cursor image up to date and return `[origin_x, origin_y, size_w, size_h]`
    /// (size 0 ⇒ CSC skips the blend). Upload only when `serial` changed. Per-slot:
    /// a shared image races the previous frame's sampled read.
    unsafe fn prep_cursor(
        &mut self,
        slot: usize,
        cursor: Option<&pf_frame::CursorOverlay>,
    ) -> Result<[i32; 4]> {
        let dev = self.device.clone();
        let cmd = self.slots[slot].cmd;
        let img = self.slots[slot].cursor_img;
        let stage = self.slots[slot].cursor_stage;
        let stage_mem = self.slots[slot].cursor_stage_mem;
        let ready = self.slots[slot].cursor_ready;
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
                if self.slots[slot].cursor_serial != c.serial {
                    // PQ sessions blend PQ-encoded codes: the 10-bit shaders mix the
                    // cursor into PQ-space samples, so the upload is re-encoded.
                    let px = if self.pq { c.pq_rgba() } else { c.rgba.clone() };
                    let bytes = (cw as usize) * (ch as usize) * 4;
                    let ptr =
                        dev.map_memory(stage_mem, 0, bytes as u64, vk::MemoryMapFlags::empty())?;
                    std::ptr::copy_nonoverlapping(px.as_ptr(), ptr as *mut u8, bytes.min(px.len()));
                    dev.unmap_memory(stage_mem);
                    let old = if ready {
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
                        stage,
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
                    self.slots[slot].cursor_serial = c.serial;
                    self.slots[slot].cursor_ready = true;
                }
                Ok([c.x, c.y, cw as i32, ch as i32])
            }
            _ => {
                if !ready {
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
                    self.slots[slot].cursor_ready = true;
                }
                Ok([0, 0, 0, 0])
            }
        }
    }

    /// Import a dmabuf through [`ImportCache`]. A deterministic refusal feeds this capture's
    /// latch and carries [`ImportRejected`], which the encode worker forwards as a rebuild.
    /// Transient OOM stays out of that sticky verdict.
    unsafe fn import_cached(
        &mut self,
        d: &pf_frame::DmabufFrame,
        cw: u32,
        ch: u32,
    ) -> Result<(vk::Image, vk::ImageView, bool)> {
        let key = pf_zerocopy::fd_identity(d.fd.as_fd()).unwrap_or((u64::MAX, self.frame_count));
        self.import_cache.get_or_import(
            &self.device,
            key,
            (cw, ch),
            || match import_rgb_dmabuf(&self.device, &self.ext_fd, &self.mem_props, d, cw, ch) {
                Ok(t) => {
                    d.health.note_raw_import_ok();
                    Ok(t)
                }
                Err(e) if import_failure_feeds_latch(&e) => {
                    reject_dmabuf(d, &format!("{e:#}"));
                    Err(e.context(ImportRejected))
                }
                Err(e) => Err(e),
            },
            |k| self.inflight.iter().any(|f| f.src_key == Some(k)),
            self.inflight.len(),
        )
    }

    /// CPU RGB staging. Per-slot: a host write while the previous frame's copy is
    /// still pending would race that copy.
    unsafe fn ensure_cpu_rgb(
        &mut self,
        slot: usize,
        fmt: vk::Format,
        bytes: &[u8],
    ) -> Result<vk::ImageView> {
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        // Widen before the multiply: `w * h * 4` wraps in u32 once `w * h > 2^30`.
        let need = w as u64 * h as u64 * 4;
        if self.slots[slot].cpu_img.map(|(_, _, _, f)| f) != Some(fmt) {
            if let Some((i, m, v, _)) = self.slots[slot].cpu_img.take() {
                dev.destroy_image_view(v, None);
                dev.destroy_image(i, None);
                dev.free_memory(m, None);
            }
            let (i, m, v) = make_plain_image(
                &dev,
                &self.mem_props,
                fmt,
                w,
                h,
                vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST,
            )?;
            self.slots[slot].cpu_img = Some((i, m, v, fmt));
        }
        if self.slots[slot]
            .cpu_stage
            .map(|(_, _, s)| s < need)
            .unwrap_or(true)
        {
            if let Some((b, m, _)) = self.slots[slot].cpu_stage.take() {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            let (buf, mem) = make_host_buffer(
                &dev,
                &self.mem_props,
                need,
                vk::BufferUsageFlags::TRANSFER_SRC,
            )?;
            self.slots[slot].cpu_stage = Some((buf, mem, need));
        }
        let (_, m, _) = self.slots[slot].cpu_stage.unwrap();
        let p = dev.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())? as *mut u8;
        let n = bytes.len().min(need as usize);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, n);
        dev.unmap_memory(m);
        Ok(self.slots[slot].cpu_img.unwrap().2)
    }

    /// Record and submit ingest, CSC and encode. The in-flight entry owns any raw
    /// source hold until [`wait_and_packetize`] retires its fence.
    unsafe fn submit_frame(&mut self, frame: &CapturedFrame, t0: std::time::Instant) -> Result<()> {
        // A failed `reset()` leaves the encoder destroyed and null. A null here is a
        // use-after-free inside pyrowave, so fail loudly.
        anyhow::ensure!(
            self.pw_encs.iter().all(|e| !e.is_null()),
            "pyrowave: encode after a failed reset (encoder was destroyed and not rebuilt)"
        );
        let dev = self.device.clone();
        let (w, h) = (self.width, self.height);
        // No alignment: a mismatch smears (`rgb2yuv.comp` clamps; CPU uploads min(len,need)).
        // A PipeWire size change is not always transient (`reset()` reopens at the same
        // dimensions).
        if frame.width != w || frame.height != h {
            bail!(
                "pyrowave: frame {}x{} != session mode {w}x{h} — refusing a mismatched encode \
                 source",
                frame.width,
                frame.height
            );
        }
        // `begin` through `queue_submit` in one closure whose error arm resets `cmd`.
        // Never PENDING on those arms. Failures after must not reset: a fence timeout
        // leaves PENDING (VUID-vkResetCommandBuffer-commandBuffer-00045).
        // Before the closure, which mutably borrows `self`.
        let rate_budget = self.budget.rate_control(self.stream.wire_chunk.is_some());
        let slot = self.next_slot;
        let seq = self.wire_seq;
        let cmd = self.slots[slot].cmd;
        let fence = self.slots[slot].fence;
        let mut cpu_ns = [mono_ns(), 0, 0, 0];
        let ts_pool = self.gpu_timer.as_ref().map(|t| t.pool);
        let q0 = slot as u32 * 3;
        let record_and_submit = (|| -> Result<()> {
            dev.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            if let Some(pool) = ts_pool {
                dev.cmd_reset_query_pool(cmd, pool, q0, 3);
                dev.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::TOP_OF_PIPE, pool, q0);
            }

            let cursor_pc = self.prep_cursor(slot, frame.cursor.as_ref())?;

            let (rgb_view, imported) = match &frame.payload {
                FramePayload::Dmabuf(d) => {
                    let (img, view, fresh) = self.import_cached(d, frame.width, frame.height)?;
                    // Fresh or cached, acquire from the producer's family: it rewrote the
                    // buffer since. A cached one sits in GENERAL where the release left it.
                    let acq = imported_acquire_barrier(
                        img,
                        fresh,
                        self.foreign_qfi,
                        self.family,
                        vk::PipelineStageFlags2::COMPUTE_SHADER,
                        vk::AccessFlags2::SHADER_READ,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    );
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[acq]),
                    );
                    (view, Some(img))
                }
                FramePayload::Cpu(bytes) => {
                    // 24-bpp Rgb/Bgr expands 3→4 first (`normalize_cpu_rgb`).
                    let mut scratch = std::mem::take(&mut self.cpu_expand);
                    let (norm_fmt, norm_bytes) =
                        normalize_cpu_rgb(frame.format, bytes, &mut scratch, false);
                    let fmt = pixel_to_vk(norm_fmt).context("unsupported CPU pixel format");
                    let view = match fmt {
                        Ok(f) => self.ensure_cpu_rgb(slot, f, norm_bytes),
                        Err(e) => Err(e),
                    };
                    self.cpu_expand = scratch;
                    let view = view?;
                    let (img, ..) = self.slots[slot].cpu_img.unwrap();
                    let (stage, ..) = self.slots[slot].cpu_stage.unwrap();
                    let to_dst = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_dst]),
                    );
                    dev.cmd_copy_buffer_to_image(
                        cmd,
                        stage,
                        img,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::BufferImageCopy::default()
                            .image_subresource(
                                vk::ImageSubresourceLayers::default()
                                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                                    .layer_count(1),
                            )
                            .image_extent(vk::Extent3D {
                                width: w,
                                height: h,
                                depth: 1,
                            })],
                    );
                    let to_read = vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::ALL_TRANSFER)
                        .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                        .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                        .dst_access_mask(vk::AccessFlags2::SHADER_READ)
                        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                        .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                        .image(img)
                        .subresource_range(color_range(0));
                    dev.cmd_pipeline_barrier2(
                        cmd,
                        &vk::DependencyInfo::default().image_memory_barriers(&[to_read]),
                    );
                    (view, None)
                }
                _ => bail!("pyrowave: unsupported FramePayload (need Dmabuf or Cpu RGB)"),
            };
            self.bind_rgb(slot, rgb_view);

            // y/uv → GENERAL for CSC storage writes. This slot's previous frame was retired
            // before `submit` chose it (the execution barrier pyrowave asks for).
            let (y_img, uv_img) = (self.slots[slot].y_img, self.slots[slot].uv_img);
            let to_general = |img| {
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::NONE)
                    .src_access_mask(vk::AccessFlags2::NONE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_WRITE)
                    .old_layout(vk::ImageLayout::UNDEFINED)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(img)
                    .subresource_range(color_range(0))
            };
            dev.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_general(y_img), to_general(uv_img)]),
            );
            dev.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.csc_pipe);
            dev.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.csc_layout,
                0,
                &[self.slots[slot].csc_set],
                &[],
            );
            let mut pc_bytes = [0u8; 16];
            for (i, v) in cursor_pc.iter().enumerate() {
                pc_bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            dev.cmd_push_constants(
                cmd,
                self.csc_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &pc_bytes,
            );
            // 4:2:0: one invocation per 2×2 luma block; 4:4:4: per pixel.
            if self.chroma444 {
                dev.cmd_dispatch(cmd, w.div_ceil(8), h.div_ceil(8), 1);
            } else {
                dev.cmd_dispatch(cmd, (w / 2).div_ceil(8), (h / 2).div_ceil(8), 1);
            }
            // The CSC was the source's last read: hand it back to the producer's family.
            if let Some(img) = imported {
                let rel = imported_release_barrier(
                    img,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    self.family,
                    self.foreign_qfi,
                    vk::PipelineStageFlags2::COMPUTE_SHADER,
                    vk::AccessFlags2::SHADER_READ,
                );
                dev.cmd_pipeline_barrier2(
                    cmd,
                    &vk::DependencyInfo::default().image_memory_barriers(&[rel]),
                );
            }
            if let Some(pool) = ts_pool {
                dev.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::ALL_COMMANDS, pool, q0 + 1);
            }

            // CSC writes → pyrowave sampled reads. Stay GENERAL (pyrowave's GPU-buffer layout).
            let to_sampled = |img| {
                vk::ImageMemoryBarrier2::default()
                    .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .src_access_mask(vk::AccessFlags2::SHADER_WRITE)
                    .dst_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                    .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::GENERAL)
                    .image(img)
                    .subresource_range(color_range(0))
            };
            dev.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default()
                    .image_memory_barriers(&[to_sampled(y_img), to_sampled(uv_img)]),
            );

            let plane = |image: vk::Image,
                         pw_w: u32,
                         pw_h: u32,
                         fmt: pw::VkFormat,
                         swizzle: pw::VkComponentSwizzle| {
                pw::pyrowave_image_view {
                    image: image.as_raw() as usize as pw::VkImage,
                    width: pw_w,
                    height: pw_h,
                    image_format: fmt,
                    view_format: fmt,
                    mip_level: 0,
                    layer: 0,
                    aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
                    swizzle,
                    layout: pw::VkImageLayout_VK_IMAGE_LAYOUT_GENERAL,
                }
            };
            let (yf, cf) = if self.ten_bit {
                (
                    pw::VkFormat_VK_FORMAT_R16_UNORM,
                    pw::VkFormat_VK_FORMAT_R16G16_UNORM,
                )
            } else {
                (
                    pw::VkFormat_VK_FORMAT_R8_UNORM,
                    pw::VkFormat_VK_FORMAT_R8G8_UNORM,
                )
            };
            let buffers = pw::pyrowave_gpu_buffers {
                planes: [
                    plane(
                        y_img,
                        w,
                        h,
                        yf,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
                    ),
                    // RG chroma: R/G swizzles synthesize Cb/Cr. Extent is this image's mip0
                    // (separate image, not a planar aspect): half-res 4:2:0, full-res 4:4:4.
                    plane(
                        uv_img,
                        if self.chroma444 { w } else { w / 2 },
                        if self.chroma444 { h } else { h / 2 },
                        cf,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_R,
                    ),
                    plane(
                        uv_img,
                        if self.chroma444 { w } else { w / 2 },
                        if self.chroma444 { h } else { h / 2 },
                        cf,
                        pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_G,
                    ),
                ],
            };
            let rc = pw::pyrowave_rate_control {
                maximum_bitstream_size: rate_budget,
            };
            cpu_ns[1] = mono_ns();
            pw::pyrowave_device_set_command_buffer(
                self.pw_dev,
                cmd.as_raw() as usize as pw::VkCommandBuffer,
            );
            // Stamp our monotonic counter before encode, or alternating handles emit
            // 1,1,2,2… and the decoder swallows repeats as more blocks of the same frame.
            // Needs `patches/0007-encoder-sequence-override.patch`.
            pw_check(
                pw::pyrowave_encoder_set_next_sequence(
                    self.pw_encs[slot],
                    seq & pw::PYROWAVE_SEQUENCE_MASK,
                ),
                "set_next_sequence",
            )?;
            let enc_res = pw::pyrowave_encoder_encode_gpu_synchronous(
                self.pw_encs[slot],
                std::ptr::null(),
                std::ptr::null(),
                &buffers,
                &rc,
            );
            pw::pyrowave_device_set_command_buffer(self.pw_dev, std::ptr::null_mut());
            pw_check(enc_res, "encode_gpu_synchronous")?;

            if let Some(pool) = ts_pool {
                dev.cmd_write_timestamp2(cmd, vk::PipelineStageFlags2::ALL_COMMANDS, pool, q0 + 2);
            }
            dev.end_command_buffer(cmd)?;
            dev.reset_fences(&[fence])?;
            let cmds = [cmd];
            cpu_ns[2] = mono_ns();
            dev.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                fence,
            )?;
            cpu_ns[3] = mono_ns();
            Ok(())
        })();
        if let Err(e) = record_and_submit {
            // SAFETY: every closure error arm is RECORDING/INVALID/EXECUTABLE — never PENDING
            // (nothing was enqueued) — and the pool allows the reset. The reset discards any
            // cursor upload recorded here, so the slot forgets it had one.
            let _ = dev.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty());
            self.slots[slot].cursor_serial = u64::MAX;
            self.slots[slot].cursor_ready = false;
            return Err(e);
        }
        // GPU may be executing: do not touch `cmd`, y/uv, or `csc_set` until retired.
        self.next_slot = (slot + 1) % SLOTS;
        // Advance only on success: a gap reads as a restart, which is right for a dropped
        // frame and wrong for one that was never emitted.
        self.wire_seq = self.wire_seq.wrapping_add(1);
        self.inflight.push_back(InFlight {
            slot,
            seq: (seq & pw::PYROWAVE_SEQUENCE_MASK) as u8,
            pts_ns: frame.pts_ns,
            cap: self.budget.bytes + BS_SLACK,
            wire_chunk: self.stream.wire_chunk,
            t0,
            cpu_ns,
            _src_hold: match &frame.payload {
                FramePayload::Dmabuf(d) => d.hold.clone(),
                _ => None,
            },
            src_key: match &frame.payload {
                FramePayload::Dmabuf(d) => pf_zerocopy::fd_identity(d.fd.as_fd()).ok(),
                _ => None,
            },
        });
        Ok(())
    }

    /// Wait the oldest in-flight fence, then packetize into `pending`. Failure does not
    /// reset the command buffer (timeout leaves PENDING —
    /// VUID-vkResetCommandBuffer-commandBuffer-00045) and does not pop the entry: that is
    /// what tells `reset()` there is still live GPU work.
    unsafe fn wait_and_packetize(&mut self) -> Result<()> {
        let Some(fr) = self.inflight.front().cloned() else {
            return Ok(());
        };
        let dev = self.device.clone();
        dev.wait_for_fences(&[self.slots[fr.slot].fence], true, 5_000_000_000)
            .context("pyrowave encode fence")?;
        let t_fence = mono_ns();
        // One-time submit: command buffer is INVALID; next `begin` may implicitly reset.
        self.inflight.pop_front();

        // Dense: boundary = whole buffer → one packet. Datagram-aligned: boundary = shard
        // payload; packets pad so a lost shard costs only those blocks. Use `fr.cap` /
        // `fr.wire_chunk`, not the live fields: bitrate/chunking can land mid-flight.
        let pkts = packetize(
            self.pw_encs[fr.slot],
            &mut self.bitstream,
            fr.cap,
            fr.wire_chunk,
            self.pq,
        )?;
        if let Some(&(offset, _)) = pkts.first() {
            // Without patch 0007 the two handles count independently and clients swallow
            // repeats. A re-vendor that loses the patch still builds. Once per process.
            if crate::pyrowave_wire::wire_sequence(&self.bitstream, offset) != Some(fr.seq) {
                static WARNED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::error!(
                        expected = fr.seq,
                        got = ?crate::pyrowave_wire::wire_sequence(&self.bitstream, offset),
                        "pyrowave: the wire sequence counter is NOT what we stamped — \
                         patches/0007-encoder-sequence-override.patch is missing or ineffective. \
                         With two alternating encoder handles this silently halves the frame rate \
                         on every client"
                    );
                }
            }
        }
        let (au, au_len) = match self.au_arena.as_mut() {
            // The worker's return buffer: the AU is laid out in place, the frame carries
            // its length and no bytes.
            Some(arena) => {
                let n = arena.build(&pkts, &self.bitstream, fr.wire_chunk)?;
                self.arena_len = Some(n);
                (Vec::new(), n)
            }
            None => {
                let au = crate::pyrowave_wire::build_au(&pkts, &self.bitstream, fr.wire_chunk);
                let n = au.len();
                (au, n)
            }
        };
        if fr.wire_chunk.is_some() {
            self.budget.observe(&pkts, au_len);
        }
        self.frame_count += 1;
        self.pending.push_back(EncodedFrame {
            data: au,
            pts_ns: fr.pts_ns,
            keyframe: true,
            recovery_anchor: false,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: fr.wire_chunk.is_some(),
        });
        if let Some(t) = self.gpu_timer.as_mut() {
            let t_pack = mono_ns();
            let mut ts = [0u64; 3];
            // The fence signaled, so all three queries are available; no WAIT flag needed.
            if dev
                .get_query_pool_results(
                    t.pool,
                    fr.slot as u32 * 3,
                    &mut ts,
                    vk::QueryResultFlags::TYPE_64,
                )
                .is_ok()
            {
                t.recalibrate(&dev);
                let span = |a: u64, b: u64| {
                    ((b.wrapping_sub(a) & t.mask) as f64 * t.period_ns / 1000.0) as u32
                };
                let us = |d: i128| (d / 1000) as i64;
                let [c0, c_pw, c_sub, c_subd] = fr.cpu_ns.map(i128::from);
                let (lag, wake) = match (t.to_cpu_ns(ts[0]), t.to_cpu_ns(ts[2])) {
                    (Some(g0), Some(g2)) => (us(g0 - c_subd), us(i128::from(t_fence) - g2)),
                    _ => (0, 0),
                };
                t.samples.push([
                    us(c_pw - c0),
                    us(c_sub - c_pw),
                    us(c_subd - c_sub),
                    lag,
                    i64::from(span(ts[0], ts[1])),
                    i64::from(span(ts[1], ts[2])),
                    wake,
                    us(i128::from(t_pack) - i128::from(t_fence)),
                ]);
            }
        }
        self.note_encode_us(fr.t0.elapsed().as_micros() as u32);
        Ok(())
    }

    unsafe fn drain_to(&mut self, keep: usize) -> Result<()> {
        while self.inflight.len() > keep {
            self.wait_and_packetize()?;
        }
        Ok(())
    }
}

impl Encoder for PyroWaveEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        // Kept above SAFETY so that comment stays attached to the block it proves.
        let t0 = std::time::Instant::now();
        // SAFETY: single-threaded encoder; both halves work on handles this struct owns.
        // `submit_frame` resets the buffer on every pre-submit failure. A fence-wait
        // failure (buffer possibly PENDING) must not reset
        // (VUID-vkResetCommandBuffer-commandBuffer-00045). Recovery idles the device first.
        unsafe {
            // At most `max_inflight - 1` still in flight, so `next_slot` is free.
            self.drain_to(self.max_inflight.saturating_sub(1))?;
            self.submit_frame(frame, t0)
        }
    }

    fn caps(&self) -> EncoderCaps {
        // Every frame is intra. Report the opened chroma: `default()` would mis-report
        // 4:4:4 as 4:2:0 and fire a spurious Welcome-chroma warn.
        EncoderCaps {
            blends_cursor: true,
            chroma_444: self.chroma444,
            ..EncoderCaps::default()
        }
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Before the fence wait: a cut still open is a caller bug, not a reason to block.
        self.stream.check_whole_poll()?;
        if self.pending.is_empty() && !self.inflight.is_empty() {
            // SAFETY: single-threaded encoder, waiting its own fence and reading its own
            // bitstream; failure leaves the entry in flight for `reset()` to re-wait.
            unsafe { self.wait_and_packetize()? };
        }
        Ok(self.pending.pop_front())
    }

    fn supports_chunked_poll(&self) -> bool {
        self.stream.supports_chunked_poll()
    }

    fn poll_chunk(&mut self) -> Result<Option<crate::AuChunk>> {
        if let Some(chunk) = self.stream.next_open() {
            return Ok(Some(chunk));
        }
        // `submit` only queues GPU work; mirror `poll`'s wait so the AU reaches `pending`.
        if self.pending.is_empty() && !self.inflight.is_empty() {
            // SAFETY: single-threaded encoder, waiting its own fence and reading its own
            // bitstream; failure leaves the entry in flight for `reset()` to re-wait.
            unsafe { self.wait_and_packetize()? };
        }
        Ok(self.pending.pop_front().and_then(|f| self.stream.cut(f)))
    }

    fn reset(&mut self) -> bool {
        // Rebuild forfeits in-flight frames, including a half-handed-out AU. Drop the
        // cursor first so the next `poll_chunk` cannot splice a dead tail onto a fresh AU.
        self.stream.reset();
        // Recreate the pyrowave encoder object only (no RC history). Bounded wait first:
        // an untimed `device_wait_idle` would park recovery on a wedged GPU. Destroying
        // the encoder under live GPU work is a use-after-free.
        if !self.inflight.is_empty() {
            // Every in-flight fence, not just the oldest: destroying under any live GPU
            // work is a use-after-free.
            let fences: Vec<vk::Fence> = self
                .inflight
                .iter()
                .map(|f| self.slots[f.slot].fence)
                .collect();
            // SAFETY: waiting this encoder's own fences under `&mut self`.
            if unsafe { self.device.wait_for_fences(&fences, true, 5_000_000_000) }.is_err() {
                tracing::error!(
                    "pyrowave: in-flight encode did not complete within the reset budget — GPU \
                     or driver wedged; in-place rebuild abandoned"
                );
                self.pending.clear();
                return false;
            }
            // Bitstream lives in the encoder about to be destroyed; GPU is done with them.
            self.inflight.clear();
        }
        // SAFETY: device idle for this encoder's work; pyrowave device outlives the encoder
        // being swapped.
        unsafe {
            self.device.device_wait_idle().ok();
            let einfo = pw::pyrowave_encoder_create_info {
                device: self.pw_dev,
                width: self.width as i32,
                height: self.height as i32,
                chroma: if self.chroma444 {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_444
                } else {
                    pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420
                },
            };
            for i in 0..SLOTS {
                pw::pyrowave_encoder_destroy(self.pw_encs[i]);
                // Null immediately: create below is fallible. `pyrowave_encoder_destroy` is
                // a plain `delete` with no null check, so `Drop` on a stale handle is a
                // double free.
                self.pw_encs[i] = std::ptr::null_mut();
                let mut enc: pw::pyrowave_encoder = std::ptr::null_mut();
                let r = pw::pyrowave_encoder_create(&einfo, &mut enc);
                if r != pw::pyrowave_result_PYROWAVE_SUCCESS {
                    tracing::error!(result = ?r, slot = i, "pyrowave: encoder rebuild failed");
                    // Stays null — `Drop` and `submit_frame` both guard on it. Drop queued AUs
                    // rather than shipping output from a dead encoder.
                    self.pending.clear();
                    return false;
                }
                self.pw_encs[i] = enc;
            }
            // Fresh handles start at 0; the client's `last_seq` does not. Keep counting so
            // a gap tells the decoder to restart.
            self.next_slot = 0;
        }
        self.pending.clear();
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        self.budget.retarget(bps);
        true
    }

    fn set_wire_chunking(&mut self, shard_payload: usize) {
        if self.stream.set_chunking(shard_payload) {
            tracing::info!(
                shard_payload,
                "pyrowave: datagram-aligned packetization on (partial-frame loss mode)"
            );
        }
    }

    fn flush(&mut self) -> Result<()> {
        // Retire submitted-but-unwaited frames so `poll`-until-`None` returns every AU.
        // SAFETY: single-threaded encoder, waiting its own fence.
        unsafe { self.drain_to(0) }
    }
}

impl Drop for PyroWaveEncoder {
    fn drop(&mut self) {
        // SAFETY: owned handles, destroyed once, GPU idled first; pyrowave objects go
        // before the VkDevice they borrow. Also `open_inner`'s unwind: a failed open runs
        // this against a partial prefix. `pyrowave_device_destroy(null)` is `delete nullptr`;
        // `vkDestroy*` of VK_NULL_HANDLE is a spec no-op; `pw_encs` are not null-safe.
        unsafe {
            self.device.device_wait_idle().ok();
            // Null when a failed `reset()` already destroyed it.
            for &e in &self.pw_encs {
                if !e.is_null() {
                    pw::pyrowave_encoder_destroy(e);
                }
            }
            pw::pyrowave_device_destroy(self.pw_dev);
            self.import_cache.clear(&self.device);
            // Failed open leaves a partial prefix; `vkDestroy*(VK_NULL_HANDLE)` is a no-op.
            for sl in std::mem::take(&mut self.slots) {
                if let Some((i, m, v, _)) = sl.cpu_img {
                    self.device.destroy_image_view(v, None);
                    self.device.destroy_image(i, None);
                    self.device.free_memory(m, None);
                }
                if let Some((b, m, _)) = sl.cpu_stage {
                    self.device.destroy_buffer(b, None);
                    self.device.free_memory(m, None);
                }
                self.device.destroy_fence(sl.fence, None);
                self.device.destroy_image_view(sl.y_view, None);
                self.device.destroy_image(sl.y_img, None);
                self.device.free_memory(sl.y_mem, None);
                self.device.destroy_image_view(sl.uv_view, None);
                self.device.destroy_image(sl.uv_img, None);
                self.device.free_memory(sl.uv_mem, None);
                self.device.destroy_image_view(sl.cursor_view, None);
                self.device.destroy_image(sl.cursor_img, None);
                self.device.free_memory(sl.cursor_mem, None);
                self.device.destroy_buffer(sl.cursor_stage, None);
                self.device.free_memory(sl.cursor_stage_mem, None);
            }
            if let Some(t) = self.gpu_timer.take() {
                self.device.destroy_query_pool(t.pool, None);
            }
            // Command buffers and descriptor sets are freed with their pools.
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_descriptor_pool(self.csc_pool, None);
            self.device.destroy_pipeline(self.csc_pipe, None);
            self.device.destroy_pipeline_layout(self.csc_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.csc_dsl, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
#[path = "pyrowave_tests.rs"]
mod tests;
