//! Zero-copy wire framing: split an access unit into FEC blocks of MTU-sized
//! shards; reassemble and FEC-recover them on the far side.
//!
//! Each packet is a fixed [`PacketHeader`] plus one FEC shard. Fields are
//! host-endian (every current target is little-endian).
//!
//! GameStream mapping is explicit fields, not bit-packs: `frame_index`↔
//! `frameIndex`, `stream_seq`↔`streamPacketIndex`, (`block_index`,
//! `block_count`)↔`multiFecBlocks` nibbles, (`data_shards`, `recovery_shards`,
//! `shard_index`)↔`fecInfo`. RTP/RTSP wire-exactness lives in the GameStream
//! host. Tests in this module pin layout and round-trip.

mod header;
mod packetize;
mod reassemble;
/// cbindgen:ignore
mod v2;

pub use header::*;
pub use packetize::*;
pub use reassemble::*;
pub use v2::*;

#[cfg(test)]
mod tests;
