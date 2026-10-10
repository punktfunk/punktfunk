//! The climb law: when a window licenses more rate, and how much more.
//!
//! A clean window only proves something if the encoder was actually
//! constrained, so a climb needs utilization — delivered ≈ target. A
//! frame-driven source never fills a wall-clock window, so both the
//! utilization bar and the proven bound are prorated by the frames that
//! arrived. Slow start doubles; afterwards the climb is additive, and
//! neither goes further than ×1.5 past what the link has proven.

use super::sample::{WindowActivity, WINDOW_US};

/// Fewest active frames before the fps-normalized utilization gate may climb.
/// Two stray frames would prorate the target to almost nothing.
const MIN_ACTIVE_FRAMES_TO_CLIMB: u32 = 4;
/// Windows per proven-throughput bucket (~30 s). The mark is the max of the
/// current and previous buckets so a regime minutes gone cannot license a
/// doubling.
const PROVEN_BUCKET_WINDOWS: u32 = 40;
/// Clean windows before an additive climb (~4.5 s). Slow start ignores this
/// and doubles on every cooled clean window.
pub(super) const CLEAN_WINDOWS_TO_INCREASE: u32 = 6;
/// Climb credit requires `actual × DEN ≥ target × NUM` (¾ of target). Below
/// that the encoder was not constrained, so the window proves nothing.
const UTILIZATION_NUM: u64 = 3;
const UTILIZATION_DEN: u64 = 4;
/// Climb may step at most ×1.5 past proven throughput. Utilization guarantees
/// `proven ≥ ¾ × current`, so the two gates cannot deadlock.
const PROVEN_HEADROOM_NUM: u32 = 3;
const PROVEN_HEADROOM_DEN: u32 = 2;

/// The highest clean delivered rate of the last ~30–60 s.
///
/// Two buckets, so a regime minutes gone cannot license a doubling, and the
/// clock ticks on idle windows too: the decay is about time, not traffic.
#[derive(Clone, Copy, Debug)]
pub(super) struct Proven {
    cur_kbps: u32,
    prev_kbps: u32,
    bucket_windows: u32,
}

impl Proven {
    pub(super) fn new() -> Self {
        Proven {
            cur_kbps: 0,
            prev_kbps: 0,
            bucket_windows: 0,
        }
    }

    /// Max clean delivered rate of the current and previous buckets.
    pub(super) fn mark(&self) -> u32 {
        self.cur_kbps.max(self.prev_kbps)
    }

    /// One window older. Rolls the bucket when the period is up.
    pub(super) fn tick(&mut self) {
        self.bucket_windows += 1;
        if self.bucket_windows >= PROVEN_BUCKET_WINDOWS {
            self.bucket_windows = 0;
            self.prev_kbps = self.cur_kbps;
            self.cur_kbps = 0;
        }
    }

    /// What an undamaged window delivered. Damaged windows overstate it
    /// (stall drain, flush queue, FEC surge), so the caller gates on the
    /// whole verdict.
    pub(super) fn note(&mut self, actual_kbps: u32) {
        self.cur_kbps = self.cur_kbps.max(actual_kbps);
    }

    /// Throughput is a property of the mode that produced it.
    pub(super) fn clear(&mut self) {
        *self = Proven::new();
    }
}

/// Active frames against the frames a full window would carry, when the
/// source ran slower than the refresh. `None` = judge on wall clock.
pub(super) fn proration(
    activity: WindowActivity,
    frame_budget_us: Option<i64>,
) -> Option<(u64, u64)> {
    match (activity, frame_budget_us) {
        (WindowActivity::Active(n), Some(budget_us))
            if budget_us > 0 && n > 0 && (n as i64) < WINDOW_US / budget_us =>
        {
            Some((n as u64, ((WINDOW_US / budget_us).max(1)) as u64))
        }
        _ => None,
    }
}

/// Did the content fill its allowance? Nothing else proves the encoder was
/// constrained, and an unconstrained window proves no capacity.
///
/// Prorated together with the proven bound or they deadlock: a 35 fps
/// source's wall-clock wire rate never exceeds ~39 % of its target.
pub(super) fn utilized(
    activity: WindowActivity,
    proration: Option<(u64, u64)>,
    actual_kbps: u32,
    current_kbps: u32,
) -> bool {
    match (activity, proration) {
        (WindowActivity::Active(0) | WindowActivity::Empty, _) => false,
        (_, Some((n, expected))) => {
            n >= MIN_ACTIVE_FRAMES_TO_CLIMB as u64
                && actual_kbps as u64 * UTILIZATION_DEN * expected
                    >= current_kbps as u64 * UTILIZATION_NUM * n
        }
        // Full-rate source, older host, or unknown refresh: wall-clock.
        _ => delivered_the_rate(actual_kbps, current_kbps),
    }
}

