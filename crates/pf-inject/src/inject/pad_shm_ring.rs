//! Host half of a Windows pad's `PadShm` DATA section: the stamp order, the input seqlock
//! and the output-ring reader.
//!
//! Every access goes through [`SectionView`], which checks bounds and alignment, so a test
//! buffer stands in for the section and the reader's tests run on every OS. The section
//! itself, its sealed delivery and the devnode live in `windows/`.

use pf_driver_proto::gamepad::PadShm;
use std::marker::PhantomData;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

/// Byte size of [`PadShm`]. Offsets and magic come from the same struct so a layout change
/// is a compile error; the driver maps that type too.
pub(crate) const SHM_SIZE: usize = core::mem::size_of::<PadShm>();
pub(crate) const SHM_MAGIC: u32 = pf_driver_proto::gamepad::PAD_MAGIC; // "PFDS"
const OFF_MAGIC: usize = core::mem::offset_of!(PadShm, magic);
pub(crate) const OFF_INPUT: usize = core::mem::offset_of!(PadShm, input);
pub(crate) const OFF_OUT_SEQ: usize = core::mem::offset_of!(PadShm, out_seq);
pub(crate) const OFF_OUTPUT: usize = core::mem::offset_of!(PadShm, output);
/// The driver picks HID identity from this byte.
pub(crate) const OFF_DEVTYPE: usize = core::mem::offset_of!(PadShm, device_type);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(PadShm, driver_proto);
const OFF_DRIVER_REV: usize = core::mem::offset_of!(PadShm, driver_rev);
pub(crate) const OFF_PAD_INDEX: usize = core::mem::offset_of!(PadShm, pad_index);
const OFF_OUT_RING_VER: usize = core::mem::offset_of!(PadShm, out_ring_ver);
const OFF_RING_HEAD: usize = core::mem::offset_of!(PadShm, ring_head);
const OFF_OUT_RING_LEN: usize = core::mem::offset_of!(PadShm, out_ring_len);
const OFF_OUT_RING: usize = core::mem::offset_of!(PadShm, out_ring);
const OUT_SLOT_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::OutSlot>();
const OUT_RING_LEN: u32 = pf_driver_proto::gamepad::OUT_RING_LEN;
const OUT_RING_LEN_V22: u32 = pf_driver_proto::gamepad::OUT_RING_LEN_V22;
/// v2.3 input seqlock — see [`publish_input`].
const OFF_INPUT_GEN: usize = core::mem::offset_of!(PadShm, input_gen);
/// The input slot's size; a longer report would overrun into `out_seq`.
pub(crate) const INPUT_SLOT: usize = 64;

/// Bounds-checked view of a mapped section, borrowed from whatever keeps it mapped.
/// Words the driver also writes go through aligned atomics; report bodies are plain copies,
/// ordered by the sequence word or seqlock around them. Every accessor panics out of range.
#[derive(Clone, Copy)]
pub(crate) struct SectionView<'a> {
    base: *mut u8,
    len: usize,
    _mapping: PhantomData<&'a [u8]>,
}

