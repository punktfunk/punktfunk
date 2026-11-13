//! Reader half of the v7 access-unit section (`pf_driver_proto::encode::au`). The driver's
//! encode thread copies an access unit — or one slice chunk of it — into the heap, fills a
//! slot, publishes a [`FrameToken`] in `latest`, and signals the ready event; [`AuReader`]
//! takes the published slots out in `wire_seq` order and hands back the bytes.
//!
//! Pure over a mapped [`AuView`], so the same code reads the driver's section on Windows and
//! a `Vec` in the tests. Section creation, `SET_ENCODE`, and the `Encoder` proxy
//! are `encoder.rs`.

use pf_driver_proto::encode::au::{self, AuHeader, AuSlot};
use pf_driver_proto::encode::FrameToken;
use pf_driver_proto::section::SectionView;
use std::mem::offset_of;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const STATE: usize = offset_of!(AuSlot, state);

/// `(distance ahead of next_wire_seq, rank inside the access unit, publish stamp or heap
/// distance)`.
type OrderKey = (u32, u8, u64);

/// One mapped v7 section, alive for the view's lifetime. Header and slot words are only ever
/// touched through aligned atomics, because the driver writes the same bytes; the heap is read
/// plainly, on a slot this side holds READING.
pub(crate) struct AuView<'a> {
    mem: SectionView<'a>,
}

// SAFETY: a raw pointer only. The owning reader moves between the prep and stream threads
// with no concurrent access on this side; the driver's writes arrive through the atomics.
unsafe impl Send for AuView<'_> {}

impl<'a> AuView<'a> {
    /// Panics when `mem` is shorter than the fixed layout, which every header read needs.
    pub(crate) fn new(mem: SectionView<'a>) -> Self {
        assert!(mem.len() >= au::HEAP_OFFSET);
        Self { mem }
    }

    /// The mapping itself, for the fake kit's own view of it.
    #[cfg(test)]
    pub(crate) fn section(&self) -> SectionView<'a> {
        self.mem
    }

    fn word32(&self, off: usize) -> &AtomicU32 {
        self.mem.atomic_u32(off)
    }

    fn word64(&self, off: usize) -> &AtomicU64 {
        self.mem.atomic_u64(off)
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
            frames_input: u32_at(offset_of!(AuHeader, frames_input)),
            _reserved: [0; 24],
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
                .is_some_and(|end| end <= heap_off + heap_len && end <= self.mem.len());
        if !inside {
            return false;
        }
        into.clear();
        into.resize(len, 0);
        // The slot naming this range is READING, so the driver does not write it meanwhile.
        self.mem.copy_to_slice(off, into);
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
    /// This access unit carries the source frame of the one before it: a re-encode, not a
    /// new picture. The same for every chunk of the unit.
    pub repeat: bool,
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
/// share a `wire_seq`; the FIRST chunk goes first, then the rest by the driver's publish stamp,
/// or in heap order from the previous chunk's end from a driver that stamps none. A slot behind
/// `next_wire_seq`, or a chunk of an access unit whose FIRST was never taken, is freed unread:
/// after a `RESET` the domain restarts where the host says, and a superseded encoder's
/// leftovers must not reach the wire.
pub(crate) struct AuReader<'a> {
    view: AuView<'a>,
    /// The `wire_seq` the next access unit carries; `SET_ENCODE` and `RESET` set it.
    next_wire_seq: u32,
    /// The access unit still owing its LAST chunk: `(wire_seq, end offset of the last chunk)`.
    open_au: Option<(u32, u32)>,
    /// `source_seq` of the last access unit started, and whether it repeated its predecessor's.
    last_source: Option<u32>,
    au_repeat: bool,
    /// PUBLISHED slots freed without reaching the wire.
    pub(crate) freed_unread: u64,
    /// Access units the driver skipped: `next_wire_seq` jumped over them.
    pub(crate) gaps: u64,
    /// Runs after a scan reads slot `i`: where a test lands a driver publish mid-pass.
    #[cfg(test)]
    pub(crate) after_slot: Option<Box<dyn FnMut(usize) + Send + 'a>>,
}