/// Did the link hand over the rate the session is running at? The same ¾ bar
/// on the wall clock, with no proration — for the caller that already knows
/// the missing frames are sitting in a queue rather than never made.
pub(super) fn delivered_the_rate(actual_kbps: u32, current_kbps: u32) -> bool {
    actual_kbps as u64 * UTILIZATION_DEN >= current_kbps as u64 * UTILIZATION_NUM
}

/// The target a climb may not pass: ×1.5 of the proven wire rate, put back
/// into the target domain the same proration took it out of.
pub(super) fn proven_target_cap(proven_kbps: u32, proration: Option<(u64, u64)>) -> u32 {
    let wire = proven_kbps.saturating_mul(PROVEN_HEADROOM_NUM) / PROVEN_HEADROOM_DEN;
    match proration {
        Some((n, expected)) => u32::try_from(wire as u64 * expected / n).unwrap_or(u32::MAX),
        None => wire,
    }
}

/// Slow start doubles every cooled clean window; after the first congestion
/// the climb is +~6 %. Both stop at `cap_kbps`.
pub(super) fn climb_step(current_kbps: u32, cap_kbps: u32, slow_start: bool) -> u32 {
    if slow_start {
        current_kbps.saturating_mul(2).min(cap_kbps)
    } else {
        (current_kbps + current_kbps / 16 + 1).min(cap_kbps)
    }
}

#[cfg(test)]
mod tests {
    use super::super::controller::BitrateController;
    use super::super::harness::*;
    use super::super::sample::WindowSample;
    use super::super::verdict::BASELINE_MIN_WINDOWS;
    use super::*;
    use std::time::Instant;

