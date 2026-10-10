//! Learned bounds and the clock that lets the session past them again.
//!
//! Evidence latches a cap, the rate parks under it, clean loaded windows
//! accrue, and the cap lifts +12.5 % when the clock runs out. Re-learning a
//! lifted cap doubles the interval toward [`CAP_REPROBE_WINDOWS_MAX`]: a
//! standing limit costs one probe every few minutes, a transient one clears
//! in twelve seconds. [`StandDown`] is the same clock without a number —
//! a signal that stopped answering the rate, re-armed by a clean run.
//! [`StepProbe`] is how a driver finds that out: take one notch, then read
//! the signal at the new rate before believing the rate was the lever.

/// Clean windows parked at a learned cap before re-probing above it, and the
/// ceiling that interval backs off to.
///
/// A short ack means "not right now" — durable encoder ceiling or a transient
/// cadence refusal. The client cannot tell, so it probes again after 16
/// windows (~12 s) and doubles the interval each time the lift is immediately
/// re-learned. A still-standing limit re-teaches itself in two short acks
/// with no encoder rebuild.
pub(super) const CAP_REPROBE_WINDOWS_MIN: u32 = 16;
pub(super) const CAP_REPROBE_WINDOWS_MAX: u32 = 128;

/// A rate this session has evidence it cannot hold, and the clock that tests
/// that evidence again.
#[derive(Clone, Copy, Debug)]
pub struct LearnedCap {
    kbps: Option<u32>,
    /// Clean loaded windows parked at the cap.
    probe_windows: u32,
    /// Windows the park must last before the cap lifts.
    reprobe_after: u32,
}

impl Default for LearnedCap {
    fn default() -> Self {
        LearnedCap::new()
    }
}

impl LearnedCap {
    pub fn new() -> Self {
        LearnedCap {
            kbps: None,
            probe_windows: 0,
            reprobe_after: CAP_REPROBE_WINDOWS_MIN,
        }
    }

    pub fn kbps(&self) -> Option<u32> {
        self.kbps
    }

    /// Report windows the park lasts before the cap is tested again. The host
    /// spends it as time; the client counts the windows.
    pub fn reprobe_after(&self) -> u32 {
        self.reprobe_after
    }

    /// Fresh evidence of a limit at `kbps`. Ignored unless it binds tighter
    /// than what is already learned; re-learning after a lift means the limit
    /// is standing, so the clock backs off. Evidence under `floor_kbps` still
    /// latches, at the floor: the session goes no lower either way.
    pub fn latch(&mut self, kbps: u32, floor_kbps: u32) -> bool {
        if self.kbps.is_some_and(|c| kbps >= c) {
            return false;
        }
        self.reprobe_after = if self.kbps.is_some() {
            self.reprobe_after
                .saturating_mul(2)
                .min(CAP_REPROBE_WINDOWS_MAX)
        } else {
            CAP_REPROBE_WINDOWS_MIN
        };
        self.kbps = Some(kbps.max(floor_kbps));
        self.probe_windows = 0;
        true
    }

    /// Latch a cap the session measured on purpose, on the long clock.
    ///
    /// A measurement holds its own margin back — the ramp licenses 70 % of
    /// what it saw — so the session is not being damaged while the cap
    /// stands, and the first re-ask is not urgent. Asking on the short clock
    /// instead spends a probe every twelve seconds on a link whose margin is
    /// load-bearing, which is what the margin was for.
    pub(super) fn latch_measured(&mut self, kbps: u32, floor_kbps: u32) -> bool {
        let latched = self.latch(kbps, floor_kbps);
        if latched {
            self.reprobe_after = CAP_REPROBE_WINDOWS_MAX;
        }
        latched
    }

    /// Park at `kbps` and restart the clock, without judging the evidence.
    /// The headroom driver moves its cap both ways as one step is answered.
    pub fn park(&mut self, kbps: u32) {
        self.kbps = Some(kbps);
        self.probe_windows = 0;
    }

    /// The limit has lifted: drop it and start the clock over.
    pub fn drop_cap(&mut self) {
        *self = LearnedCap::new();
    }

    /// Ask the wall again at the next clean window, for evidence this session
    /// could not have measured.
    ///
    /// The step is the clock's own +12.5 %, and the interval it waits before
    /// the next one is the short one again: a backed-off clock is the record
    /// of a limit that re-taught itself here, and that record is what the
    /// outside evidence disputes. The cap itself stands, so every step still
    /// has to survive a window at the new rate.
    pub(super) fn lift_now(&mut self) {
        self.probe_windows = self.reprobe_after;
        self.reprobe_after = CAP_REPROBE_WINDOWS_MIN;
    }

