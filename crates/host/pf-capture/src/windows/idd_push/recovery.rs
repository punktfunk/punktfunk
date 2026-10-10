//! The capturer's recovery supervisor (immunity plan WP13 wiring): feeds the pure
//! [`health::Classifier`] from the clocks the capturer keeps plus the session encoder's own
//! ([`EncoderTelemetry`]), hands its verdicts to the pure [`recovery::Coordinator`], and tells
//! the capturer which actuator to run. Same 15 s floor and "no evidence = idle" as the WP3b
//! watchdog it retired; the ladder replaces the one-rebuild-then-terminal rule.
//!
//! It runs ON the capture thread. The presentation restart is capture-thread-owned; the encoder
//! reset belongs to the stream loop, which polls for it and reports back. An `Encoder`
//! episode is proven by access units, every other class by new source frames. The host's
//! pipeline rebuild (its own 5-attempt budget) is the rung above `Failed`.

use std::time::{Duration, Instant};

use crate::{CaptureEpisode, CaptureHealth};
use pf_frame::health::{
    Activity, ActivityKind, Classifier, EncoderTelemetry, HealthClass, Snapshot, StallClass,
    Thresholds,
};
use pf_frame::recovery::{Action, Budget, Coordinator, Event, Stage, StageOutcome, Summary};

fn stall_name(c: StallClass) -> &'static str {
    match c {
        StallClass::Worker => "worker",
        StallClass::Encoder => "encoder",
        StallClass::Presentation => "presentation",
        StallClass::Driver => "driver",
    }
}

/// `AuHeader::encoder_state` as its lowercase name; an unknown word reads as wedged, never as
/// encoding.
fn encoder_state(w: u32) -> &'static str {
    use pf_driver_proto::encode::au;
    match w {
        au::ENCODER_CLOSED => "closed",
        au::ENCODER_OPEN => "open",
        au::ENCODER_ENCODING => "encoding",
        _ => "wedged",
    }
}

fn stage_name(s: Stage) -> &'static str {
    match s {
        Stage::EncoderReset => "encoder_reset",
        Stage::SwapChainReset => "swap_chain_reset",
        Stage::PresentationReset => "presentation_reset",
        Stage::DriverCycle => "driver_cycle",
    }
}

/// Cursor travel over a frozen image that counts as INPUT evidence — a couple of real mouse
/// movements, comfortably above sub-pixel jitter (the WP3b value).
const INPUT_EVIDENCE_PX: u32 = 64;
/// No canary before this much missed source, and at most one per interval.
const CANARY_AFTER: Duration = Duration::from_secs(5);
/// A canary that no source frame answered within this much is strong evidence.
const CANARY_ANSWER: Duration = Duration::from_secs(1);
/// Classification cadence outside an episode (the deadlines are seconds).
const TICK: Duration = Duration::from_millis(250);

/// What the capturer sampled this tick.
pub(super) struct Inputs {
    pub now: Instant,
    /// The last `FrameOrigin::Source` frame.
    pub last_source: Instant,
    pub source_seq: u64,
    /// Age of the driver worker's drain heartbeat; `None` before the encoder is open.
    pub heartbeat_age: Option<Duration>,
    /// Cursor travel since the last source frame.
    pub cursor_gap_px: u32,
    /// A presentation restart is in flight (`recovering_since`).
    pub recreating: bool,
    pub secure_desktop: bool,
    pub topology_held: bool,
    /// The session encoder's clocks; `None` for a submit-driven backend.
    pub encoder: Option<EncoderTelemetry>,
}

/// What the capturer does now.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Nothing,
    /// Present the composition canary (`pf_win_display::compose_probe`).
    Canary,
    Run(Stage),
    /// The episode closed on proof; `outage` spans the last good frame to the proving one.
    Recovered {
        summary: Summary,
        outage: Duration,
    },
    /// The ladder is exhausted; `gap` is the source gap to report in the typed fault.
    Failed {
        gap: Duration,
        summary: Summary,
    },
}

pub(super) struct Supervisor {
    classifier: Classifier,
    coordinator: Coordinator,
    last_tick: Instant,
    /// The last verdict's source gap, for the typed fault when a stage report ends the ladder.
    last_gap: Duration,
    /// The source gap when the open episode began, and when: together they measure the outage.
    opened: Option<(Duration, Instant)>,
    /// When cursor travel first crossed the evidence bar in this gap.
    input_at: Option<Instant>,
    /// When the last canary was asked for; it rate-limits the next.
    canary_at: Option<Instant>,
    /// That canary reached the screen, so an unanswered one is evidence.
    canary_shown: bool,
    /// The encoder's `published_total` at the last tick: its delta is AU progress.
    published_last: u64,
}

