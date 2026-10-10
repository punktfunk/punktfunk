//! The host encoder's hold on the rate.
//!
//! Encode time over its frame budget costs one notch, kept only if the
//! encoder's time follows the rate; one that does not stands the down-driver
//! down until a clean run. A host behind its encode cadence holds climbs on
//! the same clock and teaches no cap. A mode switch resets all of it to
//! [`EncodeHold::default`].

use super::{Rate, StepAnswer, ANSWER_PCT};
use crate::abr::cap::{StandDown, StepProbe};
use crate::abr::sample::WindowSample;
use crate::abr::verdict::{Baselines, Verdict};

/// What this mode has taught about the host encoder.
pub(in crate::abr) struct EncodeHold {
    /// The notch an encode rise cost, waiting for the encoder's answer.
    pub(in crate::abr) probe: Option<StepProbe>,
    /// Encode rises are not answering the rate. Lifted by a clean run or a
    /// mode switch — never permanent.
    pub(in crate::abr) down: StandDown,
    /// The host refused a climb because its encode is behind cadence. Holds
    /// climbs on the same clock, latches no cap: a busy GPU is not a rate the
    /// encoder cannot hold, and a cap here would cost a re-probe ladder.
    pub(super) cadence: StandDown,
}

impl Default for EncodeHold {
    /// Nothing learned: no notch in flight, both drivers armed.
    fn default() -> Self {
        EncodeHold {
            probe: None,
            down: StandDown::new(),
            cadence: StandDown::new(),
        }
    }
}

impl EncodeHold {
    /// The host's encode is behind its frame cadence. Hold climbs on the
    /// stand-down clock: a clean loaded run lifts the hold, and a refusal after
    /// one backs the clock off. The rate stands — every other signal still
    /// drives it down.
    pub(super) fn hold_for_cadence(&mut self, kbps: u32) {
        if self.cadence.disarmed() {
            self.cadence.note_bad();
            return;
        }
        self.cadence.disarm();
        tracing::info!(
            held_kbps = kbps,
            lift_after_windows = self.cadence.reprobe_after(),
            "adaptive bitrate: the host is behind its encode cadence — climbs hold here \
             until a clean run, and no cap is learned"
        );
    }

    /// Host encode time over its frame budget costs one notch, not a cascade.
    ///
    /// The rate is only the lever if the encoder answers it, so the notch is
    /// held against the encode time at the new rate
    /// ([`judge_step`](Self::judge_step)). `Some` is the rate to request;
    /// `None` while a notch is still being judged: the step in flight owns
    /// the question.
    pub(super) fn retreat(&mut self, w: &WindowSample, rate: Rate, budget_us: i64) -> Option<u32> {
        if self.probe.is_some() {
            return None;
        }
        let mean_us = w.encode_mean_us?;
        let next = rate.notch();
        self.probe = Some(StepProbe::new(rate.kbps, mean_us));
        tracing::info!(
            from_kbps = rate.kbps,
            to_kbps = next,
            encode_us = mean_us,
            budget_us,
            "adaptive bitrate: host encode time is over its frame budget — one notch, \
             kept only if the encoder follows"
        );
        Some(next)
    }

    /// The encoder's answer to a notch, averaged over the windows that
    /// reached the new rate.
    ///
    /// Followed the rate down: keep it, and the next rise may take another —
    /// a weak encoder walks down to where it keeps up. Did not: the rate was
    /// never the lever (GPU contention, a governor), so give it back and
    /// stand the driver down. Loss, OWD, decode and keyframe signals keep
    /// driving either way. `None` while the notch is still waiting.
    pub(super) fn judge_step(
        &mut self,
        w: &WindowSample,
        v: &Verdict,
        rate_kbps: u32,
        budget_us: i64,
    ) -> Option<StepAnswer> {
        let mut p = self.probe?;
        // A quiet or starved window's mean describes the interruption, and a
        // rate the host has not applied yet is not the new one.
        let at_new_rate = rate_kbps < p.from_kbps;
        let mean_us = w
            .encode_mean_us
            .filter(|_| at_new_rate && !v.quiet && !v.starved);
        let verdict = p.note(mean_us);
        let expired = p.expired();
        self.probe = Some(p);
        let Some(mean) = verdict else {
            if expired {
                self.probe = None; // the notch never reached its rate
            }
            return None;
        };
        self.probe = None;
        if p.ref_us - mean >= budget_us * ANSWER_PCT / 100 {
            tracing::info!(
                from_kbps = p.from_kbps,
                at_kbps = rate_kbps,
                encode_us = mean,
                was_us = p.ref_us,
                "adaptive bitrate: host encode time followed the rate down — notch kept"
            );
            return Some(StepAnswer::Followed);
        }
        self.down.disarm();
        tracing::info!(
            restore_kbps = p.from_kbps,
            encode_us = mean,
            was_us = p.ref_us,
            rearm_after_windows = self.down.reprobe_after(),
            "adaptive bitrate: host encode time is not answering the rate — restoring it \
             and standing the encode down-driver down until a clean run re-probes it \
             (loss, OWD, decode and keyframe signals keep driving)"
        );
        Some(StepAnswer::StandDown(p.from_kbps))
    }

    /// One window on both stand-down clocks. Stillness proves nothing about
    /// a GPU that had nothing to encode.
    pub(super) fn tick(&mut self, bad: bool, quiet: bool, baselines: &mut Baselines) {
        // GPU contention ends; a too-eager re-arm costs one ×0.7, a permanent
        // silence costs the knee protection.
        if self.down.disarmed() {
            if bad {
                self.down.note_bad();
            } else if !quiet && self.down.note_clean() {
                // Fresh baseline, no streak: the old firing level is stale.
                baselines.clear_encode();
                tracing::debug!(
                    after_windows = self.down.reprobe_after(),
                    "adaptive bitrate: re-arming the encode down-driver after a clean run"
                );
            }
        }
        // The host's refusal expires the same way.
        if self.cadence.disarmed() {
            if bad {
                self.cadence.note_bad();
            } else if !quiet && self.cadence.note_clean() {
                tracing::debug!(
                    after_windows = self.cadence.reprobe_after(),
                    "adaptive bitrate: asking the host to climb again after a clean run"
                );
            }
        }
    }
}
