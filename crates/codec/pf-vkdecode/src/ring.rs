//! Host-visible bitstream upload ring: one persistent-mapped allocation cut into
//! equal slots, honouring the profile's `minBitstreamBufferOffsetAlignment`/`SizeAlignment`.
//!
//! Each slot is its own `VIDEO_DECODE_SRC` buffer, so every decode reads at
//! `srcBufferOffset` 0. Mesa ANV's H.265 slice parse maps `srcBufferOffset + range`
//! bytes and unmaps `range`: a nonzero offset leaks one mapping per picture until
//! `vm.max_map_count` runs out.
//!
//! [`RingLayout`] and [`SlotStates`] are the unit-tested halves (offset math
//! including growth, and recycle bookkeeping). [`BitstreamRing`] allocates the
//! buffer and copies AU bytes. A slot recycles when the timeline value of the
//! submit that consumed it completes. An AU larger than the slot size recreates
//! the buffer after draining every in-flight slot — a stall there beats
//! permanently oversized slots. Evidence: `mod tests`.

use ash::vk;
use tracing::debug;

use crate::caps::DecodeProfile;
use crate::device::find_memory_type;
use crate::device::AllocError;
use crate::device::DecodeDevice;
use crate::device::Unwind;

/// 2 MiB covers a 4K IDR at streaming rates; a larger AU grows the ring.
pub const INITIAL_SLOT_SIZE: u64 = 2 * 1024 * 1024;
/// Enough in-flight uploads; pipeline depth is the output/query rings, not this.
pub const RING_SLOTS: u32 = 4;

const fn align_up(x: u64, align: u64) -> u64 {
    (x + align - 1) & !(align - 1)
}

/// Slot geometry. Both Vulkan alignments are powers of two; slot size is a
/// multiple of both, so every offset and every full-slot range satisfies both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingLayout {
    pub slot_size: u64,
    pub slots: u32,
    pub offset_alignment: u64,
    pub size_alignment: u64,
}

impl RingLayout {
    pub fn new(min_slot_size: u64, slots: u32, offset_alignment: u64, size_alignment: u64) -> Self {
        debug_assert!(
            offset_alignment.is_power_of_two() && size_alignment.is_power_of_two(),
            "Vulkan alignment values are powers of two"
        );
        debug_assert!(slots > 0 && min_slot_size > 0);
        let align = offset_alignment.max(size_alignment);
        Self {
            slot_size: align_up(min_slot_size, align),
            slots,
            offset_alignment,
            size_alignment,
        }
    }

    pub fn offset_of(&self, slot: u32) -> u64 {
        debug_assert!(slot < self.slots);
        u64::from(slot) * self.slot_size
    }

    pub fn fits(&self, len: u64) -> bool {
        self.record_range(len) <= self.slot_size
    }

    /// `srcBufferRange` for `len` bytes: rounded up to `minBitstreamBufferSizeAlignment`.
    pub fn record_range(&self, len: u64) -> u64 {
        align_up(len, self.size_alignment)
    }

    pub fn buffer_size(&self) -> u64 {
        self.slot_size * u64::from(self.slots)
    }

    /// Double slot size until `len` fits. One recreation per size class, not per AU.
    pub fn grown_for(&self, len: u64) -> Self {
        let mut slot = self.slot_size.max(1);
        while align_up(len, self.size_alignment) > slot {
            slot *= 2;
        }
        Self::new(slot, self.slots, self.offset_alignment, self.size_alignment)
    }
}

/// One AU's slice NALUs as they sit in a slot: the ranges to concatenate, and
/// the offsets those ranges land at.
///
/// [`pack_slices`] produces both together. An offset that does not land on the
/// byte the matching segment starts at points the hardware at the middle of
/// another slice — silent corruption, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackedSlices {
    /// AU ranges, each starting at a three-byte Annex-B start code.
    pub(crate) segments: Vec<std::ops::Range<usize>>,
    /// `pSliceOffsets` / `pSliceSegmentOffsets` once concatenated — one per segment.
    pub(crate) offsets: Vec<u32>,
}

