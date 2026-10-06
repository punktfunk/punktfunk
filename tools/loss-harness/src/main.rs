//! `loss-harness` — sweep packet loss against the FEC and report recovery (plan §10).
//!
//! Drives access units through the in-process loopback at increasing loss rates, for
//! both FEC schemes, and prints how many frames survive. A pure-software stand-in for
//! `tc netem` that needs no network and runs anywhere `punktfunk_core` builds. The real harness
//! adds `tc netem` jitter/reorder on the UDP path.
#![forbid(unsafe_code)]

use punktfunk_core::config::{Config, FecConfig, FecScheme, Role};
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::packet::{FLAG_PIC, FLAG_SOF, USER_FLAG_CHUNK_ALIGNED};
use punktfunk_core::session::{MediaV2, Session};
use punktfunk_core::transport::loopback_pair;

/// Unsealed: loss recovery does not depend on the AEAD. Capture times count from 0.
fn media() -> MediaV2 {
    MediaV2 {
        clock_origin_ns: 0,
        keys: None,
        clock: None,
    }
}

fn config(role: Role, scheme: FecScheme, drop_period: u32) -> Config {
    Config {
        role,
        fec: FecConfig {
            scheme,
            fec_percent: 25,
            max_data_per_block: 64,
        },
        shard_payload: 1024,
        max_frame_bytes: 8 * 1024 * 1024,
        loopback_drop_period: drop_period,
    }
}

/// Returns (frames_completed, frames_attempted) for a loss setting. `streamed` feeds each AU
/// through the VIDEO_CAP_STREAMED_AU path (three encoder-chunk pushes + finish — sentinel
/// blocks then real totals) instead of one whole-AU submit, so the two wire shapes' recovery
/// curves can be compared directly (the Phase-2 "more, smaller units must not regress FEC" gate).
fn run(
    scheme: FecScheme,
    drop_period: u32,
    frames: usize,
    frame_len: usize,
    streamed: bool,
) -> (usize, usize) {
    let (h, c) = loopback_pair(drop_period, 0);
    let mut host = Session::new(
        config(Role::Host, scheme, drop_period),
        media(),
        Box::new(h),
    )
    .unwrap();
    let mut client = Session::new(
        config(Role::Client, scheme, drop_period),
        media(),
        Box::new(c),
    )
    .unwrap();

    let send_wires = |host: &mut Session, wires: Vec<Vec<u8>>| {
        let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
        host.send_sealed(&refs).unwrap();
        drop(refs);
        host.reclaim_wires(wires);
    };
    let mut completed = 0;
    for f in 0..frames {
        let frame: Vec<u8> = (0..frame_len).map(|b| (b ^ f) as u8).collect();
        if streamed {
            let mut au = host.begin_streamed_frame_at(f as u64, 0, f as u32).unwrap();
            for chunk in frame.chunks(frame_len / 3 + 1) {
                // slice_end=false: the harness exercises the legacy full-FEC-block granularity
                // (its loopback client never advertises the P2 slice wire).
                let wires = host.seal_streamed_chunk(&mut au, chunk, false).unwrap();
                send_wires(&mut host, wires);
            }
            let wires = host.seal_streamed_finish(au).unwrap();
            send_wires(&mut host, wires);
        } else {
            host.submit_frame(&frame, f as u64, 0).unwrap();
        }
        match client.poll_frame() {
            Ok(got) => {
                if got.data == frame {
                    completed += 1;
                }
            }
            Err(PunktfunkError::NoFrame) => {} // unrecoverable at this loss rate
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    (completed, frames)
}

// ---------------------------------------------------------------------------
// PW6: partial delivery under loss — streamed AU vs whole AU
// ---------------------------------------------------------------------------
//
// The question this answers (wave-2 plan PW6, security-review finding 10): a PyroWave client
// enables `set_deliver_partial_frames` unconditionally, so a chunk-aligned AU that loses shards
// is still handed up as blocks-with-holes — one frame of localized blur instead of a freeze. But
// a STREAMED frame is excluded from that when it is UNPINNED: its size lives only on the FINAL
// block's headers (`frame_bytes` is the 0 sentinel until then), and `advance_window` refuses to
// deliver a partial it cannot truncate. So where the whole-AU path delivers blur, a streamed
// frame whose final block is entirely lost delivers NOTHING.
//
// Three legs, because a bare 2 % sweep cannot see the effect (see `partial_sweep`'s note):
//  1. `final_block_probe`  — DETERMINISTIC: drop exactly the frame's last block in both shapes.
//                            Proves the mechanism exists (or does not) without any statistics.
//  2. `partial_sweep`      — RANDOM Bernoulli loss, both shapes, same seed: the delivery rates.
//  3. the stress rows      — the same sweep at higher loss, where the gap becomes measurable.

/// The realistic PyroWave wire geometry: 1500-MTU IPv4 shards, 200 data shards per FEC block,
/// and **FEC pinned OFF** (the Phase-4 recipe — parity would mask exactly the loss under study).
fn partial_config(role: Role) -> Config {
    Config {
        role,
        fec: FecConfig {
            scheme: FecScheme::Gf16,
            fec_percent: 0,
            max_data_per_block: 200,
        },
        shard_payload: 1408,
        max_frame_bytes: 8 * 1024 * 1024,
        loopback_drop_period: 0, // loss is injected here, per packet, so it can be random
    }
}

/// Reproducible xorshift64* — the harness must be re-runnable to the same numbers, and
/// `loopback_drop_period`'s deterministic 1-in-N cannot model independent per-packet loss
/// (it would systematically hit or miss the final block, which is the whole question).
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// True with probability `pct`/10000 (basis points, so 2 % = 200).
    fn hits(&mut self, bp: u32) -> bool {
        (self.next_u64() % 10_000) < bp as u64
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u64() as usize) % (hi - lo).max(1)
    }
}

