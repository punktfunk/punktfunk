//! Wire `pts_ns` for Linux capture: compositor header vs delivery stamp.
//!
//! PipeWire stamps `spa_meta_header.pts` in the graph's `CLOCK_MONOTONIC` domain.
//! The wire and the client's plausibility gate speak realtime-since-epoch.
//! A delivery stamp is `SystemTime::now` in the process callback — arrival, not
//! production — so delivery jitter is in the timestamps the client plays back.
//!
//! [`wire_pts`] rebases a header stamp into realtime and falls back to delivery
//! when the header is missing or further than `PLAUSIBLE_NS` from the delivery
//! instant. [`PtsProvenance`] compares both clocks' interval MADs over a window
//! so an irregular compositor is not mistaken for a clean header.
//!
//! Time is passed in; this module never reads a clock. Tests pin the rebase,
//! the fallback, and the MAD comparison.

/// 4096 ≈ 34 s at 120 Hz, covering a 30 s report window. Sized once so the
/// PipeWire loop thread never reallocates.
const MAX_SAMPLES: usize = 4096;

/// 50 ms ≈ 6 frames at 120 Hz. A rebased header further than this is the wrong
/// clock domain or a stale/empty header; fall back rather than stamp the stream
/// from it.
const PLAUSIBLE_NS: i64 = 50_000_000;

pub(crate) struct WirePts {
    pub(crate) pts_ns: u64,
    pub(crate) from_header: bool,
}

/// Rebase `hdr_pts_ns` (`spa_meta_header.pts`, graph `CLOCK_MONOTONIC`) into
/// realtime-since-epoch via `rt_minus_mono_ns`. Missing or implausible headers
/// yield `delivery_ns` so a producer that never fills the header is unchanged.
pub(crate) fn wire_pts(
    hdr_pts_ns: Option<i64>,
    delivery_ns: u64,
    rt_minus_mono_ns: i64,
) -> WirePts {
    let fallback = WirePts {
        pts_ns: delivery_ns,
        from_header: false,
    };
    // Producers write 0 (or leave it negative) when they have no stamp.
    let Some(hdr) = hdr_pts_ns.filter(|&p| p > 0) else {
        return fallback;
    };
    let rebased = hdr.saturating_add(rt_minus_mono_ns);
    if rebased <= 0 || (rebased - delivery_ns as i64).abs() >= PLAUSIBLE_NS {
        return fallback;
    }
    WirePts {
        pts_ns: rebased as u64,
        from_header: true,
    }
}

#[derive(Default)]
pub(crate) struct PtsProvenance {
    frames: u64,
    with_hdr: u64,
    /// If the header MAD is not tighter than delivery, no stamp swap helps.
    hdr_intervals: Vec<i64>,
    delivery_intervals: Vec<i64>,
    /// `hdr − delivery` per frame. Huge and roughly constant (two clock origins);
    /// variance means the header is not a per-frame stamp.
    offsets: Vec<i64>,
    prev_hdr: Option<i64>,
    prev_delivery: Option<u64>,
    pub(crate) implausible: u64,
}

pub(crate) struct PtsReport {
    pub(crate) frames: u64,
    pub(crate) with_hdr: u64,
    /// A MAD over eight samples and one over thousands deserve different belief.
    pub(crate) samples: u64,
    /// Empirical period. A skipped tick is what a negotiated refresh would
    /// mis-score as jitter.
    pub(crate) period_us: i64,
    /// Ticks the producer never delivered, from the intervals that swallowed
    /// them. `period_us` is a median and `*_mad_us` a median deviation, so both
    /// are blind to a hole by construction: a producer that SKIPS and one that
    /// runs SLOW both read as a low `frames`. This separates them — skipping
    /// holds the period and raises this, running slow raises `period_us`.
    pub(crate) skipped: u64,
    /// Own-median MAD, µs. A shared centre folds period-estimation error and any
    /// genuine rate difference into "jitter".
    pub(crate) hdr_mad_us: i64,
    pub(crate) delivery_mad_us: i64,
    pub(crate) offset_p50_ms: i64,
    pub(crate) implausible: u64,
}

impl PtsProvenance {
    pub(crate) fn new() -> PtsProvenance {
        PtsProvenance {
            hdr_intervals: Vec::with_capacity(MAX_SAMPLES),
            delivery_intervals: Vec::with_capacity(MAX_SAMPLES),
            offsets: Vec::with_capacity(MAX_SAMPLES),
            ..Default::default()
        }
    }

    /// A missing header clears the header interval chain so a gap is not scored
    /// as one clean long interval.
    pub(crate) fn observe(&mut self, hdr_pts_ns: Option<i64>, delivery_ns: u64) {
        self.frames += 1;
        if let Some(prev) = self.prev_delivery {
            push_capped(
                &mut self.delivery_intervals,
                delivery_ns as i64 - prev as i64,
            );
        }
        self.prev_delivery = Some(delivery_ns);

        let Some(hdr) = hdr_pts_ns.filter(|&p| p > 0) else {
            self.prev_hdr = None;
            return;
        };
        self.with_hdr += 1;
        push_capped(&mut self.offsets, hdr - delivery_ns as i64);
        if let Some(prev) = self.prev_hdr {
            push_capped(&mut self.hdr_intervals, hdr - prev);
        }
        self.prev_hdr = Some(hdr);
    }