impl<'a> SectionView<'a> {
    /// # Safety
    /// `base..base + len` stays mapped and writable for `'a`, and no Rust reference overlaps it.
    // unsafe-fn-no-op-ok: the contract is the mapping's lifetime, checked by every accessor.
    pub(crate) unsafe fn from_raw(base: *mut u8, len: usize) -> SectionView<'a> {
        SectionView {
            base,
            len,
            _mapping: PhantomData,
        }
    }

    /// A test buffer standing in for a section.
    #[cfg(test)]
    fn over(buf: &'a mut [u32]) -> SectionView<'a> {
        SectionView {
            base: buf.as_mut_ptr().cast(),
            len: std::mem::size_of_val(buf),
            _mapping: PhantomData,
        }
    }

    /// `len` bytes at `off`, asserted inside the view.
    fn range(&self, off: usize, len: usize) -> *mut u8 {
        let end = off.checked_add(len);
        assert!(
            end.is_some_and(|e| e <= self.len),
            "section range {off}+{len} out of bounds"
        );
        self.base.wrapping_add(off)
    }

    /// The atomic word at `off`, asserted in bounds and aligned.
    fn word<T>(&self, off: usize) -> *const T {
        let p = self.range(off, size_of::<T>());
        assert!(
            p as usize % align_of::<T>() == 0,
            "section word {off} misaligned"
        );
        p.cast()
    }

    pub(crate) fn load_u32(&self, off: usize, order: Ordering) -> u32 {
        // SAFETY: `word` proved an aligned word inside the live mapping; both sides touch it
        // only atomically.
        unsafe { (*self.word::<AtomicU32>(off)).load(order) }
    }

    pub(crate) fn store_u32(&self, off: usize, value: u32, order: Ordering) {
        // SAFETY: as `load_u32`.
        unsafe { (*self.word::<AtomicU32>(off)).store(value, order) }
    }

    pub(crate) fn store_u64(&self, off: usize, value: u64, order: Ordering) {
        // SAFETY: as `load_u32`, for an 8-aligned word.
        unsafe { (*self.word::<AtomicU64>(off)).store(value, order) }
    }

    pub(crate) fn write_bytes(&self, off: usize, src: &[u8]) {
        let dst = self.range(off, src.len());
        // SAFETY: `range` proved `dst` inside the live mapping, which no Rust reference
        // overlaps; `src` is a separate allocation.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) }
    }

    pub(crate) fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        let src = self.range(off, dst.len());
        // SAFETY: as `write_bytes`, copying the other way.
        unsafe { std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), dst.len()) }
    }
}

/// Stamp a fresh section for the driver: device type, pad index, ring version and the neutral
/// report, then the magic last. The driver trusts nothing before the magic, and a device type
/// it reads late enumerates the pad as a DualSense. Ring version `2` means this host drains
/// the v2.2 long ring (a v2.1 driver reads it as a boolean and stays on 8-slot math); `0`
/// leaves the driver on the legacy slot. A neutral report past the input slot is cut to it.
pub(crate) fn stamp(shm: SectionView<'_>, devtype: u8, index: u8, ring_ver: u32, neutral: &[u8]) {
    shm.write_bytes(OFF_DEVTYPE, &[devtype]);
    shm.store_u32(OFF_PAD_INDEX, index.into(), Ordering::Relaxed);
    shm.store_u32(OFF_OUT_RING_VER, ring_ver, Ordering::Relaxed);
    shm.write_bytes(OFF_INPUT, &neutral[..neutral.len().min(INPUT_SLOT)]);
    shm.store_u32(OFF_MAGIC, SHM_MAGIC, Ordering::Relaxed);
}

/// `(driver_proto, driver_rev)` from a pad section. The driver stamps the revision first and the
/// protocol with Release, so a revision read after a nonzero protocol is the driver's.
pub(crate) fn driver_marks(shm: SectionView<'_>) -> (u32, u32) {
    let proto = shm.load_u32(OFF_DRIVER_PROTO, Ordering::Acquire);
    (proto, shm.load_u32(OFF_DRIVER_REV, Ordering::Relaxed))
}

/// Publish one HID input report into the section's input slot under the v2.3 seqlock.
///
/// The slot is a single unqueued buffer. The driver's timer can copy 64 bytes out of it mid-write,
/// which for gyro is a spike a game will integrate as aim.
///
/// `generation` goes odd before the body and even after. The driver samples either side of its
/// read and retries on disagreement. The `Release` fence keeps body stores below the odd marker;
/// the `Release` store publishes them ahead of even. Both are no-ops on x86-TSO, load-bearing on ARM64.
/// A report past [`INPUT_SLOT`] is cut to it.
pub(crate) fn publish_input(shm: SectionView<'_>, generation: &mut u32, report: &[u8]) {
    // Odd: a report is in flight.
    *generation = generation.wrapping_add(1);
    shm.store_u32(OFF_INPUT_GEN, *generation, Ordering::Relaxed);
    // Ordered, not ordering: keeps the body stores below from being hoisted above the odd marker.
    fence(Ordering::Release);
    shm.write_bytes(OFF_INPUT, &report[..report.len().min(INPUT_SLOT)]);
    // Even: the slot holds a whole report again.
    *generation = generation.wrapping_add(1);
    shm.store_u32(OFF_INPUT_GEN, *generation, Ordering::Release);
}