/// Pack the slice NAL ranges `nals` into one slot, each with the three bytes
/// before it, offsets rebased out of AU coordinates.
///
/// Vulkan slice offsets are consumed by drivers written against libavcodec's
/// `ff_vk_decode_add_slice`, which writes `{ 0x00, 0x00, 0x01 }` before each
/// slice. NVIDIA then reads the slice header at `offset + 3 + 2`, so a four-byte
/// Annex-B prefix would shift it. The three bytes before a NAL header are
/// always `00 00 01`, so the upload stays zero-copy.
///
/// The bitstream buffer carries slice NALUs only (non-VCL NALUs in the submitted
/// range hang VCN firmware). Both planners hand out AU-relative offsets, so
/// submitting them unchanged would point at bytes that were never uploaded.
/// [`crate::DecodePlanVkH265::slice_offsets`] is therefore not submission-final.
///
/// Offsets are `u32` because Vulkan's are. The sum is taken in `u64` so overflow
/// is a real check; 4 GiB of slice data cannot fit any slot this crate allocates.
/// A NAL starting before byte 3 has no start code and is `None` too.
pub(crate) fn pack_slices(nals: &[std::ops::Range<usize>]) -> Option<PackedSlices> {
    let mut packed = Vec::with_capacity(nals.len());
    let mut offsets = Vec::with_capacity(nals.len());
    let mut cursor: u64 = 0;
    for nal in nals {
        let segment = nal.start.checked_sub(3)?..nal.end;
        offsets.push(u32::try_from(cursor).ok()?);
        cursor += segment.len() as u64;
        packed.push(segment);
    }
    Some(PackedSlices {
        segments: packed,
        offsets,
    })
}

/// One AU's AV1 tile payloads as they sit in a slot: the ranges to concatenate,
/// and the offset each lands at.
///
/// A separate type from [`PackedSlices`]: an Annex-B slice has its start-code
/// prefix normalised, and an AV1 tile must not be touched. AV1 has no start
/// codes — a tile may legitimately begin `00 00 00`, and trimming those would
/// silently shorten the tile the driver decodes.
///
/// Segments are the raw tile payloads, not the OBUs that carried them. The
/// bitstream buffer holds nothing else (see [`crate::decoder_av1`]), so
/// `offsets[i]` is tile `i`'s `pTileOffsets` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackedAv1Tiles {
    /// Verbatim AU ranges, in order.
    pub(crate) segments: Vec<std::ops::Range<usize>>,
    pub(crate) offsets: Vec<u32>,
}

/// Pack AV1 `tiles` into one slot: verbatim, with the offset each lands at.
///
/// Same AU-relative rebase as [`pack_slices`]; no prefix normalisation (see
/// [`PackedAv1Tiles`]). Offsets are `u32` because Vulkan's are; the sum is
/// `u64` so overflow is a real check.
pub(crate) fn pack_av1_tiles(tiles: &[std::ops::Range<usize>]) -> Option<PackedAv1Tiles> {
    let mut offsets = Vec::with_capacity(tiles.len());
    let mut cursor: u64 = 0;
    for tile in tiles {
        offsets.push(u32::try_from(cursor).ok()?);
        cursor += tile.len() as u64;
    }
    // The last segment's end must be expressible: `pTileSizes` and
    // `srcBufferRange` are read against it.
    u32::try_from(cursor).ok()?;
    Some(PackedAv1Tiles {
        segments: tiles.to_vec(),
        offsets,
    })
}

