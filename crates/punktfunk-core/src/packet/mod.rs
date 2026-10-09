//! Wire framing: split an access unit into FEC blocks of MTU-sized shards;
//! reassemble and FEC-recover them on the far side.
//!
//! Each packet is a [`PacketHeader`] plus one FEC shard; [`encode_v2`] writes the
//! header on the wire and [`decode_v2`] reads it back.
//!
//! GameStream mapping is explicit fields, not bit-packs: `frame_index`↔
//! `frameIndex`, (`block_index`, `block_count`)↔`multiFecBlocks` nibbles,
//! (`data_shards`, `recovery_shards`, `shard_index`)↔`fecInfo`. RTP/RTSP
//! wire-exactness lives in the GameStream host. Tests in this module pin layout
//! and round-trip.

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