impl<'a> AuReader<'a> {
    pub(crate) fn new(view: AuView<'a>, wire_seq_base: u32) -> Self {
        Self {
            view,
            next_wire_seq: wire_seq_base,
            open_au: None,
            last_source: None,
            au_repeat: false,
            freed_unread: 0,
            gaps: 0,
            #[cfg(test)]
            after_slot: None,
        }
    }

    pub(crate) fn view(&self) -> &AuView<'a> {
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

    /// Ordering of a candidate slot: access unit first, then FIRST before the rest, then the
    /// open access unit's chunks in publish order. The driver's stamp gives that order across
    /// any wrap. Heap order from the open chunk's end stands in for a driver that stamps none;
    /// a wrapped chunk that ends past that end defeats it.
    fn order_key(&self, rel: u32, s: &AuSlot) -> OrderKey {
        if s.flags & au::AU_FIRST != 0 {
            return (rel, 0, 0);
        }
        match self.open_au {
            Some(_) if rel == 0 && s.qpc_published != 0 => (rel, 1, s.qpc_published),
            Some((_, end)) if rel == 0 && s.offset >= end => (rel, 2, u64::from(s.offset - end)),
            Some(_) if rel == 0 => (rel, 3, u64::from(s.offset)),
            _ => (rel, 4, u64::from(s.offset)),
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
            #[cfg(test)]
            if let Some(hook) = self.after_slot.as_mut() {
                hook(i);
            }
            if s.state != au::PUBLISHED {
                continue;
            }
            let Some(rel) = self.ahead(s.wire_seq) else {
                self.free_unread(i);
                continue;
            };
            let key = self.order_key(rel, &s);
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
    /// only guaranteed visible after that later slot's Acquire load. So a chunk of the open
    /// access unit is taken only from a second pass, which sees every chunk published before
    /// it; and a pass whose best slot is not the expected next chunk is repeated once before it
    /// is read as a skipped access unit (adopted) or a chunk without its FIRST (freed unread).
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
            let expected = rel == 0 && (first || self.open_au.is_some());
            if expected && (pass == 1 || self.open_au.is_none()) {
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
        // Chunks of one access unit share a sequence, so only its FIRST can compare.
        if s.flags & au::AU_FIRST != 0 {
            self.au_repeat = self.last_source == Some(s.source_seq);
            self.last_source = Some(s.source_seq);
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
            repeat: self.au_repeat,
        }))
    }
}

/// The fake driver: what the driver's encode thread does to a section, minus the encoder.
/// Placement is the driver's own [`HeapRing`]; then the heap bytes, the slot record and a
/// [`FrameToken`] in `latest`, in that order, as `pf-encode-session`'s `drive.rs` publishes.
#[cfg(test)]
pub(crate) mod producer {
    use super::*;
    use pf_driver_proto::encode::au::HeapRing;

    /// An 8-aligned section of the smallest legal size.
    pub(crate) fn section() -> Vec<u64> {
        vec![0u64; au::section_bytes(au::HEAP_MIN_BYTES) as usize / 8]
    }

