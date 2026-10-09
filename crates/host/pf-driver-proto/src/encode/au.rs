//! The AU section: header, a fixed slot table, and a bitstream heap, in one host-created
//! mapping the driver writes and the host reads.
//!
//! The encode thread is the only writer. It copies an access unit — or one slice chunk of one,
//! when the host asked for wire chunking — into the heap, fills a [`FREE`] slot, publishes by
//! storing a packed [`FrameToken`](super::FrameToken) into [`AuHeader::latest`], and signals
//! the ready event. The host loads `latest`, checks the generation, takes the slot and
//! releases it back to `FREE`.
//! Sixteen slots is where back-pressure lands: with none free the encode thread skips the next
//! pool slot, so the drop falls on pixels, which are free to drop, and never on an access unit,
//! which is not.
//!
//! The host sizes the heap from the session's peak bitrate ([`heap_bytes_for`]) and the section
//! from the heap ([`section_bytes`]), both before
//! [`IOCTL_SET_ENCODE`](super::IOCTL_SET_ENCODE). A reader trusts no field here until
//! [`au_readable`] passes.

use bytemuck::{Pod, Zeroable};

/// Header magic (`"PFAU"` LE), stamped by the host before the handle is delivered.
pub const AU_MAGIC: u32 = 0x5541_4650;
/// AU-section layout version; moved with `PROTOCOL_VERSION` v7 and v9.
pub const AU_VERSION: u32 = 8;
/// Slots in the table. Fixed: the host allocates exactly this many.
pub const AU_SLOTS: u32 = 16;
/// [`AuHeader`] size, and therefore where the slot table starts.
pub const AU_HEADER_SIZE: usize = 128;
/// [`AuSlot`] size.
pub const AU_SLOT_SIZE: usize = 48;
/// Byte offset of the slot table inside the section.
pub const SLOT_TABLE_OFFSET: usize = AU_HEADER_SIZE;
/// Byte offset of the heap: past the header and the whole table, still 64-byte aligned so
/// heap writes never share a cache line with a slot record the host is polling.
pub const HEAP_OFFSET: usize = SLOT_TABLE_OFFSET + AU_SLOTS as usize * AU_SLOT_SIZE;

/// Heap sizes round up to this.
pub const HEAP_GRANULE: u32 = 64 * 1024;
/// Heap floor: 16 slots need room for 16 access units whatever the bitrate implies.
pub const HEAP_MIN_BYTES: u32 = 1024 * 1024;
/// Heap cap. Past this the session is misconfigured, not bandwidth-hungry.
pub const HEAP_MAX_BYTES: u32 = 64 * 1024 * 1024;
/// Section page alignment ([`section_bytes`]).
pub const SECTION_ALIGN: u32 = 4096;

/// [`AuHeader::encoder_state`]: no encoder — before the first SET_ENCODE, or between
/// sessions. The pool keeps its stashed slot; nothing is published.
pub const ENCODER_CLOSED: u32 = 0;
/// A backend is open and the encode thread is waiting on pool slots.
pub const ENCODER_OPEN: u32 = 1;
/// Access units are flowing.
pub const ENCODER_ENCODING: u32 = 2;
/// A backend call has not returned. `source_seq` advances, `last_au_qpc` does not; the host
/// answers with [`ENCODE_CTL_RESET`](super::ENCODE_CTL_RESET) and `detached` gains one.
pub const ENCODER_WEDGED: u32 = 3;

