//! The `punktfunk/2` media packet header: 30 bytes in place of `punktfunk/1`'s 40 plus its
//! 8-byte sequence prefix, with a stream id and a config epoch.
//!
//! `tag u8 ‖ seq u32` stay in the clear and are the AEAD's associated data; `epoch u8 ‖
//! flags u16 ‖ frame u32 ‖ pts_us u32 ‖ block u16 ‖ base u32 ‖ shard u16 ‖ k u16 ‖ m u16 ‖
//! pad u16` are sealed with the shard. All little-endian. The tag is `00rr ssss`: the two top
//! bits are zero so the byte never reads as QUIC (RFC 9443), `rr` is reserved, `ssss` the stream.
//!
//! The shard is the rest of the datagram, so every packet of a frame has one length and no
//! field repeats it. No packet states the frame's size or block count: `base` places each
//! block, and the block flagged [`V2_LAST_BLOCK`] closes the frame at `base + k` shards less
//! `pad` bytes. The receiver reads every packet as `punktfunk/1`'s slice-stream form, so the
//! firewall, pinning and tiling checks are the reviewed ones.

use super::*;

/// Bytes before the sealed part: the tag and the low half of the packet number.
pub const V2_CLEAR_LEN: usize = 5;
pub const V2_HEADER_LEN: usize = 30;
/// Stream id of video frames.
pub const V2_STREAM_VIDEO: u8 = 0;
/// Stream id of speed-test filler, in its own frame-index space.
pub const V2_STREAM_PROBE: u8 = 1;
/// `flags` bit 15: this block ends the frame and `pad` counts.
pub const V2_LAST_BLOCK: u16 = 0x8000;

/// What a sender stamps beyond the logical header.
#[derive(Clone, Copy, Debug)]
pub struct V2Stamp {
    /// The full packet number. The low half goes on the wire; the nonce takes all of it.
    pub seq: u64,
    pub epoch: u8,
    /// The host instant `pts_us = 0` stands for, Unix ns.
    pub clock_origin_ns: u64,
    /// The session's full block size, which places a uniform frame's blocks.
    pub max_data_per_block: u16,
}

/// The v2 header for one packet the packetizer produced. Every packetizer shape maps: a
/// uniform frame places block `b` at `b × K`, a sentinel carries its base, a final block
/// closes the frame.
pub fn encode_v2(hdr: &PacketHeader, s: &V2Stamp) -> [u8; V2_HEADER_LEN] {
    let shard = u64::from(hdr.shard_bytes.max(1));
    let frame_bytes = u64::from(hdr.frame_bytes);
    let total_data = frame_bytes.div_ceil(shard).max(1);
    let uniform_base = u64::from(hdr.block_index) * u64::from(s.max_data_per_block);
    let slice = hdr.user_flags & USER_FLAG_SLICE_STREAM != 0;
    let (base, last) = match (hdr.block_count, slice) {
        (0, true) => (frame_bytes / shard, false),
        (0, false) => (uniform_base, false),
        (_, true) => (total_data - u64::from(hdr.data_shards), true),
        (count, false) => (uniform_base, hdr.block_index + 1 == count),
    };
    let pad = if last {
        total_data * shard - frame_bytes
    } else {
        0
    };
    let stream = if hdr.user_flags & u32::from(FLAG_PROBE) != 0 {
        V2_STREAM_PROBE
    } else {
        V2_STREAM_VIDEO
    };
    let flags = (hdr.user_flags & 0x7FFF) as u16 | if last { V2_LAST_BLOCK } else { 0 };
    let pts_us = (hdr.pts_ns as i64 - s.clock_origin_ns as i64).div_euclid(1000) as u32;
    let mut b = [0u8; V2_HEADER_LEN];
    b[0] = stream;
    b[1..5].copy_from_slice(&(s.seq as u32).to_le_bytes());
    b[5] = s.epoch;
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    b[8..12].copy_from_slice(&hdr.frame_index.to_le_bytes());
    b[12..16].copy_from_slice(&pts_us.to_le_bytes());
    b[16..18].copy_from_slice(&hdr.block_index.to_le_bytes());
    b[18..22].copy_from_slice(&(base as u32).to_le_bytes());
    b[22..24].copy_from_slice(&hdr.shard_index.to_le_bytes());
    b[24..26].copy_from_slice(&hdr.data_shards.to_le_bytes());
    b[26..28].copy_from_slice(&hdr.recovery_shards.to_le_bytes());
    b[28..30].copy_from_slice(&(pad as u16).to_le_bytes());
    b
}

