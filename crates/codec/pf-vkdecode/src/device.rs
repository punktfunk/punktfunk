//! Borrowed-device wrap: the presenter's live Vulkan handles loaded into ash
//! function tables, plus the queue-lock contract every queue submission runs under.
//!
//! Everything in [`DeviceHandles`] is BORROWED. This crate never creates or
//! destroys the instance/device — [`DecodeDevice`]'s ash wrappers are function
//! tables over foreign handles, and dropping them destroys nothing. Objects this
//! crate does create (sessions, images, buffers, pools) are destroyed by their
//! owning structs' `Drop` impls, which must run before the borrowed device dies.

use ash::vk;
use ash::vk::Handle;

/// Presenter's decode-capable device as raw integers so the type stays FFI-plain.
/// Same shape as `pf-client-core`'s `VulkanDecodeDevice`; this crate does not
/// depend on it.
///
/// Caller contract (checked where cheap, otherwise trusted):
/// - All four handles stay live for every object this crate builds from them.
/// - The instance/device were created with `VK_KHR_video_queue`,
///   `VK_KHR_video_decode_queue`, the per-codec decode extension of every decoder
///   built on the bundle, plus `synchronization2` and `timelineSemaphore`.
/// - `decode_qf`/`decode_queue_index` name a `VIDEO_DECODE_KHR` queue. Codec
///   operations are READ ([`DecodeDevice::decode_codec_ops`]), not trusted:
///   a physical-device caps query answers for hardware even when the device was
///   created without that codec's extension. `graphics_qf` is the family the
///   presenter samples on (image sharing crosses the two when they differ).
#[derive(Debug, Clone)]
pub struct DeviceHandles {
    /// Loader `PFN_vkGetInstanceProcAddr`; every other entry is resolved through it.
    pub get_instance_proc_addr: usize,
    pub instance: usize,
    pub physical_device: usize,
    pub device: usize,
    pub decode_qf: u32,
    pub decode_queue_index: u32,
    /// Presenter's graphics+present family — the other side of image sharing.
    pub graphics_qf: u32,
}

/// Serializes every `vkQueueSubmit` on the shared device. The caller supplies
/// the same lock the presenter holds around its own submits/presents; a race
/// here is `VK_ERROR_DEVICE_LOST`. Tests use [`NoopQueueLock`].
///
/// Use [`QueueSubmitGuard`] rather than calling `lock`/`unlock` by hand.
pub trait QueueLock {
    fn lock(&self);
    fn unlock(&self);
}

/// A [`QueueLock`] that guards nothing. Tests, or a decode queue no other
/// submitter shares.
#[derive(Debug, Default)]
pub struct NoopQueueLock;

impl QueueLock for NoopQueueLock {
    fn lock(&self) {}
    fn unlock(&self) {}
}

/// Holds a [`QueueLock`] for one queue submission; released on drop, including
/// unwind.
pub struct QueueSubmitGuard<'a> {
    lock: &'a dyn QueueLock,
}

impl<'a> QueueSubmitGuard<'a> {
    pub fn acquire(lock: &'a dyn QueueLock) -> Self {
        lock.lock();
        Self { lock }
    }
}

impl Drop for QueueSubmitGuard<'_> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

/// A [`DeviceHandles`] bundle that cannot host the decoder being built. Caller
/// bugs and device gaps — never stream conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceError {
    NullHandle(&'static str),
    /// Decode family advertises no op for this codec: extension missing at
    /// device create, or `decode_qf` is the wrong family. Caps queries ask the
    /// PHYSICAL device and would still green-light an unenabled codec.
    NoCodecOperation {
        family: u32,
        /// Codec name as the caller would write it (`"H.264 decode"`).
        wanted: &'static str,
    },
}

/// Device allocation failure. A memory-type miss is an error, never a fallback
/// to index 0 — a wrong type maps the wrong heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllocError {
    Vk(vk::Result),
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl From<vk::Result> for AllocError {
    fn from(r: vk::Result) -> Self {
        AllocError::Vk(r)
    }
}

/// First memory type matching `bits` and `want`. [`AllocError::NoMemoryType`]
/// when none does — never index 0.
pub(crate) fn find_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Result<u32, AllocError> {
    for i in 0..props.memory_type_count {
        if (bits & (1 << i)) != 0 && props.memory_types[i as usize].property_flags.contains(want) {
            return Ok(i);
        }
    }
    Err(AllocError::NoMemoryType {
        type_bits: bits,
        flags: want,
    })
}

