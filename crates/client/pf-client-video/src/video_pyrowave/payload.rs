//! The payload ring: memory the client's reassembler writes access units into and the decoder
//! reads them from where they lie (vendor patch 0011), so no AU byte is copied after decrypt.
//!
//! One host-coherent buffer, mapped for its whole life, with three linear images aliasing it for
//! GPUs that read textures faster than texel buffers. [`HEAD`] bytes take pyrowave's own copies
//! (this decoder makes none), then [`SLOTS`] slots of one AU each. A lent slot keeps the ring,
//! and the ring keeps the device; the last holder frees it.

use anyhow::{Context as _, Result};
use ash::vk;
use ash::vk::Handle as _;
use punktfunk_core::au::{AuMemory, AuStore};
use pyrowave_sys as pw;
use std::any::Any;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

/// Bytes pyrowave's copying push lands in. Unused here, but its upload must fit.
pub(super) const HEAD: usize = 64 * 1024;
/// Past the last slot: the dequant reads up to 4 KiB past a packet's start, and an in-place
/// push refuses packets closer than that to the end (vendor patch 0015).
const TAIL: usize = 64 * 1024;
/// AUs in flight at once: a few reassembling, one queued, the decoder's unfinished ones.
const SLOTS: usize = 8;
/// Linear image row: 4096 R8, 2048 R16 or 1024 R32 texels.
const ROW: usize = 4096;

/// Room for one AU at `width`×`height`: 6 bits a pixel at 4:2:0, 9 at 4:4:4. That is the
/// host's `pyrowave_bpp` maximum (×1.625 at 4:4:4, `kbps_for`) plus window padding.
pub(super) fn au_capacity(width: u32, height: u32, chroma444: bool) -> usize {
    let px = width as usize * height as usize;
    let bytes = if chroma444 { px * 9 / 8 } else { px * 3 / 4 };
    bytes.div_ceil(ROW) * ROW
}

pub(crate) struct PayloadRing {
    device: ash::Device,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// R8, R16, R32 linear images over the buffer, or all null.
    linear: [vk::Image; 3],
    rows: u32,
    host: NonNull<u8>,
    size: usize,
    /// Bytes per slot.
    pub(super) au_cap: usize,
    free: Mutex<Vec<usize>>,
    /// Holds the device alive past the decoder, until the last lent slot is back.
    device_keepalive: Arc<dyn Any + Send + Sync>,
}

// SAFETY: `host` is the buffer's mapping, valid while the ring lives. Each slot is handed to one
// `RingSlot` at a time through `free`, and only that holder touches the slot's bytes.
unsafe impl Send for PayloadRing {}
// SAFETY: as above; the only shared mutable state is `free`, behind its mutex.
unsafe impl Sync for PayloadRing {}

impl PayloadRing {
    /// # Safety
    ///
    /// `device` is live and `keepalive` keeps it alive until the ring drops; `mem_props` are
    /// its physical device's and `max_dim` its `maxImageDimension2D`.
    pub(super) unsafe fn new(
        device: &ash::Device,
        mem_props: &vk::PhysicalDeviceMemoryProperties,
        max_dim: u32,
        au_cap: usize,
        keepalive: Arc<dyn Any + Send + Sync>,
    ) -> Result<Arc<PayloadRing>> {
        let size = (HEAD + SLOTS * au_cap + TAIL).div_ceil(ROW) * ROW;
        let rows = u32::try_from(size / ROW).unwrap_or(u32::MAX);
        let mut ring = PayloadRing {
            device: device.clone(),
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            linear: [vk::Image::null(); 3],
            rows,
            host: NonNull::dangling(),
            size,
            au_cap,
            free: Mutex::new((0..SLOTS).rev().collect()),
            device_keepalive: keepalive,
        };
        // SAFETY: fn contract; every create-info is a local. On error `ring` drops and frees
        // whatever was made, null handles being no-ops.
        unsafe {
            ring.buffer = device.create_buffer(
                &vk::BufferCreateInfo::default().size(size as u64).usage(
                    vk::BufferUsageFlags::STORAGE_BUFFER
                        | vk::BufferUsageFlags::UNIFORM_TEXEL_BUFFER
                        | vk::BufferUsageFlags::TRANSFER_DST,
                ),
                None,
            )?;
            if rows <= max_dim {
                for (i, (fmt, width)) in [
                    (vk::Format::R8_UINT, 4096),
                    (vk::Format::R16_UINT, 2048),
                    (vk::Format::R32_UINT, 1024),
                ]
                .into_iter()
                .enumerate()
                {
                    ring.linear[i] = device
                        .create_image(
                            &vk::ImageCreateInfo::default()
                                .image_type(vk::ImageType::TYPE_2D)
                                .format(fmt)
                                .extent(vk::Extent3D {
                                    width,
                                    height: rows,
                                    depth: 1,
                                })
                                .mip_levels(1)
                                .array_layers(1)
                                .samples(vk::SampleCountFlags::TYPE_1)
                                .tiling(vk::ImageTiling::LINEAR)
                                .usage(vk::ImageUsageFlags::SAMPLED)
                                .initial_layout(vk::ImageLayout::PREINITIALIZED),
                            None,
                        )
                        .unwrap_or(vk::Image::null());
                }
                // pyrowave addresses a texel by `offset / row`, so rows must lie back to back.
                let packed = ring.linear.iter().all(|&img| {
                    !img.is_null() && {
                        let layout = device.get_image_subresource_layout(
                            img,
                            vk::ImageSubresource {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                mip_level: 0,
                                array_layer: 0,
                            },
                        );
                        layout.offset == 0 && layout.row_pitch == ROW as u64
                    }
                });
                if !packed {
                    ring.drop_linear();
                }
            }
            let mut req = device.get_buffer_memory_requirements(ring.buffer);
            for &img in ring.linear.iter().filter(|i| !i.is_null()) {
                let r = device.get_image_memory_requirements(img);
                req.size = req.size.max(r.size);
                req.alignment = req.alignment.max(r.alignment);
                req.memory_type_bits &= r.memory_type_bits;
            }
            let mut type_index = host_coherent_type(mem_props, req.memory_type_bits);
            if type_index.is_none() {
                // The images narrowed the types past a coherent one: keep the buffer alone.
                ring.drop_linear();
                req = device.get_buffer_memory_requirements(ring.buffer);
                type_index = host_coherent_type(mem_props, req.memory_type_bits);
            }
            let type_index =
                type_index.context("no host-coherent memory type for the PyroWave payload ring")?;
            ring.memory = device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(type_index),
                None,
            )?;
            device.bind_buffer_memory(ring.buffer, ring.memory, 0)?;
            for &img in ring.linear.iter().filter(|i| !i.is_null()) {
                device.bind_image_memory(img, ring.memory, 0)?;
            }
            let host =
                device.map_memory(ring.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
            ring.host = NonNull::new(host.cast::<u8>()).context("payload ring mapped at null")?;
        }
        Ok(Arc::new(ring))
    }