/// [`AuSlot::flags`], mirroring `pf_encode_core::AuChunk`: opens an access unit. AU metadata
/// is authoritative on this slot — the host opens the wire frame from it.
pub const AU_FIRST: u32 = 1 << 0;
/// Closes the access unit and releases the encoder's in-flight slot.
pub const AU_LAST: u32 = 1 << 1;
/// IDR; sets the client's SOF/keyframe wire flags.
pub const AU_KEYFRAME: u32 = 1 << 2;
/// A clean picture after RFI — the client lifts its freeze here without waiting for an IDR.
pub const AU_RECOVERY_ANCHOR: u32 = 1 << 3;
/// The AU's chunks are cut on the codec's own window boundaries (PyroWave); the host
/// forwards it as the wire's chunk-aligned user flag.
pub const AU_CHUNK_ALIGNED: u32 = 1 << 4;
/// Start or close of an encoder-driven intra refresh wave; the host forwards it as the
/// wire's recovery-point user flag. A driver that never waves leaves it clear.
pub const AU_RECOVERY_POINT: u32 = 1 << 5;
/// The wave's close, beside [`AU_RECOVERY_POINT`]; the host forwards it as the wire's
/// recovery-close user flag.
pub const AU_RECOVERY_CLOSE: u32 = 1 << 6;

/// [`AuSlot::state`]: the encode thread may take this slot. There is no WRITING state —
/// the encode thread fills the heap before it claims a slot.
pub const FREE: u32 = 0;
/// Written and published; the host may take it.
pub const PUBLISHED: u32 = 1;
/// The host is copying the bytes out; the encode thread must not touch it.
pub const READING: u32 = 2;

/// Section header. The host stamps the layout fields and the magic last, before delivering
/// the handle; the driver owns everything from `latest` down and writes it through atomic
/// views over the mapping, `latest` with Release after the slot it names is complete.
/// Telemetry here is what the classifier reads instead of inferring a stall from timers:
/// `source_seq` moving while `last_au_qpc` stands still is a wedged encoder, and both
/// standing still is a display composing nothing.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct AuHeader {
    /// [`AU_MAGIC`], host-stamped last.
    pub magic: u32,
    /// [`AU_VERSION`], host-stamped.
    pub version: u32,
    /// Host-stamped [`HEAP_OFFSET`].
    pub heap_offset: u32,
    /// Host-stamped heap size — [`heap_bytes_for`].
    pub heap_bytes: u32,
    /// Host-stamped [`SLOT_TABLE_OFFSET`].
    pub slot_table_offset: u32,
    /// Host-stamped [`AU_SLOTS`].
    pub slot_count: u32,
    /// The publish cell: a packed [`FrameToken`](super::FrameToken).
    pub latest: u64,
    /// Bumped by the driver on every SET_ENCODE; a publish carries it.
    pub generation: u32,
    /// Echo of [`SetEncodeRequest::wire_seq_base`](super::SetEncodeRequest::wire_seq_base).
    pub wire_seq_base: u32,
    /// One of the `ENCODER_*` states.
    pub encoder_state: u32,
    /// Encode threads abandoned after a wedge. Two is the `DriverCycle` threshold.
    pub detached: u32,
    /// QPC of the most recent publish — the stall clock the classifier reads.
    pub last_au_qpc: u64,
    /// QPC of the drain worker's most recent pass, stored after `FinishedProcessingFrame`.
    pub drain_heartbeat_qpc: u64,
    /// Frames the drain worker handed the pool — DWM's cadence, ahead of the encoder.
    pub source_seq: u64,
    /// Frames dropped at the pool or skipped for a full slot table.
    pub dropped_total: u64,
    /// Access units published.
    pub published_total: u64,
    /// One of the `DRV_STATUS_*` words — how a driver with no debugger reports.
    pub driver_status: u32,
    /// Raw detail for `driver_status`.
    pub driver_status_detail: u32,
    /// Rate the backend is actually encoding at, kbps, rewritten every time the driver
    /// drains an [`ENCODE_CTL_RECONFIGURE_BITRATE`](super::ENCODE_CTL_RECONFIGURE_BITRATE).
    /// The ctl is queued for the encode thread and has no reply, so a backend that
    /// declines or clamps one is otherwise invisible: the host would go on reporting a
    /// rate nothing encodes. Occupies the old `_reserved` at offset 96; `0` is a driver
    /// that predates the stamp, and the host keeps the rate it asked for.
    pub applied_bitrate_kbps: u32,
    /// Pads the header to [`AU_HEADER_SIZE`]; zero.
    pub _reserved: [u8; 28],
}