/// Concatenate `segments` of `au` into `dst`, zeroing the tail.
///
/// `dst` is a whole recorded `srcBufferRange` (packed length rounded up to
/// `minBitstreamBufferSizeAlignment`). An unzeroed tail would hand the driver
/// the previous AU's bytes past this one's end. Shared with
/// [`BitstreamRing::upload`] so CPU tests assert the bytes the ring receives.
///
/// # Panics
///
/// If `segments` are out of bounds of `au`, or their total length exceeds `dst`
/// — both caller invariants [`BitstreamRing::upload`] establishes from the layout.
pub(crate) fn pack_into(dst: &mut [u8], au: &[u8], segments: &[std::ops::Range<usize>]) {
    let mut cursor = 0usize;
    for segment in segments {
        let bytes = &au[segment.clone()];
        dst[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        cursor += bytes.len();
    }
    dst[cursor..].fill(0);
}

/// Recycle bookkeeping: which slots are free, which carry an in-flight token.
/// Generic over the token so FIFO/recycle is testable without a device
/// (`T = (vk::Semaphore, u64)` on the ring).
#[derive(Debug)]
pub(crate) struct SlotStates<T> {
    pending: Vec<Option<T>>,
    /// Round-robin: the slot at the cursor is the oldest in-flight one.
    cursor: usize,
}

impl<T> SlotStates<T> {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            pending: (0..slots).map(|_| None).collect(),
            cursor: 0,
        }
    }

    /// Next slot in round-robin order. If the slot still carries a token,
    /// `is_done` of true frees it; false yields `Ok(None)` so the caller waits
    /// on [`Self::oldest`] and retries. Errors pass through.
    pub(crate) fn acquire<E>(
        &mut self,
        mut is_done: impl FnMut(&T) -> Result<bool, E>,
    ) -> Result<Option<usize>, E> {
        let slot = self.cursor;
        if let Some(token) = &self.pending[slot] {
            if !is_done(token)? {
                return Ok(None);
            }
            self.pending[slot] = None;
        }
        self.cursor = (self.cursor + 1) % self.pending.len();
        Ok(Some(slot))
    }

    /// The token blocking [`Self::acquire`], if any.
    pub(crate) fn oldest(&self) -> Option<&T> {
        self.pending[self.cursor].as_ref()
    }

    pub(crate) fn set_pending(&mut self, slot: usize, token: T) {
        debug_assert!(
            self.pending[slot].is_none(),
            "slot handed out while pending"
        );
        self.pending[slot] = Some(token);
    }

    /// Drain-before-recreate walks these.
    pub(crate) fn in_flight(&self) -> impl Iterator<Item = &T> {
        self.pending.iter().filter_map(Option::as_ref)
    }

    /// Forget every token after the caller has drained them.
    pub(crate) fn clear(&mut self) {
        for p in &mut self.pending {
            *p = None;
        }
        self.cursor = 0;
    }
}

/// One uploaded AU: what `vkCmdDecodeVideoKHR` needs (the slot's buffer, read
/// from offset 0), plus the slot to mark pending once the submit's timeline
/// token exists.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UploadedAu {
    pub buffer: vk::Buffer,
    pub range: u64,
    pub slot: usize,
}

/// Timeline `(semaphore, value)` signalled by the submit that consumed the slot.
pub(crate) type Token = (vk::Semaphore, u64);

/// One ring backing: a buffer per slot, their memory, the mapping, and the export if asked.
struct Backing {
    buffers: Vec<vk::Buffer>,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    #[cfg(unix)]
    dmabuf: Option<std::os::fd::OwnedFd>,
}

/// Slot buffers + memory + persistent map. The spec requires a src buffer to be
/// profile-listed.
pub(crate) struct BitstreamRing {
    device: ash::Device,
    layout: RingLayout,
    profile: DecodeProfile,
    /// `buffers[slot]` is bound at `layout.offset_of(slot)`.
    buffers: Vec<vk::Buffer>,
    memory: vk::DeviceMemory,
    ptr: *mut u8,
    /// Export the memory as a dma-buf ([`Self::dmabuf_fd`]).
    export: bool,
    #[cfg(unix)]
    dmabuf: Option<std::os::fd::OwnedFd>,
    pub(crate) pending: SlotStates<Token>,
}

