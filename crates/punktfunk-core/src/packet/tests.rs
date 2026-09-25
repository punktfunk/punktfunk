use super::reassemble::LOSS_WINDOW_NS;
use super::*;
use crate::config::{Config, FecScheme};
use crate::crypto::SessionKey;
use crate::fec::coder_for;
use crate::stats::StatsCounters;
use zerocopy::{FromBytes, IntoBytes};

fn limits() -> ReassemblerLimits {
    // min == max pins every shard at 16 B. 4096/16 = 256 shards → 32 blocks.
    ReassemblerLimits {
        min_shard_bytes: 16,
        max_shard_bytes: 16,
        max_data_shards: 8,
        max_total_shards: 12,
        max_frame_bytes: 4096,
    }
}

fn base_header() -> PacketHeader {
    PacketHeader {
        pts_ns: 0,
        frame_index: 0,
        stream_seq: 0,
        frame_bytes: 16,
        user_flags: 0,
        block_index: 0,
        block_count: 1,
        data_shards: 1,
        recovery_shards: 0,
        shard_index: 0,
        shard_bytes: 16,
        magic: PUNKTFUNK_MAGIC,
        version: 1,
        fec_scheme: 0,
        flags: FLAG_PIC,
    }
}

fn packet(h: PacketHeader) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(h.as_bytes());
    p.extend_from_slice(&vec![0xAB; h.shard_bytes as usize]);
    p
}

/// 65535+65535 shards must drop, not allocate.
#[test]
fn rejects_oversized_shard_counts() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.data_shards = 65535;
    h.recovery_shards = 65535;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

/// A later packet with different geometry must drop, not index past the first packet's shard vec.
#[test]
fn rejects_inconsistent_block_geometry_without_panicking() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    let mut h1 = base_header();
    h1.data_shards = 4;
    h1.recovery_shards = 2;
    h1.frame_bytes = 64;
    assert!(r
        .push(&packet(h1), coder.as_ref(), &stats)
        .unwrap()
        .is_none());

    // shard_index 7 is in-range for this header's 8 slots, past the pinned 6.
    let mut h2 = base_header();
    h2.data_shards = 6;
    h2.recovery_shards = 2;
    h2.shard_index = 7;
    h2.frame_bytes = 64;
    assert!(r
        .push(&packet(h2), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

/// Loss window is capture-pts, not frame count: 33 ms late at 120 fps is late, not lost.
#[test]
fn incomplete_frames_age_out_by_capture_time_not_frame_count() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    const FRAME_NS: u64 = 8_333_333; // 120 fps

    let mut h = base_header();
    h.data_shards = 2;
    h.frame_bytes = 32;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());

    // 8 frames at 120 fps ≈ 67 ms, inside LOSS_WINDOW_NS; frame 0 must still be live.
    for i in 1..=8u32 {
        let mut h = base_header();
        h.frame_index = i;
        h.pts_ns = i as u64 * FRAME_NS;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_some());
    }
    assert_eq!(stats.snapshot().frames_dropped, 0);

    // ~66 ms late at 120 fps — still inside the window.
    let mut h = base_header();
    h.data_shards = 2;
    h.frame_bytes = 32;
    h.shard_index = 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_some());

    let mut h = base_header();
    h.frame_index = 20;
    h.pts_ns = 20 * FRAME_NS;
    h.data_shards = 2;
    h.frame_bytes = 32;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    let mut h = base_header();
    h.frame_index = 21;
    h.pts_ns = 20 * FRAME_NS + LOSS_WINDOW_NS + 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(stats.snapshot().frames_dropped, 1);

    let mut h = base_header();
    h.frame_index = 20;
    h.pts_ns = 20 * FRAME_NS;
    h.data_shards = 2;
    h.frame_bytes = 32;
    h.shard_index = 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().frames_dropped, 1, "no double-count");
}

/// Explicit `frame_index` must not bump the packetizer's internal video or probe counters.
#[test]
fn explicit_frame_index_is_stamped_and_internal_counter_untouched() {
    use crate::config::{FecConfig, FecScheme, ProtocolPhase, Role};
    let cfg = Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 0,
            max_data_per_block: 8,
        },
        shard_payload: 16,
        max_frame_bytes: 4096,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    };
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut seen = Vec::new();
    pk.packetize_each(&[1u8; 16], 0, 0, Some(4242), coder.as_ref(), |hdr, _| {
        seen.push(hdr.frame_index);
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, vec![4242]);
    let pkts = pk.packetize(&[1u8; 16], 0, 0, coder.as_ref()).unwrap();
    let hdr = PacketHeader::read_from_bytes(&pkts[0][..HEADER_LEN]).unwrap();
    assert_eq!(hdr.frame_index, 0);
    // Probe indexes are a third counter, not the video one.
    assert_eq!(pk.alloc_probe_index(), 0);
    assert_eq!(pk.alloc_probe_index(), 1);
}

/// FLAG_PROBE frames reassemble in their own window; they must not age out against video indexes.
#[test]
fn probe_frames_reassemble_in_their_own_window() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    let mut v = base_header();
    v.frame_index = 100_000;
    v.pts_ns = 1_000_000_000;
    assert!(r
        .push(&packet(v), coder.as_ref(), &stats)
        .unwrap()
        .is_some());

    // Probe index 0 is 100k behind the video window and must still complete.
    let mut p = base_header();
    p.frame_index = 0;
    p.pts_ns = 1_000_000_100;
    p.user_flags = FLAG_PROBE as u32;
    let got = r.push(&packet(p), coder.as_ref(), &stats).unwrap();
    assert!(got.is_some(), "probe frame must complete in its own window");
    assert_eq!(got.unwrap().flags & FLAG_PROBE as u32, FLAG_PROBE as u32);

    // Next video index must still be contiguous; probe must not have aged video.
    let mut v2 = base_header();
    v2.frame_index = 100_001;
    v2.pts_ns = 1_000_000_200;
    assert!(r
        .push(&packet(v2), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(stats.snapshot().frames_dropped, 0);
}

/// Probe-window age-out must not increment video `frames_dropped` (that fires client recovery).
#[test]
fn aged_out_probe_frames_do_not_count_as_dropped() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    let mut p = base_header();
    p.user_flags = FLAG_PROBE as u32;
    p.data_shards = 2;
    p.frame_bytes = 32;
    assert!(r
        .push(&packet(p), coder.as_ref(), &stats)
        .unwrap()
        .is_none());

    let mut p2 = base_header();
    p2.user_flags = FLAG_PROBE as u32;
    p2.frame_index = 1;
    p2.pts_ns = LOSS_WINDOW_NS + 1;
    assert!(r
        .push(&packet(p2), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(
        stats.snapshot().frames_dropped,
        0,
        "probe-window drops must not fire video loss recovery"
    );
}

fn e2e_config(scheme: FecScheme, fec_percent: u8) -> Config {
    use crate::config::{FecConfig, ProtocolPhase, Role};
    Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme,
            fec_percent,
            max_data_per_block: 4,
        },
        shard_payload: 16,
        max_frame_bytes: 4096,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    }
}

/// Reverse reconstructs early; `fec_recovered_shards - fec_late_shards` must still equal
/// the kill count or reorder reads as loss.
fn e2e_roundtrip(
    scheme: FecScheme,
    frame_len: usize,
    fec_percent: u8,
    kill: &[usize],
    reverse: bool,
) {
    let cfg = e2e_config(scheme, fec_percent);
    let coder = coder_for(scheme);
    let mut pk = Packetizer::new(&cfg);
    let src: Vec<u8> = (0..frame_len).map(|i| (i * 131 + 7) as u8).collect();
    let pkts = pk.packetize(&src, 12345, 0, coder.as_ref()).unwrap();

    let mut delivery: Vec<Vec<u8>> = pkts
        .iter()
        .enumerate()
        .filter(|(i, _)| !kill.contains(i))
        .map(|(_, p)| p.clone())
        .collect();
    if reverse {
        delivery.reverse(); // data-first wire: reverse puts parity first
    }
    if let Some(dup) = delivery.first().cloned() {
        delivery.push(dup);
    }

    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let mut got = None;
    for p in &delivery {
        if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
            assert!(got.is_none(), "frame must complete exactly once");
            got = Some(f);
        }
    }
    let f = got.expect("frame must complete within the FEC budget");
    assert_eq!(f.data, src, "reassembled AU must be byte-identical");
    assert_eq!(f.pts_ns, 12345);
    let snap = stats.snapshot();
    let (recovered, late) = (snap.fec_recovered_shards, snap.fec_late_shards);
    if reverse {
        assert!(
            recovered >= kill.len() as u64,
            "early reconstruct counts more"
        );
    } else {
        assert_eq!(recovered, kill.len() as u64);
    }
    assert_eq!(
        recovered - late,
        kill.len() as u64,
        "net recovered (recovered - late) must equal the true loss regardless of order \
             (recovered={recovered} late={late} killed={})",
        kill.len()
    );
}

