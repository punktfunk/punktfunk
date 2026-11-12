//! Seat pad audio: the contract between a seat host and `pf-seat-audio.dll`, the audio endpoint
//! enumerator a seat's Remote Desktop listener loads.
//!
//! For every DualSense the seat host serves, it creates two `Global\` sections named for its
//! session and the pad's OS slot. The presence section holds [`PRESENT_MAGIC`] while the host
//! serves the slot: then the enumerator lists the slot's render endpoint in that session alone.
//! Only a privileged process can create a `Global\` section and only SYSTEM may write this one,
//! so another session's user can neither fake it nor keep it present. The ring section carries a
//! [`RingHeader`] and a ring of 4-channel S16 frames; the endpoint object inside audiodg writes
//! what the engine renders, and the seat host reads it for the `0xD1` plane. The seat host
//! creates both before the pad's HID device, and zeroes the magic when the pad goes.

use bytemuck::{Pod, Zeroable};
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU64, Ordering};

/// `"PFSA"` little-endian. The seat host writes it last, after the other header fields.
pub const RING_MAGIC: u32 = 0x4153_4650;
/// Bumped on any incompatible change to the header or the ring. A renamed object fails safe:
/// the other side finds nothing.
pub const RING_VERSION: u32 = 1;
/// `"PFSP"` little-endian: the presence section's one `u32` while its slot is served.
pub const PRESENT_MAGIC: u32 = 0x5053_4650;
/// The presence section's size.
pub const PRESENCE_BYTES: usize = size_of::<u32>();
/// Pad OS slots a box hands out (`pf-inject`'s pad pool).
pub const SLOTS: u8 = 16;
pub const RATE: u32 = 48_000;
/// FL FR BL BR: speaker pair, then the haptic actuators.
pub const CHANNELS: usize = 4;
/// One 4-channel S16 frame.
pub const FRAME_BYTES: usize = CHANNELS * 2;
/// 8192 frames ≈ 170 ms: a power of two, far beyond one 10 ms engine period.
pub const RING_FRAMES: u64 = 8192;
/// The dedicated Remote Desktop listener whose sessions get pad endpoints.
pub const LISTENER: &str = "punktfunk-tcp";
/// Its loopback port. The keeper falls back to 3389 when nothing listens here.
pub const LISTENER_PORT: u16 = 3390;

/// `WAVEFORMATEXTENSIBLE`, 4 ch / 48 kHz / 16-bit PCM, mask 0x33: what the endpoint renders.
pub const FORMAT_PCM16: [u8; 40] = format(8, 16, [0x01, 0x00, 0x00, 0x00]);
/// The same layout as 32-bit float: the engine's mix format for the endpoint.
pub const FORMAT_F32: [u8; 40] = format(16, 32, [0x03, 0x00, 0x00, 0x00]);

/// A 4-channel 48 kHz `WAVEFORMATEXTENSIBLE` with `block` bytes per frame and the subformat
/// GUID whose first four bytes are `subtype` (`KSDATAFORMAT_SUBTYPE_PCM` / `_IEEE_FLOAT`).
const fn format(block: u16, bits: u16, subtype: [u8; 4]) -> [u8; 40] {
    let rate = RATE.to_le_bytes();
    let bytes = (RATE * block as u32).to_le_bytes();
    let [b0, b1] = block.to_le_bytes();
    let [w0, w1] = bits.to_le_bytes();
    [
        0xfe, 0xff, 0x04, 0x00, rate[0], rate[1], rate[2], rate[3], bytes[0], bytes[1], bytes[2],
        bytes[3], b0, b1, w0, w1, 22, 0, w0, w1, 0x33, 0, 0, 0, subtype[0], subtype[1], subtype[2],
        subtype[3], 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
    ]
}

/// The section's first 128 bytes. `write_pos` sits on its own cache line.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct RingHeader {
    /// [`RING_MAGIC`], written last.
    pub magic: u32,
    pub version: u32,
    /// The seat's Windows session id.
    pub session: u32,
    /// The pad's OS slot.
    pub slot: u32,
    /// [`RING_FRAMES`].
    pub ring_frames: u32,
    pub reserved: [u32; 11],
    /// Frames written since the section was created. Endpoints move it with [`publish`] after
    /// the frames; readers load it with Acquire.
    pub write_pos: u64,
    pub tail: [u64; 7],
}

pub const WRITE_POS_OFFSET: usize = offset_of!(RingHeader, write_pos);
/// Where frame 0 of the ring starts in the section.
pub const RING_OFFSET: usize = size_of::<RingHeader>();
pub const SECTION_BYTES: usize = RING_OFFSET + RING_FRAMES as usize * FRAME_BYTES;

const _: () = assert!(size_of::<RingHeader>() == 128);
const _: () = assert!(WRITE_POS_OFFSET == 64);
const _: () = assert!(RING_FRAMES.is_power_of_two());

impl RingHeader {
    /// The header a seat host writes for `session` and `slot`, magic included.
    #[must_use]
    pub fn new(session: u32, slot: u8) -> Self {
        Self {
            magic: RING_MAGIC,
            version: RING_VERSION,
            session,
            slot: u32::from(slot),
            ring_frames: RING_FRAMES as u32,
            ..Zeroable::zeroed()
        }
    }

    /// Whether this header was published by a seat host for `session` and `slot`.
    #[must_use]
    pub fn belongs_to(&self, session: u32, slot: u8) -> bool {
        self.magic == RING_MAGIC
            && self.version == RING_VERSION
            && self.session == session
            && self.slot == u32::from(slot)
            && u64::from(self.ring_frames) == RING_FRAMES
    }
}