/// One slot: where an access unit (or one chunk of one) sits in the heap, and what the host
/// stamps on the wire frame. `offset`/`len` are the driver's to choose, so a reader
/// bounds-checks them against `heap_bytes` before copying. `wire_seq` continues the host's
/// `au_seq` domain from
/// [`SetEncodeRequest::wire_seq_base`](super::SetEncodeRequest::wire_seq_base);
/// `source_seq` names the frame it encodes, which is how a dropped frame stays visible.
/// `qpc_submit` and `qpc_published` are the driver's own clocks on the way through: with
/// `qpc_pts` they split present → arrival into the pool wait, the encode and the hand-off.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct AuSlot {
    /// Byte offset from the start of the section (not from `heap_offset`).
    pub offset: u32,
    /// Bytes of bitstream.
    pub len: u32,
    /// Wire frame index the packetizer stamps.
    pub wire_seq: u32,
    /// Low half of the source frame counter this AU encodes.
    pub source_seq: u32,
    /// Source present QPC — the ground truth the classifier reads, not a timer inference.
    pub qpc_pts: u64,
    /// `AU_*` flag bits.
    pub flags: u32,
    /// [`FREE`], [`PUBLISHED`] or [`READING`].
    pub state: u32,
    /// QPC when the driver handed the frame to its encoder. `0` from a driver that
    /// predates the stamp.
    pub qpc_submit: u64,
    /// QPC when the driver wrote this record.
    pub qpc_published: u64,
}

/// Heap bytes for a session: four frames at `max_bitrate_kbps`, doubled for burst, rounded
/// up to [`HEAP_GRANULE`] and clamped into [`HEAP_MIN_BYTES`]..=[`HEAP_MAX_BYTES`].
///
/// Four frames is the slot table's working set while the host drains; the doubling pays for
/// an IDR several times the average AU. 400 Mbps at 60 fps lands at 6.4 MiB, inside the
/// 8 MiB the plan budgets, and the same bitrate at 120 fps needs half of it. `fps == 0`
/// reads as 1 rather than dividing by zero.
#[must_use]
pub const fn heap_bytes_for(max_bitrate_kbps: u32, fps: u32) -> u32 {
    let fps = if fps == 0 { 1 } else { fps as u64 };
    let per_second = (max_bitrate_kbps as u64) * 1000 / 8;
    let granule = HEAP_GRANULE as u64;
    let rounded = (8 * (per_second / fps)).div_ceil(granule) * granule;
    if rounded < HEAP_MIN_BYTES as u64 {
        HEAP_MIN_BYTES
    } else if rounded > HEAP_MAX_BYTES as u64 {
        HEAP_MAX_BYTES
    } else {
        rounded as u32
    }
}

/// Section bytes for a heap: header, slot table and heap, rounded up to a 4 KiB page. The
/// host allocates exactly this and sends it as
/// [`SetEncodeRequest::section_bytes`](super::SetEncodeRequest::section_bytes).
#[must_use]
pub const fn section_bytes(heap_bytes: u32) -> u32 {
    let align = SECTION_ALIGN as u64;
    let total = HEAP_OFFSET as u64 + heap_bytes as u64;
    (total.div_ceil(align) * align) as u32
}

/// Byte offset of slot `i`'s record inside the section.
#[must_use]
pub const fn slot_offset(i: usize) -> usize {
    SLOT_TABLE_OFFSET + i * AU_SLOT_SIZE
}

/// Whether a mapped header may be read past its magic: the host's magic, this version, the
/// layout constants both sides already agree on, and a heap within the caps. Every other
/// field is an index a writer chose, so `false` means touch nothing — the fail-closed rule
/// the ring's `check_attach` follows.
#[must_use]
pub const fn au_readable(header: &AuHeader) -> bool {
    header.magic == AU_MAGIC
        && header.version == AU_VERSION
        && header.slot_count == AU_SLOTS
        && header.slot_table_offset as usize == SLOT_TABLE_OFFSET
        && header.heap_offset as usize == HEAP_OFFSET
        && header.heap_bytes >= HEAP_MIN_BYTES
        && header.heap_bytes <= HEAP_MAX_BYTES
}

