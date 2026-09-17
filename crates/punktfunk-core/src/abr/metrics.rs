//! What a run of report windows is worth: the eight metrics plus the decision
//! checksum, and the row they print as.
//!
//! One definition for both test tiers. The simulator scores a modelled run
//! with it and pins the result in `sim/baseline.tsv`; the netem rig scores a
//! real session with it and prints the row beside that baseline. A metric that
//! meant two things would make the two tiers incomparable, which is the whole
//! point of having the second one.

/// One run's integer metrics. Every number is a whole unit, so a baseline row
/// can be compared for equality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    pub under5_pct: u32,
    pub to90_s: u32,
    pub cuts_per_10min: u32,
    pub lost_per_10min: u32,
    pub queue_p95_ms: u32,
    pub over_cap_kb_10s: u32,
    pub blip_recover_s: u32,
    pub fairness_x1000: u32,
    /// FNV-1a over every session's `(window index, requested kbps)`. Eight
    /// coarse metrics cannot see a retarget that moved by one window; this
    /// can, so "bit-identical" means the whole decision sequence.
    pub decisions_fnv1a: u32,
}

/// A metric that never happened. Visible in the table rather than silent.
pub const NEVER: u32 = 99_999;

/// The tab-separated header the baseline table and the rig both print.
pub const HEADER: &str = "scenario\tunder5_pct\tto90_s\tcuts_10min\tlost_10min\t\
                          queue_p95_ms\tover_cap_kb_10s\tblip_recover_s\tfairness_x1000\t\
                          decisions_fnv1a";

impl Metrics {
    /// One [`HEADER`] row.
    pub fn row(&self, name: &str) -> String {
        format!(
            "{name}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:08x}",
            self.under5_pct,
            self.to90_s,
            self.cuts_per_10min,
            self.lost_per_10min,
            self.queue_p95_ms,
            self.over_cap_kb_10s,
            self.blip_recover_s,
            self.fairness_x1000,
            self.decisions_fnv1a
        )
    }
}

/// What scoring needs from one closed report window. Both tiers build it from
/// whatever they record; nothing here is re-derived from the wire.
#[derive(Clone, Copy, Debug)]
pub struct MetricWindow {
    /// Milliseconds from the session's start to this window's close.
    pub t_ms: u64,
    /// Rate the session was running at for this window.
    pub rate_kbps: u32,
    /// What the controller asked for on this window, if it asked. A request
    /// below `rate_kbps` is a cut.
    pub request_kbps: Option<u32>,
    /// Unrecoverable frames.
    pub dropped: u64,
    /// The window described a burst tail or a host rebuild, not the link.
    pub discarded: bool,
}

impl MetricWindow {
    fn cut(&self) -> bool {
        self.request_kbps.is_some_and(|k| k < self.rate_kbps)
    }
}

/// One run, as the metrics read it. `sessions[0]` is the session under test;
/// the rest only carry the fairness share.
pub struct Run<'a> {
    pub sessions: &'a [&'a [MetricWindow]],
    /// Capture → received, ms, for every AU of `sessions[0]`.
    pub owd_ms: &'a [u32],
    /// The path's own delay, taken off the queue reading.
    pub base_delay_ms: u32,
    pub duration_ms: u64,
    /// Rate this run could hold if nothing went wrong — the yardstick for
    /// "time to 90 % of achievable".
    pub achievable_kbps: u32,
    /// Bytes offered to the link in the first 10 s, and what it could carry.
    pub offered_10s: u64,
    pub capacity_10s: u64,
    /// One unrecoverable frame was injected here.
    pub blip_at_ms: Option<u64>,
}

pub fn percentile(samples: &mut [u32], pct: usize) -> u32 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    samples[(samples.len() - 1) * pct / 100]
}

/// FNV-1a over the decisions every session made, in order.
fn decision_checksum(sessions: &[&[MetricWindow]]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for s in sessions {
        for (i, w) in s.iter().enumerate() {
            let Some(kbps) = w.request_kbps else { continue };
            for b in (i as u32)
                .to_le_bytes()
                .into_iter()
                .chain(kbps.to_le_bytes())
            {
                h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
            }
        }
    }
    h
}

