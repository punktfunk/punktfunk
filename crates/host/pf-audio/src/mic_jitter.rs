//! Backend-agnostic de-jitter for the client-mic uplink.
//!
//! The pump feeds every received `0xCB` frame (datagram sequence in [`MicFrame`])
//! through [`MicDejitter`]: a two-frame reorder window in front of Opus, libopus
//! concealment on true sequence gaps (so a lost datagram does not drain the
//! backend ring into silence + re-prime), and inter-arrival jitter into the
//! adaptive target depth the backend rings prime against.
//!
//! Same wrapping-sequence gap rules as the client downlink's `AudioGapTracker`
//! (punktfunk-core `audio/recovery.rs`). Downlink jitter rings own their depth; uplink
//! rings live in the backends and take their target from here.

use super::MicFrame;
use std::time::{Duration, Instant};

/// Playback-ordered work. `Conceal` is one libopus PLC frame
/// (`decode_float(&[], …)`), sized to the last real frame.
pub(super) enum Deliver {
    Frame(MicFrame),
    Conceal,
}

/// Cap on PLC frames for one gap. Five ≈ 100 ms at 20 ms Opus. A count, not
/// a time: the client never announces frame length, so we only measure it
/// from consecutive pts deltas (`frame_ms`). Past this, the ring re-primes.
const MAX_CONCEAL_FRAMES: u32 = 5;

/// How long an out-of-order frame may wait for its predecessor: ~one 20 ms
/// frame plus scheduling slack. With the incoming frame that is a two-frame
/// window; anything needing more is treated as loss.
const HOLD_MAX: Duration = Duration::from_millis(30);

/// A frame further behind what already played than a reorder or duplicate explains: the
/// client restarted its capture at seq 0. That is a new stream, not late audio.
const RESTART_BEHIND: u32 = 8;

/// Arrival gaps above this are talk-spurt pauses (DTX, mute), not network jitter — folding
/// them into the estimate would balloon the target after every pause.
const PAUSE_GAP: Duration = Duration::from_millis(250);

/// Sliding window: [`BUCKETS`] × [`BUCKET_LEN`] of per-bucket max gap.
/// Long enough to represent a two-frame ~42 ms burst cadence; short enough
/// that a one-off spike ages out in a few seconds.
const BUCKETS: usize = 4;
const BUCKET_LEN: Duration = Duration::from_millis(1000);

/// Target before a full [`WARMUP`] of evidence: historical 48 ms prime minus
/// the one consumer quantum backends add. Floor until measured — an unmeasured
/// client might still burst two frames every ~42 ms.
const DEFAULT_TARGET_MS: u32 = 40;
/// Headroom over the measured worst burst gap.
const TARGET_PAD_MS: u32 = 5;
/// How long the estimator distrusts its young window and floors at default.
const WARMUP: Duration = Duration::from_secs(1);

/// Reset-on-read snapshot for the pump's telemetry. Counters are the window
/// since last read; `cadence_ms`/`frame_ms` are gauges (EWMAs).
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct DejitterStats {
    pub seq_gaps: u64,
    pub concealed: u64,
    pub reorders: u64,
    pub late_drops: u64,
    /// EWMA inter-arrival gap — the uplink's burst cadence as seen by the pump.
    pub cadence_ms: f32,
    /// Nominal frame duration from consecutive frames' pts deltas (what the client encodes).
    pub frame_ms: f32,
}

pub(super) struct MicDejitter {
    /// Sequence the uplink owes next; `None` before the first frame / after
    /// [`reset_stream`](Self::reset_stream).
    expected: Option<u32>,
    /// Reorder window: at most one newer frame parked ≤ [`HOLD_MAX`].
    held: Option<(Instant, MicFrame)>,
    first_arrival: Option<Instant>,
    last_arrival: Option<Instant>,
    bucket_start: Option<Instant>,
    bucket_idx: usize,
    gap_max: [f32; BUCKETS],
    ewma_gap_ms: f32,
    /// Newest ingested `(seq, pts_ns)` — a pts delta over one seq step is
    /// the client's true frame duration.
    last_pts: Option<(u32, u64)>,
    frame_ms: f32,
    seq_gaps: u64,
    concealed: u64,
    reorders: u64,
    late_drops: u64,
}

