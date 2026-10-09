//! The staged-recovery coordinator (vdisplay immunity plan WP13, decision D8 "recovery is a state
//! machine, not nested retries"). PURE: it consumes [`health`](crate::health) verdicts and actuator
//! outcomes and answers with the one [`Action`] the owner should take; it runs no actuator itself.
//!
//! An EPISODE opens on a `Stalled` verdict and walks the ladder from the class's first actuator:
//!
//! ```text
//! EncoderReset -> SwapChainReset -> PresentationReset -> DriverCycle -> Failed
//! ```
//!
//! Each stage runs ONCE per episode under a deadline and records its outcome; a stage that
//! reports success still has to PROVE it — `recover_frames` new source sequences before the
//! deadline — or the ladder escalates. A stash republish or cursor regeneration keeps the source
//! sequence and therefore never counts. Episodes are budgeted: a rolling cap per window, and an
//! exponential cooldown after a failed one. While an episode owns the display, passive
//! descriptor reactions must stand down ([`Coordinator::owns_episode`]); a stage can never see its
//! own generation change as a fresh incident, because the coordinator is the only opener.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::health::{HealthClass, StallClass};

/// One recovery actuator, in ladder order. Each returns a typed [`StageOutcome`] and the owner
/// tells the coordinator through [`Event::StageDone`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Encoder reset over `ENCODE_CTL` — the `Encoder` class's first actuator.
    EncoderReset,
    /// The driver asks the OS for a new swap-chain and fresh device objects.
    SwapChainReset,
    /// One actor-mediated same-mode reset, only for presentation evidence.
    PresentationReset,
    /// Reap the WUDFHost and reload the adapter, then rebuild — the last rung before `Failed`.
    DriverCycle,
}

impl Stage {
    const LADDER: [Stage; 4] = [
        Stage::EncoderReset,
        Stage::SwapChainReset,
        Stage::PresentationReset,
        Stage::DriverCycle,
    ];

    /// The first actuator for a stall class (D7 table).
    pub fn first_for(class: StallClass) -> Stage {
        match class {
            // One transport: an access-unit hole with a live source is the encoder's, and
            // the ladder's own escalation carries it to the driver cycle.
            StallClass::Encoder => Stage::EncoderReset,
            // No host actuator today: the arm is the driver's, behind `CAP_SWAPCHAIN_RESET`.
            // Until it exists the rung reports `Unsupported` and the ladder escalates.
            StallClass::Worker => Stage::SwapChainReset,
            StallClass::Presentation => Stage::PresentationReset,
            StallClass::Driver => Stage::DriverCycle,
        }
    }

    /// The next rung up, `None` past the top.
    pub fn next(self) -> Option<Stage> {
        let i = Self::LADDER.iter().position(|&s| s == self)?;
        Self::LADDER.get(i + 1).copied()
    }
}

/// What an actuator reported back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageOutcome {
    /// The actuator ran; recovery is now proven by source frames, not by this.
    Applied,
    /// The actuator could not run (error) — escalate.
    Failed,
    /// Not available on this host/driver (capability not negotiated) — skip without penalty.
    Unsupported,
}

/// Budgets. Defaults follow the plan's shape (one execution per stage per episode, a rolling cap,
/// exponential cooldown); the numbers are the starting values tests pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// A stage has this long — actuator run plus recovery proof — before the ladder escalates.
    pub stage_deadline: Duration,
    /// New source sequences that prove a stage worked.
    pub recover_frames: u32,
    /// At most this many episodes may OPEN within `episode_window`.
    pub episode_cap: u32,
    pub episode_window: Duration,
    /// Cooldown after a failed episode: `cooldown_base * 2^(consecutive failures - 1)`, capped.
    pub cooldown_base: Duration,
    pub cooldown_cap: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            stage_deadline: Duration::from_secs(8),
            recover_frames: 3,
            episode_cap: 4,
            episode_window: Duration::from_secs(600),
            cooldown_base: Duration::from_secs(10),
            cooldown_cap: Duration::from_secs(300),
        }
    }
}

/// How long the `EncoderReset` actuator waits for the first access unit after the reset
/// before it reports: the rung's cost is one IDR plus this wait. A fresh encoder's first AU
/// lands within a couple of frames of its open, so 100 ms covers 60 Hz with room; past it the
/// stage still counts as applied and the proof clock decides.
pub const ENCODER_RESET_FIRST_AU: Duration = Duration::from_millis(100);

