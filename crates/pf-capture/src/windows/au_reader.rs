//! Reader half of the v7 access-unit section (`pf_driver_proto::encode::au`). The driver's
//! encode thread copies an access unit — or one slice chunk of it — into the heap, fills a
//! slot, publishes a [`FrameToken`] in `latest`, and signals the ready event; [`AuReader`]
//! takes the published slots out in `wire_seq` order and hands back the bytes.
//!
//! Pure over a mapped [`AuView`], so the same code reads the driver's section on Windows and
//! a `Vec` in the tests. Section creation, `SET_ENCODE`, and the `Encoder` proxy
//! are `idd_push/driver_encode.rs`.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use pf_driver_proto::encode::au::{self, AuHeader, AuSlot};
use pf_driver_proto::encode::FrameToken;
use std::mem::offset_of;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const STATE: usize = offset_of!(AuSlot, state);

/// `(distance ahead of next_wire_seq, rank inside the access unit, heap distance)`.
type OrderKey = (u32, u8, u32);

/// One mapped v7 section: `len` bytes at `base`, alive for the view's lifetime. Header and
/// slot words are only ever touched through aligned atomics, because the driver writes the
/// same bytes; the heap is read plainly, on a slot this side holds READING.
pub(crate) struct AuView {
    base: *mut u8,
    len: usize,
}

// SAFETY: a raw pointer only. The owning reader moves between the prep and stream threads
// with no concurrent access on this side; the driver's writes arrive through the atomics.
unsafe impl Send for AuView {}

impl AuView {
    /// # Safety
    /// `base` must be 8-aligned and point at `len >= au::HEAP_OFFSET` readable, writable
    /// bytes that outlive the view.
    // unsafe-fn-no-op-ok: the contract is the mapping's lifetime, checked by every accessor.
    pub(crate) unsafe fn new(base: *mut u8, len: usize) -> Self {
        assert!(len >= au::HEAP_OFFSET && base as usize % 8 == 0);
        Self { base, len }
    }

    fn word32(&self, off: usize) -> &AtomicU32 {
        assert!(off % 4 == 0 && off + 4 <= self.len);
        // SAFETY: an in-bounds, aligned word of the mapping `new`'s contract keeps alive; the
        // atomic view is the only access either side makes to header and slot words.
        unsafe { &*self.base.add(off).cast::<AtomicU32>() }
    }

    fn word64(&self, off: usize) -> &AtomicU64 {
        assert!(off % 8 == 0 && off + 8 <= self.len);
        // SAFETY: as `word32`, for an 8-aligned word.
        unsafe { &*self.base.add(off).cast::<AtomicU64>() }
    }

    /// Whole header as of now, one atomic load per field. `latest` is the publish handshake
    /// (Acquire); everything else is diagnostics.
    pub(crate) fn header(&self) -> AuHeader {
        let u32_at = |f: usize| self.word32(f).load(Ordering::Relaxed);
        let u64_at = |f: usize| self.word64(f).load(Ordering::Relaxed);
        AuHeader {
            magic: u32_at(offset_of!(AuHeader, magic)),
            version: u32_at(offset_of!(AuHeader, version)),
            heap_offset: u32_at(offset_of!(AuHeader, heap_offset)),
            heap_bytes: u32_at(offset_of!(AuHeader, heap_bytes)),
            slot_table_offset: u32_at(offset_of!(AuHeader, slot_table_offset)),
            slot_count: u32_at(offset_of!(AuHeader, slot_count)),
            latest: self
                .word64(offset_of!(AuHeader, latest))
                .load(Ordering::Acquire),
            generation: self
                .word32(offset_of!(AuHeader, generation))
                .load(Ordering::Acquire),
            wire_seq_base: u32_at(offset_of!(AuHeader, wire_seq_base)),
            encoder_state: u32_at(offset_of!(AuHeader, encoder_state)),
            detached: u32_at(offset_of!(AuHeader, detached)),
            last_au_qpc: u64_at(offset_of!(AuHeader, last_au_qpc)),
            drain_heartbeat_qpc: u64_at(offset_of!(AuHeader, drain_heartbeat_qpc)),
            source_seq: u64_at(offset_of!(AuHeader, source_seq)),
            dropped_total: u64_at(offset_of!(AuHeader, dropped_total)),
            published_total: u64_at(offset_of!(AuHeader, published_total)),
            driver_status: u32_at(offset_of!(AuHeader, driver_status)),
            driver_status_detail: u32_at(offset_of!(AuHeader, driver_status_detail)),
            applied_bitrate_kbps: u32_at(offset_of!(AuHeader, applied_bitrate_kbps)),
            _reserved: [0; 28],
        }
    }

