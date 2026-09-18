//! The AIMD controller: one decision per report window.
//!
//! Severe windows (unrecoverable frame, flush, ≥6 % loss, deep decode or
//! encode rise, keyframe storm) back off ×0.7 immediately. Ordinary
//! congestion needs two consecutive bad windows. A window host encode named
//! costs one notch instead, kept only if the encoder's time follows the rate.
//! Recovery is slow start
//! (double, bounded by proven-throughput headroom) then additive (+~6 % after
//! ~4.5 s). Slow start comes back on an idle stretch, and on a clean run that
//! refutes a verdict the link never authorised. Each change rebuilds the
//! encoder (IDR); silence after [`MAX_UNACKED`] unanswered requests.
//!
//! Caps are learned: two identical short host acks latch `host_cap_kbps`; two
//! similar decode-driven backoffs latch `decode_cap_kbps`, and so does decode
//! headroom on a clean window (park at 80 % of the frame budget, retreat a
//! notch at 90 %, each step kept only if the decoder's latency follows the
//! rate). Both re-probe on [`CAP_REPROBE_WINDOWS_MIN`]. Climbs require
//! utilization (delivered ≈ target) and stay within ×1.5 of the windowed
//! proven mark. Tests in this module pin the contract.

use super::cap::{LearnedCap, StandDown, StepProbe};
use super::growth::{self, Proven, CLEAN_WINDOWS_TO_INCREASE};
use super::sample::{self, WindowActivity, WindowSample};
use super::verdict::{
    encode_thresholds, Baselines, Reason, Verdict, HEAVY_LOSS_PPM, RECOVERY_KF_BAD,
};
use crate::quic::AckReason;
use std::time::{Duration, Instant};

/// Floor so a mis-measured window cannot crater the session. 2 Mbps: a thin
/// link is better served soft than lossy. First descent below
/// [`LOW_RATE_WARN_KBPS`] logs once.
pub(super) const FLOOR_KBPS: u32 = 2_000;
/// One-shot quality warning. 5 Mbps was the old floor; riding under it is the
/// new territory worth flagging.
const LOW_RATE_WARN_KBPS: u32 = 5_000;
/// Fully-idle windows (every AU a host-marked repeat) before the next active
/// window re-arms slow start. 4 × 750 ms ≈ 3 s of stillness.
const IDLE_WINDOWS_TO_REARM: u32 = 4;
/// Clean utilised windows that refute a verdict the link never authorised,
/// after which slow start comes back. 8 × 750 ms = 6 s at the rate the
/// verdict left: long enough to be a run, short enough that recovering from
/// a false verdict costs seconds instead of the three minutes +6 % a step
/// takes from 14 to 170 Mbps.
const CLEAN_WINDOWS_TO_REARM: u32 = 8;
/// Consecutive ordinary-bad windows before a decrease. One 750 ms window can
/// be a scheduler blip; 1.5 s is a condition. Severe skips the wait.
const BAD_WINDOWS_TO_DECREASE: u32 = 2;
/// Minimum gap between requests. Each accepted change rebuilds the encoder
/// and opens with an IDR; back-to-back steps outrun the ack RTT.
const CHANGE_COOLDOWN: Duration = Duration::from_millis(1500);
/// Decode headroom, judged on clean full-rate windows against the frame
/// budget. Received → decoded includes the queue wait, so a mean near the
/// period is a decoder with no slack: the next jitter is a missed vsync. At
/// 80 % climbs park; at 90 % the rate retreats a notch, and the decoder's
/// answer decides whether the retreat stands.
const DECODE_HOLD_PCT: i64 = 80;
const DECODE_RETREAT_PCT: i64 = 90;
/// A step is answered when its signal moves by this much of the frame budget
/// at the new rate. A pipelined decoder sits above the period with no queue
/// and a contended GPU holds its encode time up whatever the rate: neither
/// answers, and the driver that stepped stands down.
const ANSWER_PCT: i64 = 5;
/// One notch: 12.5 % off. Small enough that a wrong one costs little, big
/// enough that a signal which does follow the rate says so within the
/// window's own noise.
const RETREAT_DIV: u32 = 8;
/// A window is full-rate for the headroom judgement at ≥ ¾ of the refresh's
/// frames. Fewer frames share the same bits, so a low-fps decode mean
/// overstates the full-rate load.
const DECODE_FULL_RATE_NUM: i64 = 3;
const DECODE_FULL_RATE_DEN: i64 = 4;
/// Two decode-driven backoffs latch [`decode_cap_kbps`] only when their
/// pre-backoff rates agree within ±1/8. A cascade's second backoff sits at
/// ×0.7 of the first — outside the band by construction — so only a
/// climbed-to rate (`climb_since_backoff`) can sample the knee.
pub(super) const DECODE_CAP_SIMILAR_DIV: u32 = 8;
/// Unacked [`crate::quic::SetBitrate`] requests before the host is treated as
/// predating renegotiation and the controller goes quiet.
const MAX_UNACKED: u32 = 3;

/// A headroom step awaiting the decoder's answer at its new rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecodeProbe {
    kind: DecodeProbeKind,
    step: StepProbe,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DecodeProbeKind {
    Retreat,
    Lift,
}

/// One notch off `from_kbps`, never under the floor.
fn notch(from_kbps: u32, floor_kbps: u32) -> u32 {
    (from_kbps - from_kbps / RETREAT_DIV).max(floor_kbps)
}

/// This window's damage is the host encoder's and nothing else's: encode time
/// is the signal that named it, and no loss share came with it.
fn encode_named(w: &WindowSample, v: &Verdict) -> bool {
    v.reason == Reason::Encode && w.loss_ppm < HEAVY_LOSS_PPM
}

