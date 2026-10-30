//! The client decoder's knee: the rate past which decoding eats the frame
//! budget.
//!
//! Two decode-driven backoffs at a similar rate latch the cap. On a clean
//! full-rate window past the decoder's warm-up the bands park at
//! [`DECODE_HOLD_PCT`] of the budget and retreat a notch at
//! [`DECODE_RETREAT_PCT`]. Every step — a retreat, a ×0.7 the decode verdict
//! named, a lift on the cap's clock — is kept only if the decoder's latency
//! follows the rate against its level at the rate the step left; one that does
//! not stands the headroom driver down. A retreat is answered against the
//! level where its run began: a notch shrinks with the rate and a decoder's
//! fixed cost does not, so a last notch alone can read as nothing on a decoder
//! that answered every one before it. A ×0.7 the keyframe asks named is kept
//! only if the asks stop at the new rate; one they outlive comes back, and the
//! verdict reads no asks until they stop. A mode switch resets all of it to
//! [`DecodeKnee::default`].

use super::{Rate, StepAnswer, ANSWER_PCT, BAD_WINDOWS_TO_DECREASE, DECODE_CAP_SIMILAR_DIV};
use crate::abr::cap::{LearnedCap, Level, StandDown, StepProbe};
use crate::abr::sample::{self, WindowActivity, WindowSample};
use crate::abr::verdict::{Reason, Verdict, HEAVY_LOSS_PPM, RECOVERY_KF_BAD};

/// Decode headroom, judged on clean full-rate windows against the frame
/// budget. Received → decoded includes the queue wait, so a mean near the
/// period is a decoder with no slack: the next jitter is a missed vsync. At
/// 80 % climbs park; at 90 % the rate retreats a notch, and the decoder's
/// answer decides whether the retreat stands.
const DECODE_HOLD_PCT: i64 = 80;
const DECODE_RETREAT_PCT: i64 = 90;
/// A window is full-rate for the headroom judgement at ≥ ¾ of the refresh's
/// frames. Fewer frames share the same bits, so a low-fps decode mean
/// overstates the full-rate load.
const DECODE_FULL_RATE_NUM: i64 = 3;
const DECODE_FULL_RATE_DEN: i64 = 4;
/// Windows with decoded frames in them that the bands ignore at the start of a
/// mode. A hardware decoder warms up over its first seconds; 4 x 750 ms is 3 s.
pub(super) const DECODE_WARMUP_WINDOWS: u32 = 4;

/// A headroom step awaiting the decoder's answer at its new rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DecodeProbe {
    kind: DecodeProbeKind,
    step: StepProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeProbeKind {
    Retreat,
    Lift,
    /// A ×0.7 the decode verdict named. `leveled`: taken at a rate with a
    /// level, so it is answered as a retreat is. Without one the verdict
    /// answers first: a window it cleared is "followed". Still decode-bad, it
    /// is answered against the window that tripped it, and restored only if
    /// latency did not fall: a ×0.7 can't clear a decoder far past its knee.
    Cut {
        leveled: bool,
    },
    /// A ×0.7 the keyframe asks named, answered by the asks: a window at the
    /// new rate under [`RECOVERY_KF_BAD`] is "followed", one still at it "did
    /// not follow". A window the link damaged asks for its own frames.
    Asks,
}

/// What this mode has taught about the client decoder.
pub(in crate::abr) struct DecodeKnee {
    /// Two consecutive decode-driven backoffs at a similar rate. Without it a
    /// decoder knee below the link ceiling is a 30–60 s sawtooth.
    pub(in crate::abr) cap: LearnedCap,
    /// Previous decode-driven backoff's pre-backoff rate (`0` = last backoff
    /// was not decode-driven). One spurious flush teaches nothing.
    pub(in crate::abr) backoff_kbps: u32,
    /// Decode-flagged windows in the current bad streak. The deciding window
    /// alone would drop the first ordinary-bad window's attribution.
    pub(super) streak_windows: u32,
    /// `current_kbps` has risen (ack) since the last backoff. A cascade's
    /// second backoff sits at ×0.7 — not a knee sample, and must not erase
    /// the reference.
    pub(super) climb_since_backoff: bool,
    /// A retreat or a cap lift waiting for the decoder's answer.
    pub(super) probe: Option<DecodeProbe>,
    /// The rate a retreat run left and that rate's level, which answer every
    /// notch of the run. A rate above it starts a new run.
    run: Option<(u32, i64)>,
    /// Decode latency did not follow the rate: hold and retreat stand down
    /// until a clean run re-probes them.
    pub(super) headroom: StandDown,
    /// Keyframe asks did not follow the rate: the verdict reads none until a
    /// run without them re-arms it.
    pub(super) asks: StandDown,
    /// Decode latency over the last clean windows past the warm-up: what a
    /// retreat or a lift is answered against.
    level: Level,
    /// Windows with decoded frames in them since the mode began.
    decoded_windows: u32,
}

