//! Core acceptance: round-trip access units through host→client
//! (packetize → FEC → loopback with simulated loss → recover → reassemble)
//! and assert byte-exact recovery, both FEC schemes, sealed and unsealed.
//! Property tests cover FEC loss patterns.

use proptest::prelude::*;
use punktfunk_core::config::{Config, FecConfig, FecScheme, Role};
use punktfunk_core::crypto::{MediaKeys, MediaSuite};
use punktfunk_core::fec::coder_for;
use punktfunk_core::session::{MediaV2, Session};
use punktfunk_core::transport::loopback_pair;

fn config(role: Role, scheme: FecScheme, drop_period: u32) -> Config {
    Config {
        role,
        fec: FecConfig {
            scheme,
            fec_percent: 25,
            max_data_per_block: 32,
        },
        shard_payload: 1024,
        max_frame_bytes: 8 * 1024 * 1024,
        loopback_drop_period: drop_period,
    }
}

/// Sealed under test keys of `suite`, or unsealed; capture times count from 0.
fn media(suite: Option<MediaSuite>) -> MediaV2 {
    MediaV2 {
        clock_origin_ns: 0,
        keys: suite.map(|s| MediaKeys::derive(&[7; 32], s)),
        clock: None,
    }
}

/// Drive `frames` host→client over a lossy loopback; each must come back
/// byte-identical. Returns the client's final stats.
fn run_stream(
    scheme: FecScheme,
    suite: Option<MediaSuite>,
    drop_period: u32,
    frames: &[Vec<u8>],
) -> punktfunk_core::Stats {
    let (host_tp, client_tp) = loopback_pair(drop_period, 0);
    let mut host = Session::new(
        config(Role::Host, scheme, drop_period),
        media(suite),
        Box::new(host_tp),
    )
    .unwrap();
    let mut client = Session::new(
        config(Role::Client, scheme, drop_period),
        media(suite),
        Box::new(client_tp),
    )
    .unwrap();

    for (i, frame) in frames.iter().enumerate() {
        host.submit_frame(frame, i as u64 * 1_000_000, 0).unwrap();
        let got = client
            .poll_frame()
            .expect("frame should recover despite loss");
        assert_eq!(&got.data, frame, "frame {i} mismatched after recovery");
        assert_eq!(got.frame_index, i as u32);
        assert_eq!(got.pts_ns, i as u64 * 1_000_000);
    }
    client.stats()
}

fn sample_frames() -> Vec<Vec<u8>> {
    (0..5usize)
        .map(|f| {
            let len = 1 + f * 40_000; // 1, 40k, 80k, 120k, 160k → single- and multi-block
            (0..len)
                .map(|b| (b.wrapping_mul(31).wrapping_add(f * 7)) as u8)
                .collect()
        })
        .collect()
}

#[test]
fn gf8_stream_recovers_under_loss() {
    let frames = sample_frames();
    // drop_period 8 deletes the 1st of every 8 packets → real data-shard loss.
    let stats = run_stream(FecScheme::Gf8, None, 8, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert!(
        stats.fec_recovered_shards > 0,
        "loss should have forced FEC recovery"
    );
}

#[test]
fn gf16_stream_recovers_under_loss() {
    let frames = sample_frames();
    let stats = run_stream(FecScheme::Gf16, None, 8, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert!(stats.fec_recovered_shards > 0);
}

#[test]
fn encrypted_stream_recovers_under_loss() {
    let frames = sample_frames();
    let stats = run_stream(FecScheme::Gf8, Some(MediaSuite::Aes128Gcm), 8, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
}

/// ChaCha20-Poly1305 through the same lossy full-stream path. Loss/replay is
/// cipher-independent (the replay window keys off the authenticated packet number), so
/// recovery must be byte-identical to the AES run above.
#[test]
fn chacha20_encrypted_stream_recovers_under_loss() {
    let frames = sample_frames();
    let stats = run_stream(
        FecScheme::Gf16,
        Some(MediaSuite::ChaCha20Poly1305),
        8,
        &frames,
    );
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert!(stats.fec_recovered_shards > 0);
}

#[test]
fn lossless_stream_is_exact() {
    let frames = sample_frames();
    let stats = run_stream(FecScheme::Gf16, None, 0, &frames);
    assert_eq!(stats.frames_completed, frames.len() as u64);
    assert_eq!(
        stats.fec_recovered_shards, 0,
        "no loss → nothing to recover"
    );
}

/// `flush_backlog` must discard every queued datagram (count them dropped),
/// reset the reassembler so half-assembled frames cannot linger, and leave
/// the session healthy — the next submitted frame recovers byte-exact.
#[test]
fn flush_backlog_discards_queue_and_recovers() {
    let (host_tp, client_tp) = loopback_pair(0, 0);
    let mut host = Session::new(
        config(Role::Host, FecScheme::Gf16, 0),
        media(None),
        Box::new(host_tp),
    )
    .unwrap();
    let mut client = Session::new(
        config(Role::Client, FecScheme::Gf16, 0),
        media(None),
        Box::new(client_tp),
    )
    .unwrap();

    let frames = sample_frames();
    // Read one frame first so the client's recv ring exists and may hold an undelivered tail.
    host.submit_frame(&frames[0], 0, 0).unwrap();
    client.poll_frame().unwrap();
    for (i, f) in frames.iter().enumerate().skip(1) {
        host.submit_frame(f, i as u64 * 1_000_000, 0).unwrap();
    }
    let flushed = client.flush_backlog().unwrap();
    assert!(flushed > 0, "a queued backlog must be discarded");
    assert_eq!(client.stats().packets_dropped, flushed);
    assert!(
        matches!(
            client.poll_frame(),
            Err(punktfunk_core::PunktfunkError::NoFrame)
        ),
        "nothing pending after a flush"
    );
    let recovery = vec![0xA5u8; 100_000];
    host.submit_frame(&recovery, 99_000_000, 0).unwrap();
    let got = client.poll_frame().expect("post-flush frame completes");
    assert_eq!(got.data, recovery);
}

proptest! {
    #[test]
    fn fec_recovers_any_loss_within_budget(
        k in 1usize..40,
        extra in 0usize..16,        // recovery beyond the bare minimum
        shard_half in 1usize..64,   // shard_len = 2*shard_half (even)
        seed in any::<u64>(),
    ) {
        let m = (extra + 1).min(40);
        let shard_len = shard_half * 2;
        for coder in [coder_for(FecScheme::Gf8), coder_for(FecScheme::Gf16)] {
            // Gf8 ceiling: data + recovery <= 255.
            if matches!(coder.scheme(), FecScheme::Gf8) && k + m > 255 { continue; }

            let data: Vec<Vec<u8>> = (0..k)
                .map(|i| (0..shard_len).map(|b| (i ^ b).wrapping_add(seed as usize) as u8).collect())
                .collect();
            let refs: Vec<&[u8]> = data.iter().map(|s| s.as_slice()).collect();
            let recovery = coder.encode(&refs, m).unwrap();

            let mut received: Vec<Option<Vec<u8>>> =
                data.iter().cloned().map(Some).chain(recovery.into_iter().map(Some)).collect();

            let total = k + m;
            let lose = (seed as usize % (m + 1)).min(m);
            let mut s = seed | 1;
            for _ in 0..lose {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                let idx = (s >> 33) as usize % total;
                received[idx] = None;
            }

            let restored = coder.reconstruct(k, m, &mut received).unwrap();
            prop_assert_eq!(restored, data);
        }
    }
}