/// Drain of a pad section's output plane: the lossless report ring when the driver publishes one
/// (8 slots on v2.1, [`OUT_RING_LEN_V22`] after both sides negotiate v2.2 — the driver's
/// `out_ring_len` echo decides), else the legacy latest-report slot. The ring is the only path
/// that cannot coalesce a rumble-STOP behind a following LED/trigger report inside one ~4 ms
/// poll (`design/rumble-root-fix.md`).
pub(crate) struct OutputDrain {
    /// Driver `ring_head` value drained up to.
    tail: u32,
    /// Last `out_seq` consumed — single-slot path only.
    last_out_seq: u32,
    /// Latched on first ring activity; the legacy path never re-engages after it (the driver
    /// dual-writes both planes, so consuming both would double-parse every report).
    ring_live: bool,
}

impl OutputDrain {
    pub(crate) fn new() -> OutputDrain {
        OutputDrain {
            tail: 0,
            last_out_seq: 0,
            ring_live: false,
        }
    }

    /// Drain every output report published since the last call, oldest → newest.
    ///
    /// `per_report` gets the slot bytes and a `feature` flag: bit 31 of the raw ring length is a
    /// Triton FEATURE set ([`pf_driver_proto::triton::out_is_feature`]).
    /// [`pf_driver_proto::triton::out_len`] masks that bit **before** the 64-byte clamp, so a
    /// tagged slot clamps on payload size, not `raw_len | 0x8000_0000`.
    ///
    /// Returns `true` on overflow (more than the negotiated length landed, or the driver lapped
    /// mid-copy): the pending window is discarded as possibly torn, the untagged latest-report slot
    /// is salvaged into one `per_report` call, and the caller must `PadFeedback::resync` planes that
    /// report did not carry. Overflow salvage and the pre-ring path both read that untagged slot, so
    /// `feature` is always `false` there — a FEATURE that lands on overflow or on an old driver
    /// replays as OUTPUT until the next ring-fed poll.
    pub(crate) fn drain_tagged(
        &mut self,
        shm: SectionView<'_>,
        mut per_report: impl FnMut(&[u8], bool),
    ) -> bool {
        // The driver bumps `ring_head` AFTER writing the slot, so an Acquire load orders the
        // slot copies below.
        let head = shm.load_u32(OFF_RING_HEAD, Ordering::Acquire);
        if self.ring_live || head != 0 {
            self.ring_live = true;
            if head == self.tail {
                return false;
            }
            // Driver's slot-math modulo (0 = pre-v2.2, hardcodes 8). Loaded after Acquire on
            // `ring_head`; restamped before every bump. Out-of-range clamps to v2.1 so offsets
            // stay inside the v2.2 ring.
            let echo = shm.load_u32(OFF_OUT_RING_LEN, Ordering::Relaxed);
            let ring_len = if (1..=OUT_RING_LEN_V22).contains(&echo) {
                echo
            } else {
                OUT_RING_LEN
            };
            let pending = head.wrapping_sub(self.tail);
            if pending < ring_len {
                // Copy slots first, then re-check head: a writer that lapped the window during
                // the copy may have overwritten what we read. At `ring_len` behind, the write in
                // flight is into the oldest slot, so a full ring counts as overflow.
                let n = pending as usize;
                let mut bufs =
                    [([0u8; 64], 0usize, false); pf_driver_proto::gamepad::OUT_RING_LEN_V22_USIZE];
                for (k, buf) in bufs.iter_mut().enumerate().take(n) {
                    let idx = (self.tail.wrapping_add(k as u32) % ring_len) as usize;
                    // idx < `ring_len` ≤ OUT_RING_LEN_V22: the last slot ends at 4064 ≤ SHM_SIZE.
                    let slot = OFF_OUT_RING + idx * OUT_SLOT_SIZE;
                    let raw_len = shm.load_u32(slot, Ordering::Relaxed);
                    buf.2 = pf_driver_proto::triton::out_is_feature(raw_len);
                    buf.1 = (pf_driver_proto::triton::out_len(raw_len) as usize).min(64);
                    shm.read_bytes(slot + 4, &mut buf.0[..buf.1]);
                }
                let head2 = shm.load_u32(OFF_RING_HEAD, Ordering::Acquire);
                if head2.wrapping_sub(self.tail) < ring_len {
                    for (data, len, feature) in bufs.iter().take(n) {
                        if *len > 0 {
                            per_report(&data[..*len], *feature);
                        }
                    }
                    self.tail = head;
                    return false;
                }
            }
            // Overflow or lapped mid-copy: skip to the freshest head and salvage the untagged
            // latest-report slot (driver dual-publishes every report there). No seqlock; parser
            // gates drop most tears, caller resync silences planes the salvage does not assert.
            self.tail = shm.load_u32(OFF_RING_HEAD, Ordering::Acquire);
            let mut out = [0u8; 64];
            shm.read_bytes(OFF_OUTPUT, &mut out);
            per_report(&out, false);
            return true;
        }
        // Pre-ring driver: latest-report slot + seq, coalescing. No feature tag on this slot.
        // Acquire pairs with the driver's publish-then-bump store order.
        let seq = shm.load_u32(OFF_OUT_SEQ, Ordering::Acquire);
        if seq != self.last_out_seq {
            self.last_out_seq = seq;
            let mut out = [0u8; 64];
            shm.read_bytes(OFF_OUTPUT, &mut out);
            per_report(&out, false);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section() -> Vec<u32> {
        vec![0u32; SHM_SIZE / 4]
    }

    /// v2.1 dual write: legacy slot + seq, then ring slot (8-slot math, no length echo), then head.
    fn publish(buf: &mut [u32], bytes: &[u8]) {
        ring_publish(buf, bytes, OUT_RING_LEN, false);
    }

    /// v2.1 dual write with `OUT_FEATURE_BIT` ORed into the slot length — the tag `drain_tagged`
    /// must strip and surface as `feature`.
    fn publish_tagged(buf: &mut [u32], bytes: &[u8]) {
        legacy_publish(buf, bytes);
        let head = read32(buf, OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % OUT_RING_LEN) as usize * OUT_SLOT_SIZE;
        write32(
            buf,
            slot,
            bytes.len() as u32 | pf_driver_proto::triton::OUT_FEATURE_BIT,
        );
        let b = bytes_mut(buf);
        b[slot + 4..slot + 4 + bytes.len()].copy_from_slice(bytes);
        write32(buf, OFF_RING_HEAD, head.wrapping_add(1));
    }

    /// v2.2 dual write: long-ring slot math, `out_ring_len` echo stamped before the head bump.
    fn v22_publish(buf: &mut [u32], bytes: &[u8]) {
        ring_publish(buf, bytes, OUT_RING_LEN_V22, true);
    }

    fn ring_publish(buf: &mut [u32], bytes: &[u8], len: u32, echo: bool) {
        legacy_publish(buf, bytes);
        let head = read32(buf, OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % len) as usize * OUT_SLOT_SIZE;
        write32(buf, slot, bytes.len() as u32);
        let b = bytes_mut(buf);
        b[slot + 4..slot + 4 + bytes.len()].copy_from_slice(bytes);
        if echo {
            write32(buf, OFF_OUT_RING_LEN, len);
        }
        write32(buf, OFF_RING_HEAD, head.wrapping_add(1));
    }

    /// Pre-ring driver: latest-report slot + seq only.
    fn legacy_publish(buf: &mut [u32], bytes: &[u8]) {
        let b = bytes_mut(buf);
        b[OFF_OUTPUT..OFF_OUTPUT + bytes.len()].copy_from_slice(bytes);
        let seq = read32(buf, OFF_OUT_SEQ).wrapping_add(1);
        write32(buf, OFF_OUT_SEQ, seq);
    }

    /// Byte view of the source slice's allocation, including short test buffers.
    fn bytes_mut(buf: &mut [u32]) -> &mut [u8] {
        let byte_len = buf
            .len()
            .checked_mul(size_of::<u32>())
            .expect("u32 slice byte length overflow");
        // SAFETY: `byte_len` is exactly the source slice's allocation range; u8 needs less alignment.
        unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), byte_len) }
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn section_view_refuses_a_range_past_its_end() {
        let mut buf = [0u32; 2];
        SectionView::over(&mut buf).write_bytes(4, &[0; 8]);
    }