/// 100 B / 16 B = 7 data shards → blocks (4+2) and (3+2).
#[test]
fn e2e_multiblock_loss_reorder_dup_gf16() {
    // Data-first: blk0 data 0..4, blk1 data 4..7, blk0 rec 7..9, blk1 rec 9..11.
    // Kill 0,2,5: two data in block 0 and one in block 1, all within 50% FEC.
    e2e_roundtrip(FecScheme::Gf16, 100, 50, &[0, 2, 5], false);
    e2e_roundtrip(FecScheme::Gf16, 100, 50, &[0, 2, 5], true);
}

#[test]
fn e2e_multiblock_loss_reorder_dup_gf8() {
    e2e_roundtrip(FecScheme::Gf8, 100, 50, &[1, 3, 6], false);
    e2e_roundtrip(FecScheme::Gf8, 100, 50, &[1, 3, 6], true);
}

/// All data shards then all parity; SOF on the first packet, EOF on the last.
#[test]
fn packetize_emits_all_data_before_any_parity() {
    use zerocopy::FromBytes;
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    // 100 B / 16 → 7 data shards → blocks (4 data + 2 rec) + (3 data + 2 rec).
    let src: Vec<u8> = (0..100).map(|i| (i * 31 + 3) as u8).collect();
    let pkts = pk.packetize(&src, 1, 0, coder.as_ref()).unwrap();
    assert_eq!(pkts.len(), 11);
    let hdrs: Vec<PacketHeader> = pkts
        .iter()
        .map(|p| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap())
        .collect();
    let layout: Vec<(u16, u16)> = hdrs
        .iter()
        .map(|h| (h.block_index, h.shard_index))
        .collect();
    assert_eq!(
        layout,
        vec![
            (0, 0),
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 0),
            (1, 1),
            (1, 2),
            (0, 4),
            (0, 5),
            (1, 3),
            (1, 4),
        ],
        "data-first wire order"
    );
    let first_parity = hdrs
        .iter()
        .position(|h| h.shard_index >= h.data_shards)
        .unwrap();
    assert!(
        hdrs[first_parity..]
            .iter()
            .all(|h| h.shard_index >= h.data_shards),
        "no data shard after the first parity shard"
    );
    // Stream seqs stay sequential in emission order (the nonce contract).
    for (i, w) in hdrs.windows(2).enumerate() {
        assert_eq!(w[1].stream_seq, w[0].stream_seq + 1, "seq gap at {i}");
    }
    assert_eq!(hdrs[0].flags & FLAG_SOF, FLAG_SOF, "SOF on first packet");
    assert_eq!(
        hdrs.last().unwrap().flags & FLAG_EOF,
        FLAG_EOF,
        "EOF on last (parity) packet"
    );
    assert_eq!(
        hdrs.iter().filter(|h| h.flags & FLAG_EOF != 0).count(),
        1,
        "exactly one EOF"
    );

    // No FEC: EOF falls on the last data shard, not a parity shard.
    let cfg0 = e2e_config(FecScheme::Gf16, 0);
    let mut pk0 = Packetizer::new(&cfg0);
    let pkts0 = pk0.packetize(&src, 2, 0, coder.as_ref()).unwrap();
    assert_eq!(pkts0.len(), 7, "no parity at 0% FEC");
    let last = PacketHeader::read_from_bytes(&pkts0.last().unwrap()[..HEADER_LEN]).unwrap();
    assert_eq!(last.flags & FLAG_EOF, FLAG_EOF, "EOF on last data shard");
    assert!(last.shard_index < last.data_shards, "last packet is data");
}

#[test]
fn e2e_clean_delivery_gf16() {
    e2e_roundtrip(FecScheme::Gf16, 100, 50, &[], false);
}

/// Empty AU is one zero-padded shard; reassemble to zero bytes.
#[test]
fn e2e_empty_frame() {
    let cfg = e2e_config(FecScheme::Gf16, 0);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let pkts = pk.packetize(&[], 7, 0, coder.as_ref()).unwrap();
    assert_eq!(pkts.len(), 1);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let f = r
        .push(&pkts[0], coder.as_ref(), &stats)
        .unwrap()
        .expect("empty frame completes");
    assert!(f.data.is_empty());
}

/// Loss past FEC ages out as dropped; unrecoverable-block must not fire (never gathered k).
#[test]
fn e2e_unrecoverable_loss_ages_out() {
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let src = vec![0x5Au8; 64]; // 64/16 = 4 data + 50% = 2 recovery
    let pkts = pk.packetize(&src, 1_000, 0, coder.as_ref()).unwrap();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    // 3 of 6 shards, k=4: cannot reconstruct.
    for p in &pkts[..3] {
        assert!(r.push(p, coder.as_ref(), &stats).unwrap().is_none());
    }
    let next = pk
        .packetize(&src, 1_000 + LOSS_WINDOW_NS + 1, 0, coder.as_ref())
        .unwrap();
    let mut done = false;
    for p in &next {
        done |= r.push(p, coder.as_ref(), &stats).unwrap().is_some();
    }
    assert!(done);
    assert_eq!(stats.snapshot().frames_dropped, 1);
}

/// The count the RFI line carries: what a short frame lacks past its parity.
#[test]
fn missing_beyond_parity_counts_what_parity_cannot_rebuild() {
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let stats = StatsCounters::default();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    // Frame 0: 64 B = 4 data + 2 parity. Three data lost, both parity in: one short.
    let one = pk.packetize(&[1u8; 64], 1_000, 0, coder.as_ref()).unwrap();
    for i in [0, 4, 5] {
        r.push(&one[i], coder.as_ref(), &stats).unwrap();
    }
    assert_eq!(r.missing_beyond_parity(0), Some((1, 2)));
    // Frame 1: 100 B = blocks (4+2) and (3+2). Block 1 never arrives: its 3 data count.
    let two = pk.packetize(&[2u8; 100], 2_000, 0, coder.as_ref()).unwrap();
    for i in [0, 1, 2, 3, 7, 8] {
        r.push(&two[i], coder.as_ref(), &stats).unwrap();
    }
    assert_eq!(r.missing_beyond_parity(1), Some((3, 2)));
    assert_eq!(r.missing_beyond_parity(2), None, "no shard of it arrived");
}

/// In-flight budget is [`IN_FLIGHT_BUF_FACTOR`] × max_frame_bytes, not one max-size buffer per first shard.
#[test]
fn in_flight_buffer_budget_bounds_allocation() {
    // limits(): max_frame_bytes 4096 → budget 4 × 4096 = 16384 B.
    let lim = limits();
    let budget = IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes;
    // One frame: 4×8×16 B buffer + the opened block's state; both are metered.
    let per_frame = 512 + block_state_bytes(8, 0);
    let fits = budget / per_frame;
    let mut r = Reassembler::new(lim);
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    for i in 0..=fits as u32 {
        let mut h = base_header();
        h.frame_index = i;
        h.frame_bytes = 512;
        h.block_count = 4;
        h.data_shards = 8;
        r.push(&packet(h), coder.as_ref(), &stats).unwrap();
    }
    assert_eq!(
        stats.snapshot().packets_dropped,
        1,
        "the frame past the budget is dropped, everything under it accepted"
    );
    assert!(
        r.in_flight() <= budget,
        "in-flight commitment {} must never exceed the {budget} B budget",
        r.in_flight(),
    );
}

/// Recovery-shard payloads count toward in-flight and are credited when the frame ages out.
#[test]
fn recovery_shard_payloads_are_metered_and_released() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();

    // 4+4 block, 3 parity only (k=4): cannot reconstruct; each 16 B payload stays charged.
    let mut h = base_header();
    h.data_shards = 4;
    h.recovery_shards = 4;
    h.frame_bytes = 64;
    for j in 4..7u16 {
        let mut h = h;
        h.shard_index = j;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert_eq!(
        r.in_flight(),
        64 + block_state_bytes(4, 4) + 3 * 16,
        "held parity payloads must be part of the in-flight commitment"
    );

    // Age-out must credit the parity bytes or the session budget drifts.
    let mut h = base_header();
    h.frame_index = 1;
    h.pts_ns = LOSS_WINDOW_NS + 1;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_some());
    assert_eq!(
        r.in_flight(),
        0,
        "released frames must return every charged byte"
    );
}