    /// One report window at the cap. Damage restarts the park; a clean loaded
    /// window at the cap accrues, and the clock lifts the cap +12.5 % when it
    /// runs out. `Some((from, to))` is the lift.
    pub(super) fn on_window(
        &mut self,
        bad: bool,
        quiet: bool,
        current_kbps: u32,
        ceiling_kbps: u32,
    ) -> Option<(u32, u32)> {
        let cap = self.kbps?;
        if bad {
            self.probe_windows = 0;
            return None;
        }
        // Re-probe accrues from clean loaded windows, not stillness.
        if quiet || current_kbps < cap.saturating_sub(cap / 16) {
            return None;
        }
        self.probe_windows += 1;
        if self.probe_windows < self.reprobe_after {
            return None;
        }
        self.probe_windows = 0;
        let lifted = cap.saturating_add(cap / 8).min(ceiling_kbps);
        (lifted > cap).then(|| {
            self.kbps = Some(lifted);
            (cap, lifted)
        })
    }
}

/// Windows at the new rate a step is judged over. Two: one can be the
/// rebuild's own, and the signal is a mean over a whole window already.
pub(super) const VERDICT_WINDOWS: u32 = 2;
/// Windows a step may wait for its rate before the verdict is dropped.
pub(super) const PROBE_MAX_AGE: u32 = 16;

/// A rate step taken on one signal, waiting for that signal's answer at the
/// new rate.
///
/// The driver that took the step says what the answer means; this holds the
/// reference, averages the windows that reached the new rate, and bounds the
/// wait. Both retreats — decode headroom and host encode — are judged on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StepProbe {
    /// Rate before the step; restored when the step proves nothing.
    pub(super) from_kbps: u32,
    /// The signal's mean at `from_kbps`.
    pub(super) ref_us: i64,
    windows: u32,
    sum_us: i64,
    /// Windows since the step, at any rate.
    age: u32,
}

impl StepProbe {
    pub(super) fn new(from_kbps: u32, ref_us: i64) -> Self {
        StepProbe {
            from_kbps,
            ref_us,
            windows: 0,
            sum_us: 0,
            age: 0,
        }
    }

    /// One more window. `mean_us` is the signal at the new rate, or `None`
    /// when this window says nothing about it — time still passes.
    /// `Some(mean)` is the verdict, averaged over [`VERDICT_WINDOWS`].
    pub(super) fn note(&mut self, mean_us: Option<i64>) -> Option<i64> {
        self.age += 1;
        if let Some(mean) = mean_us {
            self.windows += 1;
            self.sum_us += mean;
        }
        (self.windows >= VERDICT_WINDOWS).then(|| self.sum_us / i64::from(self.windows))
    }

    /// The step never reached its rate: drop the verdict rather than judge a
    /// rate the session left long ago.
    pub(super) fn expired(&self) -> bool {
        self.age >= PROBE_MAX_AGE
    }
}

/// A down-driver that stopped answering the rate, and the clean run that
/// re-arms it. Same clock as [`LearnedCap`], with nothing to park under.
#[derive(Clone, Copy, Debug)]
pub(super) struct StandDown {
    disarmed: bool,
    clean_windows: u32,
    reprobe_after: u32,
    /// Lifted once already, so the next stand-down backs the clock off.
    rearmed: bool,
}

impl StandDown {
    pub(super) fn new() -> Self {
        StandDown {
            disarmed: false,
            clean_windows: 0,
            reprobe_after: CAP_REPROBE_WINDOWS_MIN,
            rearmed: false,
        }
    }

    pub(super) fn disarmed(&self) -> bool {
        self.disarmed
    }

    pub(super) fn reprobe_after(&self) -> u32 {
        self.reprobe_after
    }

    /// The signal is not a function of the rate: stand it down. Silencing a
    /// re-armed driver again is standing contention, so the clock backs off.
    pub(super) fn disarm(&mut self) {
        self.reprobe_after = if self.rearmed {
            self.reprobe_after
                .saturating_mul(2)
                .min(CAP_REPROBE_WINDOWS_MAX)
        } else {
            CAP_REPROBE_WINDOWS_MIN
        };
        self.disarmed = true;
        self.clean_windows = 0;
    }

    /// A damaged window: the clean run starts over.
    pub(super) fn note_bad(&mut self) {
        self.clean_windows = 0;
    }