    /// An oversize report is cut to the 64-byte slot, never spilling into `out_seq`.
    #[test]
    fn publish_input_stays_inside_the_input_slot() {
        let mut buf = section();
        let mut generation = 0;
        publish_input(SectionView::over(&mut buf), &mut generation, &[0xAB; 80]);
        assert_eq!(generation, 2);
        assert_eq!(read32(&mut buf, OFF_OUT_SEQ), 0);
        assert_eq!(bytes_mut(&mut buf)[OFF_INPUT + 63], 0xAB);
    }

    #[test]
    fn byte_view_stays_within_the_source_slice() {
        let mut buf = [0u32; 2];
        assert_eq!(bytes_mut(&mut buf).len(), 2 * size_of::<u32>());
    }

    fn read32(buf: &mut [u32], off: usize) -> u32 {
        u32::from_ne_bytes(bytes_mut(buf)[off..off + 4].try_into().unwrap())
    }

    fn write32(buf: &mut [u32], off: usize, v: u32) {
        bytes_mut(buf)[off..off + 4].copy_from_slice(&v.to_ne_bytes());
    }

    fn collect(d: &mut OutputDrain, buf: &mut [u32]) -> (Vec<Vec<u8>>, bool) {
        let mut got = Vec::new();
        let resync = d.drain_tagged(SectionView::over(buf), |b, _| got.push(b.to_vec()));
        (got, resync)
    }