impl Default for DecodeKnee {
    /// Nothing learned. The rate in hand was held, not drained to, so the
    /// first backoff is a legitimate knee sample.
    fn default() -> Self {
        DecodeKnee {
            cap: LearnedCap::new(),
            backoff_kbps: 0,
            streak_windows: 0,
            climb_since_backoff: true,
            probe: None,
            run: None,
            headroom: StandDown::new(),
            asks: StandDown::new(),
            level: Level::default(),
            decoded_windows: 0,
        }
    }
}

/// Enough frames for the window's decode mean to describe the full-rate
/// load: ≥ ¾ of the refresh's frames. Fewer share the same bits.
fn full_rate(w: &WindowSample, budget_us: i64) -> bool {
    match w.activity {
        WindowActivity::Active(n) if budget_us > 0 => {
            i64::from(n) * DECODE_FULL_RATE_DEN
                >= (sample::WINDOW_US / budget_us) * DECODE_FULL_RATE_NUM
        }
        _ => true,
    }
}

impl DecodeKnee {
    /// A cap lift that lost the decoder its headroom: back to the cap, and
    /// the next lift waits twice as long.
    fn undo_lift(&mut self, p: DecodeProbe, decode_us: i64, floor_kbps: u32) {
        self.cap.latch(p.step.from_kbps, floor_kbps);
        tracing::info!(
            cap_kbps = p.step.from_kbps,
            decode_us,
            was_us = p.step.ref_us,
            reprobe_after_windows = self.cap.reprobe_after(),
            "adaptive bitrate: decode cap lift undone — the decoder lost its headroom"
        );
    }

    /// The decoder's answer to a headroom step, one window at a time, damaged
    /// windows included: the latency at the new rate is the answer whatever
    /// else the window carried. A quiet or starved window's mean describes an
    /// interruption, a flush's the jump-to-live and a low-fps window's a
    /// bigger frame; none is a reading at the new rate, but time passes. An
    /// ask step reads the window's asks, and only where the link lost nothing.
    /// `None` while the step is still waiting.
    pub(super) fn judge_step(
        &mut self,
        w: &WindowSample,
        v: &Verdict,
        budget_us: i64,
        rate: Rate,
    ) -> Option<StepAnswer> {
        let mut p = self.probe?;
        let at_new_rate = match p.kind {
            DecodeProbeKind::Retreat | DecodeProbeKind::Cut { .. } | DecodeProbeKind::Asks => {
                rate.kbps < p.step.from_kbps
            }
            DecodeProbeKind::Lift => rate.kbps > p.step.from_kbps,
        };
        let readable = at_new_rate && !v.quiet && !v.starved && !w.flushed;
        let asks = (p.kind == DecodeProbeKind::Asks
            && readable
            && w.dropped == 0
            && w.loss_ppm < HEAVY_LOSS_PPM
            && !v.owd_bad)
            .then_some(w.recovery_kf);
        if asks.is_some_and(|n| n < RECOVERY_KF_BAD) {
            self.probe = None;
            tracing::info!(
                from_kbps = p.step.from_kbps,
                at_kbps = rate.kbps,
                "adaptive bitrate: the keyframe asks ended at the lower rate — cut kept"
            );
            return Some(StepAnswer::Followed);
        }
        if p.kind == (DecodeProbeKind::Cut { leveled: false }) && readable && !v.decode_bad {
            if !full_rate(w, budget_us) {
                return None;
            }
            // The verdict ended at the new rate: the cut answered.
            self.probe = None;
            tracing::info!(
                from_kbps = p.step.from_kbps,
                at_kbps = rate.kbps,
                decode_us = v.decode_mean_us.unwrap_or(-1),
                "adaptive bitrate: the decode verdict ended at the lower rate — cut kept"
            );
            return Some(StepAnswer::Followed);
        }
        // A cut counts the windows still over the verdict, not their means.
        let mean_us = match p.kind {
            DecodeProbeKind::Asks => asks.map(i64::from),
            _ => v.decode_mean_us.filter(|_| {
                readable
                    && (matches!(p.kind, DecodeProbeKind::Cut { .. }) || full_rate(w, budget_us))
            }),
        };
        let verdict = p.step.note(mean_us);
        let expired = p.step.expired();
        self.probe = Some(p);
        let Some(mean) = verdict else {
            if expired {
                self.probe = None; // the step never reached its rate
            }
            return None;
        };
        self.probe = None;
        Some(self.settle(p, mean, budget_us, rate))
    }