/// `(data_shards, block_count)` must match geometry derived from `frame_bytes`, or drop.
#[test]
fn rejects_geometry_inconsistent_with_frame_bytes() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.frame_bytes = 16; // one shard
    h.data_shards = 2; // claims two
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

#[test]
fn rejects_wrong_shard_bytes_and_oversized_frame() {
    let coder = coder_for(FecScheme::Gf8);

    let mut r = Reassembler::new(limits());
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.shard_bytes = 8; // != negotiated 16
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);

    let mut r = Reassembler::new(limits());
    let stats = StatsCounters::default();
    let mut h = base_header();
    h.frame_bytes = 1_000_000; // > max_frame_bytes
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 1);
}

/// Receiver `max_total_shards` is frozen at session start. `Packetizer::recovery_for` clamps
/// parity to that ceiling so a mid-session `fec_percent` ramp cannot emit undeliverable blocks.
#[test]
fn adaptive_fec_ramp_keeps_maximal_blocks_within_the_peers_ceiling() {
    let cfg = e2e_config(FecScheme::Gf16, 10);
    let coder = coder_for(FecScheme::Gf16);
    let lim = ReassemblerLimits::from_config(&cfg);
    let mut pk = Packetizer::new(&cfg);

    // Mid-session ramp past the negotiated 10%, as `apply_fec_target` does under loss.
    pk.set_fec_percent(90);

    let frame_len = cfg.shard_payload * cfg.fec.max_data_per_block as usize * 2;
    let src: Vec<u8> = (0..frame_len).map(|i| (i * 131 + 7) as u8).collect();
    let pkts = pk.packetize(&src, 1, 0, coder.as_ref()).unwrap();

    let k = cfg.fec.max_data_per_block as usize;
    let mut clamped = false;
    for p in &pkts {
        let hdr = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        let total = hdr.data_shards as usize + hdr.recovery_shards as usize;
        assert!(
            total <= lim.max_total_shards,
            "block total {total} exceeds the peer's ceiling {} — every packet of this block \
             would be dropped",
            lim.max_total_shards
        );
        // Unclamped 90% would put 4 parity on a full block; the 10% ceiling leaves 2.
        if hdr.data_shards as usize == k {
            assert!(
                (hdr.recovery_shards as usize) < cfg.fec.recovery_for(k).max(1) + 1,
                "parity must be clamped to the peer's ceiling"
            );
            clamped = true;
        }
    }
    assert!(clamped, "test must exercise a maximal block");

    let mut r = Reassembler::new(lim);
    let stats = StatsCounters::default();
    let mut got = None;
    for p in &pkts {
        if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
            got = Some(f);
        }
    }
    assert_eq!(
        got.expect("frame must complete after an adaptive-FEC ramp")
            .data,
        src
    );
}

// ---------------------------------------------------------------------------
// Streamed access units (VIDEO_CAP_STREAMED_AU)
// ---------------------------------------------------------------------------

fn streamed_packets(
    scheme: FecScheme,
    fec_percent: u8,
    chunks: &[&[u8]],
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let cfg = e2e_config(scheme, fec_percent);
    let coder = coder_for(scheme);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(12345, 0, Some(0));
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    let mut src = Vec::new();
    for c in chunks {
        src.extend_from_slice(c);
        // slice_end=true with USER_FLAG_SLICE_STREAM unset must be inert.
        pk.push_streamed(
            &mut au,
            c,
            true,
            coder.as_ref(),
            |h: &PacketHeader, b: &[u8]| {
                let mut p = Vec::with_capacity(HEADER_LEN + b.len());
                p.extend_from_slice(h.as_bytes());
                p.extend_from_slice(b);
                pkts.push(p);
                Ok(())
            },
        )
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h: &PacketHeader, b: &[u8]| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

/// Reverse delivery: final-block real totals arrive first; sentinels must still match the pin.
fn streamed_roundtrip(scheme: FecScheme, kill: &[usize], reverse: bool) {
    let chunks: Vec<Vec<u8>> = (0..3)
        .map(|c| (0..50).map(|i| (c * 57 + i * 131 + 7) as u8).collect())
        .collect();
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (pkts, src) = streamed_packets(scheme, 50, &chunk_refs);
    // 150 B / 16 B / 4-shard blocks → sentinels 0,1 (4+2 each) + final (2+2, the
    // `MIN_RECOVERY_SHARDS` floor) = 16 packets.
    assert_eq!(
        pkts.len(),
        16,
        "expected geometry changed — update the kills"
    );

    let mut delivery: Vec<Vec<u8>> = pkts
        .iter()
        .enumerate()
        .filter(|(i, _)| !kill.contains(i))
        .map(|(_, p)| p.clone())
        .collect();
    if reverse {
        delivery.reverse();
    }
    if let Some(dup) = delivery.first().cloned() {
        delivery.push(dup);
    }

    let cfg = e2e_config(scheme, 50);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(scheme);
    let stats = StatsCounters::default();
    let mut got = None;
    for p in &delivery {
        if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
            assert!(got.is_none(), "frame must complete exactly once");
            got = Some(f);
        }
    }
    let f = got.expect("streamed frame must complete within the FEC budget");
    assert_eq!(
        f.data, src,
        "reassembled streamed AU must be byte-identical"
    );
    assert_eq!(f.pts_ns, 12345);
    assert!(f.complete);
}

#[test]
fn streamed_roundtrip_clean_and_reversed() {
    streamed_roundtrip(FecScheme::Gf16, &[], false);
    streamed_roundtrip(FecScheme::Gf16, &[], true);
    streamed_roundtrip(FecScheme::Gf8, &[], true);
}

#[test]
fn streamed_roundtrip_survives_loss_and_reorder() {
    // Wire order: blk0 = 0..4 data + 4..6 rec, blk1 = 6..10 data + 10..12 rec,
    // final = 12..14 data + 14..16 rec. Both final data shards die: the floor covers it.
    streamed_roundtrip(FecScheme::Gf16, &[1, 12, 13], false);
    streamed_roundtrip(FecScheme::Gf16, &[1, 12, 13], true);
}

/// Sentinel: `block_count=0`, `frame_bytes=0`, full-K. Real totals + EOF on the final block.
#[test]
fn streamed_headers_sentinel_then_final() {
    let chunks: Vec<Vec<u8>> = (0..3).map(|_| vec![0xA5u8; 50]).collect();
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (pkts, src) = streamed_packets(FecScheme::Gf16, 50, &chunk_refs);
    let mut saw_final = false;
    for (i, p) in pkts.iter().enumerate() {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        assert_eq!(
            h.flags & FLAG_SOF != 0,
            i == 0,
            "SOF exactly on the first packet"
        );
        assert_eq!(
            h.flags & FLAG_EOF != 0,
            i + 1 == pkts.len(),
            "EOF exactly on the last packet"
        );
        if h.block_index < 2 {
            assert_eq!(
                h.block_count, 0,
                "non-final block must ride sentinel headers"
            );
            assert_eq!(h.frame_bytes, 0);
            assert_eq!(h.data_shards, 4, "sentinel blocks are exactly full-K");
        } else {
            saw_final = true;
            assert_eq!(h.block_count, 3, "final block carries the real block count");
            assert_eq!(h.frame_bytes as usize, src.len(), "and the real AU size");
        }
    }
    assert!(saw_final);
}

/// AU smaller than one block emits no sentinels; shape matches a legacy frame.
#[test]
fn streamed_small_frame_degenerates_to_legacy() {
    let (pkts, src) = streamed_packets(FecScheme::Gf16, 50, &[&[0x5Au8; 40]]);
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        assert_eq!(
            h.block_count, 1,
            "single-block streamed AU must be legacy-shaped"
        );
        assert_eq!(h.frame_bytes as usize, src.len());
    }
}

/// Drop a sentinel that is not full-K, claims a non-zero total, or leaves no room for a final block.
#[test]
fn streamed_sentinel_firewall_bounds() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let sentinel = |f: fn(&mut PacketHeader)| {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8; // limits().max_data_shards — only legal sentinel K
        h.recovery_shards = 0;
        f(&mut h);
        h
    };
    let h = sentinel(|h| h.data_shards = 7);
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    let h = sentinel(|h| h.frame_bytes = 64);
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // derived max_blocks is 32; no room for a final after index 31
    let h = sentinel(|h| h.block_index = 31);
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, 3);
    // Conformant sentinel accepted — rejections above are not vacuous.
    let h = sentinel(|_| {});
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(
        stats.snapshot().packets_dropped,
        3,
        "conformant sentinel accepted"
    );
}