/// First type matching `bits` that also carries `prefer`; else the first type
/// matching `bits` at all. Drivers may constrain `memoryTypeBits` to a heap that
/// lacks `prefer` (spec-legal for some video-session bindings), so a hard
/// `prefer` is unsatisfiable by construction. Still [`AllocError::NoMemoryType`]
/// when `bits` selects nothing. Mapped staging (bitstream ring) must not use
/// this: it needs `HOST_VISIBLE|HOST_COHERENT` as a hard property.
pub(crate) fn find_memory_type_preferring(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    prefer: vk::MemoryPropertyFlags,
) -> Result<u32, AllocError> {
    match find_memory_type(props, bits, prefer) {
        Ok(index) => Ok(index),
        Err(_) => find_memory_type(props, bits, vk::MemoryPropertyFlags::empty()),
    }
}

/// Unwind for one create → allocate → bind chain: Drop destroys whichever handles are
/// set. [`Self::disarm`] once the chain succeeds and the caller owns them.
pub(crate) struct Unwind<'a> {
    device: &'a ash::Device,
    pub(crate) buffers: Vec<vk::Buffer>,
    pub(crate) image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
}

impl<'a> Unwind<'a> {
    // unsafe-fn-no-op-ok: Drop destroys what the fields hold; what goes in is the contract.
    /// # Safety
    ///
    /// Until [`Self::disarm`], the fields hold only handles the caller created on
    /// `device` that no command buffer has referenced.
    pub(crate) unsafe fn new(device: &'a ash::Device) -> Self {
        Self {
            device,
            buffers: Vec::new(),
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
        }
    }

    pub(crate) fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for Unwind<'_> {
    fn drop(&mut self) {
        // SAFETY: `new`'s contract: each handle is null (a no-op) or fresh on `device`
        // and unreferenced. Buffers and image go before the memory bound to them;
        // freeing mapped memory unmaps it.
        unsafe {
            for &buffer in &self.buffers {
                self.device.destroy_buffer(buffer, None);
            }
            self.device.destroy_image(self.image, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceError::NullHandle(which) => {
                write!(f, "DeviceHandles.{which} is null — a half-filled bundle")
            }
            DeviceError::NoCodecOperation { family, wanted } => {
                write!(
                    f,
                    "decode queue family {family} advertises no {wanted} operation \
                     (extension not enabled on the device, or the wrong family)"
                )
            }
        }
    }
}

impl std::error::Error for DeviceError {}

/// Borrowed device with ash tables loaded. Clone is cheap (function-pointer
/// structs) and shares the same borrowed handles under the same liveness contract.
#[derive(Clone)]
pub struct DecodeDevice {
    instance: ash::Instance,
    device: ash::Device,
    physical_device: vk::PhysicalDevice,
    video_queue_instance: ash::khr::video_queue::Instance,
    video_queue: ash::khr::video_queue::Device,
    video_decode_queue: ash::khr::video_decode_queue::Device,
    decode_queue: vk::Queue,
    decode_qf: u32,
    graphics_qf: u32,
    /// Decode family advertises `queryResultStatusSupport`. FALSE means skip
    /// RESULT_STATUS queries entirely — recording one hangs the VCN ring (RADV);
    /// fall back to timeline-completion verdicts.
    result_status_queries: bool,
    /// Decode family's `VkQueueFamilyVideoPropertiesKHR::videoCodecOperations`.
    /// `vkGetPhysicalDeviceVideoCapabilitiesKHR` is a PHYSICAL-device query and
    /// succeeds whether or not the VkDevice enabled that codec's extension, so
    /// caps alone would walk into `vkCreateVideoSessionKHR` on an unenabled
    /// codec. Empty bits: this family decodes nothing; the decoder refuses.
    decode_codec_ops: vk::VideoCodecOperationFlagsKHR,
}

impl DecodeDevice {
    /// Load ash function tables over the borrowed handles.
    ///
    /// # Safety
    ///
    /// The full [`DeviceHandles`] caller contract: live handles (outliving `self`
    /// and everything created through it), the video-decode extensions/features
    /// enabled at creation, and truthful queue-family fields. Null handles are
    /// rejected here; everything else cannot be checked and is trusted.
    pub unsafe fn wrap(handles: &DeviceHandles) -> Result<Self, DeviceError> {
        if handles.get_instance_proc_addr == 0 {
            return Err(DeviceError::NullHandle("get_instance_proc_addr"));
        }
        if handles.instance == 0 {
            return Err(DeviceError::NullHandle("instance"));
        }
        if handles.physical_device == 0 {
            return Err(DeviceError::NullHandle("physical_device"));
        }
        if handles.device == 0 {
            return Err(DeviceError::NullHandle("device"));
        }

        // SAFETY: the usize is non-zero (checked above) and the caller contract says
        // it is the loader's PFN_vkGetInstanceProcAddr; fn pointers and usize share
        // size/ABI on every supported target.
        let gipa: vk::PFN_vkGetInstanceProcAddr = unsafe {
            std::mem::transmute::<usize, vk::PFN_vkGetInstanceProcAddr>(
                handles.get_instance_proc_addr,
            )
        };
        // SAFETY: `gipa` is a valid Vulkan-1.0-conformant loader entry point per the
        // caller contract, valid for the returned Entry's lifetime (handle liveness).
        let entry = unsafe {
            ash::Entry::from_static_fn(ash::StaticFn {
                get_instance_proc_addr: gipa,
            })
        };
        // SAFETY: `handles.instance` is a live VkInstance created through this very
        // loader (caller contract), so loading instance-level functions against it
        // is exactly the ash::Instance::load contract.
        let instance = unsafe {
            ash::Instance::load(
                entry.static_fn(),
                vk::Instance::from_raw(handles.instance as u64),
            )
        };
        // SAFETY: `handles.device` is a live VkDevice of that instance (caller
        // contract) — the ash::Device::load contract.
        let device = unsafe {
            ash::Device::load(
                instance.fp_v1_0(),
                vk::Device::from_raw(handles.device as u64),
            )
        };
        let video_queue_instance = ash::khr::video_queue::Instance::new(&entry, &instance);
        let video_queue = ash::khr::video_queue::Device::new(&instance, &device);
        let video_decode_queue = ash::khr::video_decode_queue::Device::new(&instance, &device);
        // SAFETY: the caller contract guarantees `decode_qf`/`decode_queue_index`
        // name a queue the device was created with.
        let decode_queue =
            unsafe { device.get_device_queue(handles.decode_qf, handles.decode_queue_index) };

        // One query fills both per-family facts. An out-of-range decode family
        // answers no to both; the codec check then refuses the decoder.
        let physical_device = vk::PhysicalDevice::from_raw(handles.physical_device as u64);
        // SAFETY: live physical device (caller contract); the two-call form fills
        // the chained per-family structs.
        let family_count =
            unsafe { instance.get_physical_device_queue_family_properties2_len(physical_device) };
        let (result_status_queries, decode_codec_ops) = if (handles.decode_qf as usize)
            < family_count
        {
            let mut status_props =
                vec![vk::QueueFamilyQueryResultStatusPropertiesKHR::default(); family_count];
            let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); family_count];
            let mut families: Vec<vk::QueueFamilyProperties2<'_>> = status_props
                .iter_mut()
                .zip(video_props.iter_mut())
                .map(|(status, video)| {
                    vk::QueueFamilyProperties2::default()
                        .push_next(status)
                        .push_next(video)
                })
                .collect();
            // SAFETY: as above, arrays sized to the reported count.
            unsafe {
                instance
                    .get_physical_device_queue_family_properties2(physical_device, &mut families)
            };
            drop(families);
            let family = handles.decode_qf as usize;
            (
                status_props[family].query_result_status_support != vk::FALSE,
                video_props[family].video_codec_operations,
            )
        } else {
            (false, vk::VideoCodecOperationFlagsKHR::NONE)
        };