/// One decision per report window; `Some(kbps)` = send a [`crate::quic::SetBitrate`].
pub(crate) struct BitrateController {
    /// `false` = permanently off (explicit bitrate, old host, or ack silence).
    enabled: bool,
    /// Host-acked encoder rate. Requests are not assumed applied.
    pub(super) current_kbps: u32,
    /// Climb ceiling: negotiated start until [`set_ceiling`](Self::set_ceiling)
    /// raises it from the startup probe.
    pub(super) ceiling_kbps: u32,
    /// `PUNKTFUNK_ABR_MAX_MBPS` in kbps, injected so tests never touch the env.
    /// `None` = no cap.
    ceiling_cap_kbps: Option<u32>,
    /// [`stream_ceiling_kbps`] for this mode/codec. Bounds only what
    /// [`set_ceiling`](Self::set_ceiling) learns; the negotiated start stands.
    stream_cap_kbps: Option<u32>,
    floor_kbps: u32,
    /// Slow start until the first congestion signal.
    pub(super) probing: bool,
    /// The last bad window named something the rate is the lever for: the
    /// link, or a decoder past its budget. Slow start is not given back over
    /// one of those — doubling into a real wall is what sawed the tunnel.
    rate_verdict: bool,
    /// Clean utilised windows since a verdict the rate did not cause.
    rearm_windows: u32,
    /// Rolling minima the relative signals are scored against.
    baselines: Baselines,
    /// One refresh interval, µs. `None` = the 120 Hz [`ENCODE_RISE_US`] defaults.
    frame_budget_us: Option<i64>,
    /// The notch an encode rise cost, waiting for the encoder's answer.
    pub(super) encode_probe: Option<StepProbe>,
    /// Encode rises are not answering the rate. Lifted by a clean run or a
    /// mode switch — never permanent.
    pub(super) encode_down: StandDown,
    /// The host refused a climb because its encode is behind cadence. Holds
    /// climbs on the same clock, latches no cap: a busy GPU is not a rate the
    /// encoder cannot hold, and a cap here would cost a re-probe ladder.
    pub(super) cadence_hold: StandDown,
    /// Two identical short acks latch this. Kept apart from `ceiling_kbps` so a
    /// mode switch does not drop probe-measured link authority.
    pub(super) host_cap: LearnedCap,
    /// Last [`request`](Self::request). Taken (not kept) by the ack, so one
    /// request is judged at most once.
    pub(super) last_requested_kbps: Option<u32>,
    /// Two identical short acks latch [`host_cap_kbps`](Self::host_cap_kbps).
    /// One can be a failed rebuild keeping the old rate.
    short_ack_kbps: u32,
    short_acks: u32,
    /// Two consecutive decode-driven backoffs at a similar rate. Without it a
    /// decoder knee below the link ceiling is a 30–60 s sawtooth.
    pub(super) decode_cap: LearnedCap,
    /// Previous decode-driven backoff's pre-backoff rate (`0` = last backoff
    /// was not decode-driven). One spurious flush teaches nothing.
    pub(super) decode_backoff_kbps: u32,
    /// Decode-flagged windows in the current bad streak. The deciding window
    /// alone would drop the first ordinary-bad window's attribution.
    streak_decode_windows: u32,
    /// `current_kbps` has risen (ack) since the last backoff. A cascade's
    /// second backoff sits at ×0.7 — not a knee sample, and must not erase
    /// the reference.
    climb_since_backoff: bool,
    /// A retreat or a cap lift waiting for the decoder's answer.
    decode_probe: Option<DecodeProbe>,
    /// Decode latency did not follow the rate: hold and retreat stand down
    /// until a clean run re-probes them.
    decode_headroom: StandDown,
    /// Highest clean delivered rate of the recent buckets. Shrinking capacity
    /// is the reactive decode signal's job.
    proven: Proven,
    /// Consecutive fully-idle windows. First active window after
    /// [`IDLE_WINDOWS_TO_REARM`] re-arms slow start. Empty windows do not count.
    idle_windows: u32,
    low_rate_warned: bool,
    bad_windows: u32,
    clean_windows: u32,
    last_change: Option<Instant>,
    /// Reaching [`MAX_UNACKED`] disables the controller.
    unacked: u32,
    /// Last ceiling-clamp target asked (`0` = none). Asked once per distinct
    /// target: a host that answers higher cannot go there.
    ceiling_ask_kbps: u32,
    /// What the last window was scored as. Named on every re-target.
    last_reason: Reason,
    /// What made the latest bad window bad. A backoff can fire on a quiet
    /// window after the streak, so the cause is kept from the bad one.
    streak_cut: Option<Reason>,
    /// Why the last backoff happened; cleared by the next acked climb.
    last_cut: Option<Reason>,
}

impl BitrateController {
    /// `start_kbps` is the Welcome-resolved Automatic rate, or `0` for a
    /// permanently-disabled controller (explicit bitrate / old host).
    /// `ceiling_cap_kbps` is the operator's ceiling, read from the
    /// environment once by the embedder.
    pub(crate) fn new(start_kbps: u32, ceiling_cap_kbps: Option<u32>) -> Self {
        BitrateController {
            enabled: start_kbps > 0,
            current_kbps: start_kbps,
            // The env cap binds the negotiated start too. Automatic has no
            // explicit bitrate, so a start above the cap must come down — see
            // the clamp-down step in [`on_window`](Self::on_window).
            ceiling_kbps: start_kbps.min(ceiling_cap_kbps.unwrap_or(u32::MAX)),
            ceiling_cap_kbps,
            stream_cap_kbps: None,
            floor_kbps: FLOOR_KBPS.min(start_kbps.max(1)),
            probing: true,
            rate_verdict: false,
            rearm_windows: 0,
            baselines: Baselines::new(),
            frame_budget_us: None,
            encode_probe: None,
            encode_down: StandDown::new(),
            cadence_hold: StandDown::new(),
            host_cap: LearnedCap::new(),
            last_requested_kbps: None,
            short_ack_kbps: 0,
            short_acks: 0,
            decode_cap: LearnedCap::new(),
            decode_backoff_kbps: 0,
            streak_decode_windows: 0,
            // Negotiated start was held, not drained to — the first backoff
            // is a legitimate knee sample.
            climb_since_backoff: true,
            decode_probe: None,
            decode_headroom: StandDown::new(),
            proven: Proven::new(),
            idle_windows: 0,
            low_rate_warned: false,
            bad_windows: 0,
            clean_windows: 0,
            last_change: None,
            unacked: 0,
            ceiling_ask_kbps: 0,
            last_reason: Reason::Clean,
            streak_cut: None,
            last_cut: None,
        }
    }

    /// The signal that decided the last window — what named this re-target.
    pub(crate) fn last_reason(&self) -> Reason {
        self.last_reason
    }

    /// Why the rate was last cut, while it has not climbed since.
    pub(crate) fn last_cut(&self) -> Option<Reason> {
        self.last_cut
    }

    /// Raise the climb ceiling to a measured link capacity (caller already
    /// subtracted headroom). Never lowers: a congested-moment measurement must
    /// not shrink authority below what was negotiated. The env cap clamps here
    /// — the one funnel every learned ceiling passes through.
    pub(crate) fn set_ceiling(&mut self, kbps: u32) {
        let measured = kbps;
        let kbps = kbps
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .min(self.stream_cap_kbps.unwrap_or(u32::MAX));
        if self.enabled && kbps < measured {
            // Log both numbers when it binds; a silent trim is undiagnosable.
            tracing::info!(
                measured_kbps = measured,
                bounded_kbps = kbps,
                "adaptive bitrate: link ceiling bounded by what this stream can use"
            );
        }
        if self.enabled && kbps > self.ceiling_kbps {
            self.ceiling_kbps = kbps;
        }
    }

    /// Bound future learned ceilings (same funnel as the env cap). The first
    /// set leaves a negotiated start above it standing. A re-set is a mode
    /// switch: a drop in pixel rate rebinds the standing ceiling because
    /// [`set_ceiling`](Self::set_ceiling) never lowers.
    pub(crate) fn set_stream_cap(&mut self, kbps: u32) {
        let mode_switch = self.stream_cap_kbps.is_some();
        self.stream_cap_kbps = Some(kbps);
        if mode_switch && self.enabled && self.ceiling_kbps > kbps {
            tracing::info!(
                ceiling_kbps = self.ceiling_kbps,
                stream_cap_kbps = kbps,
                "adaptive bitrate: ceiling rebound to the switched mode's stream shape"
            );
            self.ceiling_kbps = kbps;
        }
    }

    /// Size encode thresholds in frame budgets, not the 120 Hz [`ENCODE_RISE_US`]
    /// durations. Ignored for a nonsense rate — the defaults stand.
    pub(crate) fn set_frame_budget(&mut self, refresh_hz: u32) {
        if refresh_hz > 0 {
            self.frame_budget_us = Some(1_000_000 / refresh_hz as i64);
        }
    }