    fn slot32(&self, i: usize, field: usize) -> &AtomicU32 {
        self.word32(au::slot_offset(i) + field)
    }

    /// Slot `i`. `state` is loaded first (Acquire) so the fields are the ones published with it.
    fn slot(&self, i: usize) -> AuSlot {
        let state = self.slot32(i, STATE).load(Ordering::Acquire);
        let u32_at = |f: usize| self.slot32(i, f).load(Ordering::Relaxed);
        AuSlot {
            offset: u32_at(offset_of!(AuSlot, offset)),
            len: u32_at(offset_of!(AuSlot, len)),
            wire_seq: u32_at(offset_of!(AuSlot, wire_seq)),
            source_seq: u32_at(offset_of!(AuSlot, source_seq)),
            qpc_pts: self
                .word64(au::slot_offset(i) + offset_of!(AuSlot, qpc_pts))
                .load(Ordering::Relaxed),
            flags: u32_at(offset_of!(AuSlot, flags)),
            state,
            qpc_submit: self
                .word64(au::slot_offset(i) + offset_of!(AuSlot, qpc_submit))
                .load(Ordering::Relaxed),
            qpc_published: self
                .word64(au::slot_offset(i) + offset_of!(AuSlot, qpc_published))
                .load(Ordering::Relaxed),
        }
    }

    /// Copy `len` bytes at section offset `off` into `into`. `false` when the range is not
    /// inside the heap the header declares — a writer's index is never trusted.
    fn copy_heap(&self, off: u32, len: u32, into: &mut Vec<u8>) -> bool {
        let h = self.header();
        let (heap_off, heap_len) = (h.heap_offset as usize, h.heap_bytes as usize);
        let (off, len) = (off as usize, len as usize);
        let inside = off >= heap_off
            && off
                .checked_add(len)
                .is_some_and(|end| end <= heap_off + heap_len && end <= self.len);
        if !inside {
            return false;
        }
        into.clear();
        // SAFETY: the range is inside the live mapping, and the slot naming it is READING, so
        // the driver does not write it while this copy runs.
        into.extend_from_slice(unsafe { std::slice::from_raw_parts(self.base.add(off), len) });
        true
    }
}

/// One chunk taken out of the section, bytes copied out of the heap. `flags` are the slot's
/// `AU_*` bits, mirrored onto the wire chunk by the proxy.
pub(crate) struct Taken {
    pub data: Vec<u8>,
    pub wire_seq: u32,
    pub source_seq: u32,
    pub qpc_pts: u64,
    /// The driver's encode-submit and publish stamps; `0` from a driver that predates them.
    pub qpc_submit: u64,
    pub qpc_published: u64,
    pub flags: u32,
}

/// A slot whose heap range lies outside the section: the driver's index is not trusted and
/// the section is rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuFault {
    pub slot: usize,
    pub offset: u32,
    pub len: u32,
}

impl std::fmt::Display for AuFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "AU slot {} names {} bytes at offset {} outside the heap",
            self.slot, self.len, self.offset
        )
    }
}

impl std::error::Error for AuFault {}

/// Walks published slots out of an [`AuView`] in `wire_seq` order. Chunks of one access unit
/// share a `wire_seq`; the FIRST chunk goes first, then heap order from the previous chunk's
/// end (the heap is a bump allocator that wraps). A slot behind `next_wire_seq`, or a chunk of
/// an access unit whose FIRST was never taken, is freed unread: after a `RESET` the domain
/// restarts where the host says, and a superseded encoder's leftovers must not reach the wire.
pub(crate) struct AuReader {
    view: AuView,
    /// The `wire_seq` the next access unit carries; `SET_ENCODE` and `RESET` set it.
    next_wire_seq: u32,
    /// The access unit still owing its LAST chunk: `(wire_seq, end offset of the last chunk)`.
    open_au: Option<(u32, u32)>,
    /// PUBLISHED slots freed without reaching the wire.
    pub(crate) freed_unread: u64,
    /// Access units the driver skipped: `next_wire_seq` jumped over them.
    pub(crate) gaps: u64,
}

impl AuReader {
    pub(crate) fn new(view: AuView, wire_seq_base: u32) -> Self {
        Self {
            view,
            next_wire_seq: wire_seq_base,
            open_au: None,
            freed_unread: 0,
            gaps: 0,
        }
    }

