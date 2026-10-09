//! Evenly-spaced disturbance detector for the capture/encode seam.
//!
//! Random network loss is bursty. A stable multi-second period is a machine
//! (display-topology churn, a display poller, virtual-display present timing).
//! Callers log the mean period so a host-side cycle is named in the log.
//!
//! Two feeds: served client-recovery IDRs (`native`) and IDD-push capture
//! stalls (`capture::windows::idd_push`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Metronome {
    events: VecDeque<Instant>,
    last_warn: Option<Instant>,
}

impl Metronome {
    /// 1500 ms covers an IDR-cooldown re-issue (~0.7 s) without eating a multi-second cycle.
    const COALESCE: Duration = Duration::from_millis(1500);
    /// Four events (three gaps) reject irregular bursts.
    const STREAK: usize = 4;
    /// ±20 % of the mean gap: clock jitter, not a new period.
    const TOLERANCE: f64 = 0.2;
    /// 30 s so a persisting cycle does not flood the log.
    const REWARN: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
            last_warn: None,
        }
    }

    pub fn note(&mut self, now: Instant) -> Option<Duration> {
        if self
            .events
            .back()
            .is_some_and(|last| now.duration_since(*last) < Self::COALESCE)
        {
            return None;
        }
        self.events.push_back(now);
        if self.events.len() > Self::STREAK {
            self.events.pop_front();
        }
        if self.events.len() < Self::STREAK {
            return None;
        }
        let gaps: Vec<f64> = self
            .events
            .iter()
            .zip(self.events.iter().skip(1))
            .map(|(a, b)| b.duration_since(*a).as_secs_f64())
            .collect();
        let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
        if mean <= 0.0
            || gaps
                .iter()
                .any(|g| (g - mean).abs() > mean * Self::TOLERANCE)
        {
            return None;
        }
        if self
            .last_warn
            .is_some_and(|t| now.duration_since(t) < Self::REWARN)
        {
            return None;
        }
        self.last_warn = Some(now);
        Some(Duration::from_secs_f64(mean))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cadence_run(offsets_ms: &[u64]) -> Vec<Option<Duration>> {
        let base = Instant::now();
        let mut c = Metronome::new();
        offsets_ms
            .iter()
            .map(|ms| c.note(base + Duration::from_millis(*ms)))
            .collect()
    }

    #[test]
    fn cadence_detects_metronomic_events() {
        let out = cadence_run(&[0, 4_000, 8_100, 11_950]);
        assert_eq!(out[..3], [None, None, None]);
        let period = out[3].expect("metronomic series must be detected");
        assert!(
            (period.as_secs_f64() - 3.98).abs() < 0.2,
            "period={period:?}"
        );
    }

    #[test]
    fn cadence_coalesces_double_jolt_pairs() {
        let out = cadence_run(&[0, 700, 4_000, 4_700, 8_000, 8_650, 12_000]);
        assert!(out[..6].iter().all(Option::is_none));
        let period = out[6].expect("coalesced pairs must still read as a 4 s cycle");
        assert!(
            (period.as_secs_f64() - 4.0).abs() < 0.2,
            "period={period:?}"
        );
    }

    #[test]
    fn cadence_ignores_irregular_bursts() {
        assert!(cadence_run(&[0, 2_000, 9_000, 12_500, 21_000])
            .iter()
            .all(Option::is_none));
    }

    #[test]
    fn cadence_rewarns_at_most_every_30s() {
        // 4 s cycle: first warn at event 4 (t=12 s); next at t≥42 s is event 12 (index 11).
        let offsets: Vec<u64> = (0..12).map(|i| i * 4_000).collect();
        let out = cadence_run(&offsets);
        let warned: Vec<usize> = out
            .iter()
            .enumerate()
            .filter_map(|(i, o)| o.map(|_| i))
            .collect();
        assert_eq!(warned, vec![3, 11], "warn indices");
    }
}
