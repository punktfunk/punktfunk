//! Wire packet header: the fixed [`PacketHeader`] and the flag/geometry consts
//! every packet carries. Zero-copy (de)serializable; 40 bytes, unpadded.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Video packet discriminator; input datagrams use a different magic ([`crate::input`]).
pub const PUNKTFUNK_MAGIC: u8 = 0xC9;

// Transport `flags` nibble. Values match GameStream `FLAG_*`.
pub const FLAG_PIC: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;
/// Bandwidth-probe filler. Not decodable video — do not feed to the decoder.
/// Punktfunk/1 only; GameStream never sets this bit.
pub const FLAG_PROBE: u8 = 0x8;

/// `user_flags` bit, not a transport `flags` bit. Intra-refresh wave complete:
/// the picture is loss-free even though the AU is a coded P (no IDR, so the
/// decoder never sets `AV_FRAME_FLAG_KEY`). `0x10` sits above the FLAG_* nibble
/// because the host reuses those bit values inside `user_flags`.
pub const USER_FLAG_RECOVERY_POINT: u32 = 0x10;

/// `user_flags` bit. Single-frame clean re-anchor (LTR/RFI P-frame off a
/// known-good reference, not an IDR). Unlike [`USER_FLAG_RECOVERY_POINT`],
/// the picture is loss-free the instant this AU decodes — the first mark
/// is enough. Coded P, so the decoder never sets `AV_FRAME_FLAG_KEY`.
pub const USER_FLAG_RECOVERY_ANCHOR: u32 = 0x20;

/// `user_flags` bit. The close of an intra-refresh wave, set beside
/// [`USER_FLAG_RECOVERY_POINT`]: the picture is fully swept on this AU. A client that
/// has seen the bit lifts on the first close after a start seen since its loss instead
/// of counting two marks, so a close whose wave began before the loss never lifts.
pub const USER_FLAG_RECOVERY_CLOSE: u32 = 0x200;

/// `user_flags` bit. Each `shard_payload`-sized window of the frame buffer
/// is a self-delimiting codec packet, zero-padded. Missing shards stay zero
/// and the codec skips those windows; even a complete frame must be consumed
/// window-by-window (the padding is not in the stream).
pub const USER_FLAG_CHUNK_ALIGNED: u32 = 0x40;

/// `user_flags` bit. Slice-streamed AU: sentinels (`block_count == 0`) carry
/// the shard-aligned base in `frame_bytes`; the final block's base is
/// `(total_data_shards − final_data_shards) × shard_bytes`. Uniform-geometry
/// offsets do not apply. Set on every packet of the AU — reorder can deliver
/// the final block first. Only toward peers advertising
/// [`VIDEO_CAP_STREAMED_AU`](crate::quic::VIDEO_CAP_STREAMED_AU) ∧
/// [`VIDEO_CAP_MULTI_SLICE`](crate::quic::VIDEO_CAP_MULTI_SLICE).
pub const USER_FLAG_SLICE_STREAM: u32 = 0x80;

/// `user_flags` bit. Host re-encoded a held frame (idle keepalive); no new
/// content. Informational, set unconditionally. Trust a clear bit as "active"
/// only when the host advertised
/// [`HOST_CAP2_REPEAT_MARK`](crate::quic::HOST_CAP2_REPEAT_MARK) — against an
/// older host, zero means unknown, not all-active.
pub const USER_FLAG_REPEAT: u32 = 0x100;

/// Widest lost-frame range (`last - first`, wrapping) RFI may repair; wider
/// goes to the keyframe path on both ends. 256 frames is >1 s even at 240 Hz,
/// past any encoder DPB (NVENC 5 frames; AMD LTR ~1 s). A request this wide
/// has no valid reference, or the counters have desynced.
pub const RFI_MAX_RANGE: u32 = 256;

/// Acceptance ceiling, not a transmit size. 9216 fits a 9000-MTU jumbo
/// (sealed 8972 B). `Config::validate` keeps
/// `shard_payload + WIRE_OVERHEAD` under this. Receive rings
/// are sized from it so a jumbo geometry needs no mid-session resize.
pub const MAX_DATAGRAM_BYTES: usize = 9216;

#[repr(C)]
#[derive(Clone, Copy, Debug, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct PacketHeader {
    pub pts_ns: u64,
    pub frame_index: u32,
    pub stream_seq: u32,
    pub frame_bytes: u32,
    pub user_flags: u32,
    pub block_index: u16,
    pub block_count: u16,
    pub data_shards: u16,
    pub recovery_shards: u16,
    pub shard_index: u16,
    pub shard_bytes: u16,
    pub magic: u8,
    pub version: u8,
    pub fec_scheme: u8,
    pub flags: u8,
}

pub const HEADER_LEN: usize = std::mem::size_of::<PacketHeader>();

const _: () = assert!(HEADER_LEN == 40, "PacketHeader must be 40 bytes / unpadded");