    /// A cap lift that lost the decoder its headroom: back to the cap, and
    /// the next lift waits twice as long.
    fn undo_decode_lift(&mut self, p: DecodeProbe, decode_us: i64) {
        self.decode_cap.latch(p.step.from_kbps, self.floor_kbps);
        tracing::info!(
            cap_kbps = p.step.from_kbps,
            decode_us,
            was_us = p.step.ref_us,
            reprobe_after_windows = self.decode_cap.reprobe_after(),
            "adaptive bitrate: decode cap lift undone — the decoder lost its headroom"
        );
    }

    /// Decode headroom on a clean full-rate window. First the verdict on a
    /// step in flight, then the bands: park at [`DECODE_HOLD_PCT`], retreat a
    /// notch at [`DECODE_RETREAT_PCT`]. `Some` is a rate to request.
    fn judge_decode_headroom(&mut self, mean_us: i64, budget_us: i64, now: Instant) -> Option<u32> {
        if let Some(mut p) = self.decode_probe {
            let at_new_rate = match p.kind {
                DecodeProbeKind::Retreat => self.current_kbps < p.step.from_kbps,
                DecodeProbeKind::Lift => self.current_kbps > p.step.from_kbps,
            };
            let verdict = p.step.note(at_new_rate.then_some(mean_us));
            let expired = p.step.expired();
            self.decode_probe = Some(p);
            let Some(mean) = verdict else {
                if expired {
                    self.decode_probe = None; // the step never reached its rate
                }
                return None;
            };
            self.decode_probe = None;
            if let Some(kbps) = self.settle_decode_step(p, mean, budget_us, now) {
                return Some(kbps);
            }
        }
        if self.decode_headroom.disarmed() {
            if self.decode_headroom.note_clean() {
                tracing::debug!(
                    after_windows = self.decode_headroom.reprobe_after(),
                    "adaptive bitrate: re-arming the decode headroom driver after a clean run"
                );
            }
            return None;
        }
        let pct = mean_us * 100 / budget_us;
        if pct >= DECODE_RETREAT_PCT && self.current_kbps > self.floor_kbps {
            let from = self.current_kbps;
            let next = notch(from, self.floor_kbps);
            self.decode_cap.park(next);
            self.decode_probe = Some(DecodeProbe {
                kind: DecodeProbeKind::Retreat,
                step: StepProbe::new(from, mean_us),
            });
            tracing::info!(
                from_kbps = from,
                to_kbps = next,
                decode_us = mean_us,
                budget_us,
                "adaptive bitrate: the decoder has no headroom — retreating a notch, kept only \
                 if its latency follows"
            );
            self.ceiling_ask_kbps = next;
            return self.request(next, now);
        }
        if pct >= DECODE_HOLD_PCT && self.decode_cap.kbps().is_none_or(|c| c > self.current_kbps) {
            self.decode_cap.park(self.current_kbps);
            tracing::info!(
                cap_kbps = self.current_kbps,
                decode_us = mean_us,
                budget_us,
                "adaptive bitrate: decode headroom exhausted — parking here (re-probed after a \
                 clean run)"
            );
        }
        None
    }

    /// The decoder's answer to a step, averaged over its clean windows at the
    /// new rate. `Some` gives the rate back; `None` keeps the step, and this
    /// window is fresh evidence for the bands.
    fn settle_decode_step(
        &mut self,
        p: DecodeProbe,
        mean: i64,
        budget_us: i64,
        now: Instant,
    ) -> Option<u32> {
        let answer_us = budget_us * ANSWER_PCT / 100;
        match p.kind {
            DecodeProbeKind::Retreat if p.step.ref_us - mean >= answer_us => {
                tracing::info!(
                    from_kbps = p.step.from_kbps,
                    at_kbps = self.current_kbps,
                    decode_us = mean,
                    was_us = p.step.ref_us,
                    "adaptive bitrate: decode latency followed the rate down — retreat kept"
                );
                None
            }
            DecodeProbeKind::Retreat => {
                // Latency is not a function of the rate here: pipelined
                // decoder, or something else on the SoC. Give the rate
                // back and stand down; the cap ladder still probes up.
                self.decode_headroom.disarm();
                self.decode_cap.park(p.step.from_kbps);
                tracing::info!(
                    restore_kbps = p.step.from_kbps,
                    decode_us = mean,
                    was_us = p.step.ref_us,
                    rearm_after_windows = self.decode_headroom.reprobe_after(),
                    "adaptive bitrate: decode latency did not follow the rate — restoring it \
                     and standing the headroom driver down (a pipelined decoder sits above \
                     the period with no queue)"
                );
                self.ceiling_ask_kbps = p.step.from_kbps;
                self.request(p.step.from_kbps, now)
            }
            DecodeProbeKind::Lift => {
                let worse = mean - p.step.ref_us >= answer_us
                    || mean * 100 / budget_us >= DECODE_RETREAT_PCT;
                if worse {
                    self.undo_decode_lift(p, mean);
                    self.ceiling_ask_kbps = p.step.from_kbps;
                    return self.request(p.step.from_kbps, now);
                }
                tracing::info!(
                    at_kbps = self.current_kbps,
                    decode_us = mean,
                    was_us = p.step.ref_us,
                    "adaptive bitrate: decode cap lift held — the decoder kept its headroom"
                );
                None
            }
        }
    }

    /// Host [`crate::quic::BitrateChanged`]: the clamp is authoritative, and any
    /// ack proves the host renegotiates. Two identical short acks latch
    /// [`host_cap_kbps`](Self::host_cap_kbps); one can be a failed rebuild.
    ///
    /// [`AckReason`] says which limit answered. A pinned session has no rate to
    /// control; a cadence refusal is a busy GPU, held on a clock and never
    /// latched. An ack with no reason is an older host, read as today.
    pub(crate) fn on_ack(&mut self, kbps: u32, why: Option<AckReason>) {
        if why == Some(AckReason::Pinned) {
            self.on_pinned(kbps);
            return;
        }
        if kbps > 0 {
            if kbps < self.current_kbps {
                self.baselines.clear_encode();
            }
            if why == Some(AckReason::Cadence) {
                // Not evidence about a rate: the encoder is behind, and the
                // same fact the encode driver already handles. No cut, no cap.
                self.last_requested_kbps = None;
                self.short_acks = 0;
                self.hold_for_cadence(kbps);
            } else if let Some(req) = self.last_requested_kbps.take() {
                if kbps < req {
                    if self.short_ack_kbps == kbps {
                        self.short_acks += 1;
                    } else {
                        self.short_ack_kbps = kbps;
                        self.short_acks = 1;
                    }
                    if self.short_acks >= 2 && self.host_cap.latch(kbps, self.floor_kbps) {
                        tracing::info!(
                            cap_kbps = kbps,
                            reprobe_after_windows = self.host_cap.reprobe_after(),
                            "adaptive bitrate: host cap learned (encoder ceiling or cadence \
                             refusal) — climbs stop here until it lifts"
                        );
                    }
                } else {
                    self.short_acks = 0;
                    // Granted at or above the learned cap: drop it. Crawling
                    // +12.5 % is the remaining cost of a transient latch.
                    if self.host_cap.kbps().is_some_and(|c| kbps >= c) {
                        tracing::info!(
                            granted_kbps = kbps,
                            "adaptive bitrate: host granted a climb at the learned cap — the \
                             limit has lifted, dropping it"
                        );
                        self.host_cap.drop_cap();
                    }
                }
            }
            if kbps > self.current_kbps {
                // Rate rose: next choke is at a climbed-to rate. An acked
                // decrease does not arm this — drain is not a knee encounter.
                self.climb_since_backoff = true;
                self.last_cut = None;
            }
            self.current_kbps = kbps;
            // Unsolicited `BitrateChanged` can sit above our ceiling (host
            // re-resolved Automatic for what it encodes). Follow it; env cap
            // still binds. Without this, the step-down drags the host back.
            self.set_ceiling(kbps);
        }
        self.unacked = 0;
    }