    pub(crate) fn view(&self) -> &AuView {
        &self.view
    }

    /// The `wire_seq` the next access unit must carry — also the base a `RESET` restarts at.
    pub(crate) fn next_wire_seq(&self) -> u32 {
        self.next_wire_seq
    }

    /// Restart the domain at `base` after a `RESET`; a half-read access unit is abandoned.
    pub(crate) fn rebase(&mut self, base: u32) {
        self.next_wire_seq = base;
        self.open_au = None;
    }

    /// An access unit is open: its LAST chunk is still owed.
    pub(crate) fn mid_au(&self) -> bool {
        self.open_au.is_some()
    }

    /// `latest` names this section's generation, so its slots are the live encoder's. The
    /// driver bumps `generation` on every `SET_ENCODE`; a superseded publish still carries
    /// the old one and is ignored.
    fn published_for_us(&self) -> bool {
        let h = self.view.header();
        h.latest != 0
            && FrameToken::unpack(h.latest).generation == h.generation & FrameToken::GENERATION_MASK
    }

    /// `wire_seq` distance ahead of `next_wire_seq`; `None` when the slot is behind it.
    fn ahead(&self, wire_seq: u32) -> Option<u32> {
        let rel = wire_seq.wrapping_sub(self.next_wire_seq);
        (rel < 1 << 31).then_some(rel)
    }

    /// Ordering of a candidate slot: access unit first, then FIRST before the rest, then heap
    /// order from the open access unit's end so a wrapped bump allocation still sorts last.
    fn order_key(&self, rel: u32, first: bool, offset: u32) -> OrderKey {
        if first {
            return (rel, 0, 0);
        }
        match self.open_au {
            Some((_, end)) if rel == 0 && offset >= end => (rel, 1, offset - end),
            Some(_) if rel == 0 => (rel, 2, offset),
            _ => (rel, 3, offset),
        }
    }

    /// `from` → `to` on slot `i`'s state; `false` when the slot moved under us.
    fn transition(&self, i: usize, from: u32, to: u32) -> bool {
        self.view
            .slot32(i, STATE)
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn free_unread(&mut self, i: usize) {
        if self.transition(i, au::PUBLISHED, au::FREE) {
            self.freed_unread += 1;
            tracing::warn!(
                slot = i,
                freed_unread = self.freed_unread,
                "driver encode: a published access unit was freed unread (behind the domain, \
                 or a chunk without its FIRST)"
            );
        }
    }

    /// Published access units this reader has not started: the loop owes one wire index each.
    pub(crate) fn ready_aus(&self) -> usize {
        if !self.published_for_us() {
            return 0;
        }
        (0..au::AU_SLOTS as usize)
            .map(|i| self.view.slot(i))
            .filter(|s| s.state == au::PUBLISHED && s.flags & au::AU_FIRST != 0)
            .filter(|s| self.ahead(s.wire_seq).is_some())
            .count()
    }

    /// One pass over the table: the best PUBLISHED slot of ours as `(slot, record, distance
    /// ahead of next_wire_seq)`, with slots behind the domain freed on the way.
    fn scan(&mut self) -> Option<(usize, AuSlot, u32)> {
        let mut pick: Option<(OrderKey, usize, AuSlot)> = None;
        for i in 0..au::AU_SLOTS as usize {
            let s = self.view.slot(i);
            if s.state != au::PUBLISHED {
                continue;
            }
            let Some(rel) = self.ahead(s.wire_seq) else {
                self.free_unread(i);
                continue;
            };
            let key = self.order_key(rel, s.flags & au::AU_FIRST != 0, s.offset);
            if pick.as_ref().is_none_or(|p| key < p.0) {
                pick = Some((key, i, s));
            }
        }
        pick.map(|(key, i, s)| (i, s, key.0))
    }

    /// The next chunk in order, or `None` while nothing of ours is published. The slot goes
    /// PUBLISHED → READING for the copy and back to FREE (Release) before this returns.
    ///
    /// A pass is not a snapshot: a slot the driver published before the one a pass found is
    /// only guaranteed visible after that later slot's Acquire load. So a pass whose best slot
    /// is not the expected next chunk is repeated once before it is read as a skipped access
    /// unit (adopted) or a chunk without its FIRST (freed unread).
    pub(crate) fn take_next(&mut self) -> Result<Option<Taken>, AuFault> {
        if !self.published_for_us() {
            return Ok(None);
        }
        let mut chosen = None;
        for pass in 0..2 {
            let Some((i, s, rel)) = self.scan() else {
                return Ok(None);
            };
            let first = s.flags & au::AU_FIRST != 0;
            if rel == 0 && (first || self.open_au.is_some()) {
                chosen = Some((i, s));
                break;
            }
            if pass == 0 {
                continue;
            }
            if !first {
                self.free_unread(i);
                return Ok(None);
            }
            self.gaps += 1;
            tracing::warn!(
                expected = self.next_wire_seq,
                got = s.wire_seq,
                gaps = self.gaps,
                "driver encode: access unit(s) skipped — adopting the next FIRST"
            );
            self.open_au = None;
            self.next_wire_seq = s.wire_seq;
            chosen = Some((i, s));
        }
        let Some((i, s)) = chosen else {
            return Ok(None);
        };
        if !self.transition(i, au::PUBLISHED, au::READING) {
            return Ok(None);
        }
        let mut data = Vec::new();
        let inside = self.view.copy_heap(s.offset, s.len, &mut data);
        self.view
            .slot32(i, STATE)
            .store(au::FREE, Ordering::Release);
        if !inside {
            return Err(AuFault {
                slot: i,
                offset: s.offset,
                len: s.len,
            });
        }
        if s.flags & au::AU_LAST != 0 {
            self.open_au = None;
            self.next_wire_seq = s.wire_seq.wrapping_add(1);
        } else {
            self.open_au = Some((s.wire_seq, s.offset.wrapping_add(s.len)));
        }
        Ok(Some(Taken {
            data,
            wire_seq: s.wire_seq,
            source_seq: s.source_seq,
            qpc_pts: s.qpc_pts,
            qpc_submit: s.qpc_submit,
            qpc_published: s.qpc_published,
            flags: s.flags,
        }))
    }
}

/// Test-side writer: what the driver's encode thread does to a section, minus the encoder.
#[cfg(test)]
pub(crate) mod producer {
    use super::*;