impl MicDejitter {
    pub(super) fn new() -> Self {
        MicDejitter {
            expected: None,
            held: None,
            first_arrival: None,
            last_arrival: None,
            bucket_start: None,
            bucket_idx: 0,
            gap_max: [0.0; BUCKETS],
            ewma_gap_ms: 0.0,
            last_pts: None,
            frame_ms: 0.0,
            seq_gaps: 0,
            concealed: 0,
            reorders: 0,
            late_drops: 0,
        }
    }

    pub(super) fn ingest(&mut self, now: Instant, frame: MicFrame, out: &mut Vec<Deliver>) {
        self.record_arrival(now, frame.seq, frame.pts_ns);
        self.accept(now, frame, out);
    }

    fn accept(&mut self, now: Instant, frame: MicFrame, out: &mut Vec<Deliver>) {
        let Some(expected) = self.expected else {
            self.expected = Some(frame.seq.wrapping_add(1));
            out.push(Deliver::Frame(frame));
            return;
        };
        let delta = frame.seq.wrapping_sub(expected);
        if delta == 0 {
            self.expected = Some(expected.wrapping_add(1));
            out.push(Deliver::Frame(frame));
            self.release_held_in_order(out);
        } else if delta > u32::MAX / 2 {
            // wrapping_sub > MAX/2: duplicate or older than already-played.
            if expected.wrapping_sub(frame.seq) > RESTART_BEHIND {
                self.reset_stream();
                self.accept(now, frame, out);
            } else {
                self.late_drops += 1;
            }
        } else if let Some(held_seq) = self.held.as_ref().map(|(_, h)| h.seq) {
            if held_seq == frame.seq {
                self.late_drops += 1; // duplicate of the held frame
                return;
            }
            // Two newer-than-expected frames in hand — waiting longer cannot
            // fill the gap. Play in order, concealing what is genuinely missing.
            let (_, held) = self.held.take().expect("checked above");
            let (first, second) = if frame.seq.wrapping_sub(held.seq) > u32::MAX / 2 {
                (frame, held)
            } else {
                (held, frame)
            };
            self.give_up_and_play(first, out);
            self.accept(now, second, out); // depth ≤ 1: `held` is None here
        } else {
            self.held = Some((now, frame));
        }
    }

    /// If the held frame's predecessor just played, emit it (reorder repaired).
    fn release_held_in_order(&mut self, out: &mut Vec<Deliver>) {
        if let Some((_, h)) = &self.held {
            if Some(h.seq) == self.expected {
                let (_, h) = self.held.take().expect("checked above");
                self.expected = Some(h.seq.wrapping_add(1));
                self.reorders += 1;
                out.push(Deliver::Frame(h));
            }
        }
    }

    /// Stop waiting for `frame`'s predecessors: conceal the gap (capped) and play it.
    fn give_up_and_play(&mut self, frame: MicFrame, out: &mut Vec<Deliver>) {
        let gap = frame
            .seq
            .wrapping_sub(self.expected.expect("callers checked"));
        if gap > 0 {
            self.seq_gaps += 1;
            let conceal = gap.min(MAX_CONCEAL_FRAMES);
            self.concealed += u64::from(conceal);
            for _ in 0..conceal {
                out.push(Deliver::Conceal);
            }
        }
        self.expected = Some(frame.seq.wrapping_add(1));
        out.push(Deliver::Frame(frame));
    }

    /// When the current hold (if any) must be given up — the pump's next wake.
    pub(super) fn hold_deadline(&self) -> Option<Instant> {
        self.held.as_ref().map(|(t, _)| *t + HOLD_MAX)
    }

    /// Give up an expired hold (predecessor never came): conceal + play.
    pub(super) fn flush_expired_hold(&mut self, now: Instant, out: &mut Vec<Deliver>) {
        if self.hold_deadline().is_some_and(|d| now >= d) {
            let (_, h) = self.held.take().expect("deadline implies held");
            self.give_up_and_play(h, out);
        }
    }

    /// Uplink pause / backend reopen: parked frames predate the gap; the next
    /// frame restarts the chain. A pause is not loss — do not conceal it.
    pub(super) fn reset_stream(&mut self) {
        self.expected = None;
        self.held = None;
        self.last_pts = None;
    }