/// How each source frame ended up at the client.
#[derive(Default, Clone, Copy)]
struct Outcome {
    complete: usize,
    partial: usize,
    nothing: usize,
}

impl Outcome {
    fn total(&self) -> usize {
        self.complete + self.partial + self.nothing
    }
    /// Partial deliveries as a percentage of frames that did NOT arrive complete — "when the
    /// frame was damaged, how often did the user still get a picture?". That ratio, not the raw
    /// count, is what the two wire shapes must be compared on: they damage different numbers of
    /// frames at the same packet-loss rate (streamed adds no parity but does add a final block
    /// whose loss is fatal, and the shapes' block splits differ slightly).
    fn rescue_pct(&self) -> f64 {
        let damaged = self.partial + self.nothing;
        if damaged == 0 {
            return 100.0;
        }
        100.0 * self.partial as f64 / damaged as f64
    }
}

/// How many packets the frame's LAST FEC block occupies, and how many packets the whole AU
/// should seal into. With FEC pinned off there is no parity, so `packetize_each` emits exactly
/// one packet per data shard in block order — the final block is therefore the last `final_k`
/// packets of the batch. Returned together so the caller can ASSERT the packet count and fail
/// loudly if that emission shape ever changes, rather than silently probing the wrong packets.
fn final_block_span(len: usize, shard: usize, per_block: usize) -> (usize, usize) {
    let shards = len.div_ceil(shard);
    let blocks = shards.div_ceil(per_block);
    let final_k = shards - (blocks - 1) * per_block;
    (shards, final_k)
}