impl Budget {
    /// Cooldown after `failures` consecutive failed episodes (0 = none).
    pub fn cooldown(&self, failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        let shift = (failures - 1).min(8);
        (self.cooldown_base * (1u32 << shift)).min(self.cooldown_cap)
    }
}

/// What the owner feeds in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The classifier's latest class.
    Verdict(HealthClass),
    /// The running stage's actuator finished.
    StageDone(Stage, StageOutcome),
    /// `new_source_frames` NEW progress units of the episode's class arrived since the last
    /// event — source sequences, or access units for an `Encoder` episode (never regens or
    /// holds); `assignment_changed` when a fresh swap-chain assignment was observed.
    Progress {
        new_source_frames: u32,
        assignment_changed: bool,
    },
    /// The clock moved; deadlines are checked.
    Tick,
}

/// What the owner should do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do.
    None,
    /// Run this actuator, then report [`Event::StageDone`].
    Run(Stage),
    /// The episode ended in recovery.
    Recovered,
    /// The ladder is exhausted: the episode failed; the owner ends the plane with a typed error
    /// (or falls back per policy). Cooldown is in force.
    Failed,
}

/// One stage's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StageRecord {
    pub stage: Stage,
    pub outcome: Option<StageOutcome>,
    pub took: Duration,
}

#[derive(Clone, Debug)]
struct Episode {
    class: StallClass,
    opened: Instant,
    stage: Stage,
    stage_started: Instant,
    /// The running stage's actuator has reported `Applied`; frames now prove it.
    proving: bool,
    frames: u32,
    done: Vec<StageRecord>,
}

/// One closed episode, for the single summary log the plan asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub class: StallClass,
    pub recovered: bool,
    pub took: Duration,
    pub stages: Vec<StageRecord>,
    /// Consecutive failed episodes after this one (0 on recovery).
    pub consecutive_failures: u32,
    pub cooldown: Duration,
}

/// The coordinator. One per capture plane; the owner calls [`Self::step`] on every classifier
/// verdict, actuator completion, progress report and tick.
#[derive(Clone, Debug)]
pub struct Coordinator {
    budget: Budget,
    episode: Option<Episode>,
    opened: VecDeque<Instant>,
    consecutive_failures: u32,
    cooldown_until: Option<Instant>,
    /// Stalled verdicts refused for budget reasons since the last episode.
    suppressed: u32,
    last_summary: Option<Summary>,
}