/// The writer's bookkeeping over the heap and the slot table: which slot an access
/// unit (or one chunk of it) goes into and where its bytes land. Pure over a snapshot
/// of the slot states the caller loads, so it runs under `cargo test` anywhere.
///
/// Bytes are placed by a bump pointer that wraps at the heap's end and never straddles
/// it. A placement that overlaps the recorded range of a slot the host still holds
/// ([`PUBLISHED`] or [`READING`]) is refused, never moved past it: the writer then waits
/// or drops at the pool. One access unit's chunks therefore sit at ascending offsets
/// except across a single wrap — the order the host reads a multi-chunk AU in.
#[derive(Clone, Debug)]
pub struct HeapRing {
    heap_offset: u32,
    heap_bytes: u32,
    /// Next write offset, section-relative.
    head: u32,
    /// Round-robin start for the slot pick.
    next_slot: usize,
    /// `(offset, len)` last handed out per slot; `len == 0` = nothing recorded.
    ranges: [(u32, u32); AU_SLOTS as usize],
}

impl HeapRing {
    #[must_use]
    pub const fn new(heap_offset: u32, heap_bytes: u32) -> Self {
        Self {
            heap_offset,
            heap_bytes,
            head: heap_offset,
            next_slot: 0,
            ranges: [(0, 0); AU_SLOTS as usize],
        }
    }

    /// A [`FREE`] slot and a heap range of `len` bytes for it, recorded as the slot's.
    /// `None` when no slot is free, `len` exceeds the heap, or both placements — in
    /// place and after a wrap — overlap a range the host still holds.
    pub fn take(&mut self, len: u32, states: &[u32; AU_SLOTS as usize]) -> Option<(usize, u32)> {
        if len > self.heap_bytes {
            return None;
        }
        let n = AU_SLOTS as usize;
        let slot = (0..n)
            .map(|k| (self.next_slot + k) % n)
            .find(|&i| states[i] == FREE)?;
        let end = self.heap_offset + self.heap_bytes;
        let in_place = (self.head + len <= end).then_some(self.head);
        let offset = in_place
            .into_iter()
            .chain(core::iter::once(self.heap_offset))
            .find(|&at| !self.overlaps(at, len, states))?;
        self.ranges[slot] = (offset, len);
        self.head = offset + len;
        self.next_slot = (slot + 1) % n;
        Some((slot, offset))
    }

    /// Whether `[at, at + len)` touches a range a non-[`FREE`] slot still names.
    fn overlaps(&self, at: u32, len: u32, states: &[u32; AU_SLOTS as usize]) -> bool {
        self.ranges
            .iter()
            .zip(states)
            .any(|(&(off, held), &state)| {
                state != FREE && held != 0 && at < off + held && off < at + len
            })
    }
}