impl BitstreamRing {
    /// `export`: also hand the memory out as a dma-buf, so the owner can wait the decode
    /// through the kernel's own GEM interface. Needs `VK_KHR_external_memory_fd` on the
    /// device; a refused export leaves [`Self::dmabuf_fd`] `None`.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        layout: RingLayout,
        profile: DecodeProfile,
        export: bool,
    ) -> Result<Self, AllocError> {
        // SAFETY: live device; allocate_backing only creates objects it returns.
        let backing = unsafe { Self::allocate_backing(dev, &layout, profile, export)? };
        Ok(Self {
            device: dev.ash().clone(),
            layout,
            profile,
            buffers: backing.buffers,
            memory: backing.memory,
            ptr: backing.ptr,
            export,
            #[cfg(unix)]
            dmabuf: backing.dmabuf,
            pending: SlotStates::new(layout.slots as usize),
        })
    }

    /// The memory as a dma-buf, while this backing lives. A grown ring has a new one.
    #[cfg(unix)]
    pub(crate) fn dmabuf_fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd as _;
        self.dmabuf.as_ref().map(|fd| fd.as_raw_fd())
    }

    /// # Safety
    ///
    /// As [`Self::create`].
    unsafe fn allocate_backing(
        dev: &DecodeDevice,
        layout: &RingLayout,
        decode_profile: DecodeProfile,
        export: bool,
    ) -> Result<Backing, AllocError> {
        let mut chain = decode_profile.chain();
        let profile = chain.wire();
        let mut profile_list =
            vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
        let mut external = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut ci = vk::BufferCreateInfo::default()
            .size(layout.slot_size)
            .usage(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut profile_list);
        if export {
            ci = ci.push_next(&mut external);
        }
        // SAFETY: only the buffers and memory created below go in, before any use.
        let mut unwind = unsafe { Unwind::new(dev.ash()) };
        for _ in 0..layout.slots {
            // SAFETY: live device; `ci` roots a chain of locals outliving the call.
            unwind
                .buffers
                .push(unsafe { dev.ash().create_buffer(&ci, None)? });
        }
        // SAFETY: the buffer was just created on this device. Identical create
        // infos give identical requirements.
        let req = unsafe { dev.ash().get_buffer_memory_requirements(unwind.buffers[0]) };
        // Slot sizes are powers of two from 2 MiB, so any sane alignment divides them.
        if layout.slot_size % req.alignment != 0 {
            return Err(AllocError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
        }
        let type_index = find_memory_type(
            &dev.memory_properties(),
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let mut exportable = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let last = layout.slots - 1;
        let mut alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(layout.offset_of(last) + req.size)
            .memory_type_index(type_index);
        if export {
            alloc = alloc.push_next(&mut exportable);
        }
        // SAFETY: live device; `alloc` roots locals outliving the call.
        let memory = unsafe { dev.ash().allocate_memory(&alloc, None)? };
        unwind.memory = memory;
        for (slot, &buffer) in (0..).zip(&unwind.buffers) {
            // SAFETY: fresh buffer + fresh memory; the offset is a multiple of
            // `req.alignment` (checked above) and leaves `req.size` bytes after it.
            unsafe {
                dev.ash()
                    .bind_buffer_memory(buffer, memory, layout.offset_of(slot))?
            };
        }
        // SAFETY: `memory` is HOST_VISIBLE and unmapped; WHOLE_SIZE maps its full
        // range for the ring's lifetime (vkFreeMemory implicitly unmaps).
        let ptr = unsafe {
            dev.ash()
                .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
        };
        // A refused export is not a failed ring: the owner just gets no fd to wait on.
        #[cfg(unix)]
        let dmabuf = if export {
            let info = vk::MemoryGetFdInfoKHR::default()
                .memory(memory)
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            // SAFETY: `memory` is live and was allocated exportable as DMA_BUF.
            match unsafe { dev.external_memory_fd().get_memory_fd(&info) } {
                // SAFETY: the fd is the dma-buf the driver just created for this process.
                Ok(fd) => Some(unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) }),
                Err(e) => {
                    debug!(error = ?e, "bitstream ring memory does not export as a dma-buf");
                    None
                }
            }
        } else {
            None
        };
        let buffers = std::mem::take(&mut unwind.buffers);
        unwind.disarm();
        Ok(Backing {
            buffers,
            memory,
            ptr: ptr.cast::<u8>(),
            #[cfg(unix)]
            dmabuf,
        })
    }

    /// Upload one AU, recycling or growing as needed.
    ///
    /// `poll` answers whether a token has completed, without blocking. `wait`
    /// blocks until it has. The split keeps this module free of semaphore
    /// knowledge.
    ///
    /// `segments` are the concatenated slice NALUs of `au`. The VCN firmware
    /// scans the submitted range; a non-slice NALU (AUD/SEI/SPS/PPS) in it hangs
    /// the ring. Parameter sets ride the session-parameters object instead.
    ///
    /// They must be [`pack_slices`]' output, not the plan's raw ranges: the
    /// recording layer's offsets come from the same call, and the start-code
    /// normalisation keeps the driver's slice-header parse in step with the bytes.
    ///
    /// # Safety
    ///
    /// Live device (contract); `segments` are in-bounds ranges of `au`; tokens
    /// passed to prior [`SlotStates::set_pending`] calls cover every GPU read of
    /// their slots — recycling rewrites slot bytes as soon as a token reports done.
    pub(crate) unsafe fn upload<E: From<AllocError>>(
        &mut self,
        dev: &DecodeDevice,
        au: &[u8],
        segments: &[std::ops::Range<usize>],
        poll: &mut dyn FnMut(&Token) -> Result<bool, E>,
        wait: &mut dyn FnMut(&Token) -> Result<(), E>,
    ) -> Result<UploadedAu, E> {
        let len: u64 = segments.iter().map(|s| s.len() as u64).sum();
        if !self.layout.fits(len) {
            // In-flight reads target the old buffer: drain them before recreate.
            for token in self.pending.in_flight() {
                wait(token)?;
            }
            self.pending.clear();
            let grown = self.layout.grown_for(len);
            debug!(
                old = self.layout.slot_size,
                new = grown.slot_size,
                au = len,
                "bitstream ring grows for an oversized AU"
            );
            // Allocate before destroying: a failed grow keeps the old mapping and
            // layout, never a null `ptr` for the next AU that fits.
            // SAFETY: caller's live-device contract.
            let backing =
                unsafe { Self::allocate_backing(dev, &grown, self.profile, self.export)? };
            // SAFETY: every in-flight read was drained above; destroy_backing only
            // touches this ring's own objects.
            unsafe { self.destroy_backing() };
            self.layout = grown;
            self.buffers = backing.buffers;
            self.memory = backing.memory;
            self.ptr = backing.ptr;
            #[cfg(unix)]
            {
                self.dmabuf = backing.dmabuf;
            }
            self.pending = SlotStates::new(grown.slots as usize);
        }

        let slot = match self.pending.acquire(&mut *poll)? {
            Some(slot) => slot,
            None => {
                // Oldest still in flight: wait, then retry — that slot is now free.
                if let Some(token) = self.pending.oldest() {
                    wait(token)?;
                }
                self.pending
                    .acquire(|_| Ok(true))?
                    .expect("the waited slot is free")
            }
        };

        let offset = self.layout.offset_of(slot as u32);
        let range = self.layout.record_range(len);
        // SAFETY: `ptr` is the live persistent mapping of the ring memory, at least
        // `layout.buffer_size()` bytes; `offset + range <= buffer_size` because
        // `range <= slot_size` (fits/grown above) and offset is `slot * slot_size`
        // with `slot < slots`, so the `range` bytes from `offset` are one whole
        // initialized, aliasing-free slot of the mapping. The slot is not
        // concurrently read: its previous use completed (poll/wait above) and its
        // next use is submitted after this copy.
        let slot_bytes = unsafe {
            std::slice::from_raw_parts_mut(self.ptr.add(offset as usize), range as usize)
        };
        // In-bounds of `au` (fn contract) and `len <= range` (fits/grown), so
        // `pack_into` cannot panic.
        pack_into(slot_bytes, au, segments);
        Ok(UploadedAu {
            buffer: self.buffers[slot],
            range,
            slot,
        })
    }

    /// Destroy the slot buffers + memory (which implicitly unmaps). Callers must
    /// have drained in-flight reads first.
    ///
    /// # Safety
    ///
    /// Live device; no submitted-and-unfinished GPU work reads the buffer.
    unsafe fn destroy_backing(&mut self) {
        // SAFETY: the fn-level contract — objects are this ring's own, reads drained.
        unsafe {
            for buffer in self.buffers.drain(..) {
                self.device.destroy_buffer(buffer, None);
            }
            self.device.free_memory(self.memory, None);
        }
        self.memory = vk::DeviceMemory::null();
        self.ptr = std::ptr::null_mut();
    }
}