    /// One clean window toward re-arming. `true` when it re-armed.
    pub(super) fn note_clean(&mut self) -> bool {
        self.clean_windows += 1;
        if self.clean_windows < self.reprobe_after {
            return false;
        }
        self.disarmed = false;
        self.rearmed = true;
        self.clean_windows = 0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::controller::{
        BitrateController, CLEAN_WINDOWS_TO_REARM, DECODE_CAP_SIMILAR_DIV, FLOOR_KBPS,
        LINK_DRAIN_WINDOWS,
    };
    use super::super::harness::*;
    use super::super::sample::{WindowActivity, WindowSample};
    use super::super::verdict::{
        BASELINE_MIN_WINDOWS, HEAVY_LOSS_PPM, RECOVERY_KF_SEVERE, SEVERE_LOSS_PPM,
    };
    use super::*;
    use crate::quic::AckReason;
    use std::time::Instant;

    /// One link-attributed cut, at a rate the session had climbed to.
    ///
    /// The guard the last cut armed is spent first — a cut inside one is the
    /// drain's business, and these tests are about the wall.
    fn link_choke(
        c: &mut BitrateController,
        start: Instant,
        tick: &mut u32,
        delivered_kbps: u32,
    ) -> Option<u32> {
        for _ in 0..LINK_DRAIN_WINDOWS {
            let at = ticks(start, *tick);
            *tick += 1;
            c.on_window(&WindowSample {
                activity: WindowActivity::Empty,
                ..WindowSample::at(at)
            });
        }
        let at = ticks(start, *tick);
        *tick += 3;
        c.on_window(&WindowSample {
            dropped: 4,
            actual_kbps: delivered_kbps,
            ..WindowSample::at(at)
        })
    }

    /// Two deliveries at the same rate are a wall; the cap sits a tenth under
    /// it, which is where the session then rides. Two that disagree are a link
    /// that moved, and teach no cap.
    #[test]
    fn two_deliveries_at_the_same_rate_latch_the_link_cap() {
        let start = Instant::now();
        let latched = |second: u32| -> Option<u32> {
            let mut c = BitrateController::new(20_000, None);
            c.set_ceiling(300_000);
            let mut t = 0;
            let k = link_choke(&mut c, start, &mut t, 12_000).expect("the link fell short");
            c.on_ack(k, None);
            assert!(c.link_cap.kbps().is_none(), "one mark is not a wall");
            // A granted climb: the next choke samples a rate we reached.
            c.on_ack(18_000, None);
            link_choke(&mut c, start, &mut t, second);
            c.link_cap.kbps()
        };
        assert_eq!(latched(11_500), Some(11_500 - 1_150));
        assert_eq!(latched(5_000), None, "half the rate is a different wall");
    }

    /// A measurement latches the cap on its own — it offered the link far
    /// more than the session will and watched it refuse — and on the long
    /// clock, because it holds its own margin back. The ceiling rides the
    /// lift, or the wall it set would bind forever.
    #[test]
    fn a_measured_wall_latches_the_cap_and_lets_the_ceiling_follow() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(60_000);
        let start = Instant::now();
        // 0.7 of a 10 000 kbps reading, as the ramp licenses it.
        c.note_measured_wall(7_000, 10_000);
        assert_eq!(c.link_cap.kbps(), Some(7_000), "one reading is enough");
        assert_eq!(
            c.link_cap.reprobe_after(),
            CAP_REPROBE_WINDOWS_MAX,
            "and it is re-asked on the long clock"
        );
        c.set_ceiling(7_000);
        c.on_ack(7_000, None);
        // Parked at it, nothing may ask for more.
        let mut t = 0;
        for _ in 0..CAP_REPROBE_WINDOWS_MAX - 1 {
            if let Some(k) = run_clean(&mut c, start, t, 1) {
                assert!(k <= 7_000, "asked {k} above a 7 000 kbps wall");
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert_eq!(c.current_kbps, 7_000);
        // The clock runs out: the wall is asked again and the ceiling follows.
        let asked = until_request(&mut c, start, &mut t, 0, 8);
        assert_eq!(c.link_cap.kbps(), Some(7_875), "+12.5 %");
        assert!(
            asked.is_some_and(|k| k > 7_000),
            "the session must follow the lift: {asked:?}"
        );
    }

    /// An early lift is one step at the next clean window, and the ladder
    /// behind it is the short clock: a measured wall's long one was the margin
    /// it held back, which is exactly what the outside evidence disputes.
    #[test]
    fn an_early_lift_takes_one_step_and_puts_the_ladder_on_the_short_clock() {
        let mut cap = LearnedCap::new();
        cap.latch_measured(8_000, FLOOR_KBPS);
        cap.lift_now();
        assert_eq!(
            cap.on_window(false, false, 8_000, 40_000),
            Some((8_000, 9_000))
        );
        assert_eq!(cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);
        for _ in 0..CAP_REPROBE_WINDOWS_MIN - 1 {
            assert_eq!(cap.on_window(false, false, 9_000, 40_000), None);
        }
        assert!(cap.on_window(false, false, 9_000, 40_000).is_some());
    }

    /// A wall that licenses more than the stream can use is not a limit on
    /// this session, and putting a re-probe ladder on one would be a
    /// slow-link rule reaching a link with room.
    #[test]
    fn a_measured_wall_above_the_stream_cap_teaches_no_cap() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(60_000);
        c.note_measured_wall(60_000, 85_000);
        assert!(c.link_cap.kbps().is_none());
        c.note_measured_wall(59_000, 84_000);
        assert_eq!(c.link_cap.kbps(), Some(59_000), "a hair under it is");
    }

    /// A wall that moved down is not the old one standing again: the cap
    /// follows it and its clock starts over, so a link that degrades twice is
    /// not re-tested minutes after it recovers. A wall that re-asserts itself
    /// at the same rate does back the clock off.
    #[test]
    fn a_wall_that_moved_down_restarts_the_caps_clock() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        c.note_link_mark(12_000);
        c.note_link_mark(11_500);
        assert_eq!(c.link_cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);
        c.note_link_mark(11_000);
        assert_eq!(
            c.link_cap.reprobe_after(),
            CAP_REPROBE_WINDOWS_MIN * 2,
            "the same wall again is a standing limit"
        );
        // A third of the rate: a different wall.
        c.note_link_mark(4_000);
        assert_eq!(c.link_cap.kbps(), Some(3_600));
        assert_eq!(c.link_cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);
    }