    fn record_arrival(&mut self, now: Instant, seq: u32, pts_ns: u64) {
        if let Some(last) = self.last_arrival {
            let gap = now.duration_since(last);
            if gap <= PAUSE_GAP {
                let ms = gap.as_secs_f32() * 1000.0;
                self.ewma_gap_ms = if self.ewma_gap_ms == 0.0 {
                    ms
                } else {
                    self.ewma_gap_ms * 0.9 + ms * 0.1
                };
                self.rotate_buckets(now);
                self.gap_max[self.bucket_idx] = self.gap_max[self.bucket_idx].max(ms);
            }
        }
        self.first_arrival.get_or_insert(now);
        self.last_arrival = Some(now);
        if let Some((last_seq, last_pts)) = self.last_pts {
            if seq == last_seq.wrapping_add(1) && pts_ns > last_pts {
                let d = (pts_ns - last_pts) as f32 / 1_000_000.0;
                if (1.0..=120.0).contains(&d) {
                    self.frame_ms = if self.frame_ms == 0.0 {
                        d
                    } else {
                        self.frame_ms * 0.9 + d * 0.1
                    };
                }
            }
        }
        self.last_pts = Some((seq, pts_ns));
    }

    fn rotate_buckets(&mut self, now: Instant) {
        let Some(mut start) = self.bucket_start else {
            self.bucket_start = Some(now);
            return;
        };
        let mut advanced = 0;
        while now.duration_since(start) >= BUCKET_LEN {
            self.bucket_idx = (self.bucket_idx + 1) % BUCKETS;
            self.gap_max[self.bucket_idx] = 0.0;
            start += BUCKET_LEN;
            advanced += 1;
            if advanced >= BUCKETS {
                // Long pause aged the whole window out — snap it to now.
                self.gap_max = [0.0; BUCKETS];
                start = now;
                break;
            }
        }
        self.bucket_start = Some(start);
    }

    /// Jitter component of the backend ring's prime depth, in ms: worst burst
    /// gap in the window plus a pad, clamped to [10, 60]. Backends add one
    /// consumer quantum ([`VirtualMic::set_target_depth`](super::VirtualMic::set_target_depth)).
    pub(super) fn target_ms(&self, now: Instant) -> u32 {
        let measured =
            self.gap_max.iter().fold(0f32, |a, &b| a.max(b)).ceil() as u32 + TARGET_PAD_MS;
        let warm = self
            .first_arrival
            .is_some_and(|t| now.duration_since(t) >= WARMUP);
        let t = if warm {
            measured
        } else {
            measured.max(DEFAULT_TARGET_MS)
        };
        t.clamp(10, 60)
    }

    pub(super) fn take_stats(&mut self) -> DejitterStats {
        let s = DejitterStats {
            seq_gaps: self.seq_gaps,
            concealed: self.concealed,
            reorders: self.reorders,
            late_drops: self.late_drops,
            cadence_ms: self.ewma_gap_ms,
            frame_ms: self.frame_ms,
        };
        self.seq_gaps = 0;
        self.concealed = 0;
        self.reorders = 0;
        self.late_drops = 0;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(seq: u32) -> MicFrame {
        MicFrame {
            source: 1,
            seq,
            pts_ns: u64::from(seq) * 20_000_000,
            opus: vec![1],
        }
    }

    /// A client that restarts its capture starts again at seq 0. That is a new stream, played at
    /// once; a frame a couple behind is still a late duplicate.
    #[test]
    fn a_restarted_uplink_plays_instead_of_reading_as_late() {
        let t = Instant::now();
        let mut dj = MicDejitter::new();
        let mut out = Vec::new();
        for s in 0..500 {
            dj.ingest(t, f(s), &mut out);
        }
        out.clear();
        dj.ingest(t, f(498), &mut out);
        assert!(out.is_empty(), "two behind is a duplicate");
        for s in 0..3 {
            dj.ingest(t, f(s), &mut out);
        }
        assert_eq!(seqs(&out), vec![0, 1, 2]);
    }

    /// Delivery shape: frame seqs as-is, concealment as -1.
    fn seqs(out: &[Deliver]) -> Vec<i64> {
        out.iter()
            .map(|d| match d {
                Deliver::Frame(fr) => i64::from(fr.seq),
                Deliver::Conceal => -1,
            })
            .collect()
    }

    #[test]
    fn in_order_passthrough() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        for s in 5..8 {
            dj.ingest(t, f(s), &mut out);
        }
        assert_eq!(seqs(&out), [5, 6, 7]);
        let st = dj.take_stats();
        assert_eq!(
            (st.seq_gaps, st.concealed, st.reorders, st.late_drops),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn swapped_pair_is_repaired_not_concealed() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(5), &mut out);
        dj.ingest(t, f(7), &mut out);
        assert_eq!(seqs(&out), [5]);
        dj.ingest(t, f(6), &mut out);
        assert_eq!(seqs(&out), [5, 6, 7]);
        let st = dj.take_stats();
        assert_eq!(st.reorders, 1);
        assert_eq!(st.concealed, 0);
    }