    /// The host will not negotiate this session's rate (PyroWave: per-frame
    /// CBR). Nothing to control, so retire quietly — an unanswered host is
    /// already retired the same way.
    fn on_pinned(&mut self, kbps: u32) {
        self.last_requested_kbps = None;
        self.unacked = 0;
        if kbps > 0 {
            self.current_kbps = kbps;
        }
        if self.enabled {
            self.enabled = false;
            tracing::info!(
                pinned_kbps = kbps,
                "adaptive bitrate off — the host pins this session's rate"
            );
        }
    }

    /// The host's encode is behind its frame cadence. Hold climbs on the
    /// stand-down clock: a clean loaded run lifts the hold, and a refusal after
    /// one backs the clock off. The rate stands — every other signal still
    /// drives it down.
    fn hold_for_cadence(&mut self, kbps: u32) {
        if self.cadence_hold.disarmed() {
            self.cadence_hold.note_bad();
            return;
        }
        self.cadence_hold.disarm();
        tracing::info!(
            held_kbps = kbps,
            lift_after_windows = self.cadence_hold.reprobe_after(),
            "adaptive bitrate: the host is behind its encode cadence — climbs hold here \
             until a clean run, and no cap is learned"
        );
    }

    /// Drop mode-scoped learned state. Encoder/decoder knees and rolling
    /// baselines are properties of the mode; a baseline from the old mode is a
    /// floor the new one clears on the first window. Probe-measured
    /// `ceiling_kbps` (a link property) survives. Proven throughput re-earns.
    pub(crate) fn on_mode_switch(&mut self) {
        self.host_cap.drop_cap();
        self.short_acks = 0;
        self.decode_cap.drop_cap();
        self.decode_backoff_kbps = 0;
        self.streak_decode_windows = 0;
        self.climb_since_backoff = true;
        self.decode_probe = None;
        self.decode_headroom = StandDown::new();
        self.baselines.clear();
        // Encode work per frame changed with the mode. Re-arm; the caller
        // re-sizes the frame budget alongside this.
        self.encode_down = StandDown::new();
        self.cadence_hold = StandDown::new();
        self.encode_probe = None;
        self.proven.clear();
        self.idle_windows = 0;
    }