    pub(crate) struct Producer {
        view: AuView,
        generation: u32,
        heap_off: u32,
        heap_len: u32,
        bump: u32,
        publishes: u32,
    }

    impl Producer {
        /// Stamp a fresh header at `base` the way the host does (magic last) and take the
        /// driver's side of it: `generation`.
        pub(crate) fn init(base: *mut u8, len: usize, generation: u32, wire_seq_base: u32) -> Self {
            // SAFETY: the caller hands an 8-aligned buffer of `len` bytes alive for the test.
            let view = unsafe { AuView::new(base, len) };
            let heap_len = (len - au::HEAP_OFFSET) as u32 & !(au::HEAP_GRANULE - 1);
            let store = |f: usize, v: u32| view.word32(f).store(v, Ordering::Relaxed);
            store(offset_of!(AuHeader, version), au::AU_VERSION);
            store(offset_of!(AuHeader, heap_offset), au::HEAP_OFFSET as u32);
            store(offset_of!(AuHeader, heap_bytes), heap_len);
            store(
                offset_of!(AuHeader, slot_table_offset),
                au::SLOT_TABLE_OFFSET as u32,
            );
            store(offset_of!(AuHeader, slot_count), au::AU_SLOTS);
            store(offset_of!(AuHeader, wire_seq_base), wire_seq_base);
            view.word32(offset_of!(AuHeader, generation))
                .store(generation, Ordering::Release);
            view.word32(offset_of!(AuHeader, magic))
                .store(au::AU_MAGIC, Ordering::Release);
            Self {
                view,
                generation,
                heap_off: au::HEAP_OFFSET as u32,
                heap_len,
                bump: 0,
                publishes: 0,
            }
        }