/// `Global\punktfunk-seat-pad-<session>-<slot>-present`: the presence section.
#[must_use]
pub fn presence_name(session: u32, slot: u8) -> alloc::string::String {
    alloc::format!("Global\\punktfunk-seat-pad-{session}-{slot}-present")
}

/// `Global\punktfunk-seat-pad-<session>-<slot>-pcm`: the ring section.
#[must_use]
pub fn ring_name(session: u32, slot: u8) -> alloc::string::String {
    alloc::format!("Global\\punktfunk-seat-pad-{session}-{slot}-pcm")
}

/// The slot's bare endpoint GUID. AudioEndpointBuilder prefixes `{3.0.0.<session hex>}.`.
#[must_use]
pub fn endpoint_guid(slot: u8) -> alloc::string::String {
    alloc::format!("{{D0A90100-4277-4AC8-93F0-19C1000000{slot:02X}}}")
}

/// The slot an endpoint id names, bare or with the builder's session prefix.
#[must_use]
pub fn slot_of(id: &str) -> Option<u8> {
    let bare = id.rsplit_once("}.").map_or(id, |(_, tail)| tail);
    (0..SLOTS).find(|&slot| bare.eq_ignore_ascii_case(&endpoint_guid(slot)))
}

/// Where frame `pos` lives in the ring, in frames.
#[must_use]
pub fn ring_index(pos: u64) -> usize {
    (pos % RING_FRAMES) as usize
}

/// Moves the ring's cursor past `frames` frames an endpoint wrote at `from`, unless another
/// endpoint on the slot moved it since `from` was read: that endpoint's frames stand and these
/// are dropped. The cursor never moves back, so the reader repeats and skips nothing.
pub fn publish(cursor: &AtomicU64, from: u64, frames: u64) -> bool {
    cursor
        .compare_exchange(from, from + frames, Ordering::Release, Ordering::Relaxed)
        .is_ok()
}

/// The frames a reader at `read` takes when the writer is at `write`: `(from, count)`.
/// A reader lapped by the writer resumes half a ring behind it; a section that restarted
/// below `read` resumes at `write`.
#[must_use]
pub fn readable(write: u64, read: u64) -> (u64, u64) {
    if write < read {
        return (write, 0);
    }
    if write - read > RING_FRAMES {
        let from = write - RING_FRAMES / 2;
        return (from, write - from);
    }
    (read, write - read)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_round_trips_and_names_its_owner() {
        let header = RingHeader::new(5, 3);
        let bytes = bytemuck::bytes_of(&header);
        let back: RingHeader = bytemuck::pod_read_unaligned(bytes);
        assert!(back.belongs_to(5, 3));
        assert!(!back.belongs_to(6, 3));
        assert!(!back.belongs_to(5, 4));
        assert!(!RingHeader::zeroed().belongs_to(0, 0));
    }

    #[test]
    fn endpoint_ids_resolve_to_their_slot() {
        assert_eq!(endpoint_guid(10), "{D0A90100-4277-4AC8-93F0-19C10000000A}");
        assert_eq!(slot_of("{D0A90100-4277-4AC8-93F0-19C10000000A}"), Some(10));
        assert_eq!(
            slot_of("{3.0.0.00000005}.{d0a90100-4277-4ac8-93f0-19c100000003}"),
            Some(3)
        );
        assert_eq!(
            slot_of("{3.0.0.00000005}.{6C26BA7D-F0B2-4225-B422-8168C5261E45}"),
            None
        );
        assert_eq!(slot_of("{D0A90100-4277-4AC8-93F0-19C100000010}"), None);
    }

    #[test]
    fn formats_match_the_dualsense_endpoint() {
        // The local DualSense endpoint's stored blobs on Windows 11 26100.
        let hex = |b: &[u8]| {
            b.iter()
                .map(|x| alloc::format!("{x:02x}"))
                .collect::<alloc::string::String>()
        };
        assert_eq!(
            hex(&FORMAT_PCM16),
            "feff040080bb000000dc05000800100016001000330000000100000000001000800000aa00389b71"
        );
        assert_eq!(
            hex(&FORMAT_F32),
            "feff040080bb000000b80b001000200016002000330000000300000000001000800000aa00389b71"
        );
    }

    #[test]
    fn names_carry_session_and_slot() {
        assert_eq!(
            presence_name(5, 2),
            "Global\\punktfunk-seat-pad-5-2-present"
        );
        assert_eq!(ring_name(5, 2), "Global\\punktfunk-seat-pad-5-2-pcm");
    }

    #[test]
    fn a_reader_takes_what_was_written() {
        assert_eq!(readable(480, 0), (0, 480));
        assert_eq!(readable(480, 480), (480, 0));
        assert_eq!(ring_index(RING_FRAMES + 7), 7);
    }

    #[test]
    fn a_lapped_reader_resumes_half_a_ring_behind() {
        let write = 3 * RING_FRAMES;
        assert_eq!(
            readable(write, 0),
            (write - RING_FRAMES / 2, RING_FRAMES / 2)
        );
        assert_eq!(readable(RING_FRAMES, 0), (0, RING_FRAMES));
    }

    /// Two endpoints on one slot: the one that read the cursor first and published last moves it
    /// nowhere, least of all back.
    #[test]
    fn a_writer_that_lost_the_cursor_never_moves_it_back() {
        let cursor = AtomicU64::new(0);
        let late = cursor.load(Ordering::Relaxed);
        for _ in 0..2 {
            assert!(publish(&cursor, cursor.load(Ordering::Relaxed), 480));
        }
        assert!(!publish(&cursor, late, 480));
        assert_eq!(cursor.load(Ordering::Relaxed), 960);
    }

    #[test]
    fn a_restarted_writer_does_not_replay() {
        assert_eq!(readable(10, 500), (10, 0));
    }
}