    /// Decide whether this 750 ms window should ask for a new encoder rate.
    ///
    /// `Some(kbps)` is the request. Score the window, fold it into the
    /// streaks and the clocks, then — once the cooldown allows a change —
    /// take the first of the three moves it licenses: back off, give the
    /// decoder headroom, or climb.
    pub(crate) fn on_window(&mut self, w: &WindowSample) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        if self.unacked >= MAX_UNACKED {
            // Host never answered: older build. Quiet, don't spam unknown.
            self.enabled = false;
            tracing::info!("adaptive bitrate off — host never acked a SetBitrate (older host)");
            return None;
        }
        let v = self.baselines.score(
            w,
            self.current_kbps,
            self.frame_budget_us,
            self.encode_down.disarmed(),
            self.clean_windows,
        );
        self.last_reason = v.reason;
        self.note_activity(w.activity, v.quiet);
        self.note_verdict(w, &v);
        self.note_rearm(w, &v);
        self.tick_caps(&v);
        let cooled = self
            .last_change
            .is_none_or(|t| w.now.duration_since(t) >= CHANGE_COOLDOWN);
        if !cooled {
            return None;
        }
        if let Some(kbps) = self.judge_encode_step(w, &v) {
            return Some(kbps);
        }
        if (self.bad_windows >= BAD_WINDOWS_TO_DECREASE || (v.severe && self.bad_windows >= 1))
            && self.current_kbps > self.floor_kbps
        {
            return self.back_off(w, &v);
        }
        if let Some(kbps) = self.judge_headroom(w, &v) {
            return Some(kbps);
        }
        self.climb(w)
    }

    /// Stillness, and the motion that ends it.
    ///
    /// Repeat-only is idle; empty is the same neutrality but proves nothing,
    /// so it does not count; an older host that marks no repeats is never
    /// idle. A long enough idle stretch re-arms slow start on the next
    /// window that carries content.
    fn note_activity(&mut self, activity: WindowActivity, quiet: bool) {
        if activity.idle() {
            self.idle_windows = self.idle_windows.saturating_add(1);
            return;
        }
        if quiet {
            return;
        }
        // Clear the cooldown with it so this window can climb.
        if self.idle_windows >= IDLE_WINDOWS_TO_REARM && !self.probing {
            self.probing = true;
            self.last_change = None;
            tracing::debug!(
                idle_windows = self.idle_windows,
                proven_kbps = self.proven.mark(),
                "adaptive bitrate: motion onset after an idle stretch — slow start re-armed"
            );
        }
        self.idle_windows = 0;
    }

    /// Fold the window into the streaks the decrease and the climb read.
    ///
    /// The proven mark is scored after the verdict and gated on the whole of
    /// it: a damaged window overstates delivery (stall drain, flush queue,
    /// FEC surge), and those bytes arriving is not climb authority.
    fn note_verdict(&mut self, w: &WindowSample, v: &Verdict) {
        // Bucket clock ticks on idle windows too: decay is about time.
        self.proven.tick();
        if v.reason == Reason::Blip {
            // The rate stands and slow start with it, but the clean run that
            // vouched for this window starts over: a second lost frame inside
            // the next one is judged like any other damage.
            self.clean_windows = 0;
            tracing::info!(
                dropped = w.dropped,
                loss_ppm = w.loss_ppm,
                owd_mean_us = w.owd_mean_us.unwrap_or(-1),
                at_kbps = self.current_kbps,
                "adaptive bitrate: one lost frame after a clean run — a blip, not the link"
            );
            return;
        }
        if !v.bad {
            self.proven.note(w.actual_kbps);
        }
        if v.bad {
            // What the rate is the lever for, read before the streaks move:
            // loss share, a delay rise, a flush, drops the clean run does not
            // vouch for, and a decoder past its budget. Keyframe asks, host
            // encode and one lost frame behind a clean window are not.
            let repeated_drops = w.dropped > 1 || (w.dropped == 1 && self.clean_windows == 0);
            self.rate_verdict = w.loss_ppm >= HEAVY_LOSS_PPM
                || v.owd_bad
                || w.flushed
                || repeated_drops
                || v.decode_bad;
            self.bad_windows += 1;
            self.streak_cut = Some(v.reason);
            if v.decode_bad {
                // Counted here: backoff only sees the final window, and the
                // cooldown eats the first ordinary-bad window.
                self.streak_decode_windows += 1;
            }
            self.clean_windows = 0;
            self.decode_headroom.note_bad();
            // A lift that ran into damage failed; a retreat learns nothing here.
            match self.decode_probe.take() {
                Some(p)
                    if p.kind == DecodeProbeKind::Lift && self.current_kbps > p.step.from_kbps =>
                {
                    self.undo_decode_lift(p, v.decode_mean_us.unwrap_or(p.step.ref_us));
                }
                _ => {}
            }
            // Any congestion ends slow start until a later idle-onset re-arm.
            self.probing = false;
        } else if !v.quiet {
            // Stillness is neither climb credit nor a cleared streak.
            self.clean_windows += 1;
            self.bad_windows = 0;
            self.streak_decode_windows = 0;
        }
    }

    /// Give slow start back to a session that lost it to something other than
    /// the link.
    ///
    /// Host encode overload, keyframe asks and one lost frame say nothing
    /// about capacity, and the +6 % crawl they leave behind takes three
    /// minutes to undo. [`CLEAN_WINDOWS_TO_REARM`] clean utilised windows
    /// refute one, and the doubling comes back inside its usual bounds — the
    /// proven mark, the utilisation gate and every cap. A verdict the rate is
    /// the lever for is held to instead: doubling back into a wall saws it,
    /// and the decode cap latches only on two chokes at a similar rate.
    fn note_rearm(&mut self, w: &WindowSample, v: &Verdict) {
        // A backoff that recorded a knee reference is not refuted by a clean
        // run either: the decode cap latches on two chokes at a similar rate,
        // and a doubling in between samples a different one.
        if self.probing || self.rate_verdict || self.decode_backoff_kbps > 0 {
            return;
        }
        let proration = growth::proration(w.activity, self.frame_budget_us);
        if v.bad
            || v.quiet
            || v.reason == Reason::Blip
            || !growth::utilized(w.activity, proration, w.actual_kbps, self.current_kbps)
        {
            self.rearm_windows = 0;
            return;
        }
        self.rearm_windows += 1;
        if self.rearm_windows >= CLEAN_WINDOWS_TO_REARM {
            self.rearm_windows = 0;
            self.probing = true;
            tracing::info!(
                at_kbps = self.current_kbps,
                actual_kbps = w.actual_kbps,
                proven_kbps = self.proven.mark(),
                windows = CLEAN_WINDOWS_TO_REARM,
                "adaptive bitrate: the verdict that ended slow start is refuted — re-armed"
            );
        }
    }

    /// One window on each re-probe clock: the two learned caps and the encode
    /// stand-down. A lift above the decode cap starts the probe whose answer
    /// decides whether it holds.
    fn tick_caps(&mut self, v: &Verdict) {
        let (bad, quiet, rate, ceiling) = (v.bad, v.quiet, self.current_kbps, self.ceiling_kbps);
        if let Some((from, to)) = self.host_cap.on_window(bad, quiet, rate, ceiling) {
            tracing::debug!(
                from_kbps = from,
                to_kbps = to,
                "adaptive bitrate: re-probing above the learned host cap"
            );
        }
        if let Some((from, to)) = self.decode_cap.on_window(bad, quiet, rate, ceiling) {
            tracing::debug!(
                from_kbps = from,
                to_kbps = to,
                "adaptive bitrate: re-probing above the learned decode cap"
            );
            // The decoder's answer above the cap decides the lift.
            self.decode_probe = v.decode_mean_us.map(|ref_us| DecodeProbe {
                kind: DecodeProbeKind::Lift,
                step: StepProbe::new(from, ref_us),
            });
        }
        // GPU contention ends; a too-eager re-arm costs one ×0.7, a permanent
        // silence costs the knee protection.
        if self.encode_down.disarmed() {
            if bad {
                self.encode_down.note_bad();
            // Quiet because nothing needed encoding proves nothing about the knee.
            } else if !quiet && self.encode_down.note_clean() {
                // Fresh baseline, no streak: the old firing level is stale.
                self.baselines.clear_encode();
                tracing::debug!(
                    after_windows = self.encode_down.reprobe_after(),
                    "adaptive bitrate: re-arming the encode down-driver after a clean run"
                );
            }
        }
        // The host's refusal expires the same way. Stillness proves nothing
        // about a GPU that had nothing to encode.
        if self.cadence_hold.disarmed() {
            if bad {
                self.cadence_hold.note_bad();
            } else if !quiet && self.cadence_hold.note_clean() {
                tracing::debug!(
                    after_windows = self.cadence_hold.reprobe_after(),
                    "adaptive bitrate: asking the host to climb again after a clean run"
                );
            }
        }
    }

    /// The ×0.7 step, and what this window taught on the way down: a decoder
    /// knee when the evidence is the decoder's, a host-encode level when the
    /// encoder is the only explanation.
    fn back_off(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        if encode_named(w, v) {
            return self.retreat_encode(w);
        }
        self.learn_knee(w, v);
        self.climb_since_backoff = false;
        let next = ((self.current_kbps as u64 * 7 / 10) as u32).max(self.floor_kbps);
        self.warn_low_rate(next);
        self.bad_windows = 0;
        self.streak_decode_windows = 0;
        self.last_cut = self.streak_cut;
        self.request(next, w.now)
    }

    /// Host encode time over its frame budget costs one notch, not a cascade.
    ///
    /// The rate is only the lever if the encoder answers it, so the notch is
    /// held against the encode time at the new rate
    /// ([`judge_encode_step`](Self::judge_encode_step)). `None` while a notch
    /// is still being judged: the step in flight owns the question.
    fn retreat_encode(&mut self, w: &WindowSample) -> Option<u32> {
        if self.encode_probe.is_some() {
            return None;
        }
        let mean_us = w.encode_mean_us?;
        let from = self.current_kbps;
        let next = notch(from, self.floor_kbps);
        self.encode_probe = Some(StepProbe::new(from, mean_us));
        self.climb_since_backoff = false;
        self.bad_windows = 0;
        self.streak_decode_windows = 0;
        self.last_cut = self.streak_cut;
        self.warn_low_rate(next);
        tracing::info!(
            from_kbps = from,
            to_kbps = next,
            encode_us = mean_us,
            budget_us = self.encode_budget_us(),
            "adaptive bitrate: host encode time is over its frame budget — one notch, \
             kept only if the encoder follows"
        );
        self.request(next, w.now)
    }

    /// The encoder's answer to a notch, averaged over the windows that
    /// reached the new rate.
    ///
    /// Followed the rate down: keep it, and the next rise may take another —
    /// a weak encoder walks down to where it keeps up. Did not: the rate was
    /// never the lever (GPU contention, a governor), so give it back and
    /// stand the driver down. Loss, OWD, decode and keyframe signals keep
    /// driving either way.
    fn judge_encode_step(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        let mut p = self.encode_probe?;
        // A quiet or starved window's mean describes the interruption, and a
        // rate the host has not applied yet is not the new one.
        let at_new_rate = self.current_kbps < p.from_kbps;
        let mean_us = w
            .encode_mean_us
            .filter(|_| at_new_rate && !v.quiet && !v.starved);
        let verdict = p.note(mean_us);
        let expired = p.expired();
        self.encode_probe = Some(p);
        let Some(mean) = verdict else {
            if expired {
                self.encode_probe = None; // the notch never reached its rate
            }
            return None;
        };
        self.encode_probe = None;
        let budget_us = self.encode_budget_us();
        if p.ref_us - mean >= budget_us * ANSWER_PCT / 100 {
            // The encoder followed: this knee is real and the rate is its
            // lever, so slow start is not handed back over it either.
            self.rate_verdict = true;
            tracing::info!(
                from_kbps = p.from_kbps,
                at_kbps = self.current_kbps,
                encode_us = mean,
                was_us = p.ref_us,
                "adaptive bitrate: host encode time followed the rate down — notch kept"
            );
            return None;
        }
        self.encode_down.disarm();
        self.baselines.clear_encode();
        tracing::info!(
            restore_kbps = p.from_kbps,
            encode_us = mean,
            was_us = p.ref_us,
            rearm_after_windows = self.encode_down.reprobe_after(),
            "adaptive bitrate: host encode time is not answering the rate — restoring it \
             and standing the encode down-driver down until a clean run re-probes it \
             (loss, OWD, decode and keyframe signals keep driving)"
        );
        self.request(p.from_kbps, w.now)
    }

    /// The frame budget a step's answer is measured in. Without a negotiated
    /// refresh the 120 Hz durations the encode thresholds are calibrated at
    /// stand in — the rise threshold is half a budget.
    fn encode_budget_us(&self) -> i64 {
        self.frame_budget_us
            .unwrap_or_else(|| encode_thresholds(None).0 * 2)
    }

    /// First descent below the old 5 Mbps floor: log once.
    fn warn_low_rate(&mut self, next_kbps: u32) {
        if next_kbps < LOW_RATE_WARN_KBPS && !self.low_rate_warned {
            self.low_rate_warned = true;
            tracing::warn!(
                at_kbps = next_kbps,
                "adaptive bitrate: the link sustains only a very low rate — expect \
                 visibly soft video until it recovers (floor: 2 Mbps)"
            );
        }
    }

    /// Two decode-driven backoffs at a similar rate are a knee; a drained or
    /// starved one samples nothing, and neither erases the reference.
    fn learn_knee(&mut self, w: &WindowSample, v: &Verdict) {
        // Decode evidence: severe in this window; every window in the
        // ordinary streak was decode-flagged; kf-storm without heavy loss;
        // flush only if decode is bad or the signal is absent (flat decode
        // + flush is a network event).
        let decode_evidence = v.decode_severe
            || self.streak_decode_windows >= BAD_WINDOWS_TO_DECREASE
            || (w.recovery_kf >= RECOVERY_KF_BAD && w.loss_ppm < HEAVY_LOSS_PPM)
            || (w.flushed && (v.decode_bad || v.decode_mean_us.is_none()));
        if !self.climb_since_backoff {
            // Drain after ×0.7 (~100 ms ack): not a knee sample.
            tracing::debug!(
                at_kbps = self.current_kbps,
                reference_kbps = self.decode_backoff_kbps,
                "adaptive bitrate: backoff without an intervening climb — draining the \
                 previous choke, not a knee sample"
            );
        } else if v.starved {
            tracing::debug!(
                at_kbps = self.current_kbps,
                actual_kbps = w.actual_kbps,
                reference_kbps = self.decode_backoff_kbps,
                "adaptive bitrate: backoff in a starved window (delivery a fraction of \
                 the target) — starvation-shaped distress, not a knee sample"
            );
        } else if decode_evidence {
            let rate = self.current_kbps;
            let similar = self.decode_backoff_kbps > 0
                && rate.abs_diff(self.decode_backoff_kbps)
                    <= self.decode_backoff_kbps / DECODE_CAP_SIMILAR_DIV;
            // Latch just under the choke rate: a cap on the knee authorizes
            // climbing straight back into it. 1/16 is inside the ±1/8 band.
            let knee = rate.saturating_sub(rate / 16).max(self.floor_kbps);
            if similar && self.decode_cap.latch(knee, self.floor_kbps) {
                tracing::info!(
                    cap_kbps = knee,
                    choked_at_kbps = rate,
                    reprobe_after_windows = self.decode_cap.reprobe_after(),
                    "adaptive bitrate: decode cap learned (decoder knee) — climbs stop \
                     here until it lifts"
                );
            }
            self.decode_backoff_kbps = rate;
        } else {
            self.decode_backoff_kbps = 0;
        }
    }

    /// Decode headroom, on a clean loaded full-rate window that carries the
    /// signal. Loss, flush and starvation are the link's story, not the
    /// decoder's, and a low-fps window's decode mean overstates the load.
    fn judge_headroom(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        let budget_us = self.frame_budget_us?;
        let mean_us = v.decode_mean_us?;
        let full_rate = match w.activity {
            WindowActivity::Active(n) if budget_us > 0 => {
                i64::from(n) * DECODE_FULL_RATE_DEN
                    >= (sample::WINDOW_US / budget_us) * DECODE_FULL_RATE_NUM
            }
            _ => true,
        };
        if budget_us <= 0 || v.bad || v.quiet || v.starved || !full_rate {
            return None;
        }
        self.judge_decode_headroom(mean_us, budget_us, w.now)
    }

    /// A clean window under the ceilings: come down to a ceiling this session
    /// is already above, or climb toward it.
    fn climb(&mut self, w: &WindowSample) -> Option<u32> {
        let proration = growth::proration(w.activity, self.frame_budget_us);
        let utilized = growth::utilized(w.activity, proration, w.actual_kbps, self.current_kbps);
        // Probe = link, short acks = encoder, decode cap = client decoder.
        let eff_ceiling = self
            .ceiling_kbps
            .min(self.host_cap.kbps().unwrap_or(u32::MAX))
            .min(self.decode_cap.kbps().unwrap_or(u32::MAX));
        // Above the env/policy ceiling with no congestion: step down once per
        // distinct target. A host that answers higher cannot go there.
        let ceiling_target = eff_ceiling.max(self.floor_kbps);
        if self.current_kbps > ceiling_target && self.ceiling_ask_kbps != ceiling_target {
            tracing::info!(
                from_kbps = self.current_kbps,
                to_kbps = ceiling_target,
                "adaptive bitrate: session rate is above the configured ceiling — stepping down"
            );
            self.ceiling_ask_kbps = ceiling_target;
            return self.request(ceiling_target, w.now);
        }
        // The host said its encoder is behind. Asking for more bits deepens the
        // miss; the step-down above still passes.
        if self.cadence_hold.disarmed() {
            return None;
        }
        // Proven bounds the projected wire rate at ×1.5, in the same domain
        // the proration took it out of.
        let cap = eff_ceiling.min(growth::proven_target_cap(self.proven.mark(), proration));
        if self.current_kbps < eff_ceiling && utilized && cap > self.current_kbps {
            let slow_start = self.probing && self.clean_windows >= 1;
            if slow_start || self.clean_windows >= CLEAN_WINDOWS_TO_INCREASE {
                let next = growth::climb_step(self.current_kbps, cap, slow_start);
                self.clean_windows = 0;
                return self.request(next, w.now);
            }
        }
        None
    }

    fn request(&mut self, kbps: u32, now: Instant) -> Option<u32> {
        self.last_change = Some(now);
        self.unacked += 1;
        self.last_requested_kbps = Some(kbps);
        // Ack is authoritative. A lost request recomputes from the same base.
        Some(kbps)
    }

    /// Control queue was full: undo request bookkeeping. [`MAX_UNACKED`]
    /// detects a host that doesn't answer; counting a message never sent
    /// retires the controller. Also keeps a later unsolicited ack from being
    /// judged short against a rate we never asked for.
    pub(crate) fn on_request_dropped(&mut self) {
        self.unacked = self.unacked.saturating_sub(1);
        self.last_requested_kbps = None;
    }
}