/// Final totals that put a received sentinel out of range kill the whole frame; stragglers cannot resurrect it.
#[test]
fn streamed_lying_final_totals_kill_the_frame_wholesale() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    for bi in 0..2u16 {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.block_index = bi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    // Final claiming one 16-byte shard is self-valid (expect_blocks=1, K=1) but disowns both sentinels.
    let mut lying = base_header();
    lying.block_count = 1;
    lying.frame_bytes = 16;
    lying.data_shards = 1;
    lying.recovery_shards = 0;
    assert!(r
        .push(&packet(lying), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    let snap = stats.snapshot();
    assert_eq!(
        snap.frames_dropped, 1,
        "the lying frame must be counted lost"
    );
    let mut h = base_header();
    h.block_count = 0;
    h.frame_bytes = 0;
    h.data_shards = 8;
    h.recovery_shards = 0;
    let before = stats.snapshot().packets_dropped;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(
        stats.snapshot().packets_dropped,
        before + 1,
        "straggler for a killed frame must be dropped, not re-open it"
    );
}

// ---------------------------------------------------------------------------
// Slice-granularity streamed AUs (USER_FLAG_SLICE_STREAM)
// ---------------------------------------------------------------------------

/// Block size large enough that slice cuts land inside a block (variable-K sentinels).
fn slice_config() -> Config {
    use crate::config::{FecConfig, ProtocolPhase, Role};
    Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 50,
            max_data_per_block: 64,
        },
        shard_payload: 16,
        max_frame_bytes: 4096,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    }
}

/// 1023 B → blocks (K, base-shard): (19, 0), (26, 19), (18, 45), final (1, 63).
/// Chunk 0 is an exact 20-shard multiple; a flush never drains `pending` empty.
fn slice_chunks() -> Vec<Vec<u8>> {
    [320usize, 403, 100, 200]
        .iter()
        .enumerate()
        .map(|(c, &n)| (0..n).map(|i| (c * 57 + i * 131 + 7) as u8).collect())
        .collect()
}

fn slice_streamed_packets() -> (Vec<Vec<u8>>, Vec<u8>) {
    let cfg = slice_config();
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(12345, USER_FLAG_SLICE_STREAM, Some(0));
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    let mut src = Vec::new();
    for c in slice_chunks() {
        src.extend_from_slice(&c);
        pk.push_streamed(&mut au, &c, true, coder.as_ref(), |h, b| {
            let mut p = Vec::with_capacity(HEADER_LEN + b.len());
            p.extend_from_slice(h.as_bytes());
            p.extend_from_slice(b);
            pkts.push(p);
            Ok(())
        })
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

fn push_all(
    r: &mut Reassembler,
    coder: &dyn crate::fec::ErasureCoder,
    stats: &StatsCounters,
    delivery: &[Vec<u8>],
) -> Option<crate::session::Frame> {
    let mut got = None;
    for p in delivery {
        if let Some(f) = r.push(p, coder, stats).unwrap() {
            assert!(got.is_none(), "frame must complete exactly once");
            got = Some(f);
        }
    }
    got
}

/// Slice packets carry `USER_FLAG_SLICE_STREAM`; sentinel `frame_bytes` is the shard-aligned block base.
#[test]
fn slice_streamed_wire_shape_and_roundtrip() {
    let (pkts, src) = slice_streamed_packets();
    assert_eq!(src.len(), 1023);
    // Chunk 2 (100 B) is < MIN_STREAM_BLOCK_SHARDS and rides into block 2 with chunk 3.
    // Block 0 keeps one shard (chunk 0 is an exact multiple) into block 1.
    let expect = [(0u16, 19u16, 0u32), (1, 26, 304), (2, 18, 720)];
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        assert_ne!(
            h.user_flags & USER_FLAG_SLICE_STREAM,
            0,
            "the marker must ride EVERY packet — reorder can deliver any of them first"
        );
        if h.block_count == 0 {
            let (_, k, base) = expect[h.block_index as usize];
            assert_eq!(h.data_shards, k, "block {} K", h.block_index);
            assert_eq!(h.frame_bytes, base, "block {} base", h.block_index);
            assert_eq!(base % 16, 0, "sentinel bases are shard-aligned");
        } else {
            assert_eq!(h.block_index, 3);
            assert_eq!(h.block_count, 4);
            assert_eq!(h.frame_bytes as usize, src.len());
            assert_eq!(h.data_shards, 1);
        }
    }

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
        .expect("slice-streamed frame must complete");
    assert_eq!(f.data, src, "reassembled slice AU must be byte-identical");
    assert_ne!(f.flags & USER_FLAG_SLICE_STREAM, 0);
}

/// Reverse: final totals pin first; variable-K sentinels must still assemble.
#[test]
fn slice_streamed_survives_loss_and_reorder() {
    let (pkts, src) = slice_streamed_packets();
    let mut delivery: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| {
            let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
            // Kill data 2/5/17 of block 0 and 3/7 of block 1 — within 50% parity.
            !(h.block_count == 0
                && ((h.block_index == 0 && [2, 5, 17].contains(&h.shard_index))
                    || (h.block_index == 1 && [3, 7].contains(&h.shard_index))))
        })
        .cloned()
        .collect();
    delivery.reverse();
    let dup = delivery.first().cloned().unwrap();
    delivery.push(dup);

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &delivery)
        .expect("slice-streamed frame must complete within the FEC budget");
    assert_eq!(f.data, src);
}

/// After pin, a sentinel whose base reaches into the final block's range is dropped; honest packets still complete.
#[test]
fn slice_streamed_post_pin_out_of_range_sentinel_dropped() {
    let (pkts, src) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();

    // Final block first — pins totals (64 data shards, final K = 1).
    let finals: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count != 0)
        .cloned()
        .collect();
    assert!(push_all(&mut r, coder.as_ref(), &stats, &finals).is_none());

    // Block 0 claiming base shard 60: 60+20 > 63 overlaps the final block. Drop, do not kill the frame.
    let mut evil = pkts
        .iter()
        .find(|p| {
            let h = hdr_of(p);
            h.block_count == 0 && h.block_index == 0 && h.shard_index == 0
        })
        .cloned()
        .unwrap();
    let mut h = PacketHeader::read_from_bytes(&evil[..HEADER_LEN]).unwrap();
    h.frame_bytes = 60 * 16;
    evil[..HEADER_LEN].copy_from_slice(h.as_bytes());
    let before = stats.snapshot().packets_dropped;
    assert!(r.push(&evil, coder.as_ref(), &stats).unwrap().is_none());
    assert_eq!(stats.snapshot().packets_dropped, before + 1);

    let rest: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count == 0)
        .cloned()
        .collect();
    let f = push_all(&mut r, coder.as_ref(), &stats, &rest)
        .expect("the honest blocks must still complete the frame");
    assert_eq!(f.data, src);
}

/// Final totals that make a landed sentinel overlap the final range kill the whole frame.
#[test]
fn slice_streamed_lying_final_kills_frame() {
    let (pkts, _) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();

    let sentinels: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count == 0)
        .cloned()
        .collect();
    assert!(push_all(&mut r, coder.as_ref(), &stats, &sentinels).is_none());

    // Final K=30 puts final base at shard 34; block 2 (base 45, K 18) needs base ≥ 63.
    let mut lying = pkts
        .iter()
        .find(|p| {
            let h = hdr_of(p);
            h.block_count != 0 && h.shard_index == 0
        })
        .cloned()
        .unwrap();
    let mut h = PacketHeader::read_from_bytes(&lying[..HEADER_LEN]).unwrap();
    h.data_shards = 30;
    lying[..HEADER_LEN].copy_from_slice(h.as_bytes());
    assert!(r.push(&lying, coder.as_ref(), &stats).unwrap().is_none());
    assert_eq!(
        stats.snapshot().frames_dropped,
        1,
        "the lying frame must be counted lost"
    );

    let finals: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| hdr_of(p).block_count != 0)
        .cloned()
        .collect();
    assert!(push_all(&mut r, coder.as_ref(), &stats, &finals).is_none());
    assert_eq!(stats.snapshot().frames_dropped, 1);
}