impl Coordinator {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            episode: None,
            opened: VecDeque::new(),
            consecutive_failures: 0,
            cooldown_until: None,
            suppressed: 0,
            last_summary: None,
        }
    }

    pub fn budget(&self) -> &Budget {
        &self.budget
    }

    /// An episode is open: passive descriptor reactions stand down, and a topology or ring
    /// generation change is this episode's doing, not a new incident.
    pub fn owns_episode(&self) -> bool {
        self.episode.is_some()
    }

    /// The stage currently running, if any.
    pub fn current_stage(&self) -> Option<Stage> {
        self.episode.as_ref().map(|e| e.stage)
    }

    /// The open episode's stall class: it decides which clock proves a stage.
    pub fn current_class(&self) -> Option<StallClass> {
        self.episode.as_ref().map(|e| e.class)
    }

    /// Stalled verdicts refused since the last episode (budget or cooldown).
    pub fn suppressed(&self) -> u32 {
        self.suppressed
    }

    /// The last closed episode.
    pub fn last_summary(&self) -> Option<&Summary> {
        self.last_summary.as_ref()
    }

    /// Until when new episodes are refused after failures.
    pub fn cooldown_until(&self) -> Option<Instant> {
        self.cooldown_until
    }

    /// Advance the machine. Returns the action the owner takes NOW.
    pub fn step(&mut self, now: Instant, event: Event) -> Action {
        match (self.episode.take(), event) {
            (None, Event::Verdict(HealthClass::Stalled(class))) => self.try_open(now, class),
            (None, _) => Action::None,
            (Some(mut ep), event) => {
                match event {
                    Event::StageDone(stage, outcome) if stage == ep.stage => {
                        let took = now.saturating_duration_since(ep.stage_started);
                        match outcome {
                            StageOutcome::Applied => {
                                ep.proving = true;
                                ep.frames = 0;
                                self.episode = Some(ep);
                                return Action::None;
                            }
                            StageOutcome::Failed | StageOutcome::Unsupported => {
                                ep.done.push(StageRecord {
                                    stage,
                                    outcome: Some(outcome),
                                    took,
                                });
                                return self.escalate(now, ep);
                            }
                        }
                    }
                    // A report for a stage that is not running is stale (a late actuator): ignore.
                    Event::StageDone(..) => {}
                    Event::Progress {
                        new_source_frames, ..
                    } if ep.proving => {
                        ep.frames = ep.frames.saturating_add(new_source_frames);
                        if ep.frames >= self.budget.recover_frames {
                            return self.close(now, ep, true);
                        }
                    }
                    Event::Progress { .. } | Event::Verdict(_) | Event::Tick => {}
                }
                // Deadline: the running stage (applied or not) had its chance.
                if now.saturating_duration_since(ep.stage_started) >= self.budget.stage_deadline {
                    let took = now.saturating_duration_since(ep.stage_started);
                    ep.done.push(StageRecord {
                        stage: ep.stage,
                        outcome: ep.proving.then_some(StageOutcome::Applied),
                        took,
                    });
                    return self.escalate(now, ep);
                }
                self.episode = Some(ep);
                Action::None
            }
        }
    }

    fn try_open(&mut self, now: Instant, class: StallClass) -> Action {
        if self.cooldown_until.is_some_and(|t| now < t) {
            self.suppressed = self.suppressed.saturating_add(1);
            return Action::None;
        }
        while self
            .opened
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= self.budget.episode_window)
        {
            self.opened.pop_front();
        }
        if self.opened.len() as u32 >= self.budget.episode_cap {
            self.suppressed = self.suppressed.saturating_add(1);
            return Action::None;
        }
        self.opened.push_back(now);
        self.suppressed = 0;
        let stage = Stage::first_for(class);
        self.episode = Some(Episode {
            class,
            opened: now,
            stage,
            stage_started: now,
            proving: false,
            frames: 0,
            done: Vec::new(),
        });
        Action::Run(stage)
    }

    fn escalate(&mut self, now: Instant, mut ep: Episode) -> Action {
        match ep.stage.next() {
            Some(next) => {
                ep.stage = next;
                ep.stage_started = now;
                ep.proving = false;
                ep.frames = 0;
                self.episode = Some(ep);
                Action::Run(next)
            }
            None => self.close(now, ep, false),
        }
    }

    fn close(&mut self, now: Instant, ep: Episode, recovered: bool) -> Action {
        let mut stages = ep.done;
        if recovered {
            stages.push(StageRecord {
                stage: ep.stage,
                outcome: Some(StageOutcome::Applied),
                took: now.saturating_duration_since(ep.stage_started),
            });
            self.consecutive_failures = 0;
            self.cooldown_until = None;
        } else {
            self.consecutive_failures = self.consecutive_failures.saturating_add(1);
            self.cooldown_until = Some(now + self.budget.cooldown(self.consecutive_failures));
        }
        self.last_summary = Some(Summary {
            class: ep.class,
            recovered,
            took: now.saturating_duration_since(ep.opened),
            stages,
            consecutive_failures: self.consecutive_failures,
            cooldown: self.budget.cooldown(self.consecutive_failures),
        });
        if recovered {
            Action::Recovered
        } else {
            Action::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn stalled(class: StallClass) -> Event {
        Event::Verdict(HealthClass::Stalled(class))
    }

    fn frames(n: u32) -> Event {
        Event::Progress {
            new_source_frames: n,
            assignment_changed: false,
        }
    }

    #[test]
    fn ladder_order_and_class_entry_points() {
        assert_eq!(Stage::first_for(StallClass::Encoder), Stage::EncoderReset);
        assert_eq!(Stage::first_for(StallClass::Worker), Stage::SwapChainReset);
        assert_eq!(
            Stage::first_for(StallClass::Presentation),
            Stage::PresentationReset
        );
        assert_eq!(Stage::first_for(StallClass::Driver), Stage::DriverCycle);
        let mut s = Stage::EncoderReset;
        let mut n = 1;
        while let Some(next) = s.next() {
            assert!(next > s, "ladder is strictly ascending");
            s = next;
            n += 1;
        }
        assert_eq!((s, n), (Stage::DriverCycle, 4));
    }

    #[test]
    fn a_stall_recovers_on_the_first_rung_with_real_frames_only() {
        let mut c = Coordinator::new(Budget::default());
        let t0 = Instant::now();
        assert_eq!(c.step(t0, Event::Verdict(HealthClass::Idle)), Action::None);
        assert_eq!(
            c.step(t0, stalled(StallClass::Encoder)),
            Action::Run(Stage::EncoderReset)
        );
        assert!(c.owns_episode());
        // A second stalled verdict while the episode runs is not a new incident.
        assert_eq!(c.step(t0 + S, stalled(StallClass::Encoder)), Action::None);
        assert_eq!(
            c.step(
                t0 + S,
                Event::StageDone(Stage::EncoderReset, StageOutcome::Applied)
            ),
            Action::None
        );
        // Zero new source sequences (republish / cursor regen) prove nothing.
        assert_eq!(c.step(t0 + 2 * S, frames(0)), Action::None);
        assert_eq!(c.step(t0 + 2 * S, frames(2)), Action::None);
        assert_eq!(c.step(t0 + 3 * S, frames(1)), Action::Recovered);
        assert!(!c.owns_episode());
        let s = c.last_summary().unwrap();
        assert!(s.recovered);
        assert_eq!(s.stages.len(), 1);
        assert_eq!(s.stages[0].stage, Stage::EncoderReset);
        assert_eq!((s.consecutive_failures, s.cooldown), (0, Duration::ZERO));
        assert_eq!(c.cooldown_until(), None);
    }

    #[test]
    fn every_stage_runs_once_then_the_episode_fails_and_cools_down() {
        let b = Budget::default();
        let mut c = Coordinator::new(b);
        let t0 = Instant::now();
        let mut t = t0;
        assert_eq!(
            c.step(t, stalled(StallClass::Encoder)),
            Action::Run(Stage::EncoderReset)
        );
        let expect = [
            Stage::SwapChainReset,
            Stage::PresentationReset,
            Stage::DriverCycle,
        ];
        let mut running = Stage::EncoderReset;
        for next in expect {
            t += S;
            // Failure escalates immediately …
            let a = c.step(t, Event::StageDone(running, StageOutcome::Failed));
            assert_eq!(a, Action::Run(next), "after {running:?}");
            running = next;
        }
        // … the last rung applied but never proved: the deadline exhausts the ladder.
        t += S;
        assert_eq!(
            c.step(t, Event::StageDone(running, StageOutcome::Applied)),
            Action::None
        );
        t += b.stage_deadline;
        assert_eq!(c.step(t, Event::Tick), Action::Failed);
        let s = c.last_summary().unwrap();
        assert!(!s.recovered);
        assert_eq!(s.stages.len(), 4, "each stage exactly once");
        assert_eq!(s.consecutive_failures, 1);
        assert_eq!(s.cooldown, b.cooldown_base);
        // In cooldown the next stall is suppressed, not re-fought.
        assert_eq!(c.step(t + S, stalled(StallClass::Encoder)), Action::None);
        assert_eq!(c.suppressed(), 1);
        // Past the cooldown it opens again; a second failure doubles the cooldown.
        let t2 = t + b.cooldown_base + S;
        assert_eq!(
            c.step(t2, stalled(StallClass::Presentation)),
            Action::Run(Stage::PresentationReset)
        );
        let mut t3 = t2;
        let mut stage = Stage::PresentationReset;
        loop {
            t3 += S;
            match c.step(t3, Event::StageDone(stage, StageOutcome::Unsupported)) {
                Action::Run(n) => stage = n,
                Action::Failed => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(c.last_summary().unwrap().cooldown, b.cooldown_base * 2);
    }

    #[test]
    fn a_stage_that_applies_but_never_proves_escalates_at_its_deadline() {
        let b = Budget::default();
        let mut c = Coordinator::new(b);
        let t0 = Instant::now();
        assert_eq!(
            c.step(t0, stalled(StallClass::Worker)),
            Action::Run(Stage::SwapChainReset)
        );
        assert_eq!(
            c.step(
                t0 + S,
                Event::StageDone(Stage::SwapChainReset, StageOutcome::Applied)
            ),
            Action::None
        );
        // One frame is not enough; the deadline moves the ladder on.
        assert_eq!(c.step(t0 + 2 * S, frames(1)), Action::None);
        assert_eq!(
            c.step(t0 + b.stage_deadline, Event::Tick),
            Action::Run(Stage::PresentationReset)
        );
        // A late report from the retired stage is ignored, not re-applied.
        assert_eq!(
            c.step(
                t0 + b.stage_deadline + S,
                Event::StageDone(Stage::SwapChainReset, StageOutcome::Applied)
            ),
            Action::None
        );
        assert_eq!(c.current_stage(), Some(Stage::PresentationReset));
    }

    #[test]
    fn rolling_cap_refuses_a_fifth_episode_in_the_window() {
        let b = Budget::default();
        let mut c = Coordinator::new(b);
        let t0 = Instant::now();
        for i in 0..b.episode_cap {
            let t = t0 + Duration::from_secs(20 * u64::from(i));
            assert_eq!(
                c.step(t, stalled(StallClass::Encoder)),
                Action::Run(Stage::EncoderReset)
            );
            assert_eq!(
                c.step(
                    t,
                    Event::StageDone(Stage::EncoderReset, StageOutcome::Applied)
                ),
                Action::None
            );
            assert_eq!(c.step(t + S, frames(3)), Action::Recovered);
        }
        let t = t0 + Duration::from_secs(100);
        assert_eq!(c.step(t, stalled(StallClass::Encoder)), Action::None);
        assert_eq!(c.suppressed(), 1);
        // Once the oldest episode leaves the window, a new one opens.
        let t = t0 + b.episode_window + S;
        assert_eq!(
            c.step(t, stalled(StallClass::Encoder)),
            Action::Run(Stage::EncoderReset)
        );
        assert_eq!(c.suppressed(), 0);
    }

    #[test]
    fn cooldown_doubles_to_the_cap() {
        let b = Budget::default();
        assert_eq!(b.cooldown(0), Duration::ZERO);
        assert_eq!(b.cooldown(1), b.cooldown_base);
        assert_eq!(b.cooldown(3), b.cooldown_base * 4);
        assert_eq!(b.cooldown(9), b.cooldown_cap);
        assert_eq!(b.cooldown(u32::MAX), b.cooldown_cap);
    }

    /// An encoder reset is costed by its stage record: the ctl call, the bounded first-AU wait
    /// and the proof all land in `took`, and that wait fits inside the stage deadline.
    #[test]
    fn an_encoder_reset_is_costed_by_its_stage_record() {
        let b = Budget::default();
        assert!(ENCODER_RESET_FIRST_AU < b.stage_deadline);
        let mut c = Coordinator::new(b);
        let t0 = Instant::now();
        assert_eq!(
            c.step(t0, stalled(StallClass::Encoder)),
            Action::Run(Stage::EncoderReset)
        );
        assert_eq!(c.current_class(), Some(StallClass::Encoder));
        let reset_done = t0 + ENCODER_RESET_FIRST_AU;
        assert_eq!(
            c.step(
                reset_done,
                Event::StageDone(Stage::EncoderReset, StageOutcome::Applied)
            ),
            Action::None
        );
        let proven = reset_done + Duration::from_millis(50);
        assert_eq!(c.step(proven, frames(3)), Action::Recovered);
        let s = c.last_summary().unwrap();
        assert_eq!(s.stages.len(), 1);
        assert_eq!(s.stages[0].stage, Stage::EncoderReset);
        assert_eq!(
            s.stages[0].took,
            ENCODER_RESET_FIRST_AU + Duration::from_millis(50)
        );
        assert_eq!(s.took, s.stages[0].took);
    }

    /// Non-vacuity: invert the proof rule and the machine would recover on nothing.
    #[test]
    fn recovery_needs_the_budgeted_frame_count_not_one() {
        let mut c = Coordinator::new(Budget::default());
        let t0 = Instant::now();
        c.step(t0, stalled(StallClass::Encoder));
        c.step(
            t0,
            Event::StageDone(Stage::EncoderReset, StageOutcome::Applied),
        );
        assert_ne!(c.step(t0 + S, frames(1)), Action::Recovered);
        assert_ne!(c.step(t0 + S, frames(1)), Action::Recovered);
        assert_eq!(c.step(t0 + S, frames(1)), Action::Recovered);
    }
}
