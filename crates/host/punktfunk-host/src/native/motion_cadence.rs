//! Per-pad inter-arrival histogram for `RichInput::Motion`.
//!
//! Gyro aim integrates angular velocity over time, so clumped samples of a 250 Hz
//! feed feel jumpy even when the values are fine. Always on: one `Instant`
//! subtraction and one array increment per sample. Percentiles come from a fixed
//! log2 histogram (no allocation, no per-window sort); a reported value is the
//! bucket's exclusive upper bound, hence the `_le` suffixes.
//!
//! The array is keyed by wire pad. Two pads sharing one accumulator would measure
//! each other's arrivals. Gaps ≥ [`STALL_GAP`] are stalls, not cadence — they are
//! tallied separately so an interruption does not move the percentiles.
//!
//! Tests pin the per-pad split, the stall split, and the bucket edges.

use punktfunk_core::input::MAX_PADS;
use std::time::{Duration, Instant};

/// 22 log2 buckets reach ~2.1 s (`2^21` µs). Past that the feed stopped, not
/// that it is uneven; the last bucket saturates.
const BUCKETS: usize = 22;

/// 500 ms is a stop/resume, not jitter (~125 samples at 250 Hz). Folding it
/// into `hist` would drag every percentile toward the stall.
const STALL_GAP: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Default)]
struct PadCadence {
    last: Option<Instant>,
    hist: [u32; BUCKETS],
    /// Gaps in `hist`. `samples = gaps + 1` when the pad sent anything.
    gaps: u64,
    /// Exact max below [`STALL_GAP`]. A log2 bucket is a factor-of-two answer.
    max_us: u32,
    stalls: u32,
}

impl PadCadence {
    /// `true` when the gap was a stall, for the session-total counter — the threshold stays
    /// defined here, not at the call site.
    fn record(&mut self, now: Instant) -> bool {
        let Some(prev) = self.last.replace(now) else {
            return false;
        };
        let gap = now.saturating_duration_since(prev);
        if gap >= STALL_GAP {
            self.stalls = self.stalls.saturating_add(1);
            return true;
        }
        let us = gap.as_micros() as u32;
        self.max_us = self.max_us.max(us);
        self.gaps = self.gaps.saturating_add(1);
        self.hist[bucket(us)] += 1;
        false
    }