/// A sentinel base that passes range checks but breaks tiling (gap + overlap) must not
/// complete; count it lost so the client requests recovery.
#[test]
fn slice_streamed_lying_base_within_bounds_kills_frame() {
    let (pkts, _) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    // Shift block 1 base from shard 19 to 20 on every packet (base is pinned by the first).
    // Still aligned, still 20+26=46 ≤ 63, but leaves a one-shard gap at 19 and overwrites
    // block 2's first shard.
    let delivery: Vec<Vec<u8>> = pkts
        .iter()
        .map(|p| {
            let mut h = hdr_of(p);
            if h.block_count == 0 && h.block_index == 1 {
                let mut p = p.clone();
                h.frame_bytes = 320;
                p[..HEADER_LEN].copy_from_slice(h.as_bytes());
                p
            } else {
                p.clone()
            }
        })
        .collect();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    assert!(
        push_all(&mut r, coder.as_ref(), &stats, &delivery).is_none(),
        "a mis-tiled frame must never be delivered"
    );
    assert_eq!(
        stats.snapshot().frames_dropped,
        1,
        "the mis-tiled frame must be counted lost"
    );
    assert_eq!(r.in_flight(), 0, "the killed frame must release its budget");

    assert!(push_all(&mut r, coder.as_ref(), &stats, &delivery).is_none());
    assert_eq!(stats.snapshot().frames_dropped, 1);
}

/// One slice larger than a FEC block must emit multiple blocks from a single push; the final block must not be oversized.
#[test]
fn slice_streamed_giant_slice_cuts_multiple_blocks() {
    let cfg = slice_config(); // max_data_per_block 64
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(1, USER_FLAG_SLICE_STREAM, Some(0));
    let src: Vec<u8> = (0..70 * 16).map(|i| (i * 131 + 7) as u8).collect();
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    pk.push_streamed(&mut au, &src, true, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        if h.block_count == 0 {
            assert_eq!((h.block_index, h.data_shards, h.frame_bytes), (0, 64, 0));
        } else {
            assert_eq!((h.block_index, h.block_count, h.data_shards), (1, 2, 6));
            assert_eq!(h.frame_bytes as usize, src.len());
        }
    }
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts).expect("must complete");
    assert_eq!(f.data, src);
}

/// When `max_data_per_block` < [`MIN_STREAM_BLOCK_SHARDS`], a slice cut flushes full-K blocks.
#[test]
fn slice_streamed_small_kmax_roundtrip() {
    let cfg = e2e_config(FecScheme::Gf16, 50); // max_data_per_block 4 < 16
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut au = pk.begin_streamed(1, USER_FLAG_SLICE_STREAM, Some(0));
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    let mut src = Vec::new();
    for c in 0..2usize {
        let chunk: Vec<u8> = (0..320 + c * 83)
            .map(|i| (c * 57 + i * 131 + 7) as u8)
            .collect();
        src.extend_from_slice(&chunk);
        pk.push_streamed(&mut au, &chunk, true, coder.as_ref(), |h, b| {
            let mut p = Vec::with_capacity(HEADER_LEN + b.len());
            p.extend_from_slice(h.as_bytes());
            p.extend_from_slice(b);
            pkts.push(p);
            Ok(())
        })
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    for p in &pkts {
        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
        if h.block_count == 0 {
            assert_eq!(h.data_shards, 4, "floor clamps to full-K blocks");
            assert_eq!(h.frame_bytes % (4 * 16), 0, "bases advance block-wise");
        }
    }
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts).expect("must complete");
    assert_eq!(f.data, src);
}

/// A packet that disagrees on `USER_FLAG_SLICE_STREAM` with the opened frame is dropped, not used to pin.
#[test]
fn slice_streamed_mixed_flag_packet_dropped() {
    let (pkts, _) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();

    let first = pkts
        .iter()
        .find(|p| hdr_of(p).block_count == 0)
        .cloned()
        .unwrap();
    assert!(r.push(&first, coder.as_ref(), &stats).unwrap().is_none());

    // Legacy one-shard final that would pass the legacy firewall; only the flag check
    // stops it pinning this slice-opened frame under uniform rules.
    let mut h = hdr_of(&first);
    h.user_flags &= !USER_FLAG_SLICE_STREAM;
    h.block_index = 0;
    h.block_count = 1;
    h.frame_bytes = 16;
    h.data_shards = 1;
    h.recovery_shards = 0;
    h.shard_index = 0;
    let mut legacy = Vec::with_capacity(HEADER_LEN + 16);
    legacy.extend_from_slice(h.as_bytes());
    legacy.extend_from_slice(&[0xEE; 16]);
    let before = stats.snapshot().packets_dropped;
    assert!(r.push(&legacy, coder.as_ref(), &stats).unwrap().is_none());
    assert_eq!(stats.snapshot().packets_dropped, before + 1);
    assert_eq!(stats.snapshot().frames_dropped, 0, "dropped, not killed");
}

// ---------------------------------------------------------------------------
// Slice-progressive prefix delivery (Frame::part)
// ---------------------------------------------------------------------------

fn push_collect(
    r: &mut Reassembler,
    coder: &dyn crate::fec::ErasureCoder,
    stats: &StatsCounters,
    delivery: &[Vec<u8>],
) -> Vec<crate::session::Frame> {
    let mut out = Vec::new();
    for p in delivery {
        if let Some(f) = r.push(p, coder, stats).unwrap() {
            out.push(f);
        }
    }
    out
}

/// In-order: one part per completed block; last part is the suffix with `last` + `complete`.
#[test]
fn parts_stream_in_order() {
    let (pkts, src) = slice_streamed_packets();
    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &pkts);
    assert_eq!(
        got.len(),
        4,
        "three sentinel-block parts + the final suffix"
    );
    let mut rebuilt = Vec::new();
    for (i, f) in got.iter().enumerate() {
        let part = f
            .part
            .expect("parts mode: every delivery carries part meta");
        assert_eq!(
            part.offset as usize,
            rebuilt.len(),
            "parts tile with no gaps"
        );
        assert_eq!(part.first, i == 0);
        assert_eq!(part.last, i + 1 == got.len());
        assert_eq!(
            f.complete, part.last,
            "complete rides exactly the last part"
        );
        rebuilt.extend_from_slice(&f.data);
    }
    assert_eq!(rebuilt, src, "concatenated parts must be the byte-exact AU");
    assert_eq!(
        stats.snapshot().frames_completed,
        0,
        "the reassembler leaves the completion count to the session boundary"
    );
}

/// A block completing behind the prefix emits nothing; closing the gap emits one coalesced part.
#[test]
fn parts_coalesce_across_reordered_blocks() {
    let (pkts, src) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
    // Blocks 1 and 2 fully first, then block 0, then the final block.
    let mut delivery: Vec<Vec<u8>> = Vec::new();
    for want in [1u16, 2, 0] {
        delivery.extend(
            pkts.iter()
                .filter(|p| {
                    let h = hdr_of(p);
                    h.block_count == 0 && h.block_index == want
                })
                .cloned(),
        );
    }
    delivery.extend(pkts.iter().filter(|p| hdr_of(p).block_count != 0).cloned());

    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &delivery);
    assert_eq!(got.len(), 2, "one coalesced prefix part + the final suffix");
    let p0 = got[0].part.unwrap();
    assert_eq!((p0.offset, p0.first, p0.last), (0, true, false));
    assert_eq!(got[0].data.len(), 720 + 288, "blocks 0-2 in one part");
    let p1 = got[1].part.unwrap();
    assert!(p1.last && got[1].complete);
    let mut rebuilt = got[0].data.clone();
    rebuilt.extend_from_slice(&got[1].data);
    assert_eq!(rebuilt, src);
}

/// Loss inside a block delays its part until FEC reconstructs; the part then carries recovered bytes.
#[test]
fn parts_wait_for_fec_reconstruction() {
    let (pkts, src) = slice_streamed_packets();
    let hdr_of = |p: &Vec<u8>| PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
    // Kill two data shards of block 0 — within the 50% parity budget.
    let delivery: Vec<Vec<u8>> = pkts
        .iter()
        .filter(|p| {
            let h = hdr_of(p);
            !(h.block_count == 0 && h.block_index == 0 && [1, 7].contains(&h.shard_index))
        })
        .cloned()
        .collect();
    let cfg = slice_config();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &delivery);
    let mut rebuilt = Vec::new();
    for f in &got {
        rebuilt.extend_from_slice(&f.data);
    }
    assert_eq!(
        rebuilt, src,
        "reconstructed prefix parts must be byte-exact"
    );
    assert!(got.last().unwrap().complete);
}