    /// The stamp lands every field the driver reads, magic included.
    #[test]
    fn stamp_writes_every_field_the_driver_reads() {
        let mut buf = section();
        stamp(SectionView::over(&mut buf), 7, 3, 2, &[0x42, 0x01]);
        let b = bytes_mut(&mut buf);
        assert_eq!(b[OFF_DEVTYPE], 7);
        assert_eq!(&b[OFF_INPUT..OFF_INPUT + 2], &[0x42, 0x01]);
        assert_eq!(read32(&mut buf, OFF_PAD_INDEX), 3);
        assert_eq!(read32(&mut buf, OFF_OUT_RING_VER), 2);
        assert_eq!(read32(&mut buf, 0), SHM_MAGIC);
    }

    /// Bit 31 of ring `len` is FEATURE; the tagged drain must strip it from the length and surface
    /// it as a flag. Untagged slots must come through with `feature == false`.
    #[test]
    fn tagged_drain_separates_feature_frames_from_output_frames() {
        let mut buf = section();
        publish(&mut buf, &[0x80, 0x00, 0xFF]);
        publish_tagged(&mut buf, &[0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        let mut got = Vec::new();
        let mut d = OutputDrain::new();
        d.drain_tagged(SectionView::over(&mut buf), |bytes, feature| {
            got.push((bytes.to_vec(), feature));
        });
        assert_eq!(got[0], (vec![0x80, 0x00, 0xFF], false));
        assert_eq!(got[1].0, vec![0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        assert!(got[1].1);
    }

    /// A rumble-stop then an LED-only report in one poll must yield both, oldest first
    /// (`design/rumble-root-fix.md`). On the legacy single slot the stop is overwritten.
    #[test]
    fn ring_preserves_a_stop_followed_by_an_led_report() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        publish(&mut buf, &[0x02, 0x03, 0, 0xFF, 0xFF]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 0x03, 0, 0xFF, 0xFF]]);

        publish(&mut buf, &[0x02, 0x03, 0, 0, 0]);
        publish(&mut buf, &[0x02, 0, 0x04, 0, 0]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got,
            vec![vec![0x02, 0x03, 0, 0, 0], vec![0x02, 0, 0x04, 0, 0]],
            "the stop report must survive the burst, oldest first"
        );
        assert_eq!(collect(&mut d, &mut buf).0.len(), 0);
    }

    #[test]
    fn ring_wraps_across_polls() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..6u8 {
            publish(&mut buf, &[0x02, i]);
        }
        assert_eq!(collect(&mut d, &mut buf).0.len(), 6);
        for i in 6..12u8 {
            // 12 wraps past the 8-slot v2.1 ring
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            vec![6, 7, 8, 9, 10, 11]
        );
    }

