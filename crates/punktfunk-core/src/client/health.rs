//! The speed test: the ceiling the bring-up ramp proved, then one clean round under it.
//!
//! One routine for every shell. Toward a [`HOST_CAP2_RAMP`] host the ramp runs during
//! bring-up and proves what the link carries; the clean round then measures loss and jitter
//! at half of that, a rate the link holds. Toward an older host the single blast stays, and
//! its loss is the blast's, so no shell shows it.

use super::{NativeClient, ProbeOutcome};
use crate::quic::HOST_CAP2_RAMP;
use std::time::{Duration, Instant};

pub const CLEAN_ROUND_MS: u32 = 2_000;
/// The clean round's share of the ceiling: under the wall by more than the session itself
/// keeps, so its loss is the path's, not the round's.
pub const CLEAN_ROUND_PCT: u32 = 50;
/// The blast toward a host without a ramp: far more than any link carries, so the link is
/// what limits the answer.
pub const BLAST_KBPS: u32 = 3_000_000;
pub const BLAST_MS: u32 = 2_000;
/// The ramp stops within the bring-up gap; longer means it was cut short or declined.
const RAMP_WAIT: Duration = Duration::from_secs(4);
/// The clean round runs after the first video frame: until then the host serves every
/// probe as a ramp step, 50 ms long, and a round cut to that would also spend the one
/// burst the spacing allows. A pipeline builds in two to three seconds.
const VIDEO_WAIT: Duration = Duration::from_secs(8);
/// A round that never reports is a dead session, not a slow link.
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The last shards land after the host's report; counted now, they are not loss.
const SETTLE: Duration = Duration::from_millis(400);

/// One round at a rate the link holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct CleanRound {
    pub rate_kbps: u32,
    pub loss_pct: f32,
    /// p99 − p50 of the probe inter-arrival gap, µs, to a tenth of a millisecond.
    pub jitter_us: u32,
    pub reorders: u32,
    pub outcome: ProbeOutcome,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpeedReport {
    /// What the link carries, kbps: the ramp's proof, or a blast's delivered rate.
    pub ceiling_kbps: u32,
    /// The ramp found the wall; `false` is a floor under the capacity, or a blast.
    pub wall: bool,
    /// `None` toward a host without a ramp — then nothing honest can be said about loss.
    pub clean: Option<CleanRound>,
    /// The blast's own reading, toward a host without a ramp.
    pub blast: Option<ProbeOutcome>,
}

#[derive(Debug)]
pub enum SpeedError {
    Request(crate::PunktfunkError),
    /// The host answered a round with an all-zero report.
    Declined,
    Timeout,
}

/// Headroom a recommendation keeps under the ceiling: what the session opens against. In
/// this order, so every client recommends the same kilobit.
pub fn recommended_kbps(ceiling_kbps: u32) -> u32 {
    ceiling_kbps / 10 * 7
}

/// The rate the clean round runs at.
pub fn clean_rate_kbps(ceiling_kbps: u32) -> u32 {
    ceiling_kbps / 100 * CLEAN_ROUND_PCT
}

/// Blocking: the ramp's result, then the clean round, polled to its report. `progress` sees
/// the round's live throughput at every poll.
pub fn speed_test(
    c: &NativeClient,
    mut progress: impl FnMut(u32),
) -> Result<SpeedReport, SpeedError> {
    let ramp = (c.host_caps2() & HOST_CAP2_RAMP != 0)
        .then(|| wait_for_ramp(c))
        .flatten();
    let Some(ceiling_kbps) = ramp
        .as_ref()
        .map(|r| r.outcome.proven_kbps)
        .filter(|&k| k > 0)
    else {
        let blast = run_round(c, BLAST_KBPS, BLAST_MS, &mut progress)?;
        return Ok(SpeedReport {
            ceiling_kbps: blast.throughput_kbps,
            wall: false,
            clean: None,
            blast: Some(blast),
        });
    };
    wait_for_video(c);
    let rate_kbps = clean_rate_kbps(ceiling_kbps);
    let outcome = run_round(c, rate_kbps, CLEAN_ROUND_MS, &mut progress)?;
    Ok(SpeedReport {
        ceiling_kbps,
        wall: ramp.is_some_and(|r| r.outcome.wall),
        clean: Some(CleanRound {
            rate_kbps,
            loss_pct: outcome.loss_pct,
            jitter_us: outcome.gap_p99_us.saturating_sub(outcome.gap_p50_us),
            reorders: outcome.reorders,
            outcome,
        }),
        blast: None,
    })
}