// Layout crosses the process boundary; Pod rejects internal padding and these pin the
// externally-visible sizes, so a same-size field reorder is a compile error.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<AuHeader>() == AU_HEADER_SIZE);
    assert!(offset_of!(AuHeader, magic) == 0);
    assert!(offset_of!(AuHeader, version) == 4);
    assert!(offset_of!(AuHeader, heap_offset) == 8);
    assert!(offset_of!(AuHeader, heap_bytes) == 12);
    assert!(offset_of!(AuHeader, slot_table_offset) == 16);
    assert!(offset_of!(AuHeader, slot_count) == 20);
    assert!(offset_of!(AuHeader, latest) == 24);
    assert!(offset_of!(AuHeader, generation) == 32);
    assert!(offset_of!(AuHeader, wire_seq_base) == 36);
    assert!(offset_of!(AuHeader, encoder_state) == 40);
    assert!(offset_of!(AuHeader, detached) == 44);
    assert!(offset_of!(AuHeader, last_au_qpc) == 48);
    assert!(offset_of!(AuHeader, drain_heartbeat_qpc) == 56);
    assert!(offset_of!(AuHeader, source_seq) == 64);
    assert!(offset_of!(AuHeader, dropped_total) == 72);
    assert!(offset_of!(AuHeader, published_total) == 80);
    assert!(offset_of!(AuHeader, driver_status) == 88);
    assert!(offset_of!(AuHeader, driver_status_detail) == 92);
    assert!(offset_of!(AuHeader, applied_bitrate_kbps) == 96);
    assert!(offset_of!(AuHeader, _reserved) == 100);

    assert!(size_of::<AuSlot>() == AU_SLOT_SIZE);
    assert!(offset_of!(AuSlot, offset) == 0);
    assert!(offset_of!(AuSlot, len) == 4);
    assert!(offset_of!(AuSlot, wire_seq) == 8);
    assert!(offset_of!(AuSlot, source_seq) == 12);
    assert!(offset_of!(AuSlot, qpc_pts) == 16);
    assert!(offset_of!(AuSlot, flags) == 24);
    assert!(offset_of!(AuSlot, state) == 28);
    assert!(offset_of!(AuSlot, qpc_submit) == 32);
    assert!(offset_of!(AuSlot, qpc_published) == 40);

    assert!(HEAP_OFFSET == 896 && HEAP_OFFSET % 64 == 0);
    assert!(SLOT_TABLE_OFFSET % 8 == 0);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn au_section_layout_is_pinned() {
        use core::mem::{offset_of, size_of};

        println!(
            "AuHeader {} bytes: latest@{} generation@{} encoder_state@{} last_au_qpc@{} \
             driver_status@{}; AuSlot {} bytes; slot table @{}, heap @{}",
            size_of::<AuHeader>(),
            offset_of!(AuHeader, latest),
            offset_of!(AuHeader, generation),
            offset_of!(AuHeader, encoder_state),
            offset_of!(AuHeader, last_au_qpc),
            offset_of!(AuHeader, driver_status),
            size_of::<AuSlot>(),
            SLOT_TABLE_OFFSET,
            HEAP_OFFSET,
        );
        assert_eq!(size_of::<AuHeader>(), 128);
        assert_eq!(offset_of!(AuHeader, magic), 0);
        assert_eq!(offset_of!(AuHeader, version), 4);
        assert_eq!(offset_of!(AuHeader, heap_offset), 8);
        assert_eq!(offset_of!(AuHeader, heap_bytes), 12);
        assert_eq!(offset_of!(AuHeader, slot_table_offset), 16);
        assert_eq!(offset_of!(AuHeader, slot_count), 20);
        assert_eq!(offset_of!(AuHeader, latest), 24);
        assert_eq!(offset_of!(AuHeader, generation), 32);
        assert_eq!(offset_of!(AuHeader, wire_seq_base), 36);
        assert_eq!(offset_of!(AuHeader, encoder_state), 40);
        assert_eq!(offset_of!(AuHeader, detached), 44);
        assert_eq!(offset_of!(AuHeader, last_au_qpc), 48);
        assert_eq!(offset_of!(AuHeader, drain_heartbeat_qpc), 56);
        assert_eq!(offset_of!(AuHeader, source_seq), 64);
        assert_eq!(offset_of!(AuHeader, dropped_total), 72);
        assert_eq!(offset_of!(AuHeader, published_total), 80);
        assert_eq!(offset_of!(AuHeader, driver_status), 88);
        assert_eq!(offset_of!(AuHeader, driver_status_detail), 92);
        // Carved out of `_reserved`, which the host still zeroes: an older driver leaves it 0.
        assert_eq!(offset_of!(AuHeader, applied_bitrate_kbps), 96);
        assert_eq!(offset_of!(AuHeader, _reserved), 100);

        assert_eq!(size_of::<AuSlot>(), 48);
        assert_eq!(offset_of!(AuSlot, offset), 0);
        assert_eq!(offset_of!(AuSlot, len), 4);
        assert_eq!(offset_of!(AuSlot, wire_seq), 8);
        assert_eq!(offset_of!(AuSlot, source_seq), 12);
        assert_eq!(offset_of!(AuSlot, qpc_pts), 16);
        assert_eq!(offset_of!(AuSlot, flags), 24);
        assert_eq!(offset_of!(AuSlot, state), 28);
        assert_eq!(offset_of!(AuSlot, qpc_submit), 32);
        assert_eq!(offset_of!(AuSlot, qpc_published), 40);

        assert_eq!(slot_offset(0), 128);
        assert_eq!(slot_offset(15), 128 + 15 * 48);
        assert_eq!(slot_offset(AU_SLOTS as usize), HEAP_OFFSET);
        assert_eq!(HEAP_OFFSET, 896);
        assert_eq!(&AU_MAGIC.to_le_bytes(), b"PFAU");
        // The retired ring header's magic — a v6 section must never read as an AU one.
        assert_ne!(AU_MAGIC, 0x4456_4650);
    }

    #[test]
    fn au_flag_bits_and_slot_states_are_distinct() {
        let flags = [
            AU_FIRST,
            AU_LAST,
            AU_KEYFRAME,
            AU_RECOVERY_ANCHOR,
            AU_CHUNK_ALIGNED,
            AU_RECOVERY_POINT,
            AU_RECOVERY_CLOSE,
        ];
        println!("AU flags: {flags:?}; states: {FREE} {PUBLISHED} {READING}");
        assert_eq!(flags, [1, 2, 4, 8, 16, 32, 64]);
        assert_eq!(flags.iter().fold(0, |a, b| a | b), 0b111_1111);
        // A whole non-key AU is FIRST|LAST — the two must not alias.
        assert_eq!(AU_FIRST | AU_LAST, 3);
        assert_eq!([FREE, PUBLISHED, READING], [0, 1, 2]);
        assert_eq!(
            [
                ENCODER_CLOSED,
                ENCODER_OPEN,
                ENCODER_ENCODING,
                ENCODER_WEDGED
            ],
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn heap_and_section_sizing_match_the_plan() {
        let mib = 1024 * 1024;
        for (kbps, fps) in [(20_000, 60), (400_000, 60), (400_000, 120)] {
            println!(
                "{kbps} kbps @ {fps} fps -> heap {} B, section {} B",
                heap_bytes_for(kbps, fps),
                section_bytes(heap_bytes_for(kbps, fps)),
            );
        }
        // 20 Mbps is far under the floor — a 16-slot table still needs room.
        assert_eq!(heap_bytes_for(20_000, 60), HEAP_MIN_BYTES);
        assert_eq!(HEAP_MIN_BYTES, mib);
        // The plan's worst case: 400 Mbps at 60 fps must fit the 8 MiB budget.
        let big = heap_bytes_for(400_000, 60);
        assert!(big <= 8 * mib, "400 Mbps/60 wanted {big} B, over 8 MiB");
        assert!(big > 4 * mib, "and it must not be a token allocation");
        // More frames per second means less bitstream per frame.
        assert!(heap_bytes_for(400_000, 120) < big);
        // Everything lands on the granule, is clamped, and survives absurd inputs.
        assert_eq!(big % HEAP_GRANULE, 0);
        assert_eq!(heap_bytes_for(u32::MAX, 1), HEAP_MAX_BYTES);
        assert_eq!(heap_bytes_for(400_000, 0), heap_bytes_for(400_000, 1));

        assert_eq!(section_bytes(0), 4096);
        for heap in [HEAP_MIN_BYTES, big, HEAP_MAX_BYTES] {
            let s = section_bytes(heap);
            assert_eq!(s % SECTION_ALIGN, 0);
            assert!(s as usize >= HEAP_OFFSET + heap as usize);
            assert!((s as usize) < HEAP_OFFSET + heap as usize + 4096);
        }
    }

    #[test]
    fn au_readable_is_fail_closed() {
        let good = AuHeader {
            magic: AU_MAGIC,
            version: AU_VERSION,
            heap_offset: HEAP_OFFSET as u32,
            heap_bytes: heap_bytes_for(50_000, 60),
            slot_table_offset: SLOT_TABLE_OFFSET as u32,
            slot_count: AU_SLOTS,
            ..AuHeader::zeroed()
        };
        println!("readable header: {good:?}");
        assert!(au_readable(&good));
        // A zeroed section is what an unmapped or half-created one looks like.
        assert!(!au_readable(&AuHeader::zeroed()));
        for bad in [
            AuHeader {
                magic: 0x4456_4650,
                ..good
            },
            AuHeader { version: 6, ..good },
            AuHeader {
                heap_bytes: HEAP_MAX_BYTES + 1,
                ..good
            },
            AuHeader {
                heap_bytes: 4096,
                ..good
            },
            AuHeader {
                slot_count: 8,
                ..good
            },
            AuHeader {
                slot_table_offset: 64,
                ..good
            },
            AuHeader {
                heap_offset: 128,
                ..good
            },
        ] {
            assert!(!au_readable(&bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn heap_ring_back_pressures_on_the_slot_table() {
        let n = AU_SLOTS as usize;
        let mut ring = HeapRing::new(HEAP_OFFSET as u32, HEAP_MIN_BYTES);
        let mut states = [FREE; AU_SLOTS as usize];
        // Sixteen publishes fill the table; the writer claims by storing PUBLISHED itself.
        let mut offsets = Vec::new();
        for k in 0..n {
            let (slot, offset) = ring.take(1000, &states).expect("a free slot");
            assert_eq!(slot, k, "slots hand out round-robin");
            states[slot] = PUBLISHED;
            offsets.push(offset);
        }
        assert!(offsets.windows(2).all(|w| w[1] == w[0] + 1000));
        assert_eq!(offsets[0], HEAP_OFFSET as u32);
        println!(
            "16 slots placed from {} to {}",
            offsets[0],
            offsets[n - 1] + 1000
        );
        // All unread: the seventeenth is refused — the drop lands on the pool, not on an AU.
        assert_eq!(ring.take(1000, &states), None);
        // The host takes one READING then frees it; only then is a slot available again.
        states[5] = READING;
        assert_eq!(ring.take(1000, &states), None);
        states[5] = FREE;
        let (slot, offset) = ring.take(1000, &states).expect("the freed slot");
        assert_eq!(slot, 5);
        assert_eq!(
            offset,
            offsets[n - 1] + 1000,
            "bytes keep bumping, slots recycle"
        );
        // Zero-length chunks and over-heap chunks are answered, never placed wrongly.
        assert_eq!(ring.take(HEAP_MIN_BYTES + 1, &states), None);
    }

    #[test]
    fn heap_ring_wraps_without_overwriting_held_bytes() {
        let heap = HEAP_MIN_BYTES;
        let base = HEAP_OFFSET as u32;
        let mut ring = HeapRing::new(base, heap);
        let mut states = [FREE; AU_SLOTS as usize];
        let chunk = heap / 4 + 1;
        // Three chunks fit; the fourth does not fit at the end and must wrap, never straddle.
        let mut placed = Vec::new();
        for _ in 0..3 {
            let (slot, off) = ring.take(chunk, &states).unwrap();
            states[slot] = PUBLISHED;
            placed.push((slot, off));
            assert!(off + chunk <= base + heap);
        }
        // The wrap target overlaps slot 0's bytes while the host holds them.
        assert_eq!(ring.take(chunk, &states), None);
        states[placed[0].0] = FREE;
        let (slot, off) = ring.take(chunk, &states).unwrap();
        println!("wrapped to slot {slot} at {off} after {:?}", placed);
        assert_eq!(
            off, base,
            "a chunk that does not fit at the end starts the heap over"
        );
        states[slot] = PUBLISHED;
        // The next one continues after the wrap and still refuses slot 1's held bytes.
        assert_eq!(ring.take(chunk, &states), None);
        states[placed[1].0] = FREE;
        let (_, off2) = ring.take(chunk, &states).unwrap();
        assert_eq!(off2, base + chunk);
        // A freed slot's stale range never blocks: FREE ranges are ignored.
        states = [FREE; AU_SLOTS as usize];
        assert!(
            ring.take(heap, &states).is_some(),
            "a whole-heap AU fits an empty heap"
        );
    }
}