        /// One chunk into the first FREE slot; `None` when all sixteen are unread (the driver
        /// skips a pool slot here; the test waits instead).
        pub(crate) fn try_publish(
            &mut self,
            data: &[u8],
            wire_seq: u32,
            source_seq: u32,
            qpc_pts: u64,
            flags: u32,
        ) -> Option<usize> {
            let i = (0..au::AU_SLOTS as usize)
                .find(|&i| self.view.slot32(i, STATE).load(Ordering::Acquire) == au::FREE)?;
            let len = data.len() as u32;
            if self.bump + len > self.heap_len {
                self.bump = 0;
            }
            let off = self.heap_off + self.bump;
            self.bump += len;
            // SAFETY: `off..off+len` is inside the heap and no slot names it (the bump only
            // wraps past sixteen live chunks in these tests).
            unsafe {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    self.view.base.add(off as usize),
                    data.len(),
                );
            }
            let w = |f: usize, v: u32| self.view.slot32(i, f).store(v, Ordering::Relaxed);
            w(offset_of!(AuSlot, offset), off);
            w(offset_of!(AuSlot, len), len);
            w(offset_of!(AuSlot, wire_seq), wire_seq);
            w(offset_of!(AuSlot, source_seq), source_seq);
            w(offset_of!(AuSlot, flags), flags);
            let w64 = |f: usize, v: u64| {
                self.view
                    .word64(au::slot_offset(i) + f)
                    .store(v, Ordering::Relaxed)
            };
            w64(offset_of!(AuSlot, qpc_pts), qpc_pts);
            // Fixed offsets past the present stamp: what a driver's submit and publish look like.
            w64(offset_of!(AuSlot, qpc_submit), qpc_pts.saturating_add(7));
            w64(
                offset_of!(AuSlot, qpc_published),
                qpc_pts.saturating_add(11),
            );
            self.view
                .slot32(i, STATE)
                .store(au::PUBLISHED, Ordering::Release);
            self.publishes += 1;
            self.stamp_latest(self.generation, i as u8);
            Some(i)
        }

        pub(crate) fn publish(
            &mut self,
            data: &[u8],
            wire_seq: u32,
            source_seq: u32,
            qpc_pts: u64,
            flags: u32,
        ) -> usize {
            loop {
                if let Some(i) = self.try_publish(data, wire_seq, source_seq, qpc_pts, flags) {
                    return i;
                }
                std::thread::yield_now();
            }
        }

        /// Overwrite `latest` with a token of `generation` — a superseded encoder's publish.
        pub(crate) fn stamp_latest(&self, generation: u32, slot: u8) {
            let tok = FrameToken {
                generation,
                seq: self.publishes,
                slot,
            };
            self.view
                .word64(offset_of!(AuHeader, latest))
                .store(tok.pack(), Ordering::Release);
        }