impl Drop for BitstreamRing {
    fn drop(&mut self) {
        if self.buffers.is_empty() {
            return;
        }
        // SAFETY: the owning decoder drains its queue before dropping state (and the
        // borrowed device is alive by the DeviceHandles liveness contract).
        unsafe { self.destroy_backing() };
    }
}

#[cfg(test)]
mod tests {
    use pf_bitstream::testing::split_h264_aus;
    use pf_bitstream::testing::split_h265_aus;

    use super::*;

    #[test]
    fn slot_offsets_and_ranges_honour_both_alignments() {
        // Deliberately DIFFERENT alignments: offset 256, size 64.
        let layout = RingLayout::new(1000, 4, 256, 64);
        assert_eq!(layout.slot_size, 1024);
        for slot in 0..4 {
            assert_eq!(layout.offset_of(slot) % 256, 0, "offset alignment");
        }
        assert_eq!(layout.buffer_size(), 4096);
        // Ranges round to the SIZE alignment, independent of the offset one.
        assert_eq!(layout.record_range(1), 64);
        assert_eq!(layout.record_range(64), 64);
        assert_eq!(layout.record_range(65), 128);
        assert!(layout.fits(1024));
        assert!(!layout.fits(1025));
    }

    #[test]
    fn growth_doubles_the_slot_size_until_the_au_fits_and_keeps_alignment() {
        let layout = RingLayout::new(1024, 4, 128, 128);
        let grown = layout.grown_for(5000);
        assert_eq!(grown.slot_size, 8192, "1024 → 2048 → 4096 → 8192");
        assert_eq!(grown.slots, 4);
        assert!(grown.fits(5000));
        assert_eq!(grown.offset_of(3) % 128, 0);

        assert_eq!(layout.grown_for(512), layout);

        // Growth is driven by the aligned range, not raw length: 1025 bytes
        // becomes 1152 under a 128 alignment and needs the next size up.
        assert_eq!(layout.grown_for(1025).slot_size, 2048);
    }