    /// The decoder's answer to a step, read on each window that reached the
    /// new rate against the level at the rate it left. A retreat, or a cut
    /// taken with a level, is kept when every window fell [`ANSWER_PCT`] of
    /// the budget under it; a lift is undone when every window rose that far,
    /// or sat in the retreat band. A retreat or a cut it did not answer stands
    /// the driver down, and an ask cut the asks: the cap the bands parked and
    /// the cuts they named were not the rate's either.
    fn settle(&mut self, p: DecodeProbe, mean: i64, budget_us: i64, rate: Rate) -> StepAnswer {
        let answer_us = budget_us * ANSWER_PCT / 100;
        match p.kind {
            DecodeProbeKind::Retreat | DecodeProbeKind::Cut { .. } if p.step.fell(answer_us) => {
                tracing::info!(
                    from_kbps = p.step.from_kbps,
                    at_kbps = rate.kbps,
                    decode_us = mean,
                    was_us = p.step.ref_us,
                    "adaptive bitrate: decode latency followed the rate down — step kept"
                );
                StepAnswer::Followed
            }
            DecodeProbeKind::Retreat | DecodeProbeKind::Cut { .. } => {
                // Latency is not a function of the rate: a pipelined decoder,
                // or something else on the SoC.
                self.headroom.disarm();
                self.backoff_kbps = 0;
                self.cap.drop_cap();
                self.run = None;
                tracing::info!(
                    restore_kbps = p.step.from_kbps,
                    decode_us = mean,
                    was_us = p.step.ref_us,
                    rearm_after_windows = self.headroom.reprobe_after(),
                    "adaptive bitrate: decode latency did not follow the rate — restoring it \
                     and standing the headroom driver down (a pipelined decoder sits above \
                     the period with no queue)"
                );
                StepAnswer::StandDown(p.step.from_kbps)
            }
            DecodeProbeKind::Asks => {
                // A rebuilt or stalled decoder asks at any rate.
                self.asks.disarm();
                self.backoff_kbps = 0;
                self.cap.drop_cap();
                tracing::info!(
                    restore_kbps = p.step.from_kbps,
                    asks = mean,
                    rearm_after_windows = self.asks.reprobe_after(),
                    "adaptive bitrate: keyframe asks did not follow the rate — restoring it \
                     and reading none until a run without them"
                );
                StepAnswer::Restore(p.step.from_kbps)
            }
            DecodeProbeKind::Lift => {
                let worse = p.step.rose(answer_us)
                    || p.step.least_us() * 100 / budget_us >= DECODE_RETREAT_PCT;
                if worse {
                    self.undo_lift(p, mean, rate.floor_kbps);
                    return StepAnswer::Restore(p.step.from_kbps);
                }
                tracing::info!(
                    at_kbps = rate.kbps,
                    decode_us = mean,
                    was_us = p.step.ref_us,
                    "adaptive bitrate: decode cap lift held — the decoder kept its headroom"
                );
                StepAnswer::Followed
            }
        }
    }