/// Legacy single-block frame in parts mode: one delivery with `{offset 0, first, last}`.
#[test]
fn parts_degenerate_whole_frame() {
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let src: Vec<u8> = (0..40).map(|i| i as u8).collect();
    let pkts = pk.packetize(&src, 7, 0, coder.as_ref()).unwrap();
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &pkts);
    assert_eq!(got.len(), 1);
    let f = &got[0];
    assert_eq!(
        f.part,
        Some(crate::session::FramePart {
            offset: 0,
            first: true,
            last: true
        })
    );
    assert!(f.complete);
    assert_eq!(f.data, src);
}

/// Parts also flow for uniform full-K (legacy streamed) sentinels; the prefix cursor is `base_shard`.
#[test]
fn parts_flow_for_legacy_streamed_frames() {
    let chunks: Vec<Vec<u8>> = (0..3)
        .map(|c| (0..50).map(|i| (c * 57 + i * 131 + 7) as u8).collect())
        .collect();
    let chunk_refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (pkts, src) = streamed_packets(FecScheme::Gf16, 50, &chunk_refs);
    let cfg = e2e_config(FecScheme::Gf16, 50);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    r.set_deliver_parts(true);
    let coder = coder_for(FecScheme::Gf16);
    let stats = StatsCounters::default();
    let got = push_collect(&mut r, coder.as_ref(), &stats, &pkts);
    assert!(got.len() > 1, "sentinel blocks must deliver early parts");
    let mut rebuilt = Vec::new();
    for f in &got {
        rebuilt.extend_from_slice(&f.data);
    }
    assert_eq!(rebuilt, src);
    assert!(got.last().unwrap().complete);
}