    pub(crate) fn report(&mut self) -> Option<PtsReport> {
        if self.delivery_intervals.len() < 8 {
            return None;
        }
        // Both before `mad`, which rewrites the intervals into deviations.
        let period_ns = median(&mut self.delivery_intervals);
        let skipped = skipped_ticks(&self.delivery_intervals, period_ns);
        Some(PtsReport {
            frames: self.frames,
            with_hdr: self.with_hdr,
            samples: self.delivery_intervals.len() as u64,
            period_us: period_ns / 1_000,
            skipped,
            hdr_mad_us: mad(&mut self.hdr_intervals) / 1_000,
            delivery_mad_us: mad(&mut self.delivery_intervals) / 1_000,
            offset_p50_ms: median(&mut self.offsets) / 1_000_000,
            implausible: self.implausible,
        })
    }

    /// Previous stamps stay so the first interval of the new window is a real
    /// measurement rather than a hole.
    pub(crate) fn reset_window(&mut self) {
        self.frames = 0;
        self.with_hdr = 0;
        self.implausible = 0;
        self.hdr_intervals.clear();
        self.delivery_intervals.clear();
        self.offsets.clear();
    }
}

fn push_capped(v: &mut Vec<i64>, x: i64) {
    if v.len() < MAX_SAMPLES {
        v.push(x);
    }
}

