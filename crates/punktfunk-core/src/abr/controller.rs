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
//! encoder (IDR); [`MAX_UNACKED`] unanswered requests wait for an answer, and
//! one still unanswered after [`ACK_GIVE_UP`] is silence.
//!
//! Caps are learned: two identical short host acks latch `host_cap_kbps`; two
//! similar decode-driven backoffs latch `decode_cap_kbps`, and so does decode
//! headroom on a clean window (park at 80 % of the frame budget, retreat a
//! notch at 90 %, each step kept only if the decoder's latency follows the
//! rate; one that does not stands the decode signal down). Both re-probe on
//! [`CAP_REPROBE_WINDOWS_MIN`]. Climbs require utilization (delivered ≈
//! target) and stay within ×1.5 of the windowed proven mark. Tests pin this.

mod decode;
mod encode;

use super::cap::LearnedCap;
use super::growth::{self, Proven, CLEAN_WINDOWS_TO_INCREASE};
use super::sample::{WindowActivity, WindowSample};
use super::verdict::{
    encode_thresholds, Baselines, Reason, Verdict, BLIP_CLEAN_WINDOWS, HEAVY_LOSS_PPM,
};
use crate::quic::AckReason;
use decode::DecodeKnee;
use encode::EncodeHold;
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
pub(super) const CLEAN_WINDOWS_TO_REARM: u32 = 8;
/// Consecutive ordinary-bad windows before a decrease. One 750 ms window can
/// be a scheduler blip; 1.5 s is a condition. Severe skips the wait.
const BAD_WINDOWS_TO_DECREASE: u32 = 2;
/// Minimum gap between requests. Each accepted change rebuilds the encoder
/// and opens with an IDR; back-to-back steps outrun the ack RTT.
const CHANGE_COOLDOWN: Duration = Duration::from_millis(1500);
/// A step is answered when its signal moves by this much of the frame budget
/// at the new rate. A pipelined decoder sits above the period with no queue
/// and a contended GPU holds its encode time up whatever the rate: neither
/// answers, and the driver that stepped stands down.
const ANSWER_PCT: i64 = 5;
/// One notch: 12.5 % off. Small enough that a wrong one costs little, big
/// enough that a signal which does follow the rate says so within the
/// window's own noise.
const RETREAT_DIV: u32 = 8;
/// Two decode-driven backoffs latch [`decode_cap_kbps`] only when their
/// pre-backoff rates agree within ±1/8. A cascade's second backoff sits at
/// ×0.7 of the first — outside the band by construction — so only a
/// climbed-to rate (`climb_since_backoff`) can sample the knee.
pub(super) const DECODE_CAP_SIMILAR_DIV: u32 = 8;
/// Unacked [`crate::quic::SetBitrate`] requests the controller sends before
/// it waits for an answer.
const MAX_UNACKED: u32 = 3;
/// An ask this old with no answer is a host that predates renegotiation, and
/// the controller goes quiet. A deep queue answers late — 3.3 s on the rig's
/// cell — and a late answer must not retire it for the session.
const ACK_GIVE_UP: Duration = Duration::from_secs(10);
/// Where a link-attributed cut lands: this share of what the window actually
/// delivered. The 15 % held back is what drains the queue the overshoot
/// built; ×0.7 of a rate the link never carried drains nothing.
const LINK_CUT_PCT: u32 = 85;
/// The furthest one link-attributed cut may go. A window that delivered
/// almost nothing measured an interruption, not a capacity, and ×0.7 never
/// moved more than this in one step either.
const LINK_CUT_FLOOR_PCT: u32 = 50;
/// Windows a link-attributed cut is given to drain what it queued. 6 × 750 ms
/// is 4.5 s, past the deepest queue a wall of this kind sits behind (450 ms of
/// buffer, drained at the margin one cut holds back); the delay coming back to
/// where it was ends it sooner, so it composes with the change cooldown
/// instead of adding to it.
pub(super) const LINK_DRAIN_WINDOWS: u32 = 6;
/// Delay fall across a window that counts as a queue emptying. 5 ms over
/// 750 ms is past the fit's own noise on a jittery link and well under one
/// frame period at any refresh.
pub(super) const DRAIN_FALL_US: i64 = 5_000;
/// How far under the wall it measured the link cap sits, as a divisor. A
/// tenth is the room a link that moves by a few percent needs to move in
/// without the queue answering; climbs stop at the cap, so this is also where
/// the session rides.
const LINK_HOLD_DIV: u32 = 10;
/// How far over its frozen reference the delay may go at a lifted rate
/// before the lift is refused, as an absolute floor and as a share of the
/// reference. 15 ms is past any pacing jitter at any refresh; a quarter of the
/// reference is what a link whose baseline delay is already tens of
/// milliseconds needs instead.
const LIFT_BAR_US: i64 = 15_000;
const LIFT_BAR_DIV: i64 = 4;
/// Consecutive windows over that bar, or rising under it, that refuse a lift.
/// Two, because one window is a handover stall or a send loop losing a slice:
/// both read as a mean over the bar and are back at the reference the next
/// window, while a queue that is filling keeps climbing.
const LIFT_OVER_WINDOWS: u32 = 2;
const LIFT_RISING_WINDOWS: u32 = 2;
/// Clean windows at the lifted rate that settle it: the cap keeps the new
/// value and the delay baseline learns again. 8 x 750 ms = 6 s.
const LIFT_PROBE_WINDOWS: u32 = 8;
/// Windows a lift may wait for the climb to take it up before the probe is
/// dropped. Content that does not fill the target never asks for the lifted
/// rate, and a probe left open holds the delay baseline still.
const LIFT_PROBE_MAX_AGE: u32 = 16;
/// Clean windows at the current rate whose delivered wire rate is the norm a
/// short window is judged against. Four is 3 s: long enough to average out a
/// scene, short enough to be this rate's own regime.
const DELIVERY_REF_WINDOWS: u32 = 4;
/// A window is short when it carried less than this share of that norm. The
/// wire dropped against itself; under `current_kbps` means nothing, because
/// what a clean window delivers depends on the content's fill and the parity
/// floor, not only on the link.
const DELIVERY_SHORT_PCT: u32 = 90;
/// A source that produced under a quarter of the frames the delivery norm was
/// taught at went still: its wire rate says nothing about the link. A quarter
/// keeps a game's slow scene, at half its usual frames, a judged window.
const STILL_FRAMES_DIV: u64 = 4;
/// Two delivered rates this close are the same wall (±1/5). Wider than the
/// decode cap's ±1/8 because a wall is a moving physical thing — Wi-Fi and a
/// cell both wander further than that inside a minute.
const LINK_MARK_SIMILAR_DIV: u32 = 5;

/// A cap lift awaiting the link's answer, and the delay the session had
/// before it.
///
/// The reference is frozen at the lift: the baseline the ordinary delay
/// verdict uses would learn a slow rise and then call it normal, which is how
/// a 5 % overshoot fills a 450 ms queue for a minute and is first noticed as a
/// lost frame. `settled` is the probe's budget spent — the lift is kept and
/// the baseline learns again, but the reference stays, because a wall can
/// answer a lift late and that is still the lift's fault, not a cut's.
#[derive(Clone, Copy, Debug)]
struct LiftProbe {
    /// Where a refused lift retreats to: the cap as it was.
    cap_kbps: u32,
    ref_us: i64,
    clean: u32,
    over: u32,
    rising: u32,
    /// Windows since the lift, at any rate.
    age: u32,
    settled: bool,
}

/// A settled step: what the signal that took it answered at the new rate.
enum StepAnswer {
    /// The signal followed the rate: the step stands.
    Followed,
    /// The step cost the signal its headroom: back to this rate.
    Restore(u32),
    /// The signal does not follow the rate: back to this rate, and its driver
    /// stands down.
    StandDown(u32),
}

/// The acked rate a step leaves from, and the floor no step goes under.
#[derive(Clone, Copy)]
struct Rate {
    kbps: u32,
    floor_kbps: u32,
}

impl Rate {
    /// One notch off the rate, never under the floor.
    fn notch(self) -> u32 {
        (self.kbps - self.kbps / RETREAT_DIV).max(self.floor_kbps)
    }
}

/// This window's damage is the host encoder's and nothing else's: encode time
/// is the signal that named it, and no loss share came with it.
fn encode_named(w: &WindowSample, v: &Verdict) -> bool {
    v.reason == Reason::Encode && w.loss_ppm < HEAVY_LOSS_PPM
}