    #[test]
    fn loss_is_concealed_when_the_window_moves_on() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(5), &mut out);
        dj.ingest(t, f(7), &mut out);
        dj.ingest(t, f(8), &mut out);
        assert_eq!(seqs(&out), [5, -1, 7, 8]);
        let st = dj.take_stats();
        assert_eq!(st.seq_gaps, 1);
        assert_eq!(st.concealed, 1);
    }

    #[test]
    fn expired_hold_flushes_with_concealment() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(5), &mut out);
        dj.ingest(t, f(7), &mut out);
        assert!(dj.hold_deadline().is_some());
        dj.flush_expired_hold(t + HOLD_MAX, &mut out);
        assert_eq!(seqs(&out), [5, -1, 7]);
        assert!(dj.hold_deadline().is_none());
    }

    #[test]
    fn big_gap_conceal_is_capped() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(5), &mut out);
        dj.ingest(t, f(100), &mut out);
        dj.flush_expired_hold(t + HOLD_MAX, &mut out);
        let conceals = out.iter().filter(|d| matches!(d, Deliver::Conceal)).count();
        assert_eq!(conceals, MAX_CONCEAL_FRAMES as usize);
        assert_eq!(dj.take_stats().seq_gaps, 1);
    }

    #[test]
    fn duplicates_and_stale_late_frames_drop() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(5), &mut out);
        dj.ingest(t, f(6), &mut out);
        dj.ingest(t, f(6), &mut out);
        dj.ingest(t, f(3), &mut out);
        dj.ingest(t, f(8), &mut out);
        dj.ingest(t, f(8), &mut out);
        assert_eq!(seqs(&out), [5, 6]);
        assert_eq!(dj.take_stats().late_drops, 3);
    }

    #[test]
    fn wraparound_is_a_reorder_not_an_eternity() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(u32::MAX), &mut out);
        dj.ingest(t, f(0), &mut out);
        assert_eq!(seqs(&out), [i64::from(u32::MAX), 0]);
        dj.ingest(t, f(u32::MAX), &mut out); // pre-wrap replay: late, not a 2^31 gap
        assert_eq!(dj.take_stats().late_drops, 1);
    }

    #[test]
    fn reset_restarts_the_chain_without_concealing() {
        let mut dj = MicDejitter::new();
        let t = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t, f(5), &mut out);
        dj.reset_stream();
        dj.ingest(t, f(500), &mut out); // a pause is not loss
        assert_eq!(seqs(&out), [5, 500]);
        assert_eq!(dj.take_stats().concealed, 0);
    }

    #[test]
    fn bursty_uplink_grows_the_target_modern_uplink_shrinks_it() {
        // Two frames back-to-back every ~42 ms.
        let mut dj = MicDejitter::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let mut now = t0;
        let mut seq = 0u32;
        for burst in 0u64..50 {
            now = t0 + Duration::from_millis(burst * 42);
            for _ in 0..2 {
                dj.ingest(now, f(seq), &mut out);
                seq += 1;
            }
        }
        let target = dj.target_ms(now);
        assert!((42..=60).contains(&target), "bursty target {target}");

        // Steady 10 ms cadence: past warmup the target follows the small gaps down.
        let mut dj = MicDejitter::new();
        let mut now = t0;
        for i in 0u64..200 {
            now = t0 + Duration::from_millis(i * 10);
            dj.ingest(now, f(i as u32), &mut out);
        }
        let target = dj.target_ms(now);
        assert!((10..=20).contains(&target), "steady target {target}");
    }

    #[test]
    fn warmup_holds_the_crackle_safe_default() {
        let mut dj = MicDejitter::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        dj.ingest(t0, f(0), &mut out);
        dj.ingest(t0 + Duration::from_millis(10), f(1), &mut out);
        // 10 ms of evidence must not shrink the target yet — the client might be bursty.
        assert_eq!(
            dj.target_ms(t0 + Duration::from_millis(20)),
            DEFAULT_TARGET_MS
        );
    }
}