/// Empty is 0 so a log line stays parseable; the caller has already declined
/// a window this thin.
fn median(v: &mut [i64]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// Whole ticks the gaps swallowed, each interval rounded to nearest periods.
/// Ordinary jitter rounds to one period and counts nothing.
fn skipped_ticks(intervals: &[i64], period_ns: i64) -> u64 {
    if period_ns <= 0 {
        return 0;
    }
    intervals
        .iter()
        .map(|&d| (d.max(0).saturating_add(period_ns / 2) / period_ns - 1).max(0) as u64)
        .sum()
}

/// MAD about the series' own median. A mean lets a skipped tick dominate; a
/// fixed nominal mis-scores it as jitter.
fn mad(v: &mut [i64]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    let centre = median(v);
    for x in v.iter_mut() {
        *x = (*x - centre).abs();
    }
    median(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ~3.5 days of monotonic uptime vs a unix-epoch realtime origin — the domain
    /// gap the rebase closes. The plausibility gate must not treat that gap as a
    /// bad stamp.
    const MONO_BASE: i64 = 300_000 * 1_000_000_000;
    const RT_BASE: u64 = 1_786_000_000 * 1_000_000_000;
    const RT_MINUS_MONO: i64 = RT_BASE as i64 - MONO_BASE;

    #[test]
    fn a_rebased_compositor_stamp_is_used_and_a_stale_one_is_not() {
        let w = wire_pts(
            Some(MONO_BASE + 1_000_000),
            RT_BASE + 1_200_000,
            RT_MINUS_MONO,
        );
        assert!(w.from_header);
        assert_eq!(
            w.pts_ns,
            RT_BASE + 1_000_000,
            "the compositor's own instant"
        );

        let w = wire_pts(Some(MONO_BASE - 500_000_000), RT_BASE, RT_MINUS_MONO);
        assert!(!w.from_header);
        assert_eq!(w.pts_ns, RT_BASE);

        assert!(!wire_pts(Some(0), RT_BASE, RT_MINUS_MONO).from_header);
        assert_eq!(wire_pts(None, RT_BASE, RT_MINUS_MONO).pts_ns, RT_BASE);
    }

    #[test]
    fn a_raw_monotonic_stamp_never_reaches_the_wire() {
        let w = wire_pts(Some(MONO_BASE), RT_BASE, 0); // rebase forgotten
        assert!(!w.from_header);
        assert_eq!(w.pts_ns, RT_BASE);
    }

    const PERIOD: i64 = 8_333_333; // 120 Hz

    /// Deterministic LCG in ±spread around zero. A short repeating cycle makes
    /// the interval median land on a jitter peak instead of the period.
    struct Lcg(u64);
    impl Lcg {
        fn noise(&mut self, spread_ns: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as i64 % (2 * spread_ns)) - spread_ns
        }
    }

    #[test]
    fn a_clean_producer_behind_a_jittery_delivery_is_visible() {
        let mut p = PtsProvenance::new();
        let mut rng = Lcg(7);
        for i in 0..600i64 {
            let hdr = MONO_BASE + i * PERIOD;
            let delivery = (RT_BASE as i64 + i * PERIOD + rng.noise(3_000_000)) as u64;
            p.observe(Some(hdr), delivery);
        }
        let r = p.report().expect("600 frames is plenty");
        assert_eq!(r.frames, 600);
        assert_eq!(r.with_hdr, 600);
        assert!(
            (r.period_us - PERIOD / 1_000).abs() <= 100,
            "the empirical period must find the grid, got {} us",
            r.period_us
        );
        assert_eq!(r.hdr_mad_us, 0, "an exact producer has no deviation");
        assert!(
            r.delivery_mad_us > 1_000,
            "delivery jitter of milliseconds must show as milliseconds, got {} us",
            r.delivery_mad_us
        );
        let origins_ms = (MONO_BASE - RT_BASE as i64) / 1_000_000;
        assert!(
            (r.offset_p50_ms - origins_ms).abs() <= 5,
            "offset p50 {} vs clock origins {origins_ms}",
            r.offset_p50_ms
        );
    }

    #[test]
    fn an_irregular_producer_is_not_flattered() {
        let mut p = PtsProvenance::new();
        let mut rng = Lcg(11);
        for i in 0..600i64 {
            let wobble = rng.noise(3_000_000);
            p.observe(
                Some(MONO_BASE + i * PERIOD + wobble),
                (RT_BASE as i64 + i * PERIOD + wobble) as u64,
            );
        }
        let r = p.report().unwrap();
        assert_eq!(
            r.hdr_mad_us, r.delivery_mad_us,
            "an irregular producer must look exactly as bad through either clock"
        );
        assert!(
            r.hdr_mad_us > 1_000,
            "…and both must show the wobble, got {} us",
            r.hdr_mad_us
        );
    }

    /// The issue-912 field question: a 30 s window short of its frame count is
    /// either a producer skipping ticks or one running slow, and the median
    /// period cannot tell them apart. `skipped` must.
    #[test]
    fn a_skipping_producer_is_separable_from_a_slow_one() {
        let mut skip = PtsProvenance::new();
        let mut t = RT_BASE as i64;
        for i in 0..600i64 {
            // Holds the grid, but stalls 10 ticks every 100 frames.
            t += if i % 100 == 0 { PERIOD * 10 } else { PERIOD };
            skip.observe(None, t as u64);
        }
        let r = skip.report().unwrap();
        assert!(
            (r.period_us - PERIOD / 1_000).abs() <= 1,
            "a skipping producer still holds its period, got {} us",
            r.period_us
        );
        // i=0 opens the chain and yields no interval, so five stalls are scored.
        assert_eq!(r.skipped, 45, "five stalls of nine swallowed ticks each");

        let mut slow = PtsProvenance::new();
        for i in 0..600i64 {
            slow.observe(None, (RT_BASE as i64 + i * PERIOD * 3 / 2) as u64);
        }
        let r = slow.report().unwrap();
        assert_eq!(r.skipped, 0, "a slow producer skips nothing — it drags");
        assert!(
            (r.period_us - PERIOD * 3 / 2 / 1_000).abs() <= 1,
            "…and shows it in the period, got {} us",
            r.period_us
        );
    }

    #[test]
    fn a_producer_without_headers_still_reports_its_delivery_clock() {
        let mut p = PtsProvenance::new();
        for i in 0..40i64 {
            p.observe(None, (RT_BASE as i64 + i * 8_333_333) as u64);
        }
        let r = p.report().unwrap();
        assert_eq!(r.with_hdr, 0);
        assert_eq!(r.hdr_mad_us, 0, "no samples, not a clean clock");
        assert!((r.period_us - 8_333).abs() <= 1);
    }

    #[test]
    fn a_reset_window_keeps_measuring_across_the_boundary() {
        let mut p = PtsProvenance::new();
        let mut rng = Lcg(13);
        for i in 0..40i64 {
            p.observe(
                Some(MONO_BASE + i * PERIOD),
                (RT_BASE as i64 + i * PERIOD) as u64,
            );
        }
        p.implausible = 3;
        let first = p.report().unwrap();
        assert_eq!(
            first.implausible, 3,
            "the window's fallbacks must be reported"
        );

        p.reset_window();
        for i in 40..80i64 {
            let delivery = (RT_BASE as i64 + i * PERIOD + rng.noise(2_000_000)) as u64;
            p.observe(Some(MONO_BASE + i * PERIOD), delivery);
        }
        let second = p.report().unwrap();
        assert_eq!(second.frames, 40, "counts start over");
        assert_eq!(second.implausible, 0, "…and so do the fallbacks");
        assert_eq!(
            second.samples, 40,
            "40 frames across a boundary yield 40 intervals, not 39 — the chain survived"
        );
        assert_eq!(second.hdr_mad_us, 0, "the exact producer is still exact");
    }

    #[test]
    fn a_thin_window_reports_nothing() {
        let mut p = PtsProvenance::new();
        for i in 0..4i64 {
            p.observe(Some(MONO_BASE + i), RT_BASE + i as u64);
        }
        assert!(p.report().is_none());
    }
}