fn wait_for_ramp(c: &NativeClient) -> Option<crate::abr::RampRecord> {
    let deadline = Instant::now() + RAMP_WAIT;
    loop {
        if let Some(r) = c.abr_ramp() {
            return Some(r);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The first completed AU, which is the ramp window closing on the host. A session that
/// shows no video within [`VIDEO_WAIT`] goes on anyway: the round then measures what it can.
fn wait_for_video(c: &NativeClient) {
    let deadline = Instant::now() + VIDEO_WAIT;
    while Instant::now() < deadline {
        match c.next_frame(Duration::from_millis(100)) {
            Ok(_) => return,
            Err(crate::PunktfunkError::NoFrame) => {}
            Err(_) => return,
        }
    }
}

/// One burst, polled to the host's report. An all-zero report is a decline.
fn run_round(
    c: &NativeClient,
    target_kbps: u32,
    duration_ms: u32,
    progress: &mut impl FnMut(u32),
) -> Result<ProbeOutcome, SpeedError> {
    c.request_probe(target_kbps, duration_ms)
        .map_err(SpeedError::Request)?;
    let deadline = Instant::now() + POLL_BUDGET;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = c.probe_result();
        if now.done {
            std::thread::sleep(SETTLE);
            let r = c.probe_result();
            if r.wire_packets_sent == 0 && r.host_bytes == 0 {
                return Err(SpeedError::Declined);
            }
            return Ok(r);
        }
        progress(now.throughput_kbps);
        if Instant::now() > deadline {
            return Err(SpeedError::Timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clean round runs at half the ceiling and the recommendation keeps 30 % back —
    /// truncating, so neither exceeds what was proved.
    #[test]
    fn the_clean_round_runs_under_the_ceiling() {
        assert_eq!(clean_rate_kbps(940_000), 470_000);
        assert_eq!(clean_rate_kbps(99), 0);
        assert_eq!(recommended_kbps(100_000), 70_000);
        assert_eq!(recommended_kbps(9), 0);
        assert_eq!(recommended_kbps(412_345), 288_638);
        assert!(clean_rate_kbps(1_000_000) < recommended_kbps(1_000_000));
    }

    /// Jitter is the spread of the gap, never negative, and comes from the bucket edges.
    #[test]
    fn jitter_comes_from_the_arrival_ring() {
        let mut buckets = [0u32; crate::stats::PROBE_GAP_BUCKETS];
        buckets[0] = 980;
        buckets[3] = 20;
        let p50 = crate::stats::probe_gap_percentile(&buckets, 0.5);
        let p99 = crate::stats::probe_gap_percentile(&buckets, 0.99);
        assert_eq!((p50, p99), (100, 400));
        assert_eq!(p99.saturating_sub(p50), 300);
        assert_eq!(
            crate::stats::probe_gap_percentile(&[0; crate::stats::PROBE_GAP_BUCKETS], 0.5),
            0
        );
        assert_eq!(crate::stats::probe_gap_bucket(50), 0);
        assert_eq!(crate::stats::probe_gap_bucket(350), 3);
        assert_eq!(
            crate::stats::probe_gap_bucket(1_000_000),
            crate::stats::PROBE_GAP_BUCKETS - 1
        );
    }
}