    #[test]
    fn one_byte_alignments_degenerate_cleanly() {
        let layout = RingLayout::new(100, 2, 1, 1);
        assert_eq!(layout.slot_size, 100);
        assert_eq!(layout.record_range(37), 37);
        assert!(layout.fits(100));
        assert!(!layout.fits(101));
    }

    #[test]
    fn slots_recycle_in_fifo_order_only_after_their_token_completes() {
        let mut states: SlotStates<u64> = SlotStates::new(2);
        let s0 = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
        states.set_pending(s0, 10);
        let s1 = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
        states.set_pending(s1, 11);
        assert_ne!(s0, s1);

        // Full, oldest (slot 0, token 10) not done: None, and names the wait token.
        assert_eq!(states.acquire(|&t| Ok::<_, ()>(t > 10)).unwrap(), None);
        assert_eq!(states.oldest(), Some(&10));

        // Done: the oldest slot is handed back (FIFO, not LIFO).
        let s2 = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
        assert_eq!(s2, s0);

        states.set_pending(s2, 12);
        assert_eq!(states.acquire(|_| Err("gpu gone")).unwrap_err(), "gpu gone");
    }

    #[test]
    fn slice_offsets_rebase_out_of_au_coordinates_into_the_packed_slot() {
        // AUD/SPS/PPS then two slices whose start codes sit at 40 and 900. Only
        // the slices upload, so packed offsets are 0 and 860.
        let packed = pack_slices(&[43..900, 903..1500]).unwrap();
        assert_eq!(packed.offsets, vec![0, 860]);
        assert_eq!(packed.segments, vec![40..900, 900..1500]);

        // Offset 0 regardless of the AU start. `from_ref`: a one-element array
        // of a range looks like a range-fill to clippy.
        let single = 403usize..1000;
        assert_eq!(
            pack_slices(std::slice::from_ref(&single)).unwrap().offsets,
            vec![0]
        );
        assert_eq!(pack_slices(&[]).unwrap().offsets, Vec::<u32>::new());

        // Offsets accumulate by length, not AU position: an SEI gap must not
        // shift the third offset.
        let nals = [3..100, 503..600, 1003..1100];
        assert_eq!(pack_slices(&nals).unwrap().offsets, vec![0, 100, 200]);

        // No start code before byte 3.
        assert!(pack_slices(std::slice::from_ref(&(2usize..40))).is_none());
    }