    pub(crate) struct Producer<'a> {
        view: AuView<'a>,
        generation: u32,
        ring: HeapRing,
        publishes: u32,
        /// Chunks since the open access unit's FIRST: each is stamped one QPC tick after the last.
        chunk: u64,
        /// `false` is a driver that predates the submit and publish stamps: both stay `0`.
        pub(crate) stamps: bool,
    }

    impl<'a> Producer<'a> {
        /// Stamp a fresh header into `mem` the way the host does (magic last) and take the
        /// driver's side of it: `generation`.
        pub(crate) fn init(mem: SectionView<'a>, generation: u32, wire_seq_base: u32) -> Self {
            let heap_len = (mem.len() - au::HEAP_OFFSET) as u32 & !(au::HEAP_GRANULE - 1);
            crate::windows::remote::encoder::stamp_header(mem, heap_len, wire_seq_base);
            Self::attach(mem, generation)
        }

        /// The driver's side of a header the host already stamped: its `generation` over
        /// the host's word, as `EncodeSession::new` stores it, and the heap the header names.
        pub(crate) fn attach(mem: SectionView<'a>, generation: u32) -> Self {
            let view = AuView::new(mem);
            let heap_len = view.header().heap_bytes;
            view.word32(offset_of!(AuHeader, generation))
                .store(generation, Ordering::Release);
            Self {
                view,
                generation,
                ring: HeapRing::new(au::HEAP_OFFSET as u32, heap_len),
                publishes: 0,
                chunk: 0,
                stamps: true,
            }
        }

        /// A 4-aligned header word, as the driver's telemetry stamps it.
        pub(crate) fn stamp32(&self, field: usize, value: u32) {
            self.view.word32(field).store(value, Ordering::Relaxed);
        }

        /// An 8-aligned header word, as the driver's telemetry stamps it.
        pub(crate) fn stamp64(&self, field: usize, value: u64) {
            self.view.word64(field).store(value, Ordering::Relaxed);
        }

        /// One chunk into the slot and heap range the ring picks; `None` when no slot is free
        /// or every placement overlaps bytes the host still holds (the driver drops at the pool).
        pub(crate) fn try_publish(
            &mut self,
            data: &[u8],
            wire_seq: u32,
            source_seq: u32,
            qpc_pts: u64,
            flags: u32,
        ) -> Option<usize> {
            let states =
                std::array::from_fn(|i| self.view.slot32(i, STATE).load(Ordering::Acquire));
            let len = data.len() as u32;
            let (i, off) = self.ring.take(len, &states)?;
            self.view.mem.copy_from_slice(off as usize, data);
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
            // A driver submits an access unit once and publishes its chunks in order: fixed
            // offsets past the present stamp, one tick further per chunk.
            self.chunk = if flags & au::AU_FIRST != 0 {
                0
            } else {
                self.chunk + 1
            };
            let stamp = |at: u64| {
                if self.stamps {
                    qpc_pts.saturating_add(at)
                } else {
                    0
                }
            };
            w64(offset_of!(AuSlot, qpc_submit), stamp(7));
            w64(offset_of!(AuSlot, qpc_published), stamp(11 + self.chunk));
            self.view
                .slot32(i, STATE)
                .store(au::PUBLISHED, Ordering::Release);
            self.publishes += 1;
            self.stamp_latest(self.generation, i as u8);
            Some(i)
        }

        /// [`Self::try_publish`] where the test has left room for it.
        pub(crate) fn publish(
            &mut self,
            data: &[u8],
            wire_seq: u32,
            source_seq: u32,
            qpc_pts: u64,
            flags: u32,
        ) -> usize {
            self.try_publish(data, wire_seq, source_seq, qpc_pts, flags)
                .expect("a free slot and an unheld heap range")
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

        /// Where slot `i`'s bytes sit, section-relative.
        pub(crate) fn slot_offset(&self, i: usize) -> u32 {
            self.view.slot(i).offset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::producer::{section, Producer};
    use super::*;

    fn reader(mem: SectionView<'_>, wire_seq_base: u32) -> AuReader<'_> {
        AuReader::new(AuView::new(mem), wire_seq_base)
    }

    const WHOLE: u32 = au::AU_FIRST | au::AU_LAST;

    #[test]
    fn takes_access_units_in_wire_order_and_frees_the_slots() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 9, 100);
        let mut rd = reader(mem, 100);
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

    /// Stamped or not: a driver that writes no publish stamp is read in heap order.
    #[test]
    fn chunks_of_one_access_unit_keep_their_order_across_a_wrap() {
        for stamps in [true, false] {
            chunks_keep_their_order_across_a_wrap(stamps);
        }
    }

    fn chunks_keep_their_order_across_a_wrap(stamps: bool) {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 1, 0);
        prod.stamps = stamps;
        let mut rd = reader(mem, 0);
        // Leave the heap 4 bytes short of full so the third chunk wraps to its start.
        let heap = rd.view().header().heap_bytes as usize;
        let big = vec![0xEEu8; heap - 64 * 1024 - 4];
        prod.publish(&big, 0, 0, 0, WHOLE);
        assert_eq!(rd.take_next().unwrap().unwrap().data.len(), big.len());
        let filler = vec![0u8; 64 * 1024];
        prod.publish(&filler, 1, 1, 1, au::AU_FIRST);
        prod.publish(b"mid", 1, 1, 1, 0);
        prod.publish(b"tail", 1, 1, 1, au::AU_LAST);
        assert_eq!(prod.slot_offset(3) as usize, au::HEAP_OFFSET, "wrapped");
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

    /// The driver publishes an open access unit's next two chunks after the scan has passed
    /// the first one's slot and before it reaches the second's. The second is not taken
    /// ahead of the first, which would close the unit and free the first unread.
    #[test]
    fn a_chunk_published_behind_the_scan_is_not_overtaken() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 1, 0);
        let mut rd = reader(mem, 0);
        assert_eq!(prod.publish(b"head", 0, 1, 0, au::AU_FIRST), 0);
        assert!(rd.take_next().unwrap().unwrap().data == b"head" && rd.mid_au());
        let mut published = false;
        rd.after_slot = Some(Box::new(move |i| {
            if i == 1 && !published {
                published = true;
                assert_eq!(prod.publish(b"mid", 0, 1, 0, 0), 1);
                assert_eq!(prod.publish(b"tail", 0, 1, 0, au::AU_LAST), 2);
            }
        }));
        let mut got = Vec::new();
        while let Some(t) = rd.take_next().unwrap() {
            got.push(t.data);
        }
        assert_eq!(got, [b"mid".to_vec(), b"tail".to_vec()]);
        assert_eq!((rd.freed_unread, rd.next_wire_seq()), (0, 1));
    }

    /// A re-encode carries its predecessor's source sequence and reads as a repeat; a chunk
    /// of the same access unit never repeats its own FIRST.
    #[test]
    fn an_access_unit_on_the_same_source_frame_is_a_repeat() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 1, 0);
        let mut rd = reader(mem, 0);
        prod.publish(b"new", 0, 7, 0, au::AU_FIRST);
        prod.publish(b"tail", 0, 7, 0, au::AU_LAST);
        prod.publish(b"again", 1, 7, 0, WHOLE);
        prod.publish(b"next", 2, 8, 0, WHOLE);
        let mut got = Vec::new();
        while let Some(t) = rd.take_next().unwrap() {
            got.push((t.wire_seq, t.repeat));
        }
        assert_eq!(got, vec![(0, false), (0, false), (1, true), (2, false)]);
    }

    #[test]
    fn a_publish_from_another_generation_is_not_consumed() {
        let mut buf = section();
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 5, 0);
        let mut rd = reader(mem, 0);
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
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 2, 100);
        let mut rd = reader(mem, 100);
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
        let mem = SectionView::over(&mut buf);
        let mut prod = Producer::init(mem, 1, 0);
        let mut rd = reader(mem, 0);
        let i = prod.publish(b"ok", 0, 0, 0, WHOLE);
        rd.view()
            .slot32(i, offset_of!(AuSlot, len))
            .store(u32::MAX, Ordering::Relaxed);
        assert!(matches!(rd.take_next(), Err(AuFault { slot, .. }) if slot == i));
        assert_eq!(prod.slot_state(i), au::FREE);
    }
}