impl Supervisor {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            classifier: Classifier::new(Thresholds::default(), now),
            coordinator: Coordinator::new(Budget::default()),
            last_tick: now,
            last_gap: Duration::ZERO,
            opened: None,
            input_at: None,
            canary_at: None,
            canary_shown: false,
            published_last: 0,
        }
    }

    /// An episode is open: the capturer's own recover-or-drop timers stand down.
    pub(super) fn owns_episode(&self) -> bool {
        self.coordinator.owns_episode()
    }

    /// The operator-surface report (WP18): the last verdict, the driver encoder's own counters,
    /// and the last closed episode, with every enum spelled as its lowercase name.
    pub(super) fn report(&self, now: Instant, enc: Option<&EncoderTelemetry>) -> CaptureHealth {
        let verdict = self.classifier.last();
        let (class, stall_class) = match verdict.map(|v| v.class) {
            None => ("healthy", None),
            Some(HealthClass::Healthy) => ("healthy", None),
            Some(HealthClass::Idle) => ("idle", None),
            Some(HealthClass::Suspect) => ("suspect", None),
            Some(HealthClass::Stalled(c)) => ("stalled", Some(stall_name(c))),
            Some(HealthClass::Recovering) => ("recovering", None),
            Some(HealthClass::Rebuilding) => ("rebuilding", None),
            Some(HealthClass::SecureDesktop) => ("secure_desktop", None),
        };
        CaptureHealth {
            class,
            stall_class,
            source_gap: verdict.map_or(Duration::ZERO, |v| v.source_gap),
            evidence: verdict.and_then(|v| v.evidence).map(|k| match k {
                ActivityKind::Input => "input",
                ActivityKind::Canary => "canary",
            }),
            present_to_arrival: enc.and_then(|e| e.present_to_arrival),
            late_frames: verdict.is_some_and(|v| v.late),
            encoder_state: enc.map(|e| encoder_state(e.state)),
            backend_opened: enc.map(|e| e.backend),
            detached: enc.map_or(0, |e| e.detached),
            published_total: enc.map_or(0, |e| e.published_total),
            dropped_total: enc.map_or(0, |e| e.dropped_total),
            source_seq: enc.map_or(0, |e| e.source_seq),
            current_stage: self.coordinator.current_stage().map(stage_name),
            last_episode: self.coordinator.last_summary().map(|s| CaptureEpisode {
                stall_class: stall_name(s.class),
                recovered: s.recovered,
                took: s.took,
                stages: s
                    .stages
                    .iter()
                    .map(|r| {
                        let outcome = match r.outcome {
                            Some(StageOutcome::Applied) => "applied",
                            Some(StageOutcome::Failed) => "failed",
                            Some(StageOutcome::Unsupported) => "unsupported",
                            None => "timed_out",
                        };
                        (stage_name(r.stage), outcome, r.took)
                    })
                    .collect(),
                consecutive_failures: s.consecutive_failures,
                cooldown: s.cooldown,
            }),
            episodes_suppressed: self.coordinator.suppressed(),
            cooldown_remaining: self
                .coordinator
                .cooldown_until()
                .and_then(|t| t.checked_duration_since(now)),
        }
    }

    /// One tick of the classifier and coordinator over `i`. Access units published since the
    /// last tick prove an `Encoder` episode here; source frames prove every other class
    /// through [`Self::source_frame`].
    pub(super) fn tick(&mut self, i: Inputs) -> Step {
        if !self.owns_episode() && i.now.saturating_duration_since(self.last_tick) < TICK {
            return Step::Nothing;
        }
        self.last_tick = i.now;
        let au_progress = i.encoder.map_or(0, |t| {
            let n = t.published_total.saturating_sub(self.published_last);
            self.published_last = t.published_total;
            n.min(u64::from(u32::MAX)) as u32
        });
        if i.cursor_gap_px >= INPUT_EVIDENCE_PX && self.input_at.is_none() {
            self.input_at = Some(i.now);
        }
        let snap = Snapshot {
            now: i.now,
            drain_heartbeat: i.heartbeat_age.and_then(|a| i.now.checked_sub(a)),
            last_source: Some(i.last_source),
            source_seq: i.source_seq,
            last_au: i.encoder.map(|t| t.last_au),
            present_to_arrival: i.encoder.and_then(|t| t.present_to_arrival),
            activity: evidence(
                i.now,
                i.last_source,
                self.input_at,
                self.canary_at.filter(|_| self.canary_shown),
            ),
            topology_in_transaction: i.topology_held,
            rebuilding: i.recreating,
            secure_desktop: i.secure_desktop,
            encoder_detached: i.encoder.map_or(0, |t| t.detached),
        };
        let (verdict, changed) = self.classifier.observe(&snap);
        self.last_gap = verdict.source_gap;
        if changed {
            match verdict.class {
                HealthClass::Suspect | HealthClass::Stalled(_) => tracing::info!(
                    class = ?verdict.class,
                    gap_s = verdict.source_gap.as_secs(),
                    evidence = ?verdict.evidence,
                    "IDD push: capture health changed"
                ),
                _ => tracing::debug!(class = ?verdict.class, "IDD push: capture health changed"),
            }
        }
        let owned = self.owns_episode();
        let action = match self.coordinator.step(i.now, Event::Verdict(verdict.class)) {
            Action::None => self.coordinator.step(i.now, Event::Tick),
            a => a,
        };
        if !owned && self.owns_episode() {
            self.opened = Some((verdict.source_gap, i.now));
        }
        match self.act(i.now, action, verdict.source_gap) {
            Step::Nothing => {}
            step => return step,
        }
        if au_progress > 0 && self.coordinator.current_class() == Some(StallClass::Encoder) {
            let progress = Event::Progress {
                new_source_frames: au_progress,
                assignment_changed: false,
            };
            let action = self.coordinator.step(i.now, progress);
            match self.act(i.now, action, verdict.source_gap) {
                Step::Nothing => {}
                step => return step,
            }
        }
        if verdict.wants_canary
            && verdict.source_gap >= CANARY_AFTER
            && self
                .canary_at
                .is_none_or(|t| i.now.saturating_duration_since(t) >= CANARY_AFTER)
        {
            self.canary_at = Some(i.now);
            self.canary_shown = true;
            return Step::Canary;
        }
        Step::Nothing
    }

    /// The canary [`Step::Canary`] asked for could not be shown: it never counts as unanswered.
    /// The next interval asks again.
    pub(super) fn canary_not_shown(&mut self) {
        self.canary_shown = false;
    }

    /// The running stage's actuator finished.
    pub(super) fn stage_done(&mut self, now: Instant, stage: Stage, outcome: StageOutcome) -> Step {
        let action = self.coordinator.step(now, Event::StageDone(stage, outcome));
        self.act(now, action, self.last_gap)
    }

    /// A NEW source frame arrived (never a regen or hold). Clears the gap's evidence; closes a
    /// proving episode once the budgeted count has landed, returning its summary and the
    /// measured outage. An `Encoder` episode is deaf to it: source frames kept flowing
    /// through that stall, so only access units can prove it.
    pub(super) fn source_frame(&mut self, now: Instant) -> Option<(Summary, Duration)> {
        self.input_at = None;
        self.canary_at = None;
        if self.coordinator.current_class() == Some(StallClass::Encoder) {
            return None;
        }
        let progress = Event::Progress {
            new_source_frames: 1,
            assignment_changed: false,
        };
        // Two statements: `act` takes `&mut self`, so the coordinator step cannot be an
        // argument expression of it.
        let action = self.coordinator.step(now, progress);
        match self.act(now, action, self.last_gap) {
            Step::Recovered { summary, outage } => Some((summary, outage)),
            _ => None,
        }
    }

    /// The measured outage of the episode closing now: the source gap at its opening plus its
    /// own length.
    fn outage(&mut self, now: Instant) -> Duration {
        self.opened.take().map_or(Duration::ZERO, |(gap, at)| {
            gap + now.saturating_duration_since(at)
        })
    }

    fn act(&mut self, now: Instant, action: Action, gap: Duration) -> Step {
        match action {
            Action::None => Step::Nothing,
            Action::Run(stage) => Step::Run(stage),
            Action::Recovered => {
                let outage = self.outage(now);
                self.coordinator
                    .last_summary()
                    .cloned()
                    .map_or(Step::Nothing, |summary| Step::Recovered { summary, outage })
            }
            Action::Failed => {
                let summary = self
                    .coordinator
                    .last_summary()
                    .cloned()
                    .expect("a failed episode leaves its summary");
                Step::Failed { gap, summary }
            }
        }
    }
}