    /// # Safety
    ///
    /// The images are unbound or idle.
    unsafe fn drop_linear(&mut self) {
        for img in &mut self.linear {
            // SAFETY: fn contract; null is a no-op.
            unsafe { self.device.destroy_image(*img, None) };
            *img = vk::Image::null();
        }
    }

    /// What keeps the device alive, for a bigger ring after a resize.
    pub(super) fn keepalive(&self) -> Arc<dyn Any + Send + Sync> {
        self.device_keepalive.clone()
    }

    /// What pyrowave wraps for its payload.
    pub(super) fn as_pw(&self) -> pw::pyrowave_payload_memory {
        pw::pyrowave_payload_memory {
            buffer: pw_handle(self.buffer.as_raw()),
            size: self.size as u64,
            copy_reserve: HEAD as u64,
            host: self.host.as_ptr().cast(),
            linear: self.linear.map(|i| pw_handle(i.as_raw())),
            rows: self.rows,
        }
    }

    fn slot_ptr(&self, index: usize) -> *mut u8 {
        // SAFETY: `index < SLOTS`, so the slot lies inside the `size`-byte mapping.
        unsafe { self.host.as_ptr().add(HEAD + index * self.au_cap) }
    }
}

/// The ring's lowest-overhead host-coherent memory type: cached and device-local first.
fn host_coherent_type(props: &vk::PhysicalDeviceMemoryProperties, bits: u32) -> Option<u32> {
    use vk::MemoryPropertyFlags as F;
    let base = F::HOST_VISIBLE | F::HOST_COHERENT;
    [
        base | F::HOST_CACHED | F::DEVICE_LOCAL,
        base | F::HOST_CACHED,
        base | F::DEVICE_LOCAL,
        base,
    ]
    .into_iter()
    .find_map(|want| {
        (0..props.memory_type_count).find(|&i| {
            bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want)
        })
    })
}

impl Drop for PayloadRing {
    fn drop(&mut self) {
        // SAFETY: made on `device`, which `keepalive` holds alive. The decoder's last submit that
        // read the ring finished before the decoder let go, and no slot is lent any more.
        unsafe {
            self.drop_linear();
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
        }
    }
}

/// Lends ring slots to a session's reassembler.
pub(crate) struct RingMemory(pub(super) Arc<PayloadRing>);

impl AuMemory for RingMemory {
    fn take(&self, len: usize) -> Option<Box<dyn AuStore>> {
        if len > self.0.au_cap {
            return None;
        }
        let index = self
            .0
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()?;
        Some(Box::new(RingSlot {
            ring: self.0.clone(),
            index,
        }))
    }
}

/// One lent slot: its bytes are the store.
pub(crate) struct RingSlot {
    ring: Arc<PayloadRing>,
    index: usize,
}

impl RingSlot {
    pub(super) fn of(&self, ring: &Arc<PayloadRing>) -> bool {
        Arc::ptr_eq(&self.ring, ring)
    }
}

impl AuStore for RingSlot {
    fn bytes(&self) -> &[u8] {
        // SAFETY: this holder owns the slot's `au_cap` bytes inside the mapping.
        unsafe { std::slice::from_raw_parts(self.ring.slot_ptr(self.index), self.ring.au_cap) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.ring.slot_ptr(self.index), self.ring.au_cap) }
    }
}

impl Drop for RingSlot {
    fn drop(&mut self) {
        self.ring
            .free
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(self.index);
    }
}

/// A non-dispatchable handle as the bindings spell it: a pointer on 64-bit targets, `u64`
/// on 32-bit ones, where a detour through `usize` drops the upper half.
#[cfg(target_pointer_width = "64")]
pub(super) fn pw_handle<T>(raw: u64) -> *mut T {
    raw as usize as *mut T
}
#[cfg(not(target_pointer_width = "64"))]
pub(super) fn pw_handle(raw: u64) -> u64 {
    raw
}