    /// Decode headroom on a clean loaded full-rate window that carries the
    /// signal, the bands: park at [`DECODE_HOLD_PCT`], retreat a notch at
    /// [`DECODE_RETREAT_PCT`]. `Some` is a rate to request. Loss, flush and
    /// starvation are the link's story, a low-fps window's mean overstates the
    /// load, a decoder inside its warm-up is not at the rate's load yet, and a
    /// step still being judged owns the question. A window whose decode queue
    /// was shed is read whatever its frame count, and a stood-down driver
    /// re-arms only on a run with no shed queue in it. A retreat is answered
    /// against the level where its run began, or this window's at a rate with
    /// none yet.
    pub(super) fn judge_headroom(
        &mut self,
        w: &WindowSample,
        v: &Verdict,
        budget_us: i64,
        rate: Rate,
    ) -> Option<u32> {
        let mean_us = v.decode_mean_us?;
        // A shed queue thins the window's frames; its mean is still the decoder's.
        let loaded = w.backlog || full_rate(w, budget_us);
        if budget_us <= 0 || v.bad || v.quiet || v.starved || !loaded {
            return None;
        }
        if self.probe.is_some() {
            return None;
        }
        if self.headroom.disarmed() {
            if w.backlog {
                self.headroom.note_bad();
            } else if self.headroom.note_clean() {
                tracing::debug!(
                    after_windows = self.headroom.reprobe_after(),
                    "adaptive bitrate: re-arming the decode headroom driver after a clean run"
                );
            }
            return None;
        }
        if !self.warmed_up() {
            return None;
        }
        let pct = mean_us * 100 / budget_us;
        if pct >= DECODE_RETREAT_PCT && rate.kbps > rate.floor_kbps {
            let next = rate.notch();
            self.cap.park(next);
            let (from_kbps, ref_us) = self
                .run
                .filter(|&(from, _)| from >= rate.kbps)
                .unwrap_or((rate.kbps, self.level.before().unwrap_or(mean_us)));
            self.run = Some((from_kbps, ref_us));
            self.probe = Some(DecodeProbe {
                kind: DecodeProbeKind::Retreat,
                step: StepProbe::new(from_kbps, ref_us),
            });
            tracing::info!(
                from_kbps = rate.kbps,
                to_kbps = next,
                decode_us = mean_us,
                budget_us,
                "adaptive bitrate: the decoder has no headroom — retreating a notch, kept only \
                 if its latency follows"
            );
            return Some(next);
        }
        if pct >= DECODE_HOLD_PCT && self.cap.kbps().is_none_or(|c| c > rate.kbps) {
            self.cap.park(rate.kbps);
            tracing::info!(
                cap_kbps = rate.kbps,
                decode_us = mean_us,
                budget_us,
                "adaptive bitrate: decode headroom exhausted — parking here (re-probed after a \
                 clean run)"
            );
        }
        None
    }

    /// A bad window. It counts toward the streak's decode attribution, and a
    /// lift that ran into damage failed. A retreat or a cut is answered by
    /// the latency at its new rate, this window's included.
    pub(super) fn note_bad(&mut self, v: &Verdict, rate: Rate) {
        if v.decode_bad {
            // Counted here: backoff only sees the final window, and the
            // cooldown eats the first ordinary-bad window.
            self.streak_windows += 1;
        }
        self.headroom.note_bad();
        if let Some(p) = self.probe.filter(|p| p.kind == DecodeProbeKind::Lift) {
            self.probe = None;
            if rate.kbps > p.step.from_kbps {
                self.undo_lift(
                    p,
                    v.decode_mean_us.unwrap_or(p.step.ref_us),
                    rate.floor_kbps,
                );
            }
        }
    }

    /// One window on the cap's re-probe clock. A lift above the cap starts
    /// the probe whose answer decides whether it holds, against the cap
    /// rate's level, or this window's at a rate with none yet.
    pub(super) fn tick(&mut self, v: &Verdict, rate_kbps: u32, ceiling_kbps: u32) {
        if let Some((from, to)) = self.cap.on_window(v.bad, v.quiet, rate_kbps, ceiling_kbps) {
            tracing::debug!(
                from_kbps = from,
                to_kbps = to,
                "adaptive bitrate: re-probing above the learned decode cap"
            );
            self.probe = v.decode_mean_us.map(|mean_us| DecodeProbe {
                kind: DecodeProbeKind::Lift,
                step: StepProbe::new(from, self.level.before().unwrap_or(mean_us)),
            });
        }
    }

    /// One scored window, before anything decides on it: the warm-up count,
    /// and this window's latency for the level once the next one opens. Only
    /// a clean full-rate window past the warm-up is a reading of the rate.
    pub(super) fn note_window(
        &mut self,
        w: &WindowSample,
        v: &Verdict,
        budget_us: Option<i64>,
        rate_kbps: u32,
    ) {
        if v.decode_mean_us.is_some() {
            self.decoded_windows = self.decoded_windows.saturating_add(1);
        }
        let clean = !v.bad
            && !v.quiet
            && !v.starved
            && self.warmed_up()
            && budget_us.is_some_and(|b| full_rate(w, b));
        self.level
            .note(v.decode_mean_us.filter(|_| clean), rate_kbps);
    }