    #[test]
    fn sustained_clean_recovers_toward_ceiling_only() {
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000, None);
        // Slow start is over: 6 clean windows → +~6 % (14000 + 14000/16 + 1 = 14876).
        let up = run_clean(&mut c, start, 2, 7);
        assert_eq!(up, Some(14_876));
        c.on_ack(14_876, None);
        // At the ceiling, clean windows stay quiet.
        c.on_ack(20_000, None);
        assert_eq!(run_clean(&mut c, start, 40, 20), None);
    }

    #[test]
    fn slow_start_doubles_to_a_probed_ceiling_then_stops() {
        let mut c = BitrateController::new(20_000, None);
        // Probe measured ~430 Mbps delivered → ×0.7 ceiling.
        c.set_ceiling(300_000);
        let start = Instant::now();
        // Cooled clean windows double until the ceiling, then quiet.
        let mut got = Vec::new();
        for i in 0..14 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            }) {
                c.on_ack(k, None);
                got.push(k);
            }
        }
        assert_eq!(got, vec![40_000, 80_000, 160_000, 300_000]);
    }

    #[test]
    fn first_congestion_ends_slow_start_for_good() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(40_000)
        );
        c.on_ack(40_000, None);
        // Severe: immediate ×0.7, slow start over.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(28_000)
        );
        c.on_ack(28_000, None);
        // Next climb is additive, after 6 clean windows.
        let mut next = None;
        for i in 3..12 {
            next = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            });
            if next.is_some() {
                assert!(i >= 8, "additive climb must wait for the clean run");
                break;
            }
        }
        assert_eq!(next, Some(29_751)); // 28000 + 28000/16 + 1
    }

    #[test]
    fn decode_latency_caps_the_slow_start_climb() {
        // Fat link, decoder saturates below it.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // First [`BASELINE_MIN_WINDOWS`] teach the decode baseline.
        let mut last = 0;
        for i in 0..BASELINE_MIN_WINDOWS as u32 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i * 2))
            }) {
                last = k;
                c.on_ack(k, None);
            }
        }
        assert_eq!(last, 300_000, "slow start should reach the probed ceiling");
        // +30 ms decode: climb stops.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(38_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 20))
            }),
            None
        );
        // Second backed-up window: ×0.7, not park at the link ceiling.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(40_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 22))
            }),
            Some(210_000)
        );
    }

    #[test]
    fn unloaded_clean_windows_never_authorize_a_climb() {
        // Calm, under-target delivery: no climb credit.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        for i in 0..12 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 2_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // First utilized window: ×1.5 over proven 18 000 → 27 000, not 2× to 40 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 18_000,
                ..WindowSample::at(ticks(start, 12))
            }),
            Some(27_000)
        );
        // Zero active frames never authorizes a climb, whatever delivered claims.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        c.set_frame_budget(60);
        for i in 0..12 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 18_000,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None,
                "an idle window must never climb"
            );
        }
    }

    /// Frame-driven source: utilization and proven-headroom prorate together
    /// so a 35 fps source on 90 Hz is not stuck in a wall-clock dead band.
    #[test]
    fn a_frame_driven_source_climbs_at_its_own_fps() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(90); // 11 111 µs budget → 67 expected frames / window
        let start = Instant::now();
        // 26/67 frames, 8 000 kbps vs prorated 7 761: utilized. Proven headroom
        // 8 000×1.5×67/26 = 30 923, not wall-clock 12 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 8_000,
                activity: WindowActivity::Active(26),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_923)
        );
        // Under [`MIN_ACTIVE_FRAMES_TO_CLIMB`]: not utilized.
        let mut d = BitrateController::new(20_000, None);
        d.set_stream_cap(100_000);
        d.set_ceiling(60_000);
        d.set_frame_budget(90);
        assert_eq!(
            d.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 900,
                activity: WindowActivity::Active(3),
                ..WindowSample::at(ticks(start, 0))
            }),
            None,
            "three stray frames are not a utilized window"
        );
    }

    /// Motion onset after a real idle stretch re-arms slow start, bounded by
    /// ×1.5 over the windowed proven mark.
    #[test]
    fn motion_onset_rearms_slow_start_bounded_by_the_windowed_proven() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        // Severe window ends slow start…
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 18_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000, None);
        // …one clean window proves 14 000…
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        // …then ≥ [`IDLE_WINDOWS_TO_REARM`] idle windows.
        for i in 2..6 {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // Onset: ×1.5 over proven 14 000 → 21 000, not additive 14 876.
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(21_000)
        );
    }

    /// Five empty windows are not an idle stretch. [`CLEAN_WINDOWS_TO_INCREASE`]
    /// is 6, so five quiet windows plus one active would additive-climb if
    /// Empty were credited as clean (the older-host `None` path).
    #[test]
    fn empty_windows_do_not_rearm_abr_slow_start() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 18_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000, None);
        for i in 1..=5 {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Empty,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 6))
            }),
            None,
            "a blackout is not stillness and cannot authorize a climb"
        );
    }

    /// Empty is neutral on the idle counter: it neither fills nor clears it.
    #[test]
    fn empty_windows_do_not_count_toward_idle_rearm() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 18_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        c.on_ack(14_000, None);
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        for i in 2..6 {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 200,
                activity: WindowActivity::Empty,
                ..WindowSample::at(ticks(start, 6))
            }),
            None,
            "empty is not motion onset"
        );
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 7))
            }),
            Some(21_000),
            "empty must not clear a real idle stretch"
        );
    }

    /// Idle stretch outlives both proven buckets: onset doubles over what it
    /// just delivered, not the stale pre-idle mark.
    #[test]
    fn the_proven_mark_decays_with_its_buckets() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(60_000);
        c.set_frame_budget(60);
        let start = Instant::now();
        // Prove 20 000; slow start asks for the bounded double (mark matters)…
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 20_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_000)
        );
        // Severe inside cooldown scores but decides nothing; second backs off.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 20_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 20_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(14_000)
        );
        c.on_ack(14_000, None);
        // Idle 2 × [`PROVEN_BUCKET_WINDOWS`]: both buckets rotate away.
        for i in 3..3 + 2 * PROVEN_BUCKET_WINDOWS {
            assert_eq!(
                c.on_window(&WindowSample {
                    actual_kbps: 200,
                    activity: WindowActivity::Active(0),
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // Onset delivers 14 000 → 21 000, not 28 000 off the stale 20 000 mark.
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 14_000,
                activity: WindowActivity::Active(45),
                ..WindowSample::at(ticks(start, 3 + 2 * PROVEN_BUCKET_WINDOWS))
            }),
            Some(21_000)
        );
    }

    #[test]
    fn slow_start_steps_stay_within_proven_headroom() {
        // Each slow-start step is ×1.5 over delivered, not a blind 2×.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        // Full-target delivery: proven 20 000 → cap 30 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 20_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_000)
        );
        c.on_ack(30_000, None);
        // Delivers 30 000 → next step 45 000, not 60 000.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 30_000,
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(45_000)
        );
    }

    #[test]
    fn calm_period_keeps_the_validated_target() {
        // Validated target is not surrendered when the scene goes calm.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 20_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(30_000)
        );
        c.on_ack(30_000, None);
        // Long calm stretch (2 % utilization): stay silent. Keep proven headroom.
        for i in 2..30 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(4_000),
                    actual_kbps: 600,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
    }
}