    /// Exclusive upper bound (µs) of the bucket holding the `q`-quantile.
    fn percentile_us_le(&self, q: f64) -> Option<u32> {
        if self.gaps == 0 {
            return None;
        }
        // 1-based rank: q=0.5 over 4 gaps is the 2nd.
        let want = ((q * self.gaps as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (k, n) in self.hist.iter().enumerate() {
            seen += *n as u64;
            if seen >= want {
                return Some(bucket_upper_us(k));
            }
        }
        Some(bucket_upper_us(BUCKETS - 1))
    }
}

/// `32 - leading_zeros` is `floor(log2(us)) + 1` for `us > 0`; cap at [`BUCKETS`].
fn bucket(us: u32) -> usize {
    if us == 0 {
        return 0;
    }
    ((32 - us.leading_zeros()) as usize).min(BUCKETS - 1)
}

fn bucket_upper_us(k: usize) -> u32 {
    if k == 0 {
        return 0;
    }
    1u32.checked_shl(k as u32).unwrap_or(u32::MAX)
}

/// Per-pad cadence. Index is the wire pad.
pub(super) struct MotionCadence {
    pads: [PadCadence; MAX_PADS],
}

impl MotionCadence {
    pub(super) fn new() -> MotionCadence {
        MotionCadence {
            pads: [PadCadence::default(); MAX_PADS],
        }
    }

    /// `true` when this arrival closed a stall — what the session summary counts.
    pub(super) fn record(&mut self, pad: u8, now: Instant) -> bool {
        self.pads
            .get_mut(pad as usize)
            .is_some_and(|p| p.record(now))
    }

    /// One `info` line per pad that carried motion. Call once, at session end.
    pub(super) fn log_summary(&self) {
        for (i, p) in self.pads.iter().enumerate() {
            if p.gaps == 0 && p.stalls == 0 {
                continue;
            }
            tracing::info!(
                pad = i,
                samples = p.gaps + 1,
                // 0 = no gap was recorded (one sample then a stall).
                gap_p50_us_le = p.percentile_us_le(0.5).unwrap_or(0),
                gap_p95_us_le = p.percentile_us_le(0.95).unwrap_or(0),
                gap_max_us = p.max_us,
                stalls = p.stalls,
                "motion cadence for the session (client gyro inter-arrival; percentiles are \
                 log2-bucket upper bounds, stalls are gaps ≥ 500 ms)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// Pad 0 at 4 ms and pad 1 at 40 ms, interleaved. A shared accumulator would
    /// report ~2 ms for both.
    #[test]
    fn two_pads_do_not_corrupt_each_others_cadence() {
        let t0 = Instant::now();
        let mut c = MotionCadence::new();
        for i in 0..100u64 {
            c.record(0, at(t0, i * 4));
            if i % 10 == 0 {
                c.record(1, at(t0, i * 4));
            }
        }
        // 4 ms → bucket (2048, 4096] µs `_le`; 40 ms → (32768, 65536].
        assert_eq!(c.pads[0].percentile_us_le(0.5), Some(4096));
        assert_eq!(c.pads[1].percentile_us_le(0.5), Some(65536));
        assert_eq!(c.pads[0].gaps, 99);
        assert_eq!(c.pads[1].gaps, 9);
    }

    #[test]
    fn a_silent_pad_records_nothing() {
        let c = MotionCadence::new();
        assert_eq!(c.pads[3].gaps, 0);
        assert_eq!(c.pads[3].percentile_us_le(0.5), None);
        // One sample is not a gap.
        let mut c = MotionCadence::new();
        c.record(3, Instant::now());
        assert_eq!(c.pads[3].gaps, 0);
    }

    /// A multi-second silence is a stall. Folding it into `hist` would make a 4 ms
    /// feed report as a multi-second one. The verdict is also what `record` returns,
    /// which is what the session summary's stall count is built from.
    #[test]
    fn a_stall_is_counted_separately_from_the_cadence() {
        let t0 = Instant::now();
        let mut c = MotionCadence::new();
        for i in 0..50u64 {
            assert!(!c.record(0, at(t0, i * 4)), "a 4 ms gap is cadence");
        }
        assert!(c.record(0, at(t0, 5_000)), "5 s of silence is a stall");
        for i in 0..50u64 {
            assert!(!c.record(0, at(t0, 5_000 + i * 4)));
        }
        assert_eq!(c.pads[0].stalls, 1);
        assert_eq!(
            c.pads[0].percentile_us_le(0.95),
            Some(4096),
            "the stall leaked in"
        );
        assert!(c.pads[0].max_us < STALL_GAP.as_micros() as u32);
    }

    /// Nine 4 ms gaps then one 40 ms clump: p50 stays on the body, p95 on the clump.
    #[test]
    fn percentiles_separate_the_body_from_the_tail() {
        let t0 = Instant::now();
        let mut c = MotionCadence::new();
        let mut t = 0u64;
        for i in 0..100u64 {
            t += if i % 10 == 9 { 40 } else { 4 };
            c.record(0, at(t0, t));
        }
        // 90 gaps of 4 ms, 10 of 40 ms.
        assert_eq!(c.pads[0].percentile_us_le(0.5), Some(4096));
        assert_eq!(c.pads[0].percentile_us_le(0.95), Some(65536));
        assert!((39_000..=41_000).contains(&c.pads[0].max_us));
    }

    /// Bucket `k` holds `[2^(k-1), 2^k)` µs and reports `2^k`.
    #[test]
    fn bucket_edges() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 1);
        assert_eq!(bucket(2), 2);
        assert_eq!(bucket(3), 2);
        assert_eq!(bucket(4), 3);
        assert_eq!(bucket_upper_us(0), 0);
        assert_eq!(bucket_upper_us(1), 2);
        assert_eq!(bucket_upper_us(12), 4096); // 4 ms lands here
        assert_eq!(bucket(u32::MAX), BUCKETS - 1); // saturates rather than wrapping
    }
}