/// The strongest activity evidence newer than the last source frame. An unanswered canary is
/// strong; cursor travel alone is weak (a hardware-cursor desktop composes nothing while the
/// pointer moves).
fn evidence(
    now: Instant,
    last_source: Instant,
    input_at: Option<Instant>,
    canary_at: Option<Instant>,
) -> Option<Activity> {
    let mut all = Vec::with_capacity(2);
    if let Some(at) = input_at {
        all.push(Activity {
            at,
            kind: ActivityKind::Input,
        });
    }
    if let Some(at) = canary_at.filter(|t| now.saturating_duration_since(*t) >= CANARY_ANSWER) {
        all.push(Activity {
            at,
            kind: ActivityKind::Canary,
        });
    }
    Activity::strongest_since(&all, Some(last_source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(now: Instant, last_source: Instant, cursor_gap_px: u32) -> Inputs {
        Inputs {
            now,
            last_source,
            source_seq: 1,
            heartbeat_age: Some(Duration::from_millis(5)),
            cursor_gap_px,
            recreating: false,
            secure_desktop: false,
            topology_held: false,
            encoder: None,
        }
    }

    fn enc(last_au: Instant, published_total: u64, detached: u32) -> Option<EncoderTelemetry> {
        Some(EncoderTelemetry {
            last_au,
            published_total,
            detached,
            source_seq: 0,
            dropped_total: 0,
            drain_heartbeat: Some(last_au),
            present_to_arrival: None,
            driver_split: None,
            au_repeat: false,
            state: 0,
            backend: "nvenc",
        })
    }

    /// A driver encoder wedged under a flowing source: the ladder opens at the encoder reset,
    /// which the loop runs; new access units — never source frames — close the episode.
    #[test]
    fn encoder_silence_under_flowing_source_is_proven_by_access_units() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut sv = Supervisor::new(t0);
        let mut i = inputs(s(1), s(1), 0);
        i.encoder = enc(s(1), 60, 0);
        assert_eq!(sv.tick(i), Step::Nothing);
        // Source keeps flowing (fresh `last_source`), the encoder has published nothing for 16 s.
        let mut i = inputs(s(17), s(17), 0);
        i.encoder = enc(s(1), 60, 0);
        assert_eq!(sv.tick(i), Step::Run(Stage::EncoderReset));
        assert_eq!(
            sv.stage_done(s(17), Stage::EncoderReset, StageOutcome::Applied),
            Step::Nothing
        );
        for _ in 0..3 {
            assert_eq!(
                sv.source_frame(s(17)),
                None,
                "source frames prove nothing here"
            );
        }
        assert!(sv.owns_episode());
        let mut i = inputs(s(18), s(18), 0);
        i.encoder = enc(s(18), 63, 0);
        match sv.tick(i) {
            Step::Recovered { summary, outage } => {
                assert!(summary.recovered);
                assert_eq!(summary.stages[0].stage, Stage::EncoderReset);
                assert_eq!(outage, Duration::from_secs(1));
            }
            other => panic!("three new access units must close the episode, got {other:?}"),
        }
        assert!(!sv.owns_episode());
    }

    /// The WP3b contract survives the hand-over: under the floor nothing runs; over it, cursor
    /// travel alone asks for a canary and never an actuator; an unanswered canary opens the
    /// presentation ladder, and three new source frames close it.
    #[test]
    fn supervisor_walks_idle_canary_presentation_reset_recovered() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut sv = Supervisor::new(t0);
        // Static desktop, no evidence: idle forever.
        assert_eq!(sv.tick(inputs(s(60), t0, 0)), Step::Nothing);
        // Cursor travel past the floor: canary, not an actuator (weak evidence).
        assert_eq!(sv.tick(inputs(s(61), t0, 200)), Step::Canary);
        // An unanswered canary is strong: the presentation ladder opens.
        assert_eq!(
            sv.tick(inputs(s(62), t0, 200)),
            Step::Run(Stage::PresentationReset)
        );
        assert!(sv.owns_episode());
        assert_eq!(
            sv.stage_done(s(62), Stage::PresentationReset, StageOutcome::Applied),
            Step::Nothing
        );
        assert_eq!(sv.source_frame(s(63)), None);
        assert_eq!(sv.source_frame(s(63)), None);
        let (summary, outage) = sv.source_frame(s(63)).expect("three source frames recover");
        assert!(summary.recovered);
        // The outage is the whole hole: 62 s of missed source before the episode plus 1 s in it.
        assert_eq!(outage, Duration::from_secs(63));
        assert!(!sv.owns_episode());
    }

    /// A canary that never reached the screen is no evidence: the ladder stays shut, and the
    /// next canary waits out the interval.
    #[test]
    fn a_canary_nobody_saw_opens_nothing() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut sv = Supervisor::new(t0);
        assert_eq!(sv.tick(inputs(s(61), t0, 200)), Step::Canary);
        sv.canary_not_shown();
        assert_eq!(sv.tick(inputs(s(62), t0, 200)), Step::Nothing);
        assert!(!sv.owns_episode());
        assert_eq!(sv.tick(inputs(s(66), t0, 200)), Step::Canary);
    }

    /// A wedged encoder whose two resets already detached threads (`detached == 2`) skips a
    /// third encoder reset and opens straight at the driver cycle.
    #[test]
    fn two_detached_threads_open_at_the_driver_cycle() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut sv = Supervisor::new(t0);
        let mut i = inputs(s(1), s(1), 0);
        i.encoder = enc(s(1), 60, 2);
        assert_eq!(sv.tick(i), Step::Nothing);
        let mut i = inputs(s(17), s(17), 0);
        i.encoder = enc(s(1), 60, 2);
        assert_eq!(sv.tick(i), Step::Run(Stage::DriverCycle));
    }
}