/// A one-datagram open commits only the buffer its own header proves; a sentinel whose
/// wire base sits near the frame ceiling is still bounded by the in-flight budget.
#[test]
fn streamed_open_commits_its_own_extent_and_stays_bounded() {
    let coder = coder_for(FecScheme::Gf8);
    // limits(): 16 B shards, max_data_shards 8, max_frame_bytes 4096 → budget 4×4096.
    // Modest legacy sentinels (K=8 → 128 B) must not exhaust it.
    let mut r = Reassembler::new(limits());
    let stats = StatsCounters::default();
    for fi in 0..32u32 {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.frame_index = fi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert_eq!(
        stats.snapshot().packets_dropped,
        0,
        "ordinary one-datagram opens must not exhaust the in-flight budget"
    );

    // Slice sentinel at base 3968 B + K 8 = 4096 B plus block state; first past budget refuses.
    let lim = limits();
    let budget = IN_FLIGHT_BUF_FACTOR * lim.max_frame_bytes;
    let fits = budget / (4096 + block_state_bytes(8, 0));
    let mut r = Reassembler::new(lim);
    let stats = StatsCounters::default();
    for fi in 0..=fits as u32 {
        let mut h = base_header();
        h.user_flags = USER_FLAG_SLICE_STREAM;
        h.block_count = 0;
        h.frame_bytes = 4096 - 8 * 16;
        h.block_index = 1;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.frame_index = fi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert!(
        r.in_flight() <= budget,
        "in-flight commitment {} must never exceed the {budget} B budget",
        r.in_flight(),
    );
    assert_eq!(
        stats.snapshot().packets_dropped,
        1,
        "the first ceiling-claiming open past the budget must be refused"
    );
}

/// After a final-first open, a sentinel aimed at or past the pinned slot must drop (full-K
/// write would land outside the exact-sized buffer) without corrupting the in-flight frame.
#[test]
fn streamed_out_of_range_sentinel_after_final_first_is_dropped() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    // Final opens: block_count=1, frame_bytes=32 → K=2. One shard so totals pin while still in flight.
    let mut fin = base_header();
    fin.block_count = 1;
    fin.frame_bytes = 32;
    fin.data_shards = 2;
    fin.recovery_shards = 0;
    fin.shard_index = 0;
    assert!(r
        .push(&packet(fin), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // Sentinels at slot 0 and 1 are non-final under pinned block_count=1; never write the 32-byte buffer.
    for bi in 0..2u16 {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.block_index = bi;
        assert!(r
            .push(&packet(h), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
    }
    assert_eq!(stats.snapshot().packets_dropped, 2);
    let mut fin2 = fin;
    fin2.shard_index = 1;
    let got = r
        .push(&packet(fin2), coder.as_ref(), &stats)
        .unwrap()
        .expect("frame must still complete after the rejected sentinels");
    assert_eq!(got.data.len(), 32);
    assert!(got.complete);
}

/// A second final with different totals is rejected once pinned; the frame completes under the first totals.
#[test]
fn streamed_second_final_with_different_totals_is_rejected() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    let sentinel_shard = |shard_index: u16| {
        let mut h = base_header();
        h.block_count = 0;
        h.frame_bytes = 0;
        h.data_shards = 8;
        h.recovery_shards = 0;
        h.block_index = 0;
        h.shard_index = shard_index;
        h
    };
    let final_shard = |frame_bytes: u32, data_shards: u16, shard_index: u16| {
        let mut h = base_header();
        h.block_count = 2;
        h.frame_bytes = frame_bytes;
        h.data_shards = data_shards;
        h.recovery_shards = 0;
        h.block_index = 1;
        h.shard_index = shard_index;
        h
    };
    // Sentinel opens block 0, then the real final pins totals: 10 shards = 160 bytes.
    assert!(r
        .push(&packet(sentinel_shard(0)), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert!(r
        .push(&packet(final_shard(160, 2, 0)), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // Second final claiming 144 B (K=1) is self-valid but contradicts the pin.
    let before = stats.snapshot().packets_dropped;
    assert!(r
        .push(&packet(final_shard(144, 1, 0)), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(stats.snapshot().packets_dropped, before + 1);
    // Remaining block-0 shards plus the original final tail complete under the first pin.
    let mut got = None;
    for s in 1..8u16 {
        assert!(got.is_none());
        got = r
            .push(&packet(sentinel_shard(s)), coder.as_ref(), &stats)
            .unwrap();
    }
    assert!(got.is_none(), "block 1 still owes a shard");
    let got = r
        .push(&packet(final_shard(160, 2, 1)), coder.as_ref(), &stats)
        .unwrap()
        .expect("frame completes under the first pinned totals");
    assert_eq!(got.data.len(), 160);
}

/// 1500-MTU shard payload and the 8 MiB floor the QUIC handshake negotiates.
fn prod_slice_config() -> Config {
    use crate::config::{FecConfig, ProtocolPhase, Role};
    Config {
        role: Role::Host,
        phase: ProtocolPhase::P2Punktfunk,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 20,
            max_data_per_block: 200,
        },
        shard_payload: crate::config::mtu1500_shard_payload(),
        max_frame_bytes: 8 << 20,
        encrypt: false,
        key: SessionKey::Aes128Gcm([0u8; 16]),
        salt: [0u8; 4],
        loopback_drop_period: 0,
    }
}

fn streamed_packets_with(
    cfg: &Config,
    frame_index: u32,
    pts_ns: u64,
    slice: bool,
    chunks: &[usize],
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let coder = coder_for(cfg.fec.scheme);
    let mut pk = Packetizer::new(cfg);
    let uf = if slice { USER_FLAG_SLICE_STREAM } else { 0 };
    let mut au = pk.begin_streamed(pts_ns, uf, Some(frame_index));
    let (mut pkts, mut src) = (Vec::new(), Vec::new());
    let sink = |pkts: &mut Vec<Vec<u8>>, h: &PacketHeader, b: &[u8]| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
    };
    for (c, &n) in chunks.iter().enumerate() {
        let data: Vec<u8> = (0..n).map(|i| (c * 57 + i * 131 + 7) as u8).collect();
        src.extend_from_slice(&data);
        pk.push_streamed(&mut au, &data, true, coder.as_ref(), |h, b| {
            sink(&mut pkts, h, b);
            Ok(())
        })
        .unwrap();
    }
    pk.finish_streamed(au, coder.as_ref(), |h, b| {
        sink(&mut pkts, h, b);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

/// An AU whose length is an exact multiple of the shard payload must reassemble.
/// `finish_streamed` must not seal a zero-pad shard whose derived base overlaps the previous block.
#[test]
fn slice_streamed_exact_shard_multiple_completes() {
    let cfg = prod_slice_config();
    let coder = coder_for(FecScheme::Gf16);
    let payload = cfg.shard_payload;
    for shards in [16usize, 29, 30, 64] {
        let (pkts, src) = streamed_packets_with(&cfg, 1, 1000, true, &[shards * payload]);
        // Final block must carry real bytes, never a lone zero-pad sitting on the previous block.
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
            .unwrap_or_else(|| panic!("{shards}-shard AU (exact multiple) must complete"));
        assert_eq!(f.data, src, "{shards}-shard AU must be byte-identical");
    }
    // Off-by-one sweep around an exact multiple.
    for extra in 0..3usize {
        let n = 30 * payload + extra;
        let (pkts, src) = streamed_packets_with(&cfg, 2, 2000, true, &[n]);
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
            .unwrap_or_else(|| panic!("{n}-byte AU must complete"));
        assert_eq!(f.data, src);
    }
}

/// A slice-streamed frame must charge its own size, not `max_frame_bytes`, or the in-flight
/// budget is spent after a handful of ordinary AUs.
#[test]
fn slice_streamed_in_flight_budget_matches_legacy() {
    let cfg = prod_slice_config();
    let coder = coder_for(FecScheme::Gf16);
    // 40 KB AU opened but not completed — several of these sit in flight under reorder.
    for slice in [false, true] {
        let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
        let stats = StatsCounters::default();
        for i in 0..12u32 {
            let (pkts, _) = streamed_packets_with(&cfg, i, 1_000_000 * i as u64, slice, &[40_000]);
            r.push(&pkts[0], coder.as_ref(), &stats).unwrap();
        }
        assert_eq!(
            stats
                .packets_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "slice={slice}: 12 ordinary AUs in flight must fit the in-flight budget"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-frame shard geometry (mid-session shard-payload renegotiation).
// design/shard-payload-reneg.md
// ---------------------------------------------------------------------------

/// Shard sizes renegotiation uses: clamp floor 512, 1280-MTU 1216, 1500-MTU 1408,
/// 9000-MTU jumbo 8908 (sealed 8972, inside [`MAX_DATAGRAM_BYTES`]).
const PRODUCTION_SHARDS: [usize; 4] = [512, 1216, 1408, 8908];

fn geo_config(shard_payload: usize) -> Config {
    let mut c = prod_slice_config();
    c.shard_payload = shard_payload;
    c.validate().expect("geometry config must be valid");
    c
}

fn legacy_packets_with(
    pk: &mut Packetizer,
    frame_index: u32,
    pts_ns: u64,
    len: usize,
    coder: &dyn crate::fec::ErasureCoder,
) -> (Vec<Vec<u8>>, Vec<u8>) {
    let src: Vec<u8> = (0..len)
        .map(|i| (i * 131 + frame_index as usize * 7 + 3) as u8)
        .collect();
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    pk.packetize_each(&src, pts_ns, 0, Some(frame_index), coder, |h, b| {
        let mut p = Vec::with_capacity(HEADER_LEN + b.len());
        p.extend_from_slice(h.as_bytes());
        p.extend_from_slice(b);
        pkts.push(p);
        Ok(())
    })
    .unwrap();
    (pkts, src)
}

/// Slice-wire suite at every production shard size: exact-multiple, lossy reverse, legacy
/// streamed, in-flight budget — each must deliver byte-identical frames.
#[test]
fn slice_wire_suite_at_production_shard_sizes() {
    let coder = coder_for(FecScheme::Gf16);
    for &shard in &PRODUCTION_SHARDS {
        let cfg = geo_config(shard);

        for shards in [16usize, 30, 64] {
            for extra in 0..3usize {
                let n = shards * shard + extra;
                let (pkts, src) = streamed_packets_with(&cfg, 1, 1000, true, &[n]);
                let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
                let stats = StatsCounters::default();
                let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
                    .unwrap_or_else(|| panic!("shard {shard}: {n}-byte slice AU must complete"));
                assert_eq!(
                    f.data, src,
                    "shard {shard}: {n}-byte AU must be byte-identical"
                );
                assert_eq!(
                    r.in_flight(),
                    0,
                    "shard {shard}: budget must return to zero"
                );
            }
        }

        // Kill one recoverable data shard. Reverse: final totals arrive first.
        for reverse in [false, true] {
            let chunks = [20 * shard + 13, 7 * shard + 1, 17 * shard];
            let (pkts, src) = streamed_packets_with(&cfg, 2, 2000, true, &chunks);
            let killed = pkts
                .iter()
                .position(|p| {
                    let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
                    h.shard_index < h.data_shards && h.recovery_shards >= 1
                })
                .expect("suite frame must have a recoverable data shard");
            let mut delivery: Vec<Vec<u8>> = pkts
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != killed)
                .map(|(_, p)| p.clone())
                .collect();
            if reverse {
                delivery.reverse();
            }
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
            let stats = StatsCounters::default();
            let f = push_all(&mut r, coder.as_ref(), &stats, &delivery).unwrap_or_else(|| {
                panic!("shard {shard} reverse={reverse}: lossy slice AU must complete")
            });
            assert_eq!(f.data, src, "shard {shard} reverse={reverse}");
            assert_eq!(r.in_flight(), 0);
        }

        // Uniform full-K sentinel: AU spanning a K=200 sentinel plus a final block.
        {
            let (pkts, src) = streamed_packets_with(&cfg, 3, 3000, false, &[230 * shard]);
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
            let stats = StatsCounters::default();
            let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
                .unwrap_or_else(|| panic!("shard {shard}: legacy-streamed AU must complete"));
            assert_eq!(f.data, src);
            assert_eq!(r.in_flight(), 0);
        }

        for slice in [false, true] {
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
            let stats = StatsCounters::default();
            for i in 0..12u32 {
                let (pkts, _) =
                    streamed_packets_with(&cfg, i, 1_000_000 * i as u64, slice, &[40_000]);
                r.push(&pkts[0], coder.as_ref(), &stats).unwrap();
            }
            assert_eq!(
                stats
                    .packets_dropped
                    .load(std::sync::atomic::Ordering::Relaxed),
                0,
                "shard {shard} slice={slice}: 12 AUs in flight must fit the budget"
            );
        }
    }
}

/// One packetizer, one reassembler: shard payload swapped between AUs; each frame delivers
/// under its own pin and the budget returns to zero.
#[test]
fn mid_stream_shard_swap_delivers_every_frame() {
    let cfg = geo_config(1408);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();

    // (shard size, AU length); swap happens between AUs.
    let schedule = [
        (1408usize, 3 * 1408 + 100),
        (1408, 9 * 1408),
        (512, 5 * 512 + 17), // shrink
        (512, 512),
        (8908, 12 * 8908 + 1), // grow
        (1216, 4 * 1216 + 9),  // revert
    ];
    for (i, &(shard, len)) in schedule.iter().enumerate() {
        pk.set_shard_payload(shard);
        let pts = 1_000_000 * (i as u64 + 1);
        let (pkts, src) = legacy_packets_with(&mut pk, i as u32, pts, len, coder.as_ref());
        for p in &pkts {
            let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
            assert_eq!(
                h.shard_bytes as usize, shard,
                "sender must stamp the live size"
            );
        }
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts)
            .unwrap_or_else(|| panic!("frame {i} at shard {shard} must complete"));
        assert_eq!(
            f.data, src,
            "frame {i} at shard {shard} must be byte-identical"
        );
        assert!(f.complete);
    }
    assert_eq!(
        r.in_flight(),
        0,
        "budget must be exact across geometry swaps"
    );
    assert_eq!(stats.snapshot().frames_dropped, 0);
}

/// An old-geometry frame still in flight when new-geometry frames arrive completes under
/// its own pin; its straggler must not land in the new geometry's buffer.
#[test]
fn old_geometry_frame_completes_after_new_geometry_arrived() {
    let cfg = geo_config(1408);
    let coder = coder_for(FecScheme::Gf16);
    let mut pk = Packetizer::new(&cfg);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();

    // Frame 0 at 1408: 7 data + 2 parity. Withhold three data shards so FEC cannot complete it.
    let (pkts0, src0) = legacy_packets_with(&mut pk, 0, 1_000_000, 6 * 1408 + 50, coder.as_ref());
    assert_eq!(
        pkts0.len(),
        9,
        "expected geometry changed — update the split"
    );
    let head: Vec<Vec<u8>> = pkts0[..4].iter().chain(&pkts0[7..]).cloned().collect();
    let straggler = &pkts0[4];
    assert!(
        push_all(&mut r, coder.as_ref(), &stats, &head).is_none(),
        "frame 0 must still be incomplete"
    );

    pk.set_shard_payload(512);
    for i in 1..=2u32 {
        let pts = 1_000_000 + 1_000_000 * i as u64;
        let (pkts, src) = legacy_packets_with(&mut pk, i, pts, 3 * 512 + 7, coder.as_ref());
        let f = push_all(&mut r, coder.as_ref(), &stats, &pkts).expect("new-geometry frame");
        assert_eq!(f.data, src);
    }

    let f = r
        .push(straggler, coder.as_ref(), &stats)
        .unwrap()
        .expect("old-geometry frame must complete under its own pin");
    assert_eq!(f.data, src0);
    assert_eq!(f.frame_index, 0);
    assert_eq!(r.in_flight(), 0);
    assert_eq!(stats.snapshot().frames_dropped, 0);
}

/// A packet with a different in-bounds shard size for an already-pinned frame is dropped.
#[test]
fn cross_geometry_packet_for_a_pinned_frame_is_dropped() {
    let cfg = geo_config(1408);
    let coder = coder_for(FecScheme::Gf16);
    let mut r = Reassembler::new(ReassemblerLimits::from_config(&cfg));
    let stats = StatsCounters::default();

    let mut pk_a = Packetizer::new(&geo_config(1408));
    let mut pk_b = Packetizer::new(&geo_config(1216));
    let (pkts, src) = legacy_packets_with(&mut pk_a, 0, 1_000_000, 5 * 1408 + 9, coder.as_ref());
    // Same frame index at 1216: self-consistent, wrong for this frame's pin.
    let (impostor, _) = legacy_packets_with(&mut pk_b, 0, 1_000_000, 5 * 1216, coder.as_ref());

    assert!(r.push(&pkts[0], coder.as_ref(), &stats).unwrap().is_none());
    let before = stats.snapshot().packets_dropped;
    assert!(r
        .push(&impostor[1], coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    assert_eq!(
        stats.snapshot().packets_dropped,
        before + 1,
        "cross-geometry packet must be dropped by the frame pin"
    );
    let f = push_all(&mut r, coder.as_ref(), &stats, &pkts[1..])
        .expect("the pinned frame must still complete from its real packets");
    assert_eq!(f.data, src, "no impostor bytes may reach the frame");
}

/// Pinned shard size below the floor, above the ceiling, or odd is dropped before allocation; exact bounds deliver.
#[test]
fn shard_size_firewall_bounds() {
    let cfg = geo_config(1408);
    let lim = ReassemblerLimits::from_config(&cfg);
    assert_eq!(lim.min_shard_bytes, crate::config::MIN_SHARD_PAYLOAD);
    assert_eq!(lim.max_shard_bytes, crate::config::max_shard_payload());
    let coder = coder_for(FecScheme::Gf16);
    let mut r = Reassembler::new(lim);
    let stats = StatsCounters::default();

    let single = |shard: usize, frame_index: u32| {
        let mut h = base_header();
        h.frame_index = frame_index;
        h.shard_bytes = shard as u16;
        h.frame_bytes = shard as u32;
        h
    };
    // 510 below floor (even), 9154 above ceiling (even), 1409 odd within bounds: all dropped.
    for (i, shard) in [510usize, 9154, 1409].into_iter().enumerate() {
        let before = stats.snapshot().packets_dropped;
        assert!(r
            .push(&packet(single(shard, i as u32)), coder.as_ref(), &stats)
            .unwrap()
            .is_none());
        assert_eq!(
            stats.snapshot().packets_dropped,
            before + 1,
            "shard {shard} must be firewalled"
        );
    }
    for (i, shard) in [
        crate::config::MIN_SHARD_PAYLOAD,
        crate::config::max_shard_payload(),
    ]
    .into_iter()
    .enumerate()
    {
        let f = r
            .push(
                &packet(single(shard, 10 + i as u32)),
                coder.as_ref(),
                &stats,
            )
            .unwrap()
            .unwrap_or_else(|| panic!("boundary shard {shard} must deliver"));
        assert_eq!(f.data.len(), shard);
    }
}

/// The delay reading a lost frame would otherwise take with it: the sample is
/// taken when the frame OPENS, so a frame whose other shards never arrive is
/// still timed, and probe filler is not timed at all.
#[test]
fn a_frame_that_never_completes_is_still_timed() {
    let mut r = Reassembler::new(limits());
    let coder = coder_for(FecScheme::Gf8);
    let stats = StatsCounters::default();
    // Four data shards, no parity: one shard arrives, three never do.
    let mut h = base_header();
    h.frame_bytes = 64;
    h.data_shards = 4;
    h.pts_ns = crate::stats::now_realtime_ns() - 30_000_000;
    assert!(r
        .push(&packet(h), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    // A second shard of the same frame adds nothing: one sample per frame.
    let mut h2 = h;
    h2.shard_index = 1;
    assert!(r
        .push(&packet(h2), coder.as_ref(), &stats)
        .unwrap()
        .is_none());
    let mut probe = h;
    probe.frame_index = 9;
    probe.user_flags = FLAG_PROBE as u32;
    let _ = r.push(&packet(probe), coder.as_ref(), &stats);

    let got: Vec<i64> = r.take_shard_delays().collect();
    assert_eq!(got.len(), 1, "one sample for the one frame that opened");
    assert!(
        (25_000_000..60_000_000).contains(&got[0]),
        "a 30 ms-old capture read {} ns",
        got[0]
    );
    assert_eq!(
        r.take_shard_delays().count(),
        0,
        "a drained sample is not re-presented"
    );
}

mod geometry_proptests {
    use super::*;
    use proptest::prelude::*;

    /// Generated frame: shard size, slice-vs-legacy, size factor, kill one recoverable data shard.
    type GenFrame = (usize, bool, usize, bool);

    fn frame_strategy() -> impl Strategy<Value = GenFrame> {
        (
            proptest::sample::select(&PRODUCTION_SHARDS[..]),
            any::<bool>(),
            1usize..30,
            any::<bool>(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Mixed shard sizes and wire shapes in one shuffled delivery with per-frame recoverable
        /// loss; every frame must deliver byte-identically and in-flight must return to zero.
        #[test]
        fn mixed_geometry_reorder_torture(
            frames in proptest::collection::vec(frame_strategy(), 2..6),
            seed in any::<u64>(),
        ) {
            let coder = coder_for(FecScheme::Gf16);
            let mut r = Reassembler::new(ReassemblerLimits::from_config(&geo_config(1408)));
            let stats = StatsCounters::default();

            let mut all: Vec<(u64, u32, Vec<u8>)> = Vec::new(); // (shuffle key, frame, pkt)
            let mut sources: Vec<(u32, Vec<u8>)> = Vec::new();
            for (i, &(shard, slice, factor, kill)) in frames.iter().enumerate() {
                let cfg = geo_config(shard);
                let pts = 1_000_000 * (i as u64 + 1);
                let len = factor * shard + (factor % shard.min(7));
                let (mut pkts, src) = if slice {
                    streamed_packets_with(&cfg, i as u32, pts, true, &[len.max(1)])
                } else {
                    let mut pk = Packetizer::new(&cfg);
                    legacy_packets_with(&mut pk, i as u32, pts, len.max(1), coder.as_ref())
                };
                if kill {
                    if let Some(k) = pkts.iter().position(|p| {
                        let h = PacketHeader::read_from_bytes(&p[..HEADER_LEN]).unwrap();
                        h.shard_index < h.data_shards && h.recovery_shards >= 1
                    }) {
                        pkts.remove(k);
                    }
                }
                for (j, p) in pkts.into_iter().enumerate() {
                    // Deterministic shuffle key: interleaves frames, reorders within a frame.
                    let key = (seed | 1)
                        .wrapping_mul(j as u64 + 1)
                        .wrapping_add((i as u64) << 17)
                        .rotate_left((j % 61) as u32);
                    all.push((key, i as u32, p));
                }
                sources.push((i as u32, src));
            }
            all.sort_by_key(|(k, _, _)| *k);

            let mut delivered: std::collections::HashMap<u32, Vec<u8>> =
                std::collections::HashMap::new();
            for (_, _, p) in &all {
                if let Some(f) = r.push(p, coder.as_ref(), &stats).unwrap() {
                    prop_assert!(f.complete);
                    prop_assert!(delivered.insert(f.frame_index, f.data).is_none(),
                        "a frame must deliver exactly once");
                }
            }
            for (i, src) in &sources {
                let got = delivered.get(i);
                prop_assert!(got.is_some(), "frame {i} must be DELIVERED, not merely error-free");
                prop_assert_eq!(got.unwrap(), src, "frame {} must be byte-identical", i);
            }
            prop_assert_eq!(r.in_flight(), 0, "budget must be exact after all frames terminate");
            prop_assert_eq!(stats.snapshot().frames_dropped, 0u64);
        }
    }
}