        pub(crate) fn slot_state(&self, i: usize) -> u32 {
            self.view.slot32(i, STATE).load(Ordering::Acquire)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::producer::Producer;
    use super::*;

    /// An 8-aligned section of the smallest legal size.
    fn section() -> Vec<u64> {
        vec![0u64; au::section_bytes(au::HEAP_MIN_BYTES) as usize / 8]
    }

    fn base(buf: &mut [u64]) -> (*mut u8, usize) {
        (buf.as_mut_ptr().cast::<u8>(), buf.len() * 8)
    }

    fn reader(buf: &mut [u64], wire_seq_base: u32) -> AuReader {
        let (p, len) = base(buf);
        // SAFETY: `buf` outlives every reader the test builds on it.
        AuReader::new(unsafe { AuView::new(p, len) }, wire_seq_base)
    }

    const WHOLE: u32 = au::AU_FIRST | au::AU_LAST;

    #[test]
    fn takes_access_units_in_wire_order_and_frees_the_slots() {
        let mut buf = section();
        let (p, len) = base(&mut buf);
        let mut prod = Producer::init(p, len, 9, 100);
        let mut rd = reader(&mut buf, 100);
        assert!(rd.view().header().magic == au::AU_MAGIC && rd.take_next().unwrap().is_none());
        // Published out of slot order: 102 lands before 101 in the table.
        prod.publish(b"c", 102, 3, 30, WHOLE | au::AU_KEYFRAME);
        prod.publish(b"a", 100, 1, 10, WHOLE);
        prod.publish(
            b"b",
            101,
            2,
            20,
            WHOLE | au::AU_RECOVERY_ANCHOR | au::AU_CHUNK_ALIGNED,
        );
        assert_eq!(rd.ready_aus(), 3);
        let mut got = Vec::new();
        while let Some(t) = rd.take_next().unwrap() {
            assert_eq!(
                (t.qpc_submit, t.qpc_published),
                (t.qpc_pts + 7, t.qpc_pts + 11)
            );
            got.push((t.wire_seq, t.data, t.source_seq, t.qpc_pts, t.flags));
        }
        assert_eq!(
            got,
            vec![
                (100, b"a".to_vec(), 1, 10, WHOLE),
                (
                    101,
                    b"b".to_vec(),
                    2,
                    20,
                    WHOLE | au::AU_RECOVERY_ANCHOR | au::AU_CHUNK_ALIGNED
                ),
                (102, b"c".to_vec(), 3, 30, WHOLE | au::AU_KEYFRAME),
            ]
        );
        assert_eq!(rd.next_wire_seq(), 103);
        assert!((0..16).all(|i| prod.slot_state(i) == au::FREE));
        assert_eq!((rd.freed_unread, rd.gaps), (0, 0));
    }

    #[test]
    fn chunks_of_one_access_unit_keep_heap_order_across_a_wrap() {
        let mut buf = section();
        let (p, len) = base(&mut buf);
        let mut prod = Producer::init(p, len, 1, 0);
        let mut rd = reader(&mut buf, 0);
        // Leave the heap 4 bytes short of full so the third chunk wraps to its start.
        let heap = rd.view().header().heap_bytes as usize;
        let big = vec![0xEEu8; heap - 64 * 1024 - 4];
        prod.publish(&big, 0, 0, 0, WHOLE);
        assert_eq!(rd.take_next().unwrap().unwrap().data.len(), big.len());
        let filler = vec![0u8; 64 * 1024];
        prod.publish(&filler, 1, 1, 1, au::AU_FIRST);
        prod.publish(b"mid", 1, 1, 1, 0);
        prod.publish(b"tail", 1, 1, 1, au::AU_LAST);
        assert_eq!(
            rd.view().slot(2).offset as usize,
            au::HEAP_OFFSET,
            "wrapped"
        );
        assert_eq!(rd.ready_aus(), 1);
        let mut chunks = Vec::new();
        while let Some(t) = rd.take_next().unwrap() {
            chunks.push((t.flags, t.data.len()));
        }
        assert_eq!(
            chunks,
            vec![(au::AU_FIRST, filler.len()), (0, 3), (au::AU_LAST, 4)]
        );
        assert!(!rd.mid_au() && rd.next_wire_seq() == 2);
    }

    #[test]
    fn a_publish_from_another_generation_is_not_consumed() {
        let mut buf = section();
        let (p, len) = base(&mut buf);
        let mut prod = Producer::init(p, len, 5, 0);
        let mut rd = reader(&mut buf, 0);
        let slot = prod.publish(b"x", 0, 0, 0, WHOLE);
        prod.stamp_latest(4, slot as u8);
        assert_eq!(rd.ready_aus(), 0);
        assert!(rd.take_next().unwrap().is_none());
        assert_eq!(
            prod.slot_state(slot),
            au::PUBLISHED,
            "the slot is left alone"
        );
        prod.stamp_latest(5, slot as u8);
        assert_eq!(rd.take_next().unwrap().unwrap().data, b"x");
    }

    #[test]
    fn wire_seq_continues_across_a_rebase_and_stale_slots_are_freed() {
        let mut buf = section();
        let (p, len) = base(&mut buf);
        let mut prod = Producer::init(p, len, 2, 100);
        let mut rd = reader(&mut buf, 100);
        for seq in 100..106 {
            prod.publish(b"p", seq, seq, 0, WHOLE);
            assert_eq!(rd.take_next().unwrap().unwrap().wire_seq, seq);
        }
        assert_eq!(rd.next_wire_seq(), 106);
        // RESET: the host restarts the domain where its own counter stands.
        rd.rebase(200);
        let stale = prod.publish(b"old", 150, 150, 0, WHOLE);
        prod.publish(b"new", 200, 200, 0, WHOLE);
        let t = rd.take_next().unwrap().unwrap();
        assert_eq!((t.wire_seq, t.data.as_slice()), (200, &b"new"[..]));
        assert_eq!(prod.slot_state(stale), au::FREE);
        assert_eq!(rd.freed_unread, 1);
        // A skipped access unit is adopted, not waited for.
        prod.publish(b"skip", 205, 205, 0, WHOLE);
        assert_eq!(rd.take_next().unwrap().unwrap().wire_seq, 205);
        assert_eq!((rd.gaps, rd.next_wire_seq()), (1, 206));
    }

    #[test]
    fn a_slot_outside_the_heap_is_a_fault() {
        let mut buf = section();
        let (p, len) = base(&mut buf);
        let mut prod = Producer::init(p, len, 1, 0);
        let mut rd = reader(&mut buf, 0);
        let i = prod.publish(b"ok", 0, 0, 0, WHOLE);
        rd.view()
            .slot32(i, offset_of!(AuSlot, len))
            .store(u32::MAX, Ordering::Relaxed);
        assert!(matches!(rd.take_next(), Err(AuFault { slot, .. }) if slot == i));
        assert_eq!(prod.slot_state(i), au::FREE);
    }
}