    #[test]
    fn a_four_byte_annex_b_prefix_is_trimmed_to_three_and_the_offsets_follow() {
        // First slice has the four-byte prefix real encoders put on the first
        // NALU of an AU; second has three. Both must land on `00 00 01`, and the
        // second offset must count the first slice's trimmed length, not AU length.
        let mut au = vec![0xAAu8; 200];
        au[0..4].copy_from_slice(&[0, 0, 0, 1]);
        au[100..103].copy_from_slice(&[0, 0, 1]);
        let packed = pack_slices(&[4..100, 103..200]).unwrap();
        assert_eq!(packed.segments, vec![1..100, 100..200]);
        assert_eq!(packed.offsets, vec![0, 99], "99, not 100");

        let mut slot = vec![0xFFu8; 256];
        pack_into(&mut slot, &au, &packed.segments);
        for (offset, segment) in packed.offsets.iter().zip(&packed.segments) {
            let at = &slot[*offset as usize..];
            assert_eq!(
                at[..3],
                [0, 0, 1],
                "the packed slice at offset {offset} must open with a THREE-byte \
                 start code, or a driver reaching the slice header by a fixed \
                 `+3 +2` skip lands a byte early"
            );
            assert_eq!(&at[..segment.len()], &au[segment.clone()]);
        }
        // Alignment tail is zeroed, never a previous AU's bytes.
        let packed_len: usize = packed.segments.iter().map(|s| s.len()).sum();
        assert!(slot[packed_len..].iter().all(|&b| b == 0));

        // Annex-B allows more than one leading zero byte; all of them stay out.
        assert_eq!(
            pack_slices(std::slice::from_ref(&(6usize..64)))
                .unwrap()
                .segments,
            vec![3..64]
        );
    }