/// One received v2 packet: its header in `punktfunk/1`'s slice-stream form, its epoch and its
/// shard. `None` on a tag, stream or size this build does not accept. `pts_ref_us` is the
/// newest capture time seen, which unwraps the 32-bit field.
pub fn decode_v2<'a>(
    pkt: &'a [u8],
    clock_origin_ns: u64,
    pts_ref_us: &mut i64,
) -> Option<(PacketHeader, u8, &'a [u8])> {
    let h = pkt.get(..V2_HEADER_LEN)?;
    let (tag, body) = (h[0], &pkt[V2_HEADER_LEN..]);
    if tag & 0xF0 != 0 || tag > V2_STREAM_PROBE {
        return None;
    }
    let u16_at = |o: usize| u16::from_le_bytes([h[o], h[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
    let shard_bytes = u16::try_from(body.len()).ok()?;
    let (flags, block, base, k) = (u16_at(6), u16_at(16), u32_at(18), u16_at(24));
    let sb = u64::from(shard_bytes);
    let (block_count, frame_bytes) = if flags & V2_LAST_BLOCK != 0 {
        let total = u64::from(base) + u64::from(k);
        let pad = u64::from(u16_at(28));
        // A whole shard of padding is only the empty frame's one zeroed shard.
        if pad > sb || (pad == sb && total != 1) {
            return None;
        }
        let frame_bytes = (total * sb).checked_sub(pad)?;
        (block.checked_add(1)?, u32::try_from(frame_bytes).ok()?)
    } else {
        (0, u32::try_from(u64::from(base) * sb).ok()?)
    };
    let mut user_flags =
        u32::from(flags & 0x7FFF) & !u32::from(FLAG_PROBE) | USER_FLAG_SLICE_STREAM;
    if tag == V2_STREAM_PROBE {
        user_flags |= u32::from(FLAG_PROBE);
    }
    let pts_us = *pts_ref_us + i64::from(u32_at(12).wrapping_sub(*pts_ref_us as u32) as i32);
    *pts_ref_us = (*pts_ref_us).max(pts_us);
    let pts_ns = (clock_origin_ns as i64)
        .saturating_add(pts_us.saturating_mul(1000))
        .max(0) as u64;
    let hdr = PacketHeader {
        pts_ns,
        frame_index: u32_at(8),
        stream_seq: u32_at(1),
        frame_bytes,
        user_flags,
        block_index: block,
        block_count,
        data_shards: k,
        recovery_shards: u16_at(26),
        shard_index: u16_at(22),
        shard_bytes,
        magic: PUNKTFUNK_MAGIC,
        version: 2,
        fec_scheme: crate::config::FecScheme::Gf16 as u8,
        flags: FLAG_PIC,
    };
    Some((hdr, h[5], body))
}

/// The packet number nearest `reference` whose low half is `low`.
pub fn expand_seq(reference: u64, low: u32) -> u64 {
    let delta = i64::from(low.wrapping_sub(reference as u32) as i32);
    reference
        .checked_add_signed(delta)
        .unwrap_or(u64::from(low))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FecConfig, FecScheme, Role};
    use crate::fec::coder_for;
    use proptest::prelude::*;
    use zerocopy::FromBytes;

    fn stamp(seq: u64) -> V2Stamp {
        V2Stamp {
            seq,
            epoch: 3,
            clock_origin_ns: 1_700_000_000_000_000_000,
            max_data_per_block: 8,
        }
    }

    fn config(max_data: u16) -> Config {
        let mut c = Config::p1_defaults(Role::Host);
        c.fec = FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 25,
            max_data_per_block: max_data,
        };
        c.shard_payload = 64;
        c
    }

    /// Every packet the packetizer makes survives encode → decode with the geometry the
    /// receiver needs: one block, many uniform blocks, and a streamed frame.
    #[test]
    fn packetizer_output_round_trips_into_slice_form() {
        let coder = coder_for(FecScheme::Gf16);
        let mut p = Packetizer::new(&config(8));
        let origin = stamp(0).clock_origin_ns;
        for len in [0usize, 1, 63, 64, 500, 64 * 8, 64 * 8 + 1, 64 * 20 + 7] {
            let frame: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let pts = origin + 42_000_000;
            let mut out = Vec::new();
            p.packetize_each(&frame, pts, 0x20, Some(9), coder.as_ref(), |h, body| {
                out.push((*h, body.to_vec()));
                Ok(())
            })
            .unwrap();
            let mut rebuilt = vec![0u8; len.div_ceil(64).max(1) * 64];
            let mut closed = None;
            let mut pts_ref = 0;
            for (i, (h, body)) in out.iter().enumerate() {
                let mut pkt = encode_v2(h, &stamp(i as u64)).to_vec();
                pkt.extend_from_slice(body);
                let (d, epoch, shard) = decode_v2(&pkt, origin, &mut pts_ref).unwrap();
                assert_eq!((epoch, d.frame_index, d.pts_ns), (3, 9, pts));
                assert_eq!(d.user_flags, 0x20 | USER_FLAG_SLICE_STREAM);
                if d.shard_index < d.data_shards && d.block_count != 0 {
                    closed = Some(d.frame_bytes);
                }
                if d.shard_index < d.data_shards {
                    let base = if d.block_count == 0 {
                        d.frame_bytes as usize / 64
                    } else {
                        (d.frame_bytes as usize).div_ceil(64).max(1) - d.data_shards as usize
                    };
                    let at = (base + d.shard_index as usize) * 64;
                    rebuilt[at..at + 64].copy_from_slice(shard);
                }
            }
            assert_eq!(closed, Some(len as u32), "len {len}");
            assert_eq!(&rebuilt[..len], &frame[..], "len {len}");
        }
    }

    #[test]
    fn probe_rides_its_own_stream_and_bad_tags_are_refused() {
        let mut h = PacketHeader::read_from_bytes(&[0u8; HEADER_LEN]).unwrap();
        h.shard_bytes = 16;
        h.data_shards = 1;
        h.block_count = 1;
        h.user_flags = u32::from(FLAG_PROBE);
        let mut pkt = encode_v2(&h, &stamp(0)).to_vec();
        pkt.extend_from_slice(&[0; 16]);
        assert_eq!(pkt[0], V2_STREAM_PROBE);
        let mut r = 0;
        assert!(decode_v2(&pkt, 0, &mut r).unwrap().0.user_flags & u32::from(FLAG_PROBE) != 0);
        for tag in [0x02u8, 0x10, 0x40, 0xC0] {
            pkt[0] = tag;
            assert!(decode_v2(&pkt, 0, &mut r).is_none(), "tag {tag:#x}");
        }
    }

    #[test]
    fn padding_past_one_shard_is_refused() {
        let mut h = PacketHeader::read_from_bytes(&[0u8; HEADER_LEN]).unwrap();
        h.shard_bytes = 16;
        h.data_shards = 2;
        h.block_count = 1;
        h.frame_bytes = 20;
        let mut pkt = encode_v2(&h, &stamp(0)).to_vec();
        pkt.extend_from_slice(&[0; 16]);
        let mut r = 0;
        assert_eq!(decode_v2(&pkt, 0, &mut r).unwrap().0.frame_bytes, 20);
        pkt[28..30].copy_from_slice(&16u16.to_le_bytes());
        assert!(decode_v2(&pkt, 0, &mut r).is_none());
        pkt[28..30].copy_from_slice(&17u16.to_le_bytes());
        assert!(decode_v2(&pkt, 0, &mut r).is_none());
        // A last block of no data shards cannot pay for any padding.
        pkt[24..26].copy_from_slice(&0u16.to_le_bytes());
        pkt[28..30].copy_from_slice(&8u16.to_le_bytes());
        assert!(decode_v2(&pkt, 0, &mut r).is_none());
    }

    #[test]
    fn capture_time_unwraps_across_the_32_bit_edge() {
        let mut r = i64::from(u32::MAX) - 10;
        let mut h = PacketHeader::read_from_bytes(&[0u8; HEADER_LEN]).unwrap();
        h.shard_bytes = 16;
        h.data_shards = 1;
        h.block_count = 1;
        h.pts_ns = (i64::from(u32::MAX) + 5) as u64 * 1000;
        let origin0 = V2Stamp {
            clock_origin_ns: 0,
            ..stamp(0)
        };
        let mut pkt = encode_v2(&h, &origin0).to_vec();
        pkt.extend_from_slice(&[0; 16]);
        assert_eq!(decode_v2(&pkt, 0, &mut r).unwrap().0.pts_ns, h.pts_ns);
        assert_eq!(r, i64::from(u32::MAX) + 5);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        /// Hostile v2 packets never panic the reassembler, never complete a frame they did not
        /// tile, and never push in-flight memory past the budget.
        #[test]
        fn hostile_v2_packets_stay_inside_the_budget(
            pkts in proptest::collection::vec(
                (
                    (0u8..2, 0u32..3, any::<u16>(), 0u16..4, 0u32..300),
                    (0u16..14, 0u16..10, 0u16..6, 0u16..70),
                    proptest::sample::select(vec![0usize, 16, 32, 34, 64, 66]),
                ),
                1..60,
            ),
        ) {
            let lim = ReassemblerLimits {
                min_shard_bytes: 16,
                max_shard_bytes: 64,
                max_data_shards: 8,
                max_total_shards: 12,
                max_frame_bytes: 4096,
            };
            let mut r = Reassembler::new(lim);
            r.set_v2(0);
            let coder = coder_for(FecScheme::Gf16);
            let stats = crate::stats::StatsCounters::default();
            for ((tag, frame, flags, block, base), (shard, k, m, pad), body) in pkts {
                let mut pkt = vec![0u8; V2_HEADER_LEN];
                pkt[0] = tag;
                pkt[6..8].copy_from_slice(&flags.to_le_bytes());
                pkt[8..12].copy_from_slice(&frame.to_le_bytes());
                pkt[16..18].copy_from_slice(&block.to_le_bytes());
                pkt[18..22].copy_from_slice(&base.to_le_bytes());
                pkt[22..24].copy_from_slice(&shard.to_le_bytes());
                pkt[24..26].copy_from_slice(&k.to_le_bytes());
                pkt[26..28].copy_from_slice(&m.to_le_bytes());
                pkt[28..30].copy_from_slice(&pad.to_le_bytes());
                pkt.extend(std::iter::repeat_n(0xA5, body));
                if let Ok(Some(f)) = r.push(&pkt, coder.as_ref(), &stats) {
                    prop_assert!(f.data.len() <= lim.max_frame_bytes);
                }
                prop_assert!(r.in_flight() <= IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes);
            }
        }
    }

    #[test]
    fn packet_numbers_expand_around_the_reference() {
        assert_eq!(expand_seq(0, 5), 5);
        assert_eq!(expand_seq(0xFFFF_FFF0, 3), 0x1_0000_0003);
        assert_eq!(expand_seq(0x1_0000_0003, 0xFFFF_FFF0), 0xFFFF_FFF0);
        assert_eq!(expand_seq(3, 0xFFFF_FFF0), 0xFFFF_FFF0);
    }
}