    #[test]
    fn overflow_salvages_the_latest_slot_and_flags_resync_then_recovers() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..12u8 {
            // 12 > OUT_RING_LEN pending — the oldest 4 were overwritten in-ring
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(resync, "an overflowed window must be reported");
        assert_eq!(
            got.len(),
            1,
            "the possibly-torn ring window must not be parsed — only the legacy latest slot"
        );
        assert_eq!(
            &got[0][..2],
            &[0x02, 11],
            "the salvage must be the freshest coalesced state, not silence"
        );
        publish(&mut buf, &[0x02, 99]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 99]]);
    }

    #[test]
    fn a_full_ring_takes_the_salvage_path() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..OUT_RING_LEN as u8 {
            publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(
            resync,
            "the driver's next write lands in the oldest unread slot"
        );
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, OUT_RING_LEN as u8 - 1]);
    }

    /// 40 pending fits in 56 slots and overflows every poll against the 8-slot ring.
    #[test]
    fn v22_ring_absorbs_a_burst_the_v21_ring_could_not() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..40u8 {
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync, "40 pending ≤ 56 slots — no overflow");
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            (0..40).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v22_ring_wraps_across_polls() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..50u8 {
            v22_publish(&mut buf, &[0x02, i]);
        }
        assert_eq!(collect(&mut d, &mut buf).0.len(), 50);
        for i in 50..100u8 {
            // 100 wraps past the 56-slot v2.2 ring
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(
            got.iter().map(|r| r[1]).collect::<Vec<_>>(),
            (50..100).collect::<Vec<_>>()
        );
    }

    #[test]
    fn v22_overflow_still_salvages_and_recovers() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        for i in 0..60u8 {
            // 60 > OUT_RING_LEN_V22 pending
            v22_publish(&mut buf, &[0x02, i]);
        }
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(resync);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, 59]);
        v22_publish(&mut buf, &[0x02, 99]);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 99]]);
    }

    /// Torn or hostile `out_ring_len` must clamp to the v2.1 length, not index past the ring.
    #[test]
    fn garbage_length_echo_clamps_to_the_v21_length() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        publish(&mut buf, &[0x02, 1]); // 8-slot math, matching the clamp fallback
        write32(&mut buf, OFF_OUT_RING_LEN, 9999);
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got, vec![vec![0x02, 1]]);
    }

    #[test]
    fn legacy_driver_still_drains_the_latest_slot() {
        let mut buf = section();
        let mut d = OutputDrain::new();
        legacy_publish(&mut buf, &[0x02, 1]);
        legacy_publish(&mut buf, &[0x02, 2]); // coalesced: latest wins
        let (got, resync) = collect(&mut d, &mut buf);
        assert!(!resync);
        assert_eq!(got.len(), 1);
        assert_eq!(&got[0][..2], &[0x02, 2]);
        assert_eq!(collect(&mut d, &mut buf).0.len(), 0);
    }
}