        // `entry` was only the load ladder; ash tables own their function pointers.
        drop(entry);

        Ok(Self {
            instance,
            device,
            physical_device,
            video_queue_instance,
            video_queue,
            video_decode_queue,
            decode_queue,
            decode_qf: handles.decode_qf,
            graphics_qf: handles.graphics_qf,
            result_status_queries,
            decode_codec_ops,
        })
    }

    pub(crate) fn ash(&self) -> &ash::Device {
        &self.device
    }

    /// `VK_KHR_external_memory_fd` entry points. The caller enabled the extension when
    /// it asked for a dma-buf export; without it the calls fail cleanly.
    #[cfg(unix)]
    pub(crate) fn external_memory_fd(&self) -> ash::khr::external_memory_fd::Device {
        ash::khr::external_memory_fd::Device::new(&self.instance, &self.device)
    }

    pub(crate) fn result_status_queries(&self) -> bool {
        self.result_status_queries
    }

    pub fn decode_codec_ops(&self) -> vk::VideoCodecOperationFlagsKHR {
        self.decode_codec_ops
    }

    /// Refuse unless the decode family advertises `op`. Call before any caps
    /// query: those answer for the hardware and would green-light a codec the
    /// VkDevice never enabled.
    pub(crate) fn require_codec_op(
        &self,
        op: vk::VideoCodecOperationFlagsKHR,
        what: &'static str,
    ) -> Result<(), DeviceError> {
        if self.decode_codec_ops.contains(op) {
            Ok(())
        } else {
            Err(DeviceError::NoCodecOperation {
                family: self.decode_qf,
                wanted: what,
            })
        }
    }

    pub(crate) fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    pub(crate) fn video_queue_instance(&self) -> &ash::khr::video_queue::Instance {
        &self.video_queue_instance
    }

    pub(crate) fn video_queue(&self) -> &ash::khr::video_queue::Device {
        &self.video_queue
    }

    pub(crate) fn video_decode_queue(&self) -> &ash::khr::video_decode_queue::Device {
        &self.video_decode_queue
    }

    pub(crate) fn decode_queue(&self) -> vk::Queue {
        self.decode_queue
    }

    pub(crate) fn decode_qf(&self) -> u32 {
        self.decode_qf
    }

    /// Families image sharing spans: empty (EXCLUSIVE) when decode and graphics
    /// are one family, both otherwise (CONCURRENT). Per-frame ownership transfers
    /// would add latency for nothing at these frame rates.
    pub(crate) fn sharing_families(&self) -> Vec<u32> {
        if self.decode_qf == self.graphics_qf {
            Vec::new()
        } else {
            vec![self.decode_qf, self.graphics_qf]
        }
    }

    /// Device memory properties, queried fresh (cheap, stateless).
    pub(crate) fn memory_properties(&self) -> vk::PhysicalDeviceMemoryProperties {
        // SAFETY: `physical_device` is live per the DeviceHandles contract; the call
        // fills a plain struct and touches nothing else.
        unsafe {
            self.instance
                .get_physical_device_memory_properties(self.physical_device)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_filled_bundle_is_rejected_before_any_ffi() {
        let mut handles = DeviceHandles {
            get_instance_proc_addr: 0,
            instance: 1,
            physical_device: 1,
            device: 1,
            decode_qf: 0,
            decode_queue_index: 0,
            graphics_qf: 0,
        };
        // SAFETY: wrap rejects the null handle before making any Vulkan call, so no
        // part of the liveness contract is exercised. (`Err` matched by hand: the
        // Ok side holds ash tables, which carry no Debug for unwrap_err.)
        let result = unsafe { DecodeDevice::wrap(&handles) };
        let Err(err) = result else {
            panic!("a null gipa must be rejected")
        };
        assert_eq!(err, DeviceError::NullHandle("get_instance_proc_addr"));

        handles.get_instance_proc_addr = 1;
        handles.device = 0;
        // SAFETY: as above — the null device handle is rejected before any FFI.
        let result = unsafe { DecodeDevice::wrap(&handles) };
        let Err(err) = result else {
            panic!("a null device must be rejected")
        };
        assert_eq!(err, DeviceError::NullHandle("device"));
    }

    #[test]
    fn a_memory_type_miss_is_an_error_never_a_fallback_to_index_zero() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 2,
            ..Default::default()
        };
        props.memory_types[0].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[1].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;

        assert_eq!(
            find_memory_type(
                &props,
                0b11,
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            ),
            Ok(1)
        );
        assert_eq!(
            find_memory_type(&props, 0b01, vk::MemoryPropertyFlags::HOST_VISIBLE),
            Err(AllocError::NoMemoryType {
                type_bits: 0b01,
                flags: vk::MemoryPropertyFlags::HOST_VISIBLE
            })
        );
        assert_eq!(
            find_memory_type(&props, 0b11, vk::MemoryPropertyFlags::PROTECTED),
            Err(AllocError::NoMemoryType {
                type_bits: 0b11,
                flags: vk::MemoryPropertyFlags::PROTECTED
            })
        );
    }

    #[test]
    fn preferring_picks_the_preferred_type_and_falls_back_inside_the_bits() {
        let mut props = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: 4,
            ..Default::default()
        };
        props.memory_types[0].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        props.memory_types[1].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        props.memory_types[3].property_flags =
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;

        assert_eq!(
            find_memory_type_preferring(&props, 0b0011, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Ok(1)
        );
        // `memoryTypeBits` names only a host-visible type: honor the bits.
        assert_eq!(
            find_memory_type_preferring(&props, 0b1000, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Ok(3)
        );
        assert_eq!(
            find_memory_type_preferring(&props, 0b0000, vk::MemoryPropertyFlags::DEVICE_LOCAL),
            Err(AllocError::NoMemoryType {
                type_bits: 0b0000,
                flags: vk::MemoryPropertyFlags::empty()
            })
        );
    }

    #[test]
    fn the_queue_submit_guard_brackets_the_lock() {
        use std::sync::atomic::AtomicI32;
        use std::sync::atomic::Ordering;

        #[derive(Default)]
        struct CountingLock {
            depth: AtomicI32,
            peak: AtomicI32,
        }
        impl QueueLock for CountingLock {
            fn lock(&self) {
                let d = self.depth.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(d, Ordering::SeqCst);
            }
            fn unlock(&self) {
                self.depth.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let lock = CountingLock::default();
        {
            let _guard = QueueSubmitGuard::acquire(&lock);
            assert_eq!(lock.depth.load(Ordering::SeqCst), 1);
        }
        assert_eq!(lock.depth.load(Ordering::SeqCst), 0, "released on drop");
        assert_eq!(lock.peak.load(Ordering::SeqCst), 1);
    }
}