/// Drive `frames` AUs through a host→client pair and classify each one. `loss_bp` is the
/// per-packet loss probability in basis points; `final_only` instead forces the frame's LAST
/// block to be dropped wholesale, and nothing else (the deterministic mechanism probe).
///
/// `sizes` gives each frame's AU length. Real PyroWave AUs vary frame to frame under rate
/// control, and the FINAL block's size is what bounds this trap's exposure, so the sweep varies
/// the length across the whole 1..=200-shard range of final-block sizes rather than pinning one.
fn run_partial(
    streamed: bool,
    sizes: &[usize],
    loss_bp: u32,
    final_only: bool,
    seed: u64,
) -> Outcome {
    // Flush frames: `advance_window` only ages a frame out once something NEWER exists and the
    // capture-time fuse has passed (PARTIAL_WINDOW_NS = 30 ms vs a 16.67 ms frame period), so
    // the tail of the run needs successors before its verdicts land.
    const FLUSH: usize = 8;
    const FRAME_NS: u64 = 16_666_667;

    let (h, c) = loopback_pair(0, 0);
    let mut host = Session::new(partial_config(Role::Host), media(), Box::new(h)).unwrap();
    let mut client = Session::new(partial_config(Role::Client), media(), Box::new(c)).unwrap();
    // The PyroWave client's real setting (`client/pump/handshake.rs` turns this on for every
    // CODEC_PYROWAVE session).
    client.set_deliver_partial_frames(true);

    let mut rng = Rng::new(seed);
    // frame_index -> saw a complete delivery
    let mut delivered: std::collections::HashMap<u32, bool> = std::collections::HashMap::new();
    let flags = FLAG_PIC as u32 | FLAG_SOF as u32 | USER_FLAG_CHUNK_ALIGNED;

    let n = sizes.len();
    for f in 0..(n + FLUSH) {
        let len = sizes[f.min(n - 1)];
        // Busy, frame-varying content — never a flat fill (a constant buffer would still
        // reassemble byte-identically, but it makes every debug dump look alike).
        let data: Vec<u8> = (0..len).map(|b| (b.wrapping_mul(31) ^ f) as u8).collect();
        let pts = f as u64 * FRAME_NS;
        let fi = f as u32;

        // Send one sealed batch. `kill_from` is the index at/after which every packet is dropped
        // outright (the deterministic final-block probe); otherwise each packet is lost
        // independently at `loss_bp`.
        let mut send = |host: &mut Session, wires: Vec<Vec<u8>>, kill_from: usize| {
            let refs: Vec<&[u8]> = wires
                .iter()
                .enumerate()
                .filter(|(i, _)| *i < kill_from && !(loss_bp > 0 && rng.hits(loss_bp)))
                .map(|(_, w)| w.as_slice())
                .collect();
            if !refs.is_empty() {
                host.send_sealed(&refs).unwrap();
            }
            drop(refs);
            host.reclaim_wires(wires);
        };

        if streamed {
            let mut au = host.begin_streamed_frame_at(pts, flags, fi).unwrap();
            // Cut at the encoder's chunk granularity (the PW6 `AuChunker` default: 256 KiB
            // rounded down to whole 1408-byte windows = 186 windows).
            for chunk in data.chunks(186 * 1408) {
                let wires = host.seal_streamed_chunk(&mut au, chunk, false).unwrap();
                send(&mut host, wires, usize::MAX);
            }
            let wires = host.seal_streamed_finish(au).unwrap();
            // The finish batch IS the final block — the only one carrying the real totals.
            send(&mut host, wires, if final_only { 0 } else { usize::MAX });
        } else {
            let wires = host.seal_frame_at(&data, pts, flags, fi).unwrap();
            let (shards, final_k) = final_block_span(len, 1408, 200);
            assert_eq!(
                wires.len(),
                shards,
                "FEC is off, so the whole-AU batch must be exactly one packet per data shard — \
                 the final-block probe's index rule depends on it"
            );
            let kill_from = if final_only {
                wires.len() - final_k
            } else {
                usize::MAX
            };
            send(&mut host, wires, kill_from);
        }

        loop {
            match client.poll_frame() {
                Ok(got) => {
                    let e = delivered.entry(got.frame_index).or_insert(false);
                    *e |= got.complete;
                }
                Err(PunktfunkError::NoFrame) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
    }

    let mut out = Outcome::default();
    for f in 0..n {
        match delivered.get(&(f as u32)) {
            Some(true) => out.complete += 1,
            Some(false) => out.partial += 1,
            None => out.nothing += 1,
        }
    }
    out
}

/// AU lengths spanning the full range of FINAL-block sizes (1..=200 shards on top of two full
/// 200-shard blocks) — 564 KB…845 KB, i.e. the 400 Mb/s-at-60fps operating point.
fn varied_sizes(count: usize, seed: u64) -> Vec<usize> {
    let mut rng = Rng::new(seed);
    (0..count)
        .map(|_| rng.range(401 * 1408, 600 * 1408 + 1))
        .collect()
}

fn partial_section() {
    let frames: usize = std::env::var("PW6_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);

    println!("\n\npunktfunk PW6 — partial delivery under loss: STREAMED vs WHOLE AU");
    println!("(chunk-aligned AUs, deliver_partial ON, FEC pinned OFF, shard 1408, 200/block)\n");

    // ---- Leg 1: the mechanism, deterministically -------------------------------------------
    println!("Leg 1 — DETERMINISTIC probe: the frame's LAST block is lost, nothing else.");
    let sizes = varied_sizes(200, 0xC0FFEE);
    for (label, streamed) in [("whole-AU", false), ("streamed", true)] {
        let o = run_partial(streamed, &sizes, 0, true, 1);
        println!(
            "  {label:>8}: complete {:>4}  partial {:>4}  NOTHING {:>4}   (of {})",
            o.complete,
            o.partial,
            o.nothing,
            o.total()
        );
    }
    println!(
        "  → if the streamed row shows NOTHING where whole-AU shows partial, the trap is real."
    );

    // ---- Leg 2 + 3: rates under random loss -------------------------------------------------
    println!("\nLeg 2/3 — RANDOM per-packet loss, same seed and same AU sizes for both shapes.");
    println!("  'rescue' = partials / (partials + nothing): of the frames that arrived DAMAGED,");
    println!("  how many still reached the decoder as blur instead of vanishing.\n");
    println!(
        "{:>7}  {:>9}  {:>26}  {:>26}",
        "loss", "shape", "complete / partial / none", "rescue of damaged"
    );
    println!("{}", "-".repeat(78));
    let sizes = varied_sizes(frames, 0xBEEF);
    for &bp in &[200u32, 1000, 3000, 5000] {
        for (label, streamed) in [("whole-AU", false), ("streamed", true)] {
            let o = run_partial(streamed, &sizes, bp, false, 0x5EED);
            println!(
                "{:>6.1}%  {label:>9}  {:>8} / {:>7} / {:>5}  {:>24.2}%",
                bp as f64 / 100.0,
                o.complete,
                o.partial,
                o.nothing,
                o.rescue_pct()
            );
        }
    }
    println!(
        "\nNote: at 2 % the streamed penalty is bounded by P(final block fully lost) =\n\
         E[0.02^k] over final-block sizes k — ~1e-4 — so the 2 % row is EXPECTED to tie.\n\
         The higher-loss rows are what make the gap (if any) visible; Leg 1 proves the mechanism."
    );
}

fn main() {
    let frames = 50;
    let frame_len = 100_000; // ~98 shards across 2 FEC blocks
    let periods = [0u32, 32, 16, 8, 6, 4, 3, 2];

    println!("punktfunk loss-harness — 25% FEC, {frames} frames of {frame_len} bytes");
    println!("(GF8 = GameStream-sized blocks, GF16 = native, strm = streamed-AU wire)\n");
    println!(
        "{:>10}  {:>9}  {:>14}  {:>14}  {:>14}",
        "drop 1/N", "~loss %", "GF8 recovered", "GF16 recovered", "GF16 strm"
    );
    println!("{}", "-".repeat(72));
    for &p in &periods {
        let loss = if p == 0 { 0.0 } else { 100.0 / p as f64 };
        let (g8, n) = run(FecScheme::Gf8, p, frames, frame_len, false);
        let (g16, _) = run(FecScheme::Gf16, p, frames, frame_len, false);
        let (g16s, _) = run(FecScheme::Gf16, p, frames, frame_len, true);
        let label = if p == 0 {
            "none".to_string()
        } else {
            format!("1/{p}")
        };
        println!(
            "{label:>10}  {loss:>8.1}%  {:>11}/{n}  {:>11}/{n}  {:>11}/{n}",
            g8, g16, g16s
        );
    }
    println!("\nNote: recovery drops off once per-block loss exceeds the 25% recovery budget.");

    partial_section();
}