    /// Packed bytes vs offsets for every AU of both vendored vectors, checked
    /// against the streams' own NAL headers rather than a golden this code wrote.
    ///
    /// H.264's vector happens to carry three-byte prefixes on every slice — an
    /// encoder convention, not a guarantee — so it cannot be the only cover.
    /// Its leg asserts the normalisation is a no-op when nothing needs
    /// trimming. The four-byte case for both codecs is
    /// [`a_four_byte_annex_b_prefix_is_trimmed_to_three_and_the_offsets_follow`].
    #[test]
    fn every_packed_slice_of_both_vendored_vectors_opens_at_its_own_nal_header() {
        // H.265: three-byte prefix, two-byte NAL unit header (forbidden_zero,
        // nal_unit_type < 32 for VCL, nuh_layer_id 0), then the slice segment
        // header. first_slice_segment_in_pic_flag is its first bit.
        let mut planner = pf_bitstream::h265::H265Planner::new();
        let mut aus = 0usize;
        for au in split_h265_aus(TEST_25FPS_H265) {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let plan_segments: Vec<std::ops::Range<usize>> =
                plan.slices.iter().map(|s| s.nal.clone()).collect();
            let packed = pack_slices(&plan_segments).expect("offsets fit u32");
            assert_eq!(packed.offsets.len(), plan.slices.len(), "one per segment");
            let slot = packed_slot(au, &packed);
            for (index, offset) in packed.offsets.iter().enumerate() {
                let at = &slot[*offset as usize..];
                assert_eq!(
                    at[..3],
                    [0, 0, 1],
                    "AU {aus} segment {index}: a three-byte start code"
                );
                assert_eq!(at[3] & 0x80, 0, "AU {aus} segment {index}: forbidden_zero");
                let nal_type = (at[3] >> 1) & 0x3F;
                assert!(
                    nal_type < 32,
                    "AU {aus} segment {index}: VCL type, not {nal_type}"
                );
                let layer_id = ((at[3] & 1) << 5) | (at[4] >> 3);
                assert_eq!(layer_id, 0, "AU {aus} segment {index}: nuh_layer_id");
                assert!(
                    at[4] & 7 > 0,
                    "AU {aus} segment {index}: temporal_id_plus1 > 0"
                );
                // Slice segment header starts at +5. Only the first segment of
                // a picture sets first_slice_segment_in_pic_flag.
                assert_eq!(
                    at[5] & 0x80 != 0,
                    index == 0,
                    "AU {aus} segment {index}: first_slice_segment_in_pic_flag"
                );
            }
            aus += 1;
        }
        assert_eq!(aus, 250, "the vector's own golden");

        // H.264: three-byte prefix, one-byte NAL unit header, then the slice
        // header. first_mb_in_slice `ue(v)`: a leading 1-bit means 0, i.e. the
        // first slice of the picture.
        let mut planner = pf_bitstream::h264::H264Planner::new();
        let mut aus = 0usize;
        for au in split_h264_aus(TEST_25FPS_H264) {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let plan_segments: Vec<std::ops::Range<usize>> =
                plan.slices.iter().map(|s| s.nal.clone()).collect();
            let packed = pack_slices(&plan_segments).expect("offsets fit u32");
            let slot = packed_slot(au, &packed);
            for (index, offset) in packed.offsets.iter().enumerate() {
                let at = &slot[*offset as usize..];
                assert_eq!(
                    at[..3],
                    [0, 0, 1],
                    "AU {aus} segment {index}: a three-byte start code"
                );
                assert_eq!(at[3] & 0x80, 0, "AU {aus} segment {index}: forbidden_zero");
                let nal_type = at[3] & 0x1F;
                assert!(
                    nal_type == 1 || nal_type == 5,
                    "AU {aus} segment {index}: a coded slice, not type {nal_type}"
                );
                assert_eq!(
                    at[4] & 0x80 != 0,
                    index == 0,
                    "AU {aus} segment {index}: first_mb_in_slice == 0"
                );
            }
            // This vector is multi-slice; a single-slice regression would leave
            // only the H.265 leg covering multi-segment offset arithmetic.
            assert!(packed.offsets.len() >= 2, "AU {aus}: multi-slice");
            aus += 1;
        }
        assert_eq!(aus, 250, "the vector's own golden");
    }

    /// Vendored H.265 vector, as in `pic_h265`'s tests.
    const TEST_25FPS_H265: &[u8] = pf_bitstream::testing::H265_25FPS;
    /// H.264 twin — same packer, different prefix convention.
    const TEST_25FPS_H264: &[u8] = pf_bitstream::testing::H264_25FPS;

    /// One AU packed as [`BitstreamRing::upload`] would. 256 is the widest
    /// `minBitstreamBufferSizeAlignment` we size the recorded range for.
    fn packed_slot(au: &[u8], packed: &PackedSlices) -> Vec<u8> {
        let len: u64 = packed.segments.iter().map(|s| s.len() as u64).sum();
        let layout = RingLayout::new(INITIAL_SLOT_SIZE, RING_SLOTS, 256, 256);
        let mut slot = vec![0xFFu8; layout.record_range(len) as usize];
        pack_into(&mut slot, au, &packed.segments);
        slot
    }

    #[test]
    fn clear_forgets_every_token_and_restarts_the_cursor() {
        let mut states: SlotStates<u64> = SlotStates::new(3);
        for token in 0..3 {
            let s = states.acquire(|_| Ok::<_, ()>(true)).unwrap().unwrap();
            states.set_pending(s, token);
        }
        assert_eq!(states.in_flight().count(), 3);
        states.clear();
        assert_eq!(states.in_flight().count(), 0);
        assert_eq!(states.acquire(|_| Ok::<_, ()>(true)).unwrap(), Some(0));
    }
}