    /// The wall is a property of the link, not of the mode: a resolution
    /// change drops the encoder and decoder knees and keeps this.
    #[test]
    fn a_mode_switch_keeps_the_link_cap() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        let mut t = 0;
        let k = link_choke(&mut c, start, &mut t, 12_000).expect("a cut");
        c.on_ack(k, None);
        c.on_ack(18_000, None);
        link_choke(&mut c, start, &mut t, 11_500);
        let cap = c.link_cap.kbps().expect("latched");
        c.on_mode_switch();
        assert_eq!(c.link_cap.kbps(), Some(cap));
        assert!(c.decode.cap.kbps().is_none());
    }

    /// The climb holds under a latched wall instead of walking into it.
    #[test]
    fn the_climb_holds_under_the_link_cap() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        let mut t = 0;
        let k = link_choke(&mut c, start, &mut t, 12_000).expect("a cut");
        c.on_ack(k, None);
        c.on_ack(18_000, None);
        link_choke(&mut c, start, &mut t, 11_500);
        let cap = c.link_cap.kbps().expect("latched");
        c.on_ack(cap, None);
        // Clean, fully utilised windows for the whole re-probe interval bar
        // one: nothing may ask for more than the cap.
        for _ in 0..CAP_REPROBE_WINDOWS_MIN - 1 {
            if let Some(k) = run_clean(&mut c, start, t, 1) {
                assert!(k <= cap, "asked {k} above a {cap} kbps wall");
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert_eq!(c.current_kbps, cap);
    }

    /// A link verdict is held to today because nothing knows where the wall
    /// is. A latched cap does, and the doubling cannot pass it — so a session
    /// left far under it comes back in seconds.
    #[test]
    fn a_known_wall_gives_slow_start_back_under_it() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        let mut t = 0;
        let k = link_choke(&mut c, start, &mut t, 12_000).expect("a cut");
        c.on_ack(k, None);
        c.on_ack(18_000, None);
        link_choke(&mut c, start, &mut t, 11_500);
        assert!(!c.probing, "the link's verdict ended slow start");
        // A cascade left the session far under the wall it knows about.
        c.on_ack(3_000, None);
        clean_run(&mut c, start, &mut t, CLEAN_WINDOWS_TO_REARM);
        assert!(
            c.probing,
            "a session under a known wall must double back to it"
        );
    }

    #[test]
    fn two_identical_short_acks_latch_the_host_cap() {
        // Two identical short acks latch the host cap; climbs stop poking it.
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        // One short ack is not a cap.
        c.on_ack(794_000, None);
        assert!(c.host_cap.kbps().is_none());
        // Second identical short ack: latch.
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000, None);
        assert_eq!(c.host_cap.kbps(), Some(794_000));
        // Parked at the cap: no more requests.
        assert_eq!(run_clean(&mut c, start, 20, 12), None);
    }

    #[test]
    fn one_short_ack_is_a_transient_not_a_cap() {
        // One short ack (failed rebuild) must not latch.
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(400_000, None); // failed rebuild kept the old rate
        assert!(c.host_cap.kbps().is_none());
        // Full grant: streak broken, no cap.
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(800_000));
        c.on_ack(800_000, None);
        assert!(c.host_cap.kbps().is_none());
    }

    /// *Encoder limit* is the reason today's nameless short ack always meant,
    /// so it must latch exactly as an unnamed one does.
    #[test]
    fn an_encoder_limit_latches_the_cap_like_an_unnamed_short_ack() {
        let mut named = BitrateController::new(400_000, None);
        let mut unnamed = BitrateController::new(400_000, None);
        let start = Instant::now();
        for (c, why) in [
            (&mut named, Some(AckReason::EncoderLimit)),
            (&mut unnamed, None),
        ] {
            c.set_ceiling(1_400_000);
            assert_eq!(run_clean(c, start, 0, 1), Some(800_000));
            c.on_ack(794_000, why);
            assert_eq!(run_clean(c, start, 10, 1), Some(1_400_000));
            c.on_ack(794_000, why);
        }
        assert_eq!(named.host_cap.kbps(), Some(794_000));
        assert_eq!(named.host_cap.kbps(), unnamed.host_cap.kbps());
        assert_eq!(named.current_kbps, unnamed.current_kbps);
    }

    /// A cadence refusal is the host's GPU, not a rate its encoder cannot
    /// hold: no cap, and climbs wait on the stand-down clock instead of a
    /// re-probe ladder.
    #[test]
    fn a_cadence_refusal_holds_climbs_and_latches_nothing() {
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000, Some(AckReason::Cadence));
        c.on_ack(794_000, Some(AckReason::Cadence));
        assert!(c.host_cap.kbps().is_none(), "a busy GPU is not a cap");
        assert_eq!(c.current_kbps, 794_000, "the clamp is still authoritative");
        // Held for the clean run, then asked again — no cap to crawl past.
        let held = CAP_REPROBE_WINDOWS_MIN - 1;
        assert_eq!(run_clean(&mut c, start, 10, held), None);
        assert!(run_clean(&mut c, start, 10 + held, 4).is_some_and(|k| k > 794_000));
    }

    /// PyroWave is per-frame CBR: there is no rate to control, so the
    /// controller retires the way an unanswered host retires it.
    #[test]
    fn a_pinned_session_turns_the_controller_off() {
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(400_000, Some(AckReason::Pinned));
        assert_eq!(c.current_kbps, 400_000);
        assert_eq!(run_clean(&mut c, start, 10, 40), None);
        assert!(c.host_cap.kbps().is_none(), "nothing to learn from a pin");
    }

    #[test]
    fn mode_switch_clears_the_learned_cap() {
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000, None);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000, None);
        assert_eq!(c.host_cap.kbps(), Some(794_000));
        // Mode-scoped cap drops; probe-measured link ceiling survives.
        c.on_mode_switch();
        assert!(c.host_cap.kbps().is_none());
        assert_eq!(c.ceiling_kbps, 1_400_000);
    }

    #[test]
    fn learned_cap_reprobes_after_a_sustained_clean_run() {
        // After a clean run parked at the cap, lift one step.
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000, None);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000, None);
        assert_eq!(c.host_cap.kbps(), Some(794_000));
        // First re-probe is the fast interval.
        assert_eq!(c.host_cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);
        for i in 0..CAP_REPROBE_WINDOWS_MIN {
            let _ = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 20 + i))
            });
        }
        assert_eq!(c.host_cap.kbps(), Some(794_000 + 794_000 / 8));
    }

    #[test]
    fn a_transient_refusal_does_not_pin_the_session() {
        // Transient cadence refusal at the start rate must not pin the session.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        let start = Instant::now();
        let mut tick = 0u32;
        let mut windows_pinned = 0u32;
        // Two refused climbs at the same rate latch 20 Mbps.
        for _ in 0..2 {
            let k = run_clean(&mut c, start, tick, 4).expect("slow start should ask to climb");
            tick += 4;
            assert!(k > 20_000);
            c.on_ack(20_000, None);
        }
        assert_eq!(c.host_cap.kbps(), Some(20_000));
        // Host recovered; grant whatever the re-probe asks.
        while c.current_kbps < 150_000 && windows_pinned < 400 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, tick))
            }) {
                c.on_ack(k, None);
            }
            tick += 1;
            windows_pinned += 1;
        }
        assert!(
            c.current_kbps >= 150_000,
            "still pinned at {} after {windows_pinned} windows",
            c.current_kbps
        );
        // 40 windows × 750 ms ≈ 30 s.
        assert!(
            windows_pinned <= 40,
            "took {windows_pinned} windows (~{} s) to escape a transient refusal",
            windows_pinned * 3 / 4
        );
        // Disproven cap is gone, not nudged.
        assert!(c.host_cap.kbps().is_none());
    }

    #[test]
    fn a_standing_cap_backs_its_reprobe_clock_off() {
        // Standing encoder ceiling: each re-learn doubles the re-probe interval.
        let mut c = BitrateController::new(400_000, None);
        c.set_ceiling(1_400_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(800_000));
        c.on_ack(794_000, None);
        assert_eq!(run_clean(&mut c, start, 10, 1), Some(1_400_000));
        c.on_ack(794_000, None);
        assert_eq!(c.host_cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);
        // Park, lift, refuse at the same value: standing, so the clock doubles.
        let mut tick = 20;
        for round in 0..3 {
            let before = c.host_cap.reprobe_after();
            for _ in 0..before {
                let _ = c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, tick))
                });
                tick += 1;
            }
            let lifted = c.host_cap.kbps().expect("cap should still be latched");
            assert!(lifted > 794_000, "round {round}: the re-probe never lifted");
            // Host clamps the lift back to its real ceiling.
            c.acks.last_requested_kbps = Some(lifted);
            c.on_ack(794_000, None);
            assert_eq!(c.host_cap.kbps(), Some(794_000));
            assert_eq!(
                c.host_cap.reprobe_after(),
                (before * 2).min(CAP_REPROBE_WINDOWS_MAX)
            );
        }
    }

    #[test]
    fn a_stood_down_encode_signal_re_arms_after_a_clean_run() {
        // Stand-down is evidence: a clean run must re-arm the encode signal.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        assert_eq!(c.encode.down.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);

        // One window short of the run is not enough.
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        assert!(c.encode.down.disarmed());
        clean_run(&mut c, start, &mut tick, 1);
        assert!(!c.encode.down.disarmed());

        // Re-armed: a fresh excursion backs off.
        assert!(encode_choke(&mut c, start, &mut tick, 40_000).is_some());
    }

    #[test]
    fn a_standing_contention_backs_the_re_arm_clock_off() {
        // Re-silenced after re-arm: standing, so the clock doubles. Start
        // high enough that two ratchets stay above [`FLOOR_KBPS`].
        let mut c = BitrateController::new(200_000, None);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN);
        assert!(!c.encode.down.disarmed());
        // Re-armed; contention still there.
        disarm_encode(&mut c, start, &mut tick);
        assert_eq!(c.encode.down.reprobe_after(), CAP_REPROBE_WINDOWS_MIN * 2);
    }

    #[test]
    fn a_bad_window_restarts_the_re_arm_run() {
        // A spoiled window says nothing about the encoder; restart the run.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let mut tick = 0;
        disarm_encode(&mut c, start, &mut tick);
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        let at = ticks(start, tick);
        tick += 1;
        // Flush: severe ×0.7 and resets the clean run.
        assert!(c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(at)
            })
            .is_some());
        clean_run(&mut c, start, &mut tick, CAP_REPROBE_WINDOWS_MIN - 1);
        assert!(
            c.encode.down.disarmed(),
            "the spoiled window must restart the run"
        );
        clean_run(&mut c, start, &mut tick, 1);
        assert!(!c.encode.down.disarmed());
    }

    #[test]
    fn an_unanswered_encode_notch_is_given_back_and_disarms_the_driver() {
        // GPU contention holds encode time up whatever the rate: one notch,
        // the rate back, and the driver stood down — not three ×0.7 cuts.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let mut tick = 0;
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), Some(17_500));
        c.on_ack(17_500, None);
        assert_eq!(
            encode_windows(&mut c, start, &mut tick, 20_000, 8),
            Some(20_000),
            "the encoder did not follow, so the notch is given back"
        );
        c.on_ack(20_000, None);
        assert!(c.encode.down.disarmed());

        // Same excursion no longer moves the rate…
        assert_eq!(encode_choke(&mut c, start, &mut tick, 20_000), None);
        // …and the session climbs out instead of parking.
        c.set_ceiling(200_000);
        assert!(
            run_clean(&mut c, start, tick, 8).is_some_and(|k| k > 20_000),
            "a disarmed encode signal must not keep the session pinned"
        );
    }

    #[test]
    fn an_encode_notch_the_encoder_answers_is_kept_and_may_be_followed() {
        // Encode time that falls with the rate is a real knee: keep the notch,
        // and let the next rise take another until the encoder keeps up.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let mut tick = 0;
        assert_eq!(encode_choke(&mut c, start, &mut tick, 40_000), Some(17_500));
        c.on_ack(17_500, None);
        // Five windows: the verdict lands on the third, before a climb can.
        assert_eq!(
            encode_windows(&mut c, start, &mut tick, 22_000, 5),
            None,
            "a notch the encoder answered asks for nothing back"
        );
        assert!(!c.encode.down.disarmed());
        assert!(c.encode.probe.is_none(), "and the step is settled");
        let next = encode_choke(&mut c, start, &mut tick, 40_000)
            .expect("the next rise takes the next notch");
        assert_eq!(next, c.current_kbps - c.current_kbps / 8);
    }

    #[test]
    fn a_network_driven_backoff_is_not_the_encoder_s() {
        // Encode time is elevated, but a flush explains the window: ×0.7 on
        // the link's evidence, and no notch to judge.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let mut tick = 0;
        for _ in 0..BASELINE_MIN_WINDOWS {
            let at = ticks(start, tick);
            tick += 1;
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(7_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(at)
                }),
                None
            );
        }
        let at = ticks(start, tick);
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(20_000),
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(at)
            }),
            Some(14_000)
        );
        assert!(c.encode.probe.is_none());
        assert!(!c.encode.down.disarmed());
    }

    #[test]
    fn capture_stall_windows_never_latch_a_decode_cap() {
        // Repeated stall-shaped backoffs at the same rate must not latch a knee.
        let mut c = BitrateController::new(240_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        climb_to(&mut c, start, &mut t, 400_000);
        let at = c.current_kbps;
        let r1 = stall_choke(&mut c, start, &mut t).expect("stall damage still backs off");
        assert!(
            c.decode.cap.kbps().is_none(),
            "one starved window must not latch"
        );
        assert_eq!(
            c.decode.backoff_kbps, 0,
            "a starved window is not a knee sample — no reference recorded"
        );
        c.on_ack(r1, None);
        climb_to(&mut c, start, &mut t, at - at / DECODE_CAP_SIMILAR_DIV);
        let r2 = stall_choke(&mut c, start, &mut t).expect("second stall edge backs off too");
        c.on_ack(r2, None);
        assert!(
            c.decode.cap.kbps().is_none(),
            "a starved pair at the same rate must not latch a phantom knee"
        );
    }

    #[test]
    fn starved_window_preserves_the_knee_reference() {
        // Real knee, then stall, then re-climb choke: stall neither latches nor erases.
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        let knee = c.current_kbps;
        let r1 = choke(&mut c, start, &mut t).expect("real choke backs off");
        assert_eq!(
            c.decode.backoff_kbps, knee,
            "real choke records the reference"
        );
        c.on_ack(r1, None);
        climb_to(&mut c, start, &mut t, knee - knee / DECODE_CAP_SIMILAR_DIV);
        let r2 = stall_choke(&mut c, start, &mut t).expect("stall edge backs off");
        assert_eq!(
            c.decode.backoff_kbps, knee,
            "the starved window must not erase the real reference"
        );
        assert!(
            c.decode.cap.kbps().is_none(),
            "and must not latch against it"
        );
        c.on_ack(r2, None);
        climb_to(&mut c, start, &mut t, knee - knee / DECODE_CAP_SIMILAR_DIV);
        let rate = c.current_kbps;
        choke(&mut c, start, &mut t).expect("genuine re-climb choke backs off");
        assert_eq!(
            c.decode.cap.kbps(),
            Some(rate - rate / 16),
            "the genuine pair still latches around the starved interruption"
        );
    }

    #[test]
    fn decode_cap_latches_when_the_reclimb_chokes_at_the_same_knee() {
        // Choke, recover, re-climb, choke inside the band: latch.
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        latch_knee(&mut c, start, &mut t);
        // Climbs must stop at the knee, not the 900 Mbps link ceiling.
        let mut max_req = 0;
        for _ in 0..62 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, t))
            }) {
                // Cap in force at decision time. Re-probe may lift it; the link
                // ceiling must not.
                assert!(
                    k <= c.decode.cap.kbps().unwrap(),
                    "climb past the decode cap: {k}"
                );
                max_req = max_req.max(k);
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert!(
            max_req < 600_000,
            "the decode knee stopped binding: climbed to {max_req}"
        );
    }

    #[test]
    fn a_single_flush_or_dissimilar_backoffs_never_latch_a_decode_cap() {
        // Lone flush at a climbed-to rate backs off but teaches nothing…
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let r1 = c
            .on_window(&WindowSample {
                actual_kbps: 490_000,
                flushed: true,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush must back off");
        assert_eq!(r1, 350_000);
        assert!(c.decode.cap.kbps().is_none());
        c.on_ack(r1, None);
        // …loss-driven backoff at the re-climbed rate breaks the streak…
        climb_to(&mut c, start, &mut t, 460_000);
        t += 2;
        let r2 = c
            .on_window(&WindowSample {
                dropped: 1,
                actual_kbps: c.current_kbps,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("loss must back off");
        t += 1;
        assert!(c.decode.cap.kbps().is_none());
        assert_eq!(
            c.decode.backoff_kbps, 0,
            "a climbed-to non-decode backoff must reset the knee reference"
        );
        c.on_ack(r2, None);
        // …next flush is a first decode event again — still no latch…
        climb_to(&mut c, start, &mut t, 460_000);
        t += 2;
        let r3 = c
            .on_window(&WindowSample {
                actual_kbps: c.current_kbps,
                flushed: true,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush must back off");
        t += 1;
        assert!(c.decode.cap.kbps().is_none());
        c.on_ack(r3, None);
        // …dissimilar climbed-to rates share no knee.
        let dissimilar_target = c.current_kbps + 20_000;
        climb_to(&mut c, start, &mut t, dissimilar_target);
        t += 2;
        let _ = c
            .on_window(&WindowSample {
                actual_kbps: c.current_kbps,
                flushed: true,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush must back off");
        assert!(c.decode.cap.kbps().is_none());
    }

    #[test]
    fn decode_cap_reprobes_after_a_sustained_clean_run() {
        // After a clean run parked at the cap, lift +12.5 %.
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let knee = latch_knee(&mut c, start, &mut t);
        // Host parks at the knee (unsolicited re-target).
        c.on_ack(knee, None);
        for _ in 0..CAP_REPROBE_WINDOWS_MIN {
            let _ = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 490_000,
                ..WindowSample::at(ticks(start, t))
            });
            t += 1;
        }
        assert_eq!(c.decode.cap.kbps(), Some(knee + knee / 8));
    }

    #[test]
    fn mode_switch_clears_the_decode_cap() {
        // Decode cap is mode-scoped; probe-measured link ceiling survives.
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        let _ = latch_knee(&mut c, start, &mut t);
        c.on_mode_switch();
        assert!(c.decode.cap.kbps().is_none());
        assert_eq!(c.ceiling_kbps, 900_000);
    }

    #[test]
    fn ordinary_decode_bad_window_pairs_latch_the_knee_field_trace() {
        // Ordinary two-window decode rise (15–45 ms) must latch, not reset the streak.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(657_788);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        // One heavy-loss window ends slow start so the climb is additive.
        let _ = c.on_window(&WindowSample {
            loss_ppm: HEAVY_LOSS_PPM,
            owd_mean_us: Some(10_000),
            decode_mean_us: Some(8_000),
            actual_kbps: 15_000,
            ..WindowSample::at(ticks(start, t))
        });
        t += 1;
        // First sample: flush + 40 ms decode.
        climb_to(&mut c, start, &mut t, 417_277);
        let first = c.current_kbps;
        t += 2;
        let r1 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(8_313),
                decode_mean_us: Some(40_087),
                actual_kbps: first,
                flushed: true,
                recovery_kf: 1,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("flush choke must back off");
        t += 1;
        assert!(c.decode.cap.kbps().is_none());
        assert_eq!(c.decode.backoff_kbps, first);
        c.on_ack(r1, None);
        // Two consecutive ~26 ms decode-bad windows: ordinary path, latch.
        climb_to(&mut c, start, &mut t, 440_000);
        let second = c.current_kbps;
        t += 2;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(6_877),
                decode_mean_us: Some(26_474),
                actual_kbps: second,
                ..WindowSample::at(ticks(start, t))
            }),
            None,
            "the first bad window must not decide"
        );
        t += 1;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(6_877),
                decode_mean_us: Some(26_474),
                actual_kbps: second,
                ..WindowSample::at(ticks(start, t))
            }),
            Some(((second as u64 * 7 / 10) as u32).max(FLOOR_KBPS))
        );
        assert_eq!(
            c.decode.cap.kbps(),
            Some(second - second / 16),
            "two decode-bad windows are knee evidence"
        );
    }

    #[test]
    fn cascade_backoffs_neither_sample_nor_erase_the_knee_reference() {
        // Drain backoff at the already-reduced rate must neither latch nor
        // erase; the re-climb choke latches against the original sample.
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        let r1 = choke(&mut c, start, &mut t).expect("knee choke must back off");
        assert_eq!(c.decode.backoff_kbps, 500_000);
        c.on_ack(r1, None);
        t += 2;
        let r2 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(43_305),
                actual_kbps: r1,
                flushed: true,
                recovery_kf: 1,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("drain flush must back off");
        t += 1;
        assert!(
            c.decode.cap.kbps().is_none(),
            "a drain backoff must not latch"
        );
        assert_eq!(
            c.decode.backoff_kbps, 500_000,
            "…nor erase the knee reference"
        );
        c.on_ack(r2, None);
        climb_to(&mut c, start, &mut t, 460_000);
        let rate = c.current_kbps;
        choke(&mut c, start, &mut t).expect("re-climb choke must back off");
        assert_eq!(c.decode.cap.kbps(), Some(rate - rate / 16));
    }

    #[test]
    fn keyframe_storms_on_a_clean_link_latch_the_knee() {
        // Kf-storm on a clean link (no decode latency) is decode evidence.
        let mut c = BitrateController::new(300_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        t += 2;
        let r1 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 300_000,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("keyframe storm must back off");
        t += 1;
        assert!(c.decode.cap.kbps().is_none());
        c.on_ack(r1, None);
        climb_to(&mut c, start, &mut t, 280_000);
        let rate = c.current_kbps;
        t += 2;
        let _ = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: rate,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("second storm must back off");
        assert_eq!(c.decode.cap.kbps(), Some(rate - rate / 16));
    }

    #[test]
    fn keyframe_storms_with_real_loss_teach_no_knee() {
        // Same storm with heavy loss is network-attributed: reset, no latch.
        let mut c = BitrateController::new(300_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        t += 2;
        let r1 = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 300_000,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("clean storm must back off");
        t += 1;
        assert_eq!(c.decode.backoff_kbps, 300_000);
        c.on_ack(r1, None);
        climb_to(&mut c, start, &mut t, 280_000);
        t += 2;
        let _ = c
            .on_window(&WindowSample {
                loss_ppm: SEVERE_LOSS_PPM,
                owd_mean_us: Some(10_000),
                actual_kbps: c.current_kbps,
                recovery_kf: RECOVERY_KF_SEVERE,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("lossy storm must back off");
        assert!(c.decode.cap.kbps().is_none());
        assert_eq!(
            c.decode.backoff_kbps, 0,
            "a loss-attributed storm must reset the knee reference"
        );
    }

    #[test]
    fn a_mixed_streak_without_decode_attribution_is_no_knee_evidence() {
        // Mixed streak (one OWD, one decode): not a knee sample.
        let mut c = BitrateController::new(500_000, None);
        c.set_ceiling(900_000);
        let start = Instant::now();
        let mut t = 0;
        for _ in 0..4 {
            calm_window(&mut c, ticks(start, t));
            t += 1;
        }
        t += 2;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(40_000),
                decode_mean_us: Some(8_000),
                actual_kbps: 490_000,
                ..WindowSample::at(ticks(start, t))
            }),
            None,
            "one OWD-bad window must not decide"
        );
        t += 1;
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(26_000),
                actual_kbps: 490_000,
                ..WindowSample::at(ticks(start, t))
            }),
            Some(350_000)
        );
        assert!(c.decode.cap.kbps().is_none());
        assert_eq!(
            c.decode.backoff_kbps, 0,
            "a mixed-attribution backoff must reset the knee reference"
        );
    }
}