/// This window's damage is the client decoder's and nothing else's: decode
/// latency is the signal that named it, and no loss share came with it.
fn decode_named(w: &WindowSample, v: &Verdict) -> bool {
    v.reason == Reason::Decode && w.loss_ppm < HEAVY_LOSS_PPM
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
    /// The ceiling is still the rate the Welcome resolved blind — nothing has
    /// been measured and nothing has ruled a measurement out. The first of
    /// either settles it, and the first measurement binds, up or down.
    blind: bool,
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
    /// The last bad window was the link's, and the link handed over less than
    /// it was asked for. Only then does the rate land on what was delivered.
    link_verdict: bool,
    /// The last link-attributed cut's own queue, still emptying.
    drain: Drain,
    /// Windows left in which a second lone lost frame is ordinary damage. The
    /// blip exemption is spent once; a link losing a frame every window is
    /// not a recovery-plane event however clean each window looks.
    blip_hold: u32,
    /// Where the link stopped carrying what it was asked for. Latched from
    /// two deliveries at the same rate (the ramp's wall counts as one), it
    /// holds the climb a tenth under that rate and is re-tested on the same
    /// clock the other two caps use.
    pub(super) link_cap: LearnedCap,
    /// Previous delivered mark (`0` = none). Two within ±1/5 are a wall.
    pub(super) link_mark_kbps: u32,
    /// What clean windows at this rate deliver, and the delay they sit at.
    norms: RateNorms,
    /// The standing cap came from a measurement, which holds 30 % back by
    /// design. Its early lifts are that margin coming back, not a wall that
    /// moved, so they must not retire it.
    link_cap_measured: bool,
    /// The last bad window was the link's. Wider than
    /// [`link_verdict`](Self::link_verdict): a session sitting exactly on its
    /// wall is getting what it asks for and still has to learn where it is.
    link_evidence: bool,
    /// A lift the wall has not answered. A second one means the session held
    /// the lifted rate for a whole re-probe interval: the wall moved, and the
    /// cap goes rather than crawl after it at +12.5 % a time.
    link_lifted: bool,
    /// The lift in flight, and the delay the session had before it.
    lift: Option<LiftProbe>,
    /// Rolling minima the relative signals are scored against.
    baselines: Baselines,
    /// One refresh interval, µs. `None` = the 120 Hz [`ENCODE_RISE_US`] defaults.
    frame_budget_us: Option<i64>,
    /// The host encoder's notch, stand-down and cadence hold. Mode-scoped.
    pub(super) encode: EncodeHold,
    /// Two identical short acks latch this. Kept apart from `ceiling_kbps` so a
    /// mode switch does not drop probe-measured link authority.
    pub(super) host_cap: LearnedCap,
    /// This session's share of a path it is not alone on
    /// ([`super::governor`]). A plain ceiling, not a [`LearnedCap`]: the host
    /// owns both directions and re-tests the path itself, so there is no
    /// clock here to re-probe on. Survives a mode switch — the group does.
    pub(super) share_cap: Option<u32>,
    /// A share above this session's ceiling owes the link cap one early step,
    /// taken at the first window that leaves a delay to judge it against.
    share_lift: bool,
    /// The asks the host has not answered, and its short answers.
    pub(super) acks: AckTracker,
    /// The client decoder's knee, headroom step and stand-down. Mode-scoped.
    pub(super) decode: DecodeKnee,
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
            blind: true,
            stream_cap_kbps: None,
            floor_kbps: FLOOR_KBPS.min(start_kbps.max(1)),
            probing: true,
            rate_verdict: false,
            rearm_windows: 0,
            link_verdict: false,
            drain: Drain::default(),
            blip_hold: 0,
            link_cap: LearnedCap::new(),
            link_mark_kbps: 0,
            norms: RateNorms::default(),
            link_cap_measured: false,
            link_evidence: false,
            link_lifted: false,
            lift: None,
            baselines: Baselines::new(),
            frame_budget_us: None,
            encode: EncodeHold::default(),
            host_cap: LearnedCap::new(),
            share_cap: None,
            share_lift: false,
            acks: AckTracker::default(),
            decode: DecodeKnee::default(),
            proven: Proven::new(),
            idle_windows: 0,
            low_rate_warned: false,
            bad_windows: 0,
            clean_windows: 0,
            last_change: None,
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

    /// The climb ceiling from a measured link capacity (caller already
    /// subtracted headroom). The env cap and the stream shape clamp here —
    /// the one funnel every learned ceiling passes through.
    ///
    /// The FIRST measurement binds, up or down: the rate the Welcome resolved
    /// is not evidence about the link, so a reading under it is the wall, not
    /// noise to discard (#1131). Later ones only raise — a congested-moment
    /// measurement must not shrink what an earlier one proved.
    pub(crate) fn set_ceiling(&mut self, kbps: u32) {
        let measured = kbps;
        let kbps = kbps
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .min(self.stream_cap_kbps.unwrap_or(u32::MAX));
        if !self.enabled {
            return;
        }
        // A clamped reading is the stream shape or the operator's cap talking,
        // not the link: it may raise the ceiling, never bind it downward.
        let clamped = kbps < measured;
        if clamped {
            // Log both numbers when it binds; a silent trim is undiagnosable.
            tracing::info!(
                measured_kbps = measured,
                bounded_kbps = kbps,
                "adaptive bitrate: link ceiling bounded by what this stream can use"
            );
        }
        if (self.blind && !clamped) || kbps > self.ceiling_kbps {
            self.ceiling_kbps = kbps.max(self.floor_kbps);
        }
        self.blind = false;
    }

    /// Nothing will be measured this session: the probe is off, the host
    /// declined it, or it timed out.
    ///
    /// No measurement is not no authority. What the stream can use is a bound
    /// on the absurd, not a claim about the link (L1), and the growth law
    /// still has to earn every step up to it — so it becomes the ceiling
    /// instead of the rate the Welcome guessed, which bounded nothing and
    /// pinned every un-probed session at 20 Mbps (#1175).
    pub(crate) fn no_link_evidence(&mut self, stream_cap_kbps: u32) {
        if !self.blind || !self.enabled {
            return;
        }
        self.blind = false;
        let kbps = stream_cap_kbps
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .max(self.ceiling_kbps);
        if kbps > self.ceiling_kbps {
            tracing::info!(
                ceiling_kbps = kbps,
                "adaptive bitrate: nothing measured the link — the stream's own shape is the \
                 bound, and every step up to it still has to be earned"
            );
            self.ceiling_kbps = kbps;
        }
    }

    /// The host granted a rate above the ceiling we thought we had. Not a link
    /// measurement: it raises, and leaves a later measurement free to bind.
    fn raise_ceiling(&mut self, kbps: u32) {
        let kbps = kbps
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .min(self.stream_cap_kbps.unwrap_or(u32::MAX));
        if self.enabled && kbps > self.ceiling_kbps {
            self.ceiling_kbps = kbps;
        }
    }

    /// Open the session at what the bring-up ramp measured, instead of the
    /// rate the Welcome resolved blind. `None` = nothing to ask for.
    ///
    /// Floored and clamped like any other ask. The caller sets the ceiling
    /// first, so a measured start is never above the authority that measured
    /// it.
    pub(crate) fn start_from_measurement(&mut self, kbps: u32, now: Instant) -> Option<u32> {
        let kbps = kbps
            .max(self.floor_kbps)
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .min(self.ceiling_kbps);
        if !self.enabled || kbps == self.current_kbps {
            return None;
        }
        tracing::info!(
            from_kbps = self.current_kbps,
            to_kbps = kbps,
            "adaptive bitrate: opening the session at what the ramp measured"
        );
        self.request(kbps, now)
    }

    /// Bound future learned ceilings (same funnel as the env cap). The first
    /// set leaves a negotiated start above it standing. A re-set is a mode
    /// switch: a drop in pixel rate rebinds the standing ceiling because
    /// [`set_ceiling`](Self::set_ceiling) never lowers below what it learned.
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
    /// durations. Ignored for a nonsense rate — the defaults stand. Past 1 MHz the
    /// budget rounds to 0 µs, which the decode check divides by.
    pub(crate) fn set_frame_budget(&mut self, refresh_hz: u32) {
        if (1..=1_000_000).contains(&refresh_hz) {
            self.frame_budget_us = Some(1_000_000 / refresh_hz as i64);
        }
    }

    /// The decoder's answer to a step in flight. One it did not answer gives
    /// the rate back and withholds the decode verdict: the cuts it named were
    /// not the rate's either.
    fn judge_decode_step(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        let budget_us = self.frame_budget_us?;
        let answer = self.decode.judge_step(w, v, budget_us, self.rate())?;
        let kbps = match answer {
            StepAnswer::Followed => return None,
            StepAnswer::Restore(kbps) => kbps,
            StepAnswer::StandDown(kbps) => {
                self.baselines.clear_decode();
                if self.streak_cut == Some(Reason::Decode) {
                    self.rate_verdict = false;
                }
                kbps
            }
        };
        self.ceiling_ask_kbps = kbps;
        self.request(kbps, w.now)
    }

    /// Host [`crate::quic::BitrateChanged`]: the clamp is authoritative, and any
    /// ack proves the host renegotiates. Two identical short acks latch
    /// [`host_cap_kbps`](Self::host_cap_kbps); one can be a failed rebuild.
    ///
    /// [`AckReason`] says which limit answered. A pinned session has no rate to
    /// control; a cadence refusal is a busy GPU, held on a clock and never
    /// latched; a governor share is a ceiling, not an answer to anything this
    /// session asked. An ack with no reason is an older host, read as today.
    pub(crate) fn on_ack(&mut self, kbps: u32, why: Option<AckReason>) {
        if why == Some(AckReason::Pinned) {
            self.on_pinned(kbps);
            return;
        }
        if why == Some(AckReason::Governor) {
            self.on_share(kbps);
            return;
        }
        if kbps > 0 {
            if kbps < self.current_kbps {
                self.baselines.clear_encode();
            }
            if why == Some(AckReason::Cadence) {
                // Not evidence about a rate: the encoder is behind, and the
                // same fact the encode driver already handles. No cut, no cap.
                self.acks.last_requested_kbps = None;
                self.acks.short_acks = 0;
                self.encode.hold_for_cadence(kbps);
            } else if let Some(req) = self.acks.last_requested_kbps.take() {
                if kbps < req {
                    if self.acks.on_short(kbps) && self.host_cap.latch(kbps, self.floor_kbps) {
                        tracing::info!(
                            cap_kbps = kbps,
                            reprobe_after_windows = self.host_cap.reprobe_after(),
                            "adaptive bitrate: host cap learned (encoder ceiling or cadence \
                             refusal) — climbs stop here until it lifts"
                        );
                    }
                } else {
                    self.acks.short_acks = 0;
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
                self.decode.climb_since_backoff = true;
                self.last_cut = None;
            }
            if kbps != self.current_kbps {
                // The host moved the rate: a clamp, or a re-resolve nobody
                // asked for. Either way what the old rate put on the wire
                // says nothing about this one.
                self.norms.forget();
            }
            self.current_kbps = kbps;
            // Unsolicited `BitrateChanged` can sit above our ceiling (host
            // re-resolved Automatic for what it encodes). Follow it; env cap
            // still binds. Without this, the step-down drags the host back.
            self.raise_ceiling(kbps);
        }
        self.acks.unacked = 0;
    }

    /// The host divided a path this session shares with another
    /// ([`super::governor`]): `kbps` is the ceiling it may climb to, and
    /// [`NO_SHARE_KBPS`](super::governor::NO_SHARE_KBPS) is the group ending.
    ///
    /// Never a target, and never authority to climb: the growth law still has
    /// to earn every step under it. A share below the live rate is a retarget
    /// the host has already applied, so it is the rate now — reading it as a
    /// short ack instead would latch a host cap off the host's own clamp.
    fn on_share(&mut self, kbps: u32) {
        self.acks.unacked = 0;
        self.acks.last_requested_kbps = None;
        self.acks.short_acks = 0;
        // Whatever the overlay was naming, it is not what moved the rate now.
        // A share has no cause among the five the wire carries, and showing
        // the last link fault instead would be a lie.
        self.last_cut = None;
        if kbps == 0 {
            self.share_cap = None;
            tracing::info!("adaptive bitrate: alone on this path again — the share is released");
            return;
        }
        self.share_cap = Some(kbps);
        if kbps < self.current_kbps {
            self.baselines.clear_encode();
            self.current_kbps = kbps;
            // The host moved the rate to divide the path. What the old rate
            // put on the wire, and the delay behind it, are not this one's.
            self.norms.forget();
        }
        // A share above this session's ceiling is the host saying the path has
        // room, not that this session's wall was a sibling's queue: one
        // address is one NAT, and the sibling may be on other air. The
        // allowance rises, the wall stands, and the cap is asked again soon.
        if kbps > self.ceiling_kbps {
            self.raise_ceiling(kbps);
            self.share_lift = true;
        }
        tracing::info!(
            share_kbps = kbps,
            "adaptive bitrate: the host divided this path — climbs stop at this session's share"
        );
    }

    /// The host will not negotiate this session's rate (PyroWave: per-frame
    /// CBR). Nothing to control, so retire quietly — an unanswered host is
    /// already retired the same way.
    fn on_pinned(&mut self, kbps: u32) {
        self.acks.last_requested_kbps = None;
        self.acks.unacked = 0;
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

    /// Drop mode-scoped learned state. Encoder/decoder knees and rolling
    /// baselines are properties of the mode; a baseline from the old mode is a
    /// floor the new one clears on the first window. Probe-measured
    /// `ceiling_kbps` and the link cap are link properties and survive — the
    /// wall does not move because the client changed resolution. Proven
    /// throughput re-earns.
    pub(crate) fn on_mode_switch(&mut self) {
        self.host_cap.drop_cap();
        self.acks.short_acks = 0;
        self.decode = DecodeKnee::default();
        // Encode work per frame changed with the mode. Re-arm; the caller
        // re-sizes the frame budget alongside this.
        self.encode = EncodeHold::default();
        self.baselines.clear();
        self.proven.clear();
        self.idle_windows = 0;
        // A frame of the new mode is a different size on the wire.
        self.norms.forget();
        self.norms.frames_norm = 0;
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
        if self.acks.unacked >= MAX_UNACKED {
            // No new ask until an answer lands; an older host logs every
            // unknown message. Silence past the give-up is that older host.
            let oldest = self.acks.first_unacked.unwrap_or(w.now);
            if w.now.duration_since(oldest) >= ACK_GIVE_UP {
                self.enabled = false;
                tracing::info!("adaptive bitrate off — host never acked a SetBitrate (older host)");
            }
            return None;
        }
        let draining = self.drain.note(w);
        self.blip_hold = self.blip_hold.saturating_sub(1);
        let v = self.baselines.score(
            w,
            self.current_kbps,
            self.frame_budget_us,
            self.encode.down.disarmed(),
            self.decode.headroom.disarmed(),
            self.clean_windows,
            draining,
            self.link_vouches_for(w),
            self.lift_probing(),
            self.norms.went_still(w),
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
        // A lift the link refused is answered before anything else: the
        // session was fine at the cap one window ago, so the answer is that
        // rate and not a fraction of this one.
        if let Some(kbps) = self.judge_lift(w, w.now) {
            return Some(kbps);
        }
        if let Some(kbps) = self.judge_encode_step(w, &v) {
            return Some(kbps);
        }
        if let Some(kbps) = self.judge_decode_step(w, &v) {
            return Some(kbps);
        }
        if (self.bad_windows >= BAD_WINDOWS_TO_DECREASE || (v.severe && self.bad_windows >= 1))
            && self.current_kbps > self.floor_kbps
        {
            // Damage inside the guard is the overflow the last cut is still
            // clearing: the lost frames and the loss in it are that queue's
            // tail, and cutting again deepens what caused them. FEC and
            // keyframe recovery answer them as always. The host encoder's own
            // verdict is not the link's and is never guarded.
            if draining && !encode_named(w, &v) {
                self.drain.suppressed += 1;
                self.bad_windows = 0;
                self.decode.streak_windows = 0;
                return None;
            }
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

    /// Is a lift being judged right now? While it is, the window's delay is
    /// the lift's to answer. Not before the lifted rate runs: until then no
    /// judge is reading the delay, and a queue filling must still cut.
    fn lift_probing(&self) -> bool {
        self.lift
            .is_some_and(|p| !p.settled && self.current_kbps > p.cap_kbps)
    }

    /// How far over the frozen reference the delay may go before the lift is
    /// refused.
    fn lift_bar_us(ref_us: i64) -> i64 {
        LIFT_BAR_US.max(ref_us / LIFT_BAR_DIV)
    }

    /// One window's answer to a lift. `Some` is the retreat to ask for.
    ///
    /// The delay bar outlives the probe: a wall can answer a lift a minute
    /// later, and the session was fine at the cap, so the answer is the cap
    /// and not a cut. Loss, a lost frame and a rising trend only refuse while
    /// the probe is still open — after it the ordinary verdict owns them.
    /// The retreat backs the cap's clock off only where this session is the
    /// one on the path; on a divided one the refusal names no wall of its own.
    fn judge_lift(&mut self, w: &WindowSample, now: Instant) -> Option<u32> {
        let mut p = self.lift?;
        p.age += 1;
        if self.current_kbps <= p.cap_kbps {
            // The host has not applied the lift, or the climb never asked for
            // it. Nothing here is about the lift, and a probe kept open would
            // hold the delay baseline still for nothing.
            self.lift = (p.age < LIFT_PROBE_MAX_AGE).then_some(p);
            return None;
        }
        let bar = Self::lift_bar_us(p.ref_us);
        p.over = match w.delay {
            Some(d) if d.mean_us > p.ref_us.saturating_add(bar) => p.over + 1,
            Some(_) => 0,
            None => p.over,
        };
        // Rising, and already halfway to the bar: a fit that wobbles either
        // side of the reference is not a queue filling, and a lift the link
        // is carrying must not be spent on one.
        let half_bar = p.ref_us.saturating_add(bar / 2);
        p.rising = match w.delay {
            Some(d) if d.rise_us >= DRAIN_FALL_US && d.mean_us > half_bar => p.rising + 1,
            Some(_) => 0,
            None => p.rising,
        };
        let over = p.over >= LIFT_OVER_WINDOWS;
        // The link showing itself at a lifted rate is the lift's answer,
        // whatever form it took. `link_evidence` is this window's, scored a
        // moment ago, and it already excludes the lone lost frame a long
        // clean run vouches for.
        let refused =
            over || (!p.settled && (p.rising >= LIFT_RISING_WINDOWS || self.link_evidence));
        if !refused {
            if !p.settled && !w.activity.quiet() {
                p.clean += 1;
                p.settled = p.clean >= LIFT_PROBE_WINDOWS;
            }
            self.lift = Some(p);
            return None;
        }
        self.lift = None;
        self.link_lifted = false;
        if self.share_cap.is_some() {
            // On a path the host has divided, the queue this lift met may be
            // a sibling's: backing the clock off would record a wall this
            // session cannot have seen. It retreats, and asks again soon.
            self.link_cap.park(p.cap_kbps);
        } else {
            self.link_cap.latch(p.cap_kbps, self.floor_kbps);
        }
        self.arm_drain();
        tracing::info!(
            from_kbps = self.current_kbps,
            to_kbps = p.cap_kbps,
            delay_us = w.delay.map_or(-1, |d| d.mean_us),
            reference_us = p.ref_us,
            settled = p.settled,
            reprobe_after_windows = self.link_cap.reprobe_after(),
            "adaptive bitrate: the link refused the lift — back to the cap it came from"
        );
        self.bad_windows = 0;
        self.decode.streak_windows = 0;
        // The delay is the probe's own signal; anything else came with a
        // verdict that named itself.
        self.last_cut = Some(if over || p.rising >= LIFT_RISING_WINDOWS {
            Reason::Owd
        } else {
            self.last_reason
        });
        self.request(p.cap_kbps, now)
    }

    /// Does this window say for itself what a clean run says: one frame gone
    /// and a link with room behind it?
    ///
    /// The wire carried what the rate asked for and the queue is not growing,
    /// so nothing here is the rate's doing — which is what the run stands for
    /// and what a session climbing back to its cap can never accumulate. One
    /// exemption at a time: [`blip_hold`](Self::blip_hold) holds the next
    /// window to the ordinary verdict.
    fn link_vouches_for(&self, w: &WindowSample) -> bool {
        self.blip_hold == 0
            && w.dropped == 1
            && w.delay.is_some_and(|d| d.rise_us < DRAIN_FALL_US)
            && !self.short_of_offered(w, false)
    }

    /// Did the link hand over less than the rate it was running at?
    ///
    /// The climb's own bar, prorated by the frames that arrived: content that
    /// never filled the target is not the link falling short, and reading it
    /// as one would land the rate on a still picture. A source that went still
    /// is never short unless a queue is holding its frames.
    fn short_of_offered(&self, w: &WindowSample, owd_bad: bool) -> bool {
        if !owd_bad && self.norms.went_still(w) {
            return false;
        }
        if let Some(reference) = self.norms.delivery_reference() {
            return u64::from(w.actual_kbps) * 100
                < u64::from(reference) * u64::from(DELIVERY_SHORT_PCT);
        }
        // Nothing to compare against yet. The climb's own bar, prorated by the
        // frames that arrived; a standing queue breaks that denominator, so
        // when delay says so the wall clock is the honest measure.
        let proration = growth::proration(w.activity, self.frame_budget_us);
        if !growth::utilized(w.activity, proration, w.actual_kbps, self.current_kbps) {
            return true;
        }
        owd_bad && !growth::delivered_the_rate(w.actual_kbps, self.current_kbps)
    }

    /// A wall a deliberate measurement found, and the rate it licenses.
    ///
    /// One reading latches it. Two agreeing windows are what the controller
    /// needs when nobody measured; a ramp step or a burst offered the link
    /// several times what the session will and watched it refuse, which is
    /// the same evidence gathered on purpose. The cap goes at the licensed
    /// rate, not a tenth under it: the measurement already holds 30 % back,
    /// and the hold margin exists for a wall read off a window the session
    /// was merely running in.
    ///
    /// From here the cap is a cap like any other — parked at, lifted +12.5 %
    /// on the clock, re-latched with a doubled wait when the wall answers,
    /// dropped when two lifts go unanswered. That is the whole point: a
    /// measurement that binds for the session's life is a bound with no
    /// expiry (L1), and on Klos54's tunnel it cost 45 % of the link.
    pub(crate) fn note_measured_wall(&mut self, park_kbps: u32, delivered_kbps: u32) {
        // A wall that licenses more than this stream can use is not a limit
        // on the session: the stream's own shape is, and it is already the
        // ceiling.
        // A wall licensing more than the stream can use is not a limit on
        // the session: its own shape is, and that is already the ceiling.
        if !self.enabled || park_kbps == 0 || self.stream_cap_kbps.is_some_and(|c| park_kbps >= c) {
            return;
        }
        self.link_mark_kbps = delivered_kbps;
        self.link_cap_measured = true;
        if self.link_cap.latch_measured(park_kbps, self.floor_kbps) {
            tracing::info!(
                cap_kbps = park_kbps,
                delivered_kbps,
                reprobe_after_windows = self.link_cap.reprobe_after(),
                "adaptive bitrate: link cap measured — the climb holds here until the \
                 re-probe clock asks the wall again"
            );
        }
    }

    /// The link's wall as this controller holds it: the delivered mark that latched the cap,
    /// or the cap once lifts carried it past the mark. `None` while no cap stands.
    pub(crate) fn link_wall_kbps(&self) -> Option<u32> {
        self.link_cap.kbps().map(|cap| cap.max(self.link_mark_kbps))
    }

    /// What a link-attributed cut delivered: one mark toward the wall.
    ///
    /// Two marks at the same rate are a wall, and the bring-up ramp's own wall
    /// is the first of them. The cap sits a tenth under what was delivered —
    /// the rate the session then rides — so the re-probe ladder's +12.5 %
    /// lands just above the wall and asks it again. Once a cap stands, one
    /// mark re-latches it: that mark is the wall's answer to a lift.
    pub(crate) fn note_link_mark(&mut self, delivered_kbps: u32) {
        if !self.enabled || delivered_kbps == 0 {
            return;
        }
        let similar = self.link_mark_kbps > 0
            && delivered_kbps.abs_diff(self.link_mark_kbps)
                <= self.link_mark_kbps / LINK_MARK_SIMILAR_DIV;
        let previous = std::mem::replace(&mut self.link_mark_kbps, delivered_kbps);
        if !similar && self.link_cap.kbps().is_none() {
            tracing::debug!(
                delivered_kbps,
                previous_kbps = previous,
                "adaptive bitrate: one mark toward the link's wall"
            );
            return;
        }
        let hold = delivered_kbps.saturating_sub(delivered_kbps / LINK_HOLD_DIV);
        self.link_cap_measured = false;
        self.lift = None;
        // A wall a fifth below the one already learned is a different wall,
        // not the same one standing again: start its clock over rather than
        // back it off, or a link that degrades twice is re-tested minutes
        // after it recovers.
        if self
            .link_cap
            .kbps()
            .is_some_and(|c| hold < c.saturating_sub(c / LINK_MARK_SIMILAR_DIV))
        {
            self.link_cap.drop_cap();
        }
        if self.link_cap.latch(hold, self.floor_kbps) {
            self.link_lifted = false;
            tracing::info!(
                cap_kbps = hold,
                delivered_kbps,
                reprobe_after_windows = self.link_cap.reprobe_after(),
                "adaptive bitrate: link cap learned — the climb holds under this wall until \
                 the re-probe clock tests it again"
            );
        }
    }

    /// Arm the guard: this cut queued something, and what that queue does on
    /// the way out is not fresh evidence.
    ///
    /// The reference is where clean windows sat before the cut: this rate's
    /// own norm, or the one the last rate move left behind. A session climbing
    /// back to its cap has no norm of its own when the next cut lands, and a
    /// guard with no mark to wait for spends its whole budget.
    fn arm_drain(&mut self) {
        let norms = &self.norms;
        self.drain
            .arm(norms.delay_mean_us().unwrap_or(norms.delay_norm_us));
    }

    /// The rate a cut is measured from: the ask still in flight, when there is
    /// one.
    ///
    /// A `BitrateChanged` waits behind the same queue the video does — 3.3 s on
    /// the rig's cell — and every window until it lands still reads the rate
    /// the session has already asked to leave. Cutting from that rate three
    /// times took a session to 2 464 kbps where one cut would have left it at
    /// about 2 480.
    fn cut_base_kbps(&self) -> u32 {
        self.acks
            .last_requested_kbps
            .map_or(self.current_kbps, |asked| asked.min(self.current_kbps))
    }

    /// Where a link-attributed cut lands: what the window delivered, less the
    /// margin that drains the queue, and never further than one ×0.7-sized
    /// step from the rate the session is running at.
    fn link_cut_kbps(&self, delivered_kbps: u32) -> u32 {
        let base = self.cut_base_kbps();
        let share = |kbps: u32, pct: u32| (u64::from(kbps) * u64::from(pct) / 100) as u32;
        // The drop the wire showed against its own norm, put back into target
        // units. A content-bound source delivers 78 % of every clean window
        // and the parity floor puts 210 % on the wire at the bottom of the
        // range; dividing by the norm cancels both, and what is left is the
        // link's share of the fall.
        let target = match self.norms.delivery_reference() {
            Some(reference) if reference > 0 => {
                (u64::from(delivered_kbps) * u64::from(base) / u64::from(reference)) as u32
            }
            _ => delivered_kbps,
        };
        share(target, LINK_CUT_PCT)
            .clamp(share(base, LINK_CUT_FLOOR_PCT), share(base, LINK_CUT_PCT))
            .max(self.floor_kbps)
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
            self.blip_hold = BLIP_CLEAN_WINDOWS;
            // A window the decoder sailed through ends the knee reference on
            // the terms a backoff without decode evidence ends it: a climbed-to
            // rate, delivery that means something. Nothing else expires one.
            if self.decode.climb_since_backoff && !v.starved {
                self.decode.backoff_kbps = 0;
            }
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
            self.norms.note(w);
        }
        if v.bad {
            // What the rate is the lever for, read before the streaks move:
            // loss share, a delay rise, a flush, drops the clean run does not
            // vouch for, and a decoder past its budget. Keyframe asks and one
            // lost frame behind a clean window are not.
            let repeated_drops = w.dropped > 1 || (w.dropped == 1 && self.clean_windows == 0);
            let signature = super::verdict::link_signature(w);
            let link = w.loss_ppm >= HEAVY_LOSS_PPM
                || v.owd_bad
                || w.flushed
                || repeated_drops
                || signature;
            // A decoder past its budget is a rate verdict but not a link one:
            // the link delivered, the client could not decode it. Host encode
            // says nothing until its notch is answered: the answer writes this.
            if v.reason != Reason::Encode {
                self.rate_verdict = link || v.decode_bad;
            }
            // The link showed itself in one of two ways: a queue filling on
            // this session's own delay floor, or a window that carried less
            // than it was asked for. A link with room shows neither, so
            // neither branch can fire where there is no wall (L2).
            let short = self.short_of_offered(w, v.owd_bad);
            self.link_evidence = link && (v.owd_bad || short);
            // Only a shortfall, or a socket that could not keep up, makes what
            // was delivered the number to land on. At the wall itself the
            // session is already getting what it asks for, and the blind step
            // is what drains the queue.
            self.link_verdict = link && (short || signature);
            self.bad_windows += 1;
            self.streak_cut = Some(v.reason);
            self.clean_windows = 0;
            self.decode.note_bad(v, self.rate());
            // Any congestion ends slow start until a later idle-onset re-arm.
            self.probing = false;
        } else if !v.quiet {
            // Stillness is neither climb credit nor a cleared streak.
            self.clean_windows += 1;
            self.bad_windows = 0;
            self.decode.streak_windows = 0;
            // A window the link did not damage is what the cap's clock runs on.
            self.link_evidence = false;
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
        // A knee reference is not refuted by a clean run either. A wall the
        // session ran into is: the doubling cannot pass it, so a session left
        // far under it comes back in seconds. A wall a measurement set is
        // not — it holds 30 % back by design, so being under it is the
        // ordinary state, not a verdict's aftermath.
        let far_under_the_wall = !self.link_cap_measured
            && self
                .link_cap
                .kbps()
                .is_some_and(|c| self.current_kbps < c.saturating_sub(c / LINK_HOLD_DIV));
        if self.probing
            || (self.rate_verdict && !far_under_the_wall)
            || self.decode.backoff_kbps > 0
        {
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

    /// One window on each re-probe clock: the three learned caps and the
    /// encoder's two stand-downs. A lift above the decode cap starts the probe
    /// whose answer decides whether it holds.
    fn tick_caps(&mut self, v: &Verdict) {
        let (bad, quiet, rate, ceiling) = (v.bad, v.quiet, self.current_kbps, self.ceiling_kbps);
        // The step a share asked for, once a clean window at this rate has
        // left a delay to freeze. Taken any sooner it is a commitment, and
        // the host's evidence is about the path, not about this session's
        // own air — so it is the one lift that must not go unjudged.
        if self.share_lift && self.norms.delay_mean_us().is_some() {
            self.share_lift = false;
            self.link_cap.lift_now();
        }
        if let Some((from, to)) = self.host_cap.on_window(bad, quiet, rate, ceiling) {
            tracing::debug!(
                from_kbps = from,
                to_kbps = to,
                "adaptive bitrate: re-probing above the learned host cap"
            );
        }
        self.decode.tick(v, rate, ceiling);
        // Only the wall breaks this park, by re-latching: a cell drops a frame
        // every few seconds, so a clock waiting for clean windows never runs.
        let lift_to = self
            .stream_cap_kbps
            .unwrap_or(u32::MAX)
            .min(self.ceiling_cap_kbps.unwrap_or(u32::MAX))
            .max(ceiling);
        // `lift_to` is the stream's shape, not the ceiling: on a measured
        // wall the ceiling IS this cap, so clamping there clamps it to itself.
        if let Some((from, to)) = self.link_cap.on_window(false, quiet, rate, lift_to) {
            // A measured wall is the ceiling as well as the cap, so asking it
            // again means carrying the ceiling up with the answer. Nothing
            // else's authority moves: the host and decode caps keep the
            // bound they had.
            self.raise_ceiling(to);
            if std::mem::replace(&mut self.link_lifted, true) && !self.link_cap_measured {
                self.link_cap.drop_cap();
                self.link_lifted = false;
                self.lift = None;
                // The link just carried a rate the cap said it could not.
                // Doubling back to what it now holds is what makes coming
                // back cost about what the cut cost.
                self.probing = true;
                self.rate_verdict = false;
                tracing::info!(
                    was_kbps = from,
                    "adaptive bitrate: the link carried a lifted cap for a whole re-probe \
                     interval — the wall moved, dropping it and doubling after it"
                );
            } else {
                // Freeze what the session's delay looked like before the ask.
                // Everything after this judges the lift against this number,
                // not against a baseline that will learn the rise. No delay
                // at this rate is no reference: the ordinary verdict keeps
                // the lift, as it did before there was a probe.
                self.lift = self.norms.delay_mean_us().map(|ref_us| LiftProbe {
                    cap_kbps: from,
                    ref_us,
                    clean: 0,
                    over: 0,
                    rising: 0,
                    age: 0,
                    settled: false,
                });
                tracing::info!(
                    from_kbps = from,
                    to_kbps = to,
                    reference_us = self.lift.map_or(-1, |p| p.ref_us),
                    "adaptive bitrate: asking the link's wall again — lifting the cap"
                );
            }
        }
        self.encode.tick(bad, quiet, &mut self.baselines);
    }

    /// The ×0.7 step, and what this window taught on the way down: a decoder
    /// knee when the evidence is the decoder's, a host-encode level when the
    /// encoder is the only explanation. Nothing while a decode step at its new
    /// rate is still being judged on this same latency.
    fn back_off(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        // A decode step at its new rate owns the question: this window's
        // latency is its answer, not a second cut. Any other cut moves the
        // rate from under the step, so the step is dropped.
        if decode_named(w, v) && self.decode.owns_cut(self.current_kbps) {
            return None;
        }
        self.decode.probe = None;
        if encode_named(w, v) {
            return self.retreat_encode(w);
        }
        self.decode.learn_knee(w, v, self.rate());
        // A backoff with no climb behind it is draining the previous one, not
        // meeting the wall: the same reference the decode cap keeps.
        if self.link_evidence && self.decode.climb_since_backoff {
            self.note_link_mark(w.actual_kbps);
        }
        self.decode.climb_since_backoff = false;
        // Whatever kind of cut this is, if the link is what it answered then
        // the queue it leaves behind is this cut's own tail.
        if self.link_evidence {
            self.arm_drain();
        }
        let next = if self.link_verdict {
            let next = self.link_cut_kbps(w.actual_kbps);
            tracing::info!(
                from_kbps = self.current_kbps,
                to_kbps = next,
                delivered_kbps = w.actual_kbps,
                reason = ?v.reason,
                "adaptive bitrate: the link carried less than it was asked for — down to what \
                 it delivered, and a falling delay while it drains is not a second verdict"
            );
            next
        } else {
            let from = self.cut_base_kbps();
            // The decoder's cut is judged like its notch: the verdict at the
            // new rate is its answer, and a rate it does not answer comes back.
            if decode_named(w, v) && self.frame_budget_us.is_some() {
                self.decode.start_cut(from, v);
            }
            ((from as u64 * 7 / 10) as u32).max(self.floor_kbps)
        };
        self.warn_low_rate(next);
        self.bad_windows = 0;
        self.decode.streak_windows = 0;
        self.last_cut = self.streak_cut;
        self.request(next, w.now)
    }

    /// Host encode time over its frame budget costs one notch, not a cascade.
    /// The notch is a backoff like any other: the streak it answered ends, and
    /// it is no climb a knee could be sampled at.
    fn retreat_encode(&mut self, w: &WindowSample) -> Option<u32> {
        let next = self
            .encode
            .retreat(w, self.rate(), self.encode_budget_us())?;
        self.decode.climb_since_backoff = false;
        self.bad_windows = 0;
        self.decode.streak_windows = 0;
        self.last_cut = self.streak_cut;
        self.warn_low_rate(next);
        self.request(next, w.now)
    }

    /// The encoder's answer to a notch in flight. One it did not answer gives
    /// the rate back and starts the encode baseline over.
    fn judge_encode_step(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        let budget_us = self.encode_budget_us();
        match self.encode.judge_step(w, v, self.current_kbps, budget_us)? {
            StepAnswer::Followed => {
                // The encoder followed: this knee is real and the rate is its
                // lever, so slow start is not handed back over it either.
                self.rate_verdict = true;
                None
            }
            StepAnswer::Restore(kbps) | StepAnswer::StandDown(kbps) => {
                self.baselines.clear_encode();
                self.request(kbps, w.now)
            }
        }
    }

    /// The frame budget a step's answer is measured in. Without a negotiated
    /// refresh the 120 Hz durations the encode thresholds are calibrated at
    /// stand in — the rise threshold is half a budget.
    fn encode_budget_us(&self) -> i64 {
        self.frame_budget_us
            .unwrap_or_else(|| encode_thresholds(None).0 * 2)
    }

    /// The rate a step leaves from, and the floor under it.
    fn rate(&self) -> Rate {
        Rate {
            kbps: self.current_kbps,
            floor_kbps: self.floor_kbps,
        }
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

    /// Decode headroom on a clean full-rate window. The notch the bands
    /// retreat to is also the ceiling ask, so the climb's step-down does not
    /// ask for it twice.
    fn judge_headroom(&mut self, w: &WindowSample, v: &Verdict) -> Option<u32> {
        let budget_us = self.frame_budget_us?;
        let next = self.decode.judge_headroom(w, v, budget_us, self.rate())?;
        self.ceiling_ask_kbps = next;
        self.request(next, w.now)
    }

    /// A clean window under the ceilings: come down to a ceiling this session
    /// is already above, or climb toward it.
    fn climb(&mut self, w: &WindowSample) -> Option<u32> {
        let proration = growth::proration(w.activity, self.frame_budget_us);
        let utilized = growth::utilized(w.activity, proration, w.actual_kbps, self.current_kbps);
        // Probe = link, short acks = encoder, decode cap = client decoder,
        // link cap = the wall this session walked into, share = the host
        // dividing a path this session is not alone on.
        let eff_ceiling = self
            .ceiling_kbps
            .min(self.host_cap.kbps().unwrap_or(u32::MAX))
            .min(self.decode.cap.kbps().unwrap_or(u32::MAX))
            .min(self.link_cap.kbps().unwrap_or(u32::MAX))
            .min(self.share_cap.unwrap_or(u32::MAX));
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
        if self.encode.cadence.disarmed() {
            return None;
        }
        // Proven bounds the projected wire rate at ×1.5, in the same domain
        // the proration took it out of.
        let cap = eff_ceiling.min(growth::proven_target_cap(self.proven.mark(), proration));
        if self.current_kbps < eff_ceiling && utilized && cap > self.current_kbps {
            // A blip holds the doubling for as long as it holds the next lost
            // frame's exemption. The additive climb already waits six clean
            // windows; slow start would take its step one window later, and
            // half again a rate a frame has just died at is a wall's cascade.
            let slow_start = self.probing && self.clean_windows >= 1 && self.blip_hold == 0;
            if slow_start || self.clean_windows >= CLEAN_WINDOWS_TO_INCREASE {
                let next = growth::climb_step(self.current_kbps, cap, slow_start);
                self.clean_windows = 0;
                return self.request(next, w.now);
            }
        }
        None
    }

    /// Ask the host for `kbps`. Ack is authoritative: a lost request
    /// recomputes from the same base.
    fn request(&mut self, kbps: u32, now: Instant) -> Option<u32> {
        self.norms.forget();
        self.last_change = Some(now);
        self.acks.on_request(kbps, now);
        Some(kbps)
    }

    /// Control queue was full: undo request bookkeeping. [`MAX_UNACKED`]
    /// detects a host that doesn't answer; counting a message never sent
    /// retires the controller. Also keeps a later unsolicited ack from being
    /// judged short against a rate we never asked for.
    pub(crate) fn on_request_dropped(&mut self) {
        self.acks.on_dropped();
    }
}

/// The guard that holds a second cut while the last link-attributed cut's own
/// queue drains.
#[derive(Default)]
struct Drain {
    /// Windows left in which the damage is that queue emptying. `0` = nothing
    /// to drain.
    windows: u32,
    /// Delay level the guard is waiting to see again: what clean windows read
    /// before the cut. `0` = none was known, so the budget ends the guard.
    ref_us: i64,
    /// Windows the guard kept a cut off, for the line it logs when it ends.
    suppressed: u32,
}

impl Drain {
    fn arm(&mut self, ref_us: i64) {
        self.windows = LINK_DRAIN_WINDOWS;
        self.ref_us = ref_us;
        self.suppressed = 0;
    }

    /// Is this window the last link-attributed cut draining the queue it
    /// caused?
    ///
    /// The queue is the measure, not one window's slope: a queue still filling
    /// reads as rising, and standing down there is what turned one cut into
    /// four. The guard ends when the delay is back where clean windows had it
    /// before the cut, or when its budget runs out — after that the next cut
    /// is an ordinary one.
    fn note(&mut self, w: &WindowSample) -> bool {
        if self.windows == 0 {
            return false;
        }
        let delay_us = w.delay.map_or(-1, |d| d.mean_us);
        // A queue that is not emptying while frames are still being lost is
        // not this cut's tail — the rate is over the wall again, or still.
        // Damage on a falling delay is that tail, whatever form it takes.
        let over_again = (w.dropped > 0 || w.loss_ppm >= HEAVY_LOSS_PPM)
            && w.delay.is_none_or(|d| d.rise_us > -DRAIN_FALL_US);
        if over_again || (self.ref_us > 0 && w.delay.is_some_and(|d| d.mean_us <= self.ref_us)) {
            self.end(delay_us, !over_again);
            return false;
        }
        self.windows -= 1;
        if self.windows == 0 {
            self.end(delay_us, false);
        } else {
            tracing::debug!(
                windows_left = self.windows,
                delay_us,
                reference_us = self.ref_us,
                "adaptive bitrate: the queue the last cut caused is still emptying"
            );
        }
        true
    }

    /// The guard is over, and says what it kept off the rate.
    fn end(&mut self, delay_us: i64, drained: bool) {
        self.windows = 0;
        tracing::info!(
            suppressed_windows = std::mem::take(&mut self.suppressed),
            delay_us,
            reference_us = self.ref_us,
            drained,
            "adaptive bitrate: the last cut's queue is done — the link is judged again"
        );
    }
}

/// What clean windows at the current rate deliver, and the delay they sit at.
#[derive(Default)]
struct RateNorms {
    /// Delivered wire rate: the sum and the count, reset on every rate change
    /// because the parity floor and the content's fill both move with the rate.
    delivery_sum_kbps: u64,
    delivery_windows: u32,
    /// New-content frames those windows carried, summed beside them.
    delivery_frames: u64,
    /// Frames a window carried the last time a norm formed (`0` = never).
    /// Kept across rate moves: how often the source draws is not the rate's.
    frames_norm: u64,
    /// Shard delay: the sum and the count, the reference a lift is frozen
    /// against. Dropped with the delivery norm.
    delay_sum_us: i64,
    delay_windows: u32,
    /// The last delay norm, kept when a rate move drops the one above. A
    /// climb step or a cut changes the queue's occupancy, not the path's own
    /// delay, so this is still the mark a drained queue comes back to.
    delay_norm_us: i64,
}

impl RateNorms {
    /// Forget what this rate looked like, on the wire and in the delay.
    ///
    /// Both norms are only true of the rate they were taken at: the parity
    /// floor, the content's fill and the FEC share move with the rate, and so
    /// does the queue behind it. The delay's last value is kept anyway: the
    /// drain guard needs a mark to wait for.
    fn forget(&mut self) {
        if self.delivery_windows >= DELIVERY_REF_WINDOWS {
            self.frames_norm = self.delivery_frames / u64::from(self.delivery_windows);
        }
        self.delivery_sum_kbps = 0;
        self.delivery_windows = 0;
        self.delivery_frames = 0;
        if let Some(mean) = self.delay_mean_us() {
            self.delay_norm_us = mean;
        }
        self.delay_sum_us = 0;
        self.delay_windows = 0;
    }

    /// What clean windows at this rate have been delivering, or `None` until
    /// there are enough of them to mean anything.
    fn delivery_reference(&self) -> Option<u32> {
        (self.delivery_windows >= DELIVERY_REF_WINDOWS)
            .then(|| (self.delivery_sum_kbps / u64::from(self.delivery_windows)) as u32)
    }

    /// Mean shard delay of this rate's clean windows; `None` before the first.
    fn delay_mean_us(&self) -> Option<i64> {
        (self.delay_windows > 0).then(|| self.delay_sum_us / i64::from(self.delay_windows))
    }

    /// One clean window's delivered wire rate. Stillness teaches nothing: a
    /// repeat-marked, empty or still window is not this rate's norm, which is
    /// the same exclusion the climb makes.
    fn note(&mut self, w: &WindowSample) {
        if w.activity.quiet() || self.went_still(w) {
            return;
        }
        self.delivery_sum_kbps += u64::from(w.actual_kbps);
        self.delivery_windows += 1;
        if let WindowActivity::Active(n) = w.activity {
            self.delivery_frames += u64::from(n);
        }
        if let Some(d) = w.delay {
            self.delay_sum_us += d.mean_us;
            self.delay_windows += 1;
        }
    }

    /// Did the source produce under a quarter of the frames it was last seen
    /// producing? Lost frames count as produced: the link, not the source,
    /// took them. A source never seen busy is never still — the first window
    /// of video is partial, not quiet.
    fn went_still(&self, w: &WindowSample) -> bool {
        let WindowActivity::Active(n) = w.activity else {
            return false;
        };
        let norm = if self.delivery_windows >= DELIVERY_REF_WINDOWS {
            self.delivery_frames / u64::from(self.delivery_windows)
        } else {
            self.frames_norm
        };
        (u64::from(n) + w.dropped) * STILL_FRAMES_DIV < norm
    }
}

/// The asks the host has not answered, and what its short answers taught.
#[derive(Default)]
pub(super) struct AckTracker {
    /// Last [`request`](BitrateController::request). Taken (not kept) by the
    /// ack, so one request is judged at most once.
    pub(super) last_requested_kbps: Option<u32>,
    /// Two identical short acks latch the host cap. One can be a failed
    /// rebuild keeping the old rate.
    short_ack_kbps: u32,
    short_acks: u32,
    /// Reaching [`MAX_UNACKED`] holds the controller until an answer lands.
    unacked: u32,
    /// When the oldest ask still unanswered went out.
    first_unacked: Option<Instant>,
}

impl AckTracker {
    fn on_request(&mut self, kbps: u32, now: Instant) {
        if self.unacked == 0 {
            self.first_unacked = Some(now);
        }
        self.unacked += 1;
        self.last_requested_kbps = Some(kbps);
    }

    fn on_dropped(&mut self) {
        self.unacked = self.unacked.saturating_sub(1);
        self.last_requested_kbps = None;
    }

    /// The host answered the ask in flight with less. `true` = the second
    /// identical short answer in a row.
    fn on_short(&mut self, kbps: u32) -> bool {
        if self.short_ack_kbps == kbps {
            self.short_acks += 1;
        } else {
            self.short_ack_kbps = kbps;
            self.short_acks = 1;
        }
        self.short_acks >= 2
    }
}

#[cfg(test)]
mod tests {
    use super::super::cap::CAP_REPROBE_WINDOWS_MIN;
    use super::super::harness::*;
    use super::super::verdict::{BASELINE_MIN_WINDOWS, RECOVERY_KF_BAD};
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

    /// A link that carried a third of what it was asked for: the rate lands on
    /// what it delivered, not on a fraction of a rate it never carried. A link
    /// with room keeps the blind step, whatever damaged the window.
    #[test]
    fn a_link_short_of_offered_is_cut_to_what_it_delivered() {
        let start = Instant::now();
        let cut = |delivered: u32| -> Option<u32> {
            let mut c = BitrateController::new(20_000, None);
            c.on_window(&WindowSample {
                dropped: 4,
                actual_kbps: delivered,
                ..WindowSample::at(ticks(start, 0))
            })
        };
        // 0.85 x 12 000, inside the bounds.
        assert_eq!(cut(12_000), Some(10_200));
        // 0.85 x 4 000 is under half the rate: one cut goes no further.
        assert_eq!(cut(4_000), Some(10_000));
        // A window that delivered nothing measured no capacity at all.
        assert_eq!(cut(0), Some(10_000));
        // The link handed over what it was asked for: today's arithmetic.
        assert_eq!(cut(20_000), Some(14_000));
    }

    /// What a clean window delivers at this rate is the norm a short one is
    /// read against.
    ///
    /// A source filling 78 % of its allowance makes every clean window read
    /// 78 % of the rate; sizing a cut from that delivery charges the content's
    /// fill to the link. Against the norm the fill cancels, and a wire above
    /// its own target — the parity floor at the bottom of the range — has
    /// measured no fall at all.
    #[test]
    fn a_content_bound_window_is_cut_against_the_wires_own_norm() {
        let start = Instant::now();
        let cut = |windows: u32, delivered: u32| -> Option<u32> {
            let mut c = BitrateController::new(20_000, None);
            for i in 0..windows {
                assert_eq!(
                    c.on_window(&WindowSample {
                        owd_mean_us: Some(10_000),
                        actual_kbps: 15_600,
                        ..WindowSample::at(ticks(start, i))
                    }),
                    None,
                    "78 % of the rate is what this content delivers"
                );
            }
            c.on_window(&WindowSample {
                dropped: 4,
                actual_kbps: delivered,
                ..WindowSample::at(ticks(start, windows))
            })
        };
        // 13 400 of a 15 600 norm: the wire fell to 86 % of itself, and the
        // cut is that fall in target units — not 0.85 × 13 400.
        assert_eq!(cut(DELIVERY_REF_WINDOWS, 13_400), Some(14_602));
        // Too few clean windows to know this rate's norm: the blind step.
        assert_eq!(cut(DELIVERY_REF_WINDOWS - 1, 13_400), Some(11_390));
        // The same window on a wire carrying more than its target.
        assert_eq!(
            cut(DELIVERY_REF_WINDOWS, 21_000),
            Some(14_000),
            "a blind step, not a link cut"
        );
    }

    /// A norm belongs to the rate it was taken at, and the host can move the
    /// rate without being asked: a re-resolve, a clamp, a mode switch.
    #[test]
    fn a_rate_the_host_moved_drops_the_wires_norm() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        for i in 0..DELIVERY_REF_WINDOWS {
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 15_600,
                ..WindowSample::at(ticks(start, i))
            });
        }
        assert_eq!(c.norms.delivery_reference(), Some(15_600));
        c.on_ack(40_000, None);
        assert_eq!(
            c.norms.delivery_reference(),
            None,
            "a different rate, a different wire"
        );
    }

    /// A decoder past its budget is not the link: the rate may be the lever,
    /// but what the link delivered is not the number to land on.
    #[test]
    fn a_decode_verdict_never_lands_on_the_delivered_rate() {
        let mut c = BitrateController::new(20_000, None);
        c.set_frame_budget(60);
        let start = Instant::now();
        for i in 0..BASELINE_MIN_WINDOWS as u32 {
            assert_eq!(loaded(&mut c, ticks(start, i), 2_000), None);
        }
        // Deep decode excursion, and delivery a third of the target.
        let at = ticks(start, BASELINE_MIN_WINDOWS as u32);
        assert_eq!(
            c.on_window(&WindowSample {
                decode_mean_us: Some(40_000),
                actual_kbps: 7_000,
                activity: WindowActivity::Unmarked,
                ..WindowSample::at(at)
            }),
            Some(14_000),
            "the decoder's verdict keeps the blind step"
        );
    }

    fn trend(mean_us: i64, rise_us: i64) -> crate::abr::DelayTrend {
        crate::abr::DelayTrend {
            samples: 20,
            mean_us,
            rise_us,
            last_us: mean_us,
        }
    }

    /// A session parked at a learned wall with `ref_us` of delay behind it, at
    /// the window the re-probe clock lifts the cap. `take_up` is the host
    /// applying the lift; without it the rate stays at the cap. The returned
    /// tick is past the cooldown the climb's own ask holds.
    fn lifted_cap(start: Instant, ref_us: i64, take_up: bool) -> (BitrateController, u32, u32) {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(300_000);
        // A wall at 12 000 twice: the cap latches a tenth under it.
        c.note_link_mark(12_000);
        c.note_link_mark(11_500);
        let cap = c.link_cap.kbps().expect("two marks are a wall");
        c.on_ack(cap, None);
        let mut t = 0;
        for _ in 0..CAP_REPROBE_WINDOWS_MIN {
            let at = ticks(start, t);
            t += 1;
            c.on_window(&WindowSample {
                owd_mean_us: Some(ref_us),
                delay: Some(trend(ref_us, 0)),
                actual_kbps: cap,
                ..WindowSample::at(at)
            });
        }
        let lift = c.link_cap.kbps().expect("the clock must lift the cap");
        assert!(lift > cap, "the re-probe never lifted");
        if take_up {
            c.on_ack(lift, None);
        }
        (c, t + 2, if take_up { lift } else { cap })
    }

    /// A lift is a probe, and a queue filling under it is the wall answering.
    ///
    /// The retreat goes to the cap the lift came from — the session was fine
    /// there a window ago — and the cap's clock doubles. One window over the
    /// bar is a handover stall or a send loop losing a slice, and a lift the
    /// link is carrying outlives it.
    #[test]
    fn a_lift_the_queue_answers_retreats_to_the_cap() {
        let start = Instant::now();
        let judged = |ref_us: i64, mean_us: i64, rise_us: i64, n: u32| -> (Option<u32>, u32) {
            let (mut c, mut t, rate) = lifted_cap(start, ref_us, true);
            let mut out = None;
            for _ in 0..n {
                let at = ticks(start, t);
                t += 1;
                out = out.or(c.on_window(&WindowSample {
                    owd_mean_us: Some(mean_us),
                    delay: Some(trend(mean_us, rise_us)),
                    actual_kbps: rate,
                    ..WindowSample::at(at)
                }));
            }
            (out, c.link_cap.reprobe_after())
        };
        let bar = BitrateController::lift_bar_us(10_000);
        let over = 10_000 + bar + 1_000;
        assert_eq!(
            judged(10_000, over, 0, 1).0,
            None,
            "one window over the bar is a stall"
        );
        let (retreat, reprobe_after) = judged(10_000, over, 0, LIFT_OVER_WINDOWS);
        assert_eq!(retreat, Some(10_350), "back to the cap, not a fraction");
        assert_eq!(
            reprobe_after,
            CAP_REPROBE_WINDOWS_MIN * 2,
            "and the next ask waits twice as long"
        );
        assert_eq!(
            judged(
                10_000,
                10_000 + bar / 2 + 1_000,
                DRAIN_FALL_US,
                LIFT_RISING_WINDOWS
            )
            .0,
            Some(10_350),
            "a queue filling under the bar is the same answer"
        );
        // A tunnel whose own delay is 100 ms: the bar is a quarter of that,
        // not the 15 ms a LAN is judged by.
        assert_eq!(
            judged(100_000, 120_000, 0, LIFT_OVER_WINDOWS).0,
            None,
            "20 ms on a 100 ms link is inside the session's own noise"
        );
    }

    /// A lift refused on a path the host has divided costs the step, not the
    /// clock.
    ///
    /// The queue that refused it may be the sibling's, and one window cannot
    /// tell. Doubling the wait there records a wall this session never
    /// measured, and the ladder stops closing on the share it was given.
    #[test]
    fn a_lift_refused_on_a_shared_path_keeps_the_short_clock() {
        let start = Instant::now();
        let (mut c, mut t, rate) = lifted_cap(start, 10_000, true);
        c.share_cap = Some(rate * 2);
        let over = 10_000 + BitrateController::lift_bar_us(10_000) + 1_000;
        let mut out = None;
        for _ in 0..LIFT_OVER_WINDOWS {
            let at = ticks(start, t);
            t += 1;
            out = out.or(c.on_window(&WindowSample {
                owd_mean_us: Some(over),
                delay: Some(trend(over, 0)),
                actual_kbps: rate,
                ..WindowSample::at(at)
            }));
        }
        assert_eq!(out, Some(10_350), "back to the cap it came from");
        assert_eq!(
            c.link_cap.reprobe_after(),
            CAP_REPROBE_WINDOWS_MIN,
            "the host, not this window, says when the path has room again"
        );
    }

    /// The probe's budget settles the lift, and a lift nothing took up gives
    /// the delay baseline back.
    ///
    /// After the budget the cap keeps the new value and the ordinary verdict
    /// owns the window again — but the frozen reference stays, because a wall
    /// that answers late is still the lift's doing and not a cut's.
    #[test]
    fn a_settled_lift_keeps_its_reference_and_nothing_else() {
        let start = Instant::now();
        let (mut c, mut t, rate) = lifted_cap(start, 10_000, true);
        let bar = BitrateController::lift_bar_us(10_000);
        let window = |c: &mut BitrateController, t: &mut u32, mean_us: i64, rise_us: i64| {
            let at = ticks(start, *t);
            *t += 1;
            c.on_window(&WindowSample {
                owd_mean_us: Some(mean_us),
                delay: Some(trend(mean_us, rise_us)),
                actual_kbps: rate,
                ..WindowSample::at(at)
            })
        };
        for _ in 0..LIFT_PROBE_WINDOWS {
            assert_eq!(
                window(&mut c, &mut t, 10_000, 0),
                None,
                "the link carries it"
            );
        }
        for _ in 0..LIFT_RISING_WINDOWS {
            assert_eq!(
                window(&mut c, &mut t, 10_000 + bar / 2 + 1_000, DRAIN_FALL_US),
                None,
                "a settled lift leaves a rise under the bar to the verdict"
            );
        }
        let mut out = None;
        for _ in 0..LIFT_OVER_WINDOWS {
            out = out.or(window(&mut c, &mut t, 10_000 + bar + 1_000, 0));
        }
        assert_eq!(
            out,
            Some(10_350),
            "a standing rise at a lifted rate retreats"
        );
    }

    /// Until the lifted rate runs nothing judges the lift, so the delay is the
    /// ordinary verdict's: a queue filling at the old rate still cuts.
    #[test]
    fn a_queue_before_the_lift_runs_still_cuts() {
        let start = Instant::now();
        let (mut c, mut t, rate) = lifted_cap(start, 10_000, false);
        // 30 ms over a 10 ms floor: past the 25 ms rise the verdict reads.
        let queue = 40_000;
        let mut out = None;
        for _ in 0..BAD_WINDOWS_TO_DECREASE {
            let at = ticks(start, t);
            t += 1;
            assert!(c.lift.is_some(), "the lift is still waiting for its rate");
            out = out.or(c.on_window(&WindowSample {
                owd_mean_us: Some(queue),
                delay: Some(trend(queue, DRAIN_FALL_US)),
                actual_kbps: rate,
                ..WindowSample::at(at)
            }));
        }
        assert!(
            out.is_some_and(|k| k < rate),
            "the queue went unanswered: {out:?}"
        );
    }

    /// A lift the climb never takes up is dropped: a probe left open holds the
    /// delay baseline still, and a baseline that never learns judges nothing.
    #[test]
    fn a_lift_nothing_takes_up_is_dropped() {
        let start = Instant::now();
        let (mut c, mut t, rate) = lifted_cap(start, 10_000, false);
        let window = |c: &mut BitrateController, t: &mut u32| {
            let at = ticks(start, *t);
            *t += 1;
            // Half the rate: the climb has no reason to ask for the lift.
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                delay: Some(trend(10_000, 0)),
                actual_kbps: rate / 2,
                ..WindowSample::at(at)
            })
        };
        for _ in 0..LIFT_PROBE_MAX_AGE / 2 {
            window(&mut c, &mut t);
        }
        assert!(c.lift.is_some(), "the probe waits for the rate to arrive");
        for _ in 0..LIFT_PROBE_MAX_AGE {
            window(&mut c, &mut t);
        }
        assert!(
            c.lift.is_none(),
            "and gives up rather than freeze the delay"
        );
    }

    /// The queue a link cut caused is what makes the next windows read badly.
    ///
    /// Standing delay, lost frames and loss inside the guard are that queue's
    /// tail, whether the fit says it is falling yet or not: a queue that has
    /// not peaked still reads as rising, and cutting there is the cascade the
    /// rig recorded. Only the delay coming back to where it was, or the
    /// budget, ends it.
    #[test]
    fn damage_while_the_last_cut_drains_is_not_a_second_verdict() {
        let start = Instant::now();
        let after = |mean_us: i64, rise_us: i64, dropped: u64| -> Option<u32> {
            let mut c = BitrateController::new(20_000, None);
            // Four clean windows: 10 ms is where this rate's delay sits.
            for i in 0..BASELINE_MIN_WINDOWS as u32 {
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    delay: Some(trend(10_000, 0)),
                    actual_kbps: 20_000,
                    ..WindowSample::at(ticks(start, i))
                });
            }
            let mut t = BASELINE_MIN_WINDOWS as u32;
            // A link-attributed cut: delivery a third of the rate.
            let cut = c.on_window(&WindowSample {
                dropped: 4,
                owd_mean_us: Some(400_000),
                delay: Some(trend(400_000, 100_000)),
                actual_kbps: 7_000,
                ..WindowSample::at(ticks(start, t))
            });
            assert_eq!(cut, Some(10_000), "the cut lands on what was delivered");
            c.on_ack(10_000, None);
            // The overflow's tail: standing delay and the frames it swallowed.
            let mut out = None;
            for _ in 0..2 {
                t += 1;
                out = out.or(c.on_window(&WindowSample {
                    dropped,
                    owd_mean_us: Some(300_000),
                    actual_kbps: 9_500,
                    delay: Some(trend(mean_us, rise_us)),
                    ..WindowSample::at(ticks(start, t))
                }));
            }
            out
        };
        assert_eq!(
            after(300_000, -60_000, 2),
            None,
            "frames lost out of a draining queue are that queue's tail"
        );
        assert_eq!(
            after(300_000, 0, 0),
            None,
            "and a queue that has not peaked yet is still the cut's own"
        );
        assert_eq!(
            after(300_000, 0, 2),
            Some(7_000),
            "a queue that is not emptying and still losing frames is the rate"
        );
        assert_eq!(
            after(9_000, 0, 2),
            Some(7_000),
            "a delay back where it was ends the guard, and the window is judged"
        );
    }

    /// A blip costs the doubling for as long as it holds the exemption.
    ///
    /// Slow start takes its next step off one clean window, so a lost frame it
    /// waves through would be answered by half again the rate that lost it.
    /// The rate stands still until the additive climb earns its step.
    #[test]
    fn a_vouched_blip_holds_the_rate_and_the_doubling() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        let held = |at: u32, dropped: u64| WindowSample {
            owd_mean_us: Some(10_000),
            delay: Some(trend(10_000, 0)),
            dropped,
            actual_kbps: 20_000,
            ..WindowSample::at(ticks(start, at))
        };
        for i in 0..4 {
            assert_eq!(c.on_window(&held(i, 0)), None);
        }
        assert!(c.probing, "slow start is still armed");
        c.set_ceiling(300_000);
        assert_eq!(c.on_window(&held(4, 1)), None);
        assert_eq!(c.last_reason(), Reason::Blip);
        assert!(c.probing, "the blip does not spend slow start");
        for i in 5..10 {
            assert_eq!(c.on_window(&held(i, 0)), None, "window {i} moved the rate");
        }
        assert_eq!(
            c.on_window(&held(10, 0)),
            Some(21_251),
            "and the first step after it is additive, not a doubling"
        );
    }

    /// A blip draining the last backoff teaches no knee, the way that
    /// backoff's own successor does not.
    ///
    /// The reference the choke left stands until a rate the session climbed to
    /// meets it again; a climbed-to rate the decoder sailed through ends it.
    #[test]
    fn a_blip_without_a_climb_behind_it_keeps_the_knee_reference() {
        let start = Instant::now();
        let after_backoff = |climbed: bool| -> u32 {
            let mut c = BitrateController::new(20_000, None);
            c.set_frame_budget(120);
            for i in 0..BASELINE_MIN_WINDOWS as u32 {
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    delay: Some(trend(10_000, 0)),
                    actual_kbps: 20_000,
                    ..WindowSample::at(ticks(start, i))
                });
            }
            let mut t = BASELINE_MIN_WINDOWS as u32;
            let cut = c
                .on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(60_000),
                    delay: Some(trend(10_000, 0)),
                    actual_kbps: 20_000,
                    ..WindowSample::at(ticks(start, t))
                })
                .expect("a decode excursion backs off");
            assert_eq!(c.decode.backoff_kbps, 20_000, "and leaves its reference");
            c.on_ack(cut, None);
            if climbed {
                c.on_ack(cut + 1_000, None);
            }
            t += 1;
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    delay: Some(trend(10_000, 0)),
                    dropped: 1,
                    actual_kbps: c.current_kbps,
                    ..WindowSample::at(ticks(start, t))
                }),
                None
            );
            assert_eq!(c.last_reason(), Reason::Blip);
            c.decode.backoff_kbps
        };
        assert_eq!(after_backoff(false), 20_000, "no climb, no knee sample");
        assert_eq!(after_backoff(true), 0, "a climbed-to rate ends it");
    }

    /// A cut one window after a climb step still knows where the delay sat.
    ///
    /// The step forgets this rate's norms, and one window is too few to build
    /// another, so the guard would have no mark to wait for and would spend
    /// its whole budget. The norm the step left behind is that mark.
    #[test]
    fn a_guard_armed_right_after_a_climb_keeps_a_reference() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        // Four windows at the ceiling: 10 ms is where this rate's delay sits.
        for i in 0..BASELINE_MIN_WINDOWS as u32 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    delay: Some(trend(10_000, 0)),
                    actual_kbps: 20_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        let mut t = BASELINE_MIN_WINDOWS as u32;
        c.set_ceiling(30_000);
        let up = c
            .on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                delay: Some(trend(10_000, 0)),
                actual_kbps: 20_000,
                ..WindowSample::at(ticks(start, t))
            })
            .expect("the room is there, so the session climbs");
        c.on_ack(up, None);
        assert_eq!(c.norms.delay_windows, 0, "the step forgot the rate's norms");
        // The link answers the step: the queue fills, then it swallows frames.
        for dropped in [0, 4] {
            t += 1;
            let _ = c.on_window(&WindowSample {
                dropped,
                owd_mean_us: Some(300_000),
                delay: Some(trend(300_000, 100_000)),
                actual_kbps: 10_000,
                ..WindowSample::at(ticks(start, t))
            });
        }
        assert!(c.drain.windows > 0, "a link cut arms the guard");
        assert_eq!(c.drain.ref_us, 10_000, "the norm the step left behind");
        t += 1;
        assert!(
            !c.drain.note(&WindowSample {
                delay: Some(trend(10_000, -20_000)),
                ..WindowSample::at(ticks(start, t))
            }),
            "and the guard ends when the delay is back at it"
        );
    }

    /// The guard runs until the queue is gone or the budget is, and a window
    /// with nothing to read from is not evidence that it drained.
    #[test]
    fn the_drain_guard_ends_with_the_queue_or_with_its_budget() {
        let mut c = BitrateController::new(20_000, None);
        let now = Instant::now();
        let at = |mean_us: i64, rise_us: i64| WindowSample {
            delay: Some(trend(mean_us, rise_us)),
            ..WindowSample::at(now)
        };
        c.drain.windows = LINK_DRAIN_WINDOWS;
        c.drain.ref_us = 10_000;
        for _ in 0..LINK_DRAIN_WINDOWS {
            assert!(c.drain.note(&at(300_000, 40_000)), "still filling");
        }
        assert!(!c.drain.note(&at(300_000, 40_000)), "budget spent");

        c.drain.windows = LINK_DRAIN_WINDOWS;
        assert!(
            !c.drain.note(&WindowSample {
                dropped: 3,
                ..at(300_000, 0)
            }),
            "a full queue still losing frames is the rate, not the tail"
        );

        c.drain.windows = LINK_DRAIN_WINDOWS;
        assert!(!c.drain.note(&at(9_000, 0)), "back where it was");
        assert_eq!(c.drain.windows, 0, "and the guard is over");

        c.drain.windows = LINK_DRAIN_WINDOWS;
        c.drain.ref_us = 0;
        assert!(c.drain.note(&at(1_000, 0)), "no reference, only the budget");
        assert!(
            c.drain.note(&WindowSample::at(now)),
            "no delay reading is no evidence of draining"
        );
    }

    /// Three windows cannot cut from one number.
    ///
    /// Until the host's ack lands, `current_kbps` is a rate the session has
    /// already asked to leave: on the rig's cell three windows cut from the
    /// same 4 043 in 4.5 s. Each cut measures from the ask in flight.
    #[test]
    fn a_cut_while_a_request_is_unacked_measures_from_the_ask() {
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let bad = |c: &mut BitrateController, t: u32| {
            c.on_window(&WindowSample {
                loss_ppm: 25_000,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, t))
            })
        };
        assert_eq!(bad(&mut c, 0), None);
        assert_eq!(bad(&mut c, 1), Some(14_000));
        // Nothing acked: the rate the host is running is still 20 000.
        assert_eq!(bad(&mut c, 6), None);
        assert_eq!(
            bad(&mut c, 7),
            Some(9_800),
            "x0.7 of the ask, not of 20 000"
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

    /// A disabled controller learns nothing, and the FIRST measurement binds
    /// whichever way it points: a 10 Mbps wall under a blind 20 Mbps start is
    /// the tunnel case, and discarding it is what left #1131 climbing into it.
    /// Later measurements only raise.
    #[test]
    fn set_ceiling_is_ignored_when_disabled_and_the_first_one_binds() {
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
        c.set_ceiling(10_000);
        assert_eq!(c.ceiling_kbps, 10_000, "the wall is what was measured");
        c.set_ceiling(6_000);
        assert_eq!(c.ceiling_kbps, 10_000, "a second, worse reading is not");
        c.set_ceiling(30_000);
        assert_eq!(c.ceiling_kbps, 30_000, "a better one still raises");
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

    /// The host's share bounds the climb, and a share under the live rate is
    /// the live rate: the host applied it before it said so.
    #[test]
    fn a_share_is_a_ceiling_the_climb_stops_at() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(200_000);
        let start = Instant::now();
        let mut t = 0;
        climb_to(&mut c, start, &mut t, 40_000);
        c.on_ack(24_000, Some(AckReason::Governor));
        assert_eq!(c.current_kbps, 24_000, "the host already retargeted to it");
        assert_eq!(c.share_cap, Some(24_000));
        assert_eq!(c.host_cap.kbps(), None, "a share is not a short ack");
        // Clean windows for a minute: the climb stops at the share.
        for _ in 0..80 {
            if let Some(k) = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, t))
            }) {
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert!(
            c.current_kbps <= 24_000,
            "climbed past the share to {}",
            c.current_kbps
        );
        // The group ends: the ceiling goes with it and the session climbs.
        c.on_ack(
            super::super::governor::NO_SHARE_KBPS,
            Some(AckReason::Governor),
        );
        assert_eq!(c.share_cap, None);
        climb_to(&mut c, start, &mut t, 40_000);
    }

    /// A share above this session's ceiling is an allowance, not evidence
    /// about its own air: the wall it measured stands, slow start does not
    /// re-arm, and the cap is asked again at the next window that can judge
    /// the answer instead of on its clock.
    #[test]
    fn a_share_above_the_ceiling_keeps_the_wall_and_probes_it_again() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        // Two deliveries at one rate are a wall; the verdict that marked them
        // is what ends slow start in a live session.
        c.note_link_mark(12_000);
        c.note_link_mark(11_500);
        c.probing = false;
        let cap = c.link_cap.kbps().expect("two marks are a wall");
        c.on_ack(24_000, Some(AckReason::Governor));
        assert_eq!(c.link_cap.kbps(), Some(cap), "a sibling's queue is not it");
        assert!(!c.probing, "and a share is not licence to double");
        assert_eq!(c.ceiling_kbps, 24_000, "only the allowance moved");
        c.on_ack(cap, None);
        let mut t = 0;
        let window = |c: &mut BitrateController, t: &mut u32, mean_us: i64| {
            let at = ticks(start, *t);
            *t += 1;
            c.on_window(&WindowSample {
                owd_mean_us: Some(mean_us),
                delay: Some(trend(mean_us, 0)),
                actual_kbps: c.current_kbps,
                ..WindowSample::at(at)
            })
        };
        // A window the shard path said nothing in freezes nothing, and a lift
        // with no reference is a commitment. The step waits for one that does.
        run_clean(&mut c, start, t, 1);
        t += 1;
        assert_eq!(c.link_cap.kbps(), Some(cap), "no reference, no step");
        window(&mut c, &mut t, 10_000);
        assert_eq!(c.link_cap.kbps(), Some(cap + cap / 8), "the share's step");
        assert!(c.lift.is_some(), "and it is judged, not granted");
        // The host takes the lift up and the queue answers it: back to the cap
        // exactly, no ×0.7, and still on the short clock — this session cannot
        // tell that queue from the sibling it is sharing with.
        c.on_ack(cap + cap / 8, None);
        let over = 10_000 + BitrateController::lift_bar_us(10_000) + 1_000;
        let mut out = None;
        for _ in 0..LIFT_OVER_WINDOWS {
            out = out.or(window(&mut c, &mut t, over));
        }
        assert_eq!(out, Some(cap), "the rate it was fine at a window ago");
        assert_eq!(c.link_cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN);
    }

    /// A share that lands while a request is outstanding is not an answer to
    /// it. The same pair of numbers from a host that names nothing is, which
    /// is what an old client sees and why it is only slower to lift.
    #[test]
    fn a_share_is_never_read_as_the_answer_to_a_request() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(200_000);
        let ask = run_clean(&mut c, start, 0, 8).expect("a clean run climbs");
        assert!(ask > 14_000);
        c.on_ack(14_000, Some(AckReason::Governor));
        assert_eq!(c.host_cap.kbps(), None, "a share is not a short ack");
        assert_eq!(c.share_cap, Some(14_000));
        assert_eq!(c.current_kbps, 14_000);

        let mut old = BitrateController::new(20_000, None);
        old.set_ceiling(200_000);
        let mut t = 0;
        for _ in 0..2 {
            let ask = run_clean(&mut old, start, t, 8).expect("a clean run climbs");
            assert!(ask > 14_000);
            t += 8;
            old.on_ack(14_000, None);
        }
        assert_eq!(
            old.host_cap.kbps(),
            Some(14_000),
            "a nameless ack still binds — safe, and only slower to lift"
        );
    }

    /// A share is a rate the session never asked for: the wire's norm at the
    /// old rate goes with it, the ask it did not answer does not become the
    /// base of the next cut, and the guard the last cut armed keeps measuring
    /// against the number it was given.
    #[test]
    fn a_share_that_moves_the_rate_drops_what_only_the_old_rate_knew() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        for i in 0..DELIVERY_REF_WINDOWS {
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                delay: Some(trend(10_000, 0)),
                actual_kbps: 15_600,
                ..WindowSample::at(ticks(start, i))
            });
        }
        assert_eq!(c.norms.delivery_reference(), Some(15_600));
        c.arm_drain();
        // An ask in flight, and a share arriving where its answer would.
        c.acks.last_requested_kbps = Some(18_000);
        c.on_ack(14_000, Some(AckReason::Governor));
        assert_eq!(
            c.norms.delivery_reference(),
            None,
            "a different rate, a different wire"
        );
        assert_eq!(c.cut_base_kbps(), 14_000, "no ask survives a share");
        assert_eq!(
            (c.drain.windows, c.drain.ref_us),
            (LINK_DRAIN_WINDOWS, 10_000),
            "the guard keeps the delay it was armed on"
        );
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
        assert!(!c.enabled, "45 s of silence is a host that never answers");
    }

    /// Three cuts inside a deep queue's ack path wait for the answer; the
    /// answer arriving late does not retire the controller.
    #[test]
    fn a_late_ack_keeps_the_controller() {
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let bad = |c: &mut BitrateController, i: u32| {
            c.on_window(&WindowSample {
                dropped: 1,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            })
        };
        let mut asked = Vec::new();
        for i in 0..8 {
            asked.extend(bad(&mut c, i));
        }
        assert_eq!(asked.len() as u32, MAX_UNACKED, "{asked:?}");
        // The first answer lands 6 s after the first ask.
        c.on_ack(asked[0], None);
        assert!(c.enabled, "a late answer is an answer");
        assert!(
            (8..20).any(|i| bad(&mut c, i).is_some()),
            "and the controller asks again"
        );
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
        assert_eq!(c.decode.cap.kbps(), Some(100_000));
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
        assert_eq!(c.decode.cap.kbps(), Some(112_500));
        assert!(c.decode.probe.is_none(), "verdict must have been reached");
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
        assert_eq!(c.decode.cap.kbps(), Some(100_000));
        assert_eq!(c.decode.cap.reprobe_after(), CAP_REPROBE_WINDOWS_MIN * 2);
    }

    #[test]
    fn a_decoder_without_headroom_retreats_and_keeps_it_when_latency_follows() {
        let (mut c, start, mut t) = seeded_120(100_000);
        // 7 800 µs: 93 % — retreat one notch, not the ×0.7 congestion backoff.
        let r = loaded(&mut c, ticks(start, t), 7_800).expect("retreat");
        t += 1;
        assert_eq!(r, 87_500);
        assert_eq!(c.decode.cap.kbps(), Some(87_500));
        c.on_ack(r, None);
        // Latency followed (−1 300 µs): the retreat stands, nothing else moves.
        assert_eq!(until_request(&mut c, start, &mut t, 6_500, 8), None);
        assert_eq!(c.current_kbps, 87_500);
        assert!(!c.decode.headroom.disarmed());
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
        assert!(c.decode.headroom.disarmed());
        // No cap either: the bands parked one on a level the rate does not move.
        assert_eq!(c.decode.cap.kbps(), None);
        // Stood down: the same 93 % no longer retreats, and the climb is free.
        for _ in 0..10 {
            if let Some(k) = loaded(&mut c, ticks(start, t), 7_800) {
                assert!(k > c.current_kbps, "only climbs, never a retreat");
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert!(c.current_kbps >= 100_000);
    }

    /// The field trace: a pipelined decoder whose latency swings at every
    /// rate. Each swing is a severe rise over the rolling minimum, so the
    /// retreat has to be answered through the damage, and once the driver
    /// has stood down the swing cuts nothing.
    #[test]
    fn a_swinging_decoder_answers_the_retreat_and_stops_cutting_once_stood_down() {
        let (mut c, start, mut t) = seeded_120(100_000);
        // 7 800 µs: 93 % — one notch, the step pending.
        let r = loaded(&mut c, ticks(start, t), 7_800).expect("retreat");
        t += 1;
        assert_eq!(r, 87_500);
        c.on_ack(r, None);
        // 21 000 µs is severe over the 7 800 floor, inside the cooldown and
        // past it. Neither window cuts: the step owns the question.
        assert_eq!(loaded(&mut c, ticks(start, t), 21_000), None);
        t += 1;
        assert_eq!(
            loaded(&mut c, ticks(start, t), 21_000),
            None,
            "a cut over the step"
        );
        t += 1;
        assert!(c.decode.probe.is_some(), "the step survives the damage");
        // Its second window at the new rate answers: 14 400 mean, over the
        // reference — not the rate's. The rate comes back, the driver stands down.
        assert_eq!(loaded(&mut c, ticks(start, t), 7_800), Some(100_000));
        t += 1;
        c.on_ack(100_000, None);
        assert!(c.decode.headroom.disarmed());
        assert_eq!(
            c.decode.cap.kbps(),
            None,
            "no cap on a level the rate does not move"
        );
        assert!(
            !c.rate_verdict,
            "the cuts on this latency were not the rate's"
        );
        // Stood down: the swing is no verdict.
        for _ in 0..8 {
            if let Some(k) = loaded(&mut c, ticks(start, t), 21_000) {
                assert!(k > c.current_kbps, "only climbs, never a cut");
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert!(c.current_kbps >= 100_000);
    }

    /// The field trace at its worst: no window is ever clean, so the bands
    /// never get to retreat and the verdict cuts straight from the first
    /// swing. The cut is judged by the verdict it came from: still bad at the
    /// lower rate is not the rate's, and the rate comes back.
    #[test]
    fn a_decode_cut_the_verdict_outlives_is_given_back() {
        let (mut c, start, mut t) = seeded_120(100_000);
        // 21 000 µs over an 8 000 floor: severe, ×0.7 at once.
        let cut = loaded(&mut c, ticks(start, t), 21_000).expect("severe decode cuts");
        t += 1;
        assert_eq!(cut, 70_000);
        c.on_ack(cut, None);
        // Still 21 000 at the lower rate, inside the cooldown and past it:
        // the cut owns the question, so neither window cuts again.
        assert_eq!(loaded(&mut c, ticks(start, t), 21_000), None);
        t += 1;
        assert_eq!(
            loaded(&mut c, ticks(start, t), 21_000),
            None,
            "a second cut"
        );
        t += 1;
        // Two windows over the verdict at the new rate: not the rate's.
        assert_eq!(loaded(&mut c, ticks(start, t), 21_000), Some(100_000));
        t += 1;
        c.on_ack(100_000, None);
        assert!(c.decode.headroom.disarmed());
        assert_eq!(c.decode.cap.kbps(), None);
        for _ in 0..8 {
            if let Some(k) = loaded(&mut c, ticks(start, t), 21_000) {
                assert!(k > c.current_kbps, "only climbs, never a cut");
                c.on_ack(k, None);
            }
            t += 1;
        }
        assert!(c.current_kbps >= 100_000);
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
        assert_eq!(c.decode.cap.kbps(), None);
    }
}