    /// Past the first [`DECODE_WARMUP_WINDOWS`] windows with decoded frames.
    fn warmed_up(&self) -> bool {
        self.decoded_windows > DECODE_WARMUP_WINDOWS
    }

    /// A step under the rate it left still owns the decode verdict: this
    /// window's latency is its answer, not a second cut.
    pub(super) fn owns_cut(&self, rate_kbps: u32) -> bool {
        self.probe.is_some_and(|p| rate_kbps < p.step.from_kbps)
    }

    /// The ×0.7 from `from_kbps` the decoder or its keyframe asks named. A
    /// decode cut taken at a rate with a level is answered against it, as the
    /// notch is; without one the verdict at the new rate answers first, then
    /// the window that tripped it. The asks answer for themselves.
    pub(super) fn start_cut(&mut self, from_kbps: u32, v: &Verdict) {
        let level = self.level.before();
        let kind = if v.reason == Reason::KeyframeAsks {
            DecodeProbeKind::Asks
        } else {
            DecodeProbeKind::Cut {
                leveled: level.is_some(),
            }
        };
        self.probe = Some(DecodeProbe {
            kind,
            step: StepProbe::new(from_kbps, level.or(v.decode_mean_us).unwrap_or(0)),
        });
    }

    /// The keyframe asks the verdict reads. Stood down, none: a window at
    /// [`RECOVERY_KF_BAD`] starts the re-arming run over, and a run under it
    /// re-arms them.
    pub(super) fn judged_asks(&mut self, w: &WindowSample) -> u32 {
        if !self.asks.disarmed() {
            return w.recovery_kf;
        }
        if w.recovery_kf >= RECOVERY_KF_BAD {
            self.asks.note_bad();
        } else if self.asks.note_clean() {
            tracing::debug!(
                after_windows = self.asks.reprobe_after(),
                "adaptive bitrate: re-arming the keyframe asks after a run without them"
            );
            return w.recovery_kf;
        }
        0
    }

    /// Two decode-driven backoffs at a similar rate are a knee; a drained or
    /// starved one samples nothing, and neither erases the reference.
    pub(super) fn learn_knee(&mut self, w: &WindowSample, v: &Verdict, rate: Rate) {
        // Decode evidence: severe in this window; every window in the
        // ordinary streak was decode-flagged; kf-storm without heavy loss;
        // flush only if decode is bad or the signal is absent (flat decode
        // + flush is a network event).
        let decode_evidence = v.decode_severe
            || self.streak_windows >= BAD_WINDOWS_TO_DECREASE
            || (w.recovery_kf >= RECOVERY_KF_BAD && w.loss_ppm < HEAVY_LOSS_PPM)
            || (w.flushed && (v.decode_bad || v.decode_mean_us.is_none()));
        if !self.climb_since_backoff {
            // Drain after ×0.7 (~100 ms ack): not a knee sample.
            tracing::debug!(
                at_kbps = rate.kbps,
                reference_kbps = self.backoff_kbps,
                "adaptive bitrate: backoff without an intervening climb — draining the \
                 previous choke, not a knee sample"
            );
        } else if v.starved {
            tracing::debug!(
                at_kbps = rate.kbps,
                actual_kbps = w.actual_kbps,
                reference_kbps = self.backoff_kbps,
                "adaptive bitrate: backoff in a starved window (delivery a fraction of \
                 the target) — starvation-shaped distress, not a knee sample"
            );
        } else if decode_evidence {
            let similar = self.backoff_kbps > 0
                && rate.kbps.abs_diff(self.backoff_kbps)
                    <= self.backoff_kbps / DECODE_CAP_SIMILAR_DIV;
            // Latch just under the choke rate: a cap on the knee authorizes
            // climbing straight back into it. 1/16 is inside the ±1/8 band.
            let knee = rate
                .kbps
                .saturating_sub(rate.kbps / 16)
                .max(rate.floor_kbps);
            if similar && self.cap.latch(knee, rate.floor_kbps) {
                tracing::info!(
                    cap_kbps = knee,
                    choked_at_kbps = rate.kbps,
                    reprobe_after_windows = self.cap.reprobe_after(),
                    "adaptive bitrate: decode cap learned (decoder knee) — climbs stop \
                     here until it lifts"
                );
            }
            self.backoff_kbps = rate.kbps;
        } else {
            self.backoff_kbps = 0;
        }
    }
}