#[cfg(test)]
mod tests {
    use super::super::cap::CAP_REPROBE_WINDOWS_MIN;
    use super::super::harness::*;
    use super::*;

    #[test]
    fn disabled_when_not_automatic_or_old_host() {
        // start 0 = explicit bitrate or a host that didn't echo one.
        let mut c = BitrateController::new(0, None);
        let now = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 5,
                loss_ppm: 900_000,
                owd_mean_us: Some(500_000),
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(now)
            }),
            None
        );
    }

    #[test]
    fn two_ordinary_bad_windows_step_down_multiplicatively() {
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        // 2–6 % loss is ordinary: one window is a blip.
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            None
        );
        // Second consecutive ordinary-bad window: ×0.7.
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 1))
            }),
            Some(14_000)
        );
        c.on_ack(14_000, None);
        // Still bad after cooldown: another ×0.7 from the acked rate.
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 7))
            }),
            Some(9_800)
        );
    }

    #[test]
    fn severe_window_backs_off_immediately() {
        // Unrecoverable frame skips the two-window wait…
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
        // …and so does a jump-to-live flush.
        let mut c = BitrateController::new(20_000, None);
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 1_000_000,
                flushed: true,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        // …and ≥6 % window loss.
        let mut c = BitrateController::new(20_000, None);
        assert_eq!(
            c.on_window(&WindowSample {
                loss_ppm: 80_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
        assert_eq!(c.last_cut(), Some(Reason::Loss));
    }

    /// A clean run refutes a verdict the rate never caused and slow start
    /// comes back; the link's own verdict is held to.
    #[test]
    fn a_clean_run_re_arms_slow_start_only_for_a_refutable_verdict() {
        let start = Instant::now();
        let re_armed = |ender: WindowSample| -> bool {
            let mut c = BitrateController::new(20_000, None);
            c.set_ceiling(300_000);
            assert_eq!(c.on_window(&ender), None, "one bad window cuts nothing");
            assert!(!c.probing, "but it does end slow start");
            for i in 1..=CLEAN_WINDOWS_TO_REARM {
                let at_kbps = c.current_kbps;
                if let Some(k) = c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: at_kbps,
                    ..WindowSample::at(ticks(start, i))
                }) {
                    c.on_ack(k, None);
                }
            }
            c.probing
        };
        let base = WindowSample {
            owd_mean_us: Some(10_000),
            actual_kbps: 20_000,
            ..WindowSample::at(ticks(start, 0))
        };
        assert!(
            re_armed(WindowSample {
                recovery_kf: RECOVERY_KF_BAD,
                ..base
            }),
            "keyframe asks say nothing about what the link can carry"
        );
        assert!(
            !re_armed(WindowSample {
                loss_ppm: HEAVY_LOSS_PPM,
                ..base
            }),
            "a loss share is the link's own — doubling back into it saws"
        );
    }

    #[test]
    fn cooldown_blocks_back_to_back_steps() {
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
        // Tick 1 = 750 ms, inside cooldown; tick 2 = 1.5 s, fires.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 1))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 2))
            }),
            Some(9_800)
        );
    }

    #[test]
    fn floor_is_never_crossed() {
        let mut c = BitrateController::new(2_500, None);
        let start = Instant::now();
        // ×0.7 of 2500 = 1750 < floor → 2000.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(2_000)
        );
        c.on_ack(2_000, None);
        // At the floor, further bad windows request nothing.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 7))
            }),
            None
        );
    }

    #[test]
    fn set_ceiling_is_ignored_when_disabled_and_never_lowers() {
        let mut c = BitrateController::new(0, None);
        c.set_ceiling(1_000_000);
        assert_eq!(
            c.on_window(&WindowSample {
                actual_kbps: 1_000_000,
                ..WindowSample::at(Instant::now())
            }),
            None
        );
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(10_000); // below the negotiated start → ignored
        assert_eq!(c.ceiling_kbps, 20_000);
    }

    /// Stream bound clamps learned ceilings only; a host-resolved start stands.
    #[test]
    fn the_stream_bound_clamps_a_learned_ceiling_only() {
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 100_000, "a learned ceiling is bounded");

        // Never set: no stream bound.
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 657_000);

        // Negotiated start above the bound stands.
        let mut c = BitrateController::new(300_000, None);
        c.set_stream_cap(100_000);
        assert_eq!(c.ceiling_kbps, 300_000);
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 300_000,
            "and a learned ceiling under it never lowers what was negotiated"
        );

        // Tighter of env and stream caps wins.
        let mut c = BitrateController::new(20_000, Some(50_000));
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 50_000,
            "the env cap still binds when it is tighter"
        );
    }

    /// Mode switch re-teaches the stream cap both ways: upswitch opens room,
    /// downswitch rebinds because [`BitrateController::set_ceiling`] never lowers.
    #[test]
    fn a_mode_switch_reteaches_the_stream_cap_both_ways() {
        // 1080p on a fat link: ceiling bound at the 1080p shape.
        let mut c = BitrateController::new(20_000, None);
        c.set_stream_cap(100_000);
        c.set_ceiling(657_000);
        assert_eq!(c.ceiling_kbps, 100_000);

        // Upswitch to 4K: new shape allows more; probe measurement may re-authorize.
        c.on_mode_switch();
        c.set_stream_cap(400_000);
        assert_eq!(
            c.ceiling_kbps, 100_000,
            "an upswitch alone raises nothing — authority still needs a measurement"
        );
        c.set_ceiling(657_000);
        assert_eq!(
            c.ceiling_kbps, 400_000,
            "the 4K shape no longer pins the session to the 1080p bound"
        );

        // Downswitch to 720p: re-taught cap rebinds; `set_ceiling` never lowers.
        c.on_mode_switch();
        c.set_stream_cap(42_000);
        assert_eq!(
            c.ceiling_kbps, 42_000,
            "a downswitch rebinds the already-learned ceiling"
        );

        // Disabled controller (explicit bitrate) is untouched.
        let mut d = BitrateController::new(0, None);
        d.set_stream_cap(100_000);
        d.set_stream_cap(42_000);
        assert_eq!(d.ceiling_kbps, 0);
    }

    /// One-shot warning on first descent below the old 5 Mbps floor.
    #[test]
    fn the_low_rate_warning_fires_once_below_the_old_floor() {
        let mut c = BitrateController::new(6_000, None);
        let start = Instant::now();
        assert!(!c.low_rate_warned);
        // 6000 × 0.7 = 4200: under the old floor, over the new one.
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(4_200)
        );
        assert!(c.low_rate_warned, "the descent below 5 000 warns");
        c.on_ack(4_200, None);
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(2_940)
        );
        assert!(c.low_rate_warned, "…exactly once");
    }

    #[test]
    fn a_host_retarget_above_the_ceiling_raises_it() {
        // Unsolicited host re-target above the negotiated rate must raise the ceiling.
        let mut c = BitrateController::new(20_000, None);
        assert_eq!(c.ceiling_kbps, 20_000);
        c.on_ack(60_000, None); // unsolicited, no request outstanding
        assert_eq!(c.current_kbps, 60_000);
        assert_eq!(c.ceiling_kbps, 60_000);
        let start = Instant::now();
        // No step-down.
        assert_eq!(run_clean(&mut c, start, 0, 4), None);
        // Env cap still outranks the host retarget.
        let mut c = BitrateController::new(20_000, Some(50_000));
        c.on_ack(60_000, None);
        assert_eq!(c.ceiling_kbps, 50_000);
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(50_000));
    }

    #[test]
    fn env_max_mbps_caps_every_learned_ceiling() {
        // Injected 50 Mbps env cap outranks an 886 Mbps probe.
        let mut c = BitrateController::new(20_000, Some(50_000));
        c.set_ceiling(886_312);
        assert_eq!(c.ceiling_kbps, 50_000);
        // Measurement under the cap stands.
        let mut c = BitrateController::new(20_000, Some(50_000));
        c.set_ceiling(40_000);
        assert_eq!(c.ceiling_kbps, 40_000);
        // Climb honors it: 20→40→50, then quiet.
        let mut c = BitrateController::new(20_000, Some(50_000));
        c.set_ceiling(886_312);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(40_000));
        c.on_ack(40_000, None);
        assert_eq!(run_clean(&mut c, start, 2, 1), Some(50_000));
        c.on_ack(50_000, None);
        assert_eq!(run_clean(&mut c, start, 4, 20), None);
    }

    #[test]
    fn a_session_above_the_env_cap_steps_down_to_it_once() {
        // Env cap binds the negotiated start, not only probe-learned ceilings.
        let mut c = BitrateController::new(100_000, Some(50_000));
        assert_eq!(c.ceiling_kbps, 50_000);
        let start = Instant::now();
        assert_eq!(run_clean(&mut c, start, 0, 1), Some(50_000));
        // Host answers higher: cannot go there; do not re-ask every cooldown.
        c.on_ack(80_000, None);
        assert_eq!(run_clean(&mut c, start, 2, 20), None);
        // Same clamped target: no new ask.
        c.set_ceiling(90_000);
        assert_eq!(run_clean(&mut c, start, 24, 20), None);
    }

    #[test]
    fn ack_silence_disables_the_controller() {
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let mut sent = 0;
        let mut i = 0;
        // Never ack: exactly [`MAX_UNACKED`] requests, then silence.
        while i < 60 {
            if c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            })
            .is_some()
            {
                sent += 1;
            }
            i += 1;
        }
        assert_eq!(sent, MAX_UNACKED);
    }
    #[test]
    fn decode_headroom_parks_a_clean_climb() {
        // 7 000 µs of 8 333: 84 % — inside the hold band. No climb, cap at the rate.
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..12 {
            assert_eq!(
                loaded(&mut c, ticks(start, t), 7_000),
                None,
                "window {t} climbed"
            );
            t += 1;
        }
        assert_eq!(c.decode_cap.kbps(), Some(100_000));
        assert_eq!(c.current_kbps, 100_000);
    }

    #[test]
    fn a_lifted_decode_cap_is_held_when_headroom_stays() {
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..4 {
            assert_eq!(loaded(&mut c, ticks(start, t), 7_000), None);
            t += 1;
        }
        // The ladder lifts the parked cap +12.5 % and the climb takes it.
        let lift = until_request(&mut c, start, &mut t, 7_000, 40).expect("ladder lift");
        assert_eq!(lift, 112_500);
        // Flat latency above the old cap: the lift stands.
        assert_eq!(until_request(&mut c, start, &mut t, 7_000, 6), None);
        assert_eq!(c.current_kbps, 112_500);
        assert_eq!(c.decode_cap.kbps(), Some(112_500));
        assert!(c.decode_probe.is_none(), "verdict must have been reached");
    }

    #[test]
    fn a_lift_is_undone_when_the_decoder_loses_headroom() {
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..4 {
            assert_eq!(loaded(&mut c, ticks(start, t), 7_000), None);
            t += 1;
        }
        assert_eq!(
            until_request(&mut c, start, &mut t, 7_000, 40),
            Some(112_500)
        );
        // 7 700 µs: +700 over the reference (≥ 5 % of the budget) and 92 %.
        let back = until_request(&mut c, start, &mut t, 7_700, 6).expect("lift undone");
        assert_eq!(back, 100_000);
        assert_eq!(c.decode_cap.kbps(), Some(100_000));
        assert_eq!(c.decode_cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN * 2);
    }

    #[test]
    fn a_decoder_without_headroom_retreats_and_keeps_it_when_latency_follows() {
        let (mut c, start, mut t) = seeded_120(100_000);
        // 7 800 µs: 93 % — retreat one notch, not the ×0.7 congestion backoff.
        let r = loaded(&mut c, ticks(start, t), 7_800).expect("retreat");
        t += 1;
        assert_eq!(r, 87_500);
        assert_eq!(c.decode_cap.kbps(), Some(87_500));
        c.on_ack(r, None);
        // Latency followed (−1 300 µs): the retreat stands, nothing else moves.
        assert_eq!(until_request(&mut c, start, &mut t, 6_500, 8), None);
        assert_eq!(c.current_kbps, 87_500);
        assert!(!c.decode_headroom.disarmed());
    }

    #[test]
    fn a_flat_decoder_gets_its_rate_back_and_the_driver_stands_down() {
        let (mut c, start, mut t) = seeded_120(100_000);
        let r = loaded(&mut c, ticks(start, t), 7_800).expect("retreat");
        t += 1;
        c.on_ack(r, None);
        // Same latency at the lower rate: not a function of the rate.
        let restore = until_request(&mut c, start, &mut t, 7_800, 6).expect("restore");
        assert_eq!(restore, 100_000);
        assert!(c.decode_headroom.disarmed());
        assert_eq!(c.decode_cap.kbps(), Some(100_000));
        // Stood down: the same 93 % no longer retreats.
        assert_eq!(until_request(&mut c, start, &mut t, 7_800, 10), None);
        assert_eq!(c.current_kbps, 100_000);
    }

    #[test]
    fn frame_driven_windows_do_not_judge_headroom() {
        // 30 of 90 expected frames: bigger frames per budget, so 93 % here
        // says nothing about the full-rate load. Climbs are still allowed.
        let (mut c, start, mut t) = seeded_120(100_000);
        for _ in 0..6 {
            let r = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(7_800),
                actual_kbps: c.current_kbps,
                activity: WindowActivity::Active(30),
                ..WindowSample::at(ticks(start, t))
            });
            t += 1;
            if let Some(k) = r {
                assert!(k > c.current_kbps, "only climbs, never a retreat");
                c.on_ack(k, None);
            }
        }
        assert_eq!(c.decode_cap.kbps(), None);
    }
}