/// Score a run. Discarded windows describe something other than the link, so
/// every rate metric skips them; fairness does not, because a session's share
/// of the path is what it ran at whatever the window described.
pub fn measure(r: &Run) -> Metrics {
    let first: &[MetricWindow] = r.sessions.first().copied().unwrap_or(&[]);
    let live: Vec<&MetricWindow> = first.iter().filter(|w| !w.discarded).collect();
    let under5 = live.iter().filter(|w| w.rate_kbps < 5_000).count();
    let under5_pct = if live.is_empty() {
        0
    } else {
        (under5 * 100 / live.len()) as u32
    };
    let want = r.achievable_kbps / 10 * 9;
    let to90_s = live
        .iter()
        .find(|w| w.rate_kbps >= want)
        .map_or(NEVER, |w| (w.t_ms / 1_000) as u32);
    let scale = |n: u64| (n * 600_000 / r.duration_ms.max(1)) as u32;
    let cuts = live.iter().filter(|w| w.cut()).count() as u64;
    let lost: u64 = live.iter().map(|w| w.dropped).sum();
    let mut owd = r.owd_ms.to_vec();
    let queue_p95_ms = percentile(&mut owd, 95).saturating_sub(r.base_delay_ms);
    let over_cap_kb_10s = r.offered_10s.saturating_sub(r.capacity_10s) / 1_000;
    // Recovery is measured from the blip to the first window back at the rate
    // it was holding — but only once the blip has actually cost something.
    // The window the verdict lands in still reports the old rate.
    let blip_recover_s = match r.blip_at_ms {
        None => 0,
        Some(at) => {
            let before = live
                .iter()
                .rev()
                .find(|w| w.t_ms <= at)
                .map_or(0, |w| w.rate_kbps);
            match live.iter().find(|w| w.t_ms > at && w.rate_kbps < before) {
                None => 0,
                Some(dip) => live
                    .iter()
                    .find(|w| w.t_ms > dip.t_ms && w.rate_kbps >= before)
                    .map_or(NEVER, |w| ((w.t_ms - at) / 1_000) as u32),
            }
        }
    };
    // Jain over the sessions' mean rate. One session is fair by definition.
    let means: Vec<u64> = r
        .sessions
        .iter()
        .map(|w| {
            if w.is_empty() {
                0
            } else {
                w.iter().map(|w| u64::from(w.rate_kbps)).sum::<u64>() / w.len() as u64
            }
        })
        .collect();
    let sum: u64 = means.iter().sum();
    let sq: u64 = means.iter().map(|m| m * m).sum();
    let fairness_x1000 = if sq == 0 {
        1_000
    } else {
        (sum * sum * 1_000 / (means.len() as u64 * sq)) as u32
    };
    Metrics {
        under5_pct,
        to90_s,
        cuts_per_10min: scale(cuts),
        lost_per_10min: scale(lost),
        queue_p95_ms,
        over_cap_kb_10s: over_cap_kb_10s as u32,
        blip_recover_s,
        fairness_x1000,
        decisions_fnv1a: decision_checksum(r.sessions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(t_ms: u64, rate_kbps: u32, request_kbps: Option<u32>) -> MetricWindow {
        MetricWindow {
            t_ms,
            rate_kbps,
            request_kbps,
            dropped: 0,
            discarded: false,
        }
    }

    /// A discarded window is not the link's: it carries neither a cut, a lost
    /// frame, nor a reading of where the rate got to.
    #[test]
    fn a_discarded_window_scores_nothing() {
        let mut ws = vec![w(750, 20_000, Some(9_000)), w(1_500, 9_000, None)];
        ws[0].dropped = 4;
        let run = |ws: &[MetricWindow]| {
            measure(&Run {
                sessions: &[ws],
                owd_ms: &[],
                base_delay_ms: 0,
                duration_ms: 600_000,
                achievable_kbps: 20_000,
                offered_10s: 0,
                capacity_10s: 0,
                blip_at_ms: None,
            })
        };
        let judged = run(&ws);
        assert_eq!((judged.cuts_per_10min, judged.lost_per_10min), (1, 4));
        ws[0].discarded = true;
        let dropped = run(&ws);
        assert_eq!((dropped.cuts_per_10min, dropped.lost_per_10min), (0, 0));
    }

    /// The share under 5 Mbps and the time to 90 % of achievable are what a
    /// slow-link row is read for.
    #[test]
    fn the_slow_link_metrics_read_the_rate_trajectory() {
        let ws: Vec<MetricWindow> = (0..8)
            .map(|i| w(750 * (i + 1), if i < 4 { 4_000 } else { 18_000 }, None))
            .collect();
        let owd: Vec<u32> = (1..=100).collect();
        let m = measure(&Run {
            sessions: &[&ws],
            owd_ms: &owd,
            base_delay_ms: 10,
            duration_ms: 6_000,
            achievable_kbps: 20_000,
            offered_10s: 3_000_000,
            capacity_10s: 1_000_000,
            blip_at_ms: None,
        });
        assert_eq!(m.under5_pct, 50, "half the windows sat under 5 Mbps");
        assert_eq!(m.to90_s, 3, "18 000 is 90 % of 20 000, reached at 3 750 ms");
        assert_eq!(m.queue_p95_ms, 85, "p95 owd less the path's own delay");
        assert_eq!(m.over_cap_kb_10s, 2_000, "two megabytes over what fitted");
        assert_eq!(m.fairness_x1000, 1_000, "one session is fair by definition");
    }

    /// 90 % of achievable that never arrives is [`NEVER`], not a zero that
    /// reads like an instant start.
    #[test]
    fn a_rate_that_never_arrives_says_so() {
        let ws = [w(750, 4_000, None)];
        let m = measure(&Run {
            sessions: &[&ws],
            owd_ms: &[],
            base_delay_ms: 0,
            duration_ms: 750,
            achievable_kbps: 20_000,
            offered_10s: 0,
            capacity_10s: 0,
            blip_at_ms: None,
        });
        assert_eq!(m.to90_s, NEVER);
    }
}
