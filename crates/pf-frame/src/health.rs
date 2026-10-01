//! Live capture-health classifier (vdisplay immunity plan WP12; decisions D7 "recovery follows
//! evidence" and D8 "recovery is a state machine"). PURE: it reads the clocks the driver stamps
//! and names the class of a gap — it does no I/O, owns no thread, and fires no actuator. The
//! WP13 coordinator turns a [`HealthClass::Stalled`] verdict into its first actuator; everything
//! before that stage is a label.
//!
//! Two ground-truth clocks decide, both from the driver's AU-section header: the drain worker's
//! heartbeat, and encode progress — the pool's source counter against the last published access
//! unit. A stale heartbeat convicts the worker; a source that keeps moving while access units
//! stop convicts the encoder; both moving while the OS present stamp ages past
//! [`Thresholds::arrival_late`] is a late frame — reported, never a rung. A gap with no activity
//! evidence at all is IDLE: a static desktop composes nothing, and that is healthy.

use std::time::{Duration, Instant};

/// Something that shows the desktop SHOULD have produced a new image. Strength decides how far a
/// gap may escalate on it (immunity plan WP12, "activity sources, strongest first").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActivityKind {
    /// Host input or cursor motion aimed at this display. With a hardware cursor a moving pointer
    /// composes nothing, so on its own this can only raise suspicion (and request a canary).
    Input,
    /// A targeted composition canary was presented after suspicion and no source frame followed.
    Canary,
}

impl ActivityKind {
    /// Whether this evidence may carry a gap past the stall floor into a recovery verdict.
    /// Weak evidence stops at [`HealthClass::Suspect`] and asks for a canary instead.
    pub fn is_strong(self) -> bool {
        matches!(self, Self::Canary)
    }
}

/// One activity observation: when, and what kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activity {
    pub at: Instant,
    pub kind: ActivityKind,
}

impl Activity {
    /// The strongest observation newer than `after` (ties: the latest).
    pub fn strongest_since<'a>(
        all: impl IntoIterator<Item = &'a Activity>,
        after: Option<Instant>,
    ) -> Option<Activity> {
        all.into_iter()
            .filter(|a| after.is_none_or(|t| a.at > t))
            .copied()
            .max_by(|a, b| a.kind.cmp(&b.kind).then(a.at.cmp(&b.at)))
    }
}

/// Present → host arrival as the driver measured it: the wait in its pool (`None` for a frame
/// with no present stamp, a keyframe re-encode of the stash), the encode, and the hand-off from
/// the publish to the host's take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverSplit {
    pub pool: Option<Duration>,
    pub encode: Duration,
    pub ipc: Duration,
}

/// What the driver's encoder says about itself over the AU section — the whole view a host that
/// owns no pixels has of the display's frame path, and the operator surface's numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderTelemetry {
    /// The last access unit's publish time (`last_au_qpc`); the encoder's open time before the
    /// first one, so an encoder that never produces still reads as silent from a known instant.
    pub last_au: Instant,
    /// Access units published so far — an encoder episode's proof clock.
    pub published_total: u64,
    /// Encode threads the driver abandoned after a wedge.
    pub detached: u32,
    /// Frames the driver's drain worker handed its encode pool: DWM's own cadence, and the
    /// only source clock a host that owns no pixels has.
    pub source_seq: u64,
    /// Frames dropped at the pool or skipped for a full slot table.
    pub dropped_total: u64,
    /// The drain worker's last completed pass (`drain_heartbeat_qpc`); `None` before the first.
    pub drain_heartbeat: Option<Instant>,
    /// The newest access unit's OS present stamp (`qpc_pts`) against the moment the host took it.
    /// It grows when frames arrive late rather than not at all.
    pub present_to_arrival: Option<Duration>,
    /// The driver's own split of that span for the newest access unit, from the stamps on its
    /// slot. `None` from a driver that does not stamp, and before the first access unit.
    pub driver_split: Option<DriverSplit>,
    /// The newest access unit re-encodes the frame the one before it carried: a keyframe over
    /// a still desktop. The loop's own tick cannot tell, it races the driver's next frame.
    pub au_repeat: bool,
    /// The driver's encoder state word (`pf_driver_proto::encode::ENCODER_*`).
    pub state: u32,
    /// The backend the driver opened, for the status surface.
    pub backend: &'static str,
}

impl EncoderTelemetry {
    /// The drain worker's progress clock, and the ONLY sound value for [`Snapshot::source_seq`].
    ///
    /// [`Self::source_seq`] counts frames the pool accepted. That is the wrong clock for health:
    /// a wedged encode thread keeps its pool slot, the pool fills, and every later offer is a
    /// drop — so takes freeze while the drain worker is still acquiring at DWM's cadence. Feed
    /// takes alone to [`classify`] and the source gap grows, the encoder branch (which needs a
    /// SMALL source gap beside a stale access unit) is unreachable, and a composited-cursor
    /// session with no evidence lands on [`HealthClass::Idle`]: the wedge is invisible and no
    /// rung fires. Counting drops keeps the clock moving for as long as the worker does.
    pub fn drain_progress(&self) -> u64 {
        self.source_seq.saturating_add(self.dropped_total)
    }
}

/// Everything the classifier looks at, sampled at `now`. `None` clocks mean "never observed".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub now: Instant,
    /// [`EncoderTelemetry::drain_heartbeat`]. Stale = the worker is starved or wedged.
    pub drain_heartbeat: Option<Instant>,
    /// When the driver's pool last took a composed frame, and its counter
    /// ([`EncoderTelemetry::source_seq`]).
    pub last_source: Option<Instant>,
    pub source_seq: u64,
    /// [`EncoderTelemetry::last_au`]; `None` before the driver's encoder is open.
    pub last_au: Option<Instant>,
    /// [`EncoderTelemetry::present_to_arrival`] for the newest access unit.
    pub present_to_arrival: Option<Duration>,
    /// The strongest activity evidence newer than `last_source` ([`Activity::strongest_since`]).
    pub activity: Option<Activity>,
    /// A display-actor topology transaction owns the display right now.
    pub topology_in_transaction: bool,
    /// A presentation restart or a fresh `SET_ENCODE` is in flight — hold, count nothing.
    pub rebuilding: bool,
    /// UAC / Winlogon secure desktop is up: a separate state, never a failed canary.
    pub secure_desktop: bool,
    /// Encode threads the driver abandoned after a wedge ([`EncoderTelemetry::detached`]); each
    /// encoder reset that had to detach a thread bumps it. Two means the resets stopped holding.
    pub encoder_detached: u32,
}

/// What a stalled gap points at — and thereby its first actuator (D7 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StallClass {
    /// The drain heartbeat went stale → replace the swap-chain and its device.
    Worker,
    /// The pool kept taking frames and the encoder published none → encoder reset.
    Encoder,
    /// Heartbeat fresh, source stopped, a canary answered by nothing → one presentation reset.
    Presentation,
    /// An encoder stall that two resets did not hold (detached ≥ 2), or the WUDFHost is gone →
    /// cycle the adapter and its host process.
    Driver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthClass {
    /// Source frames arrive within the expected cadence.
    Healthy,
    /// No source frames and no activity evidence — DWM composed nothing. No recovery, ever.
    Idle,
    /// Source missed several intervals with activity evidence, below the stall floor — or past it
    /// on WEAK evidence only. The verdict asks for a canary here.
    Suspect,
    /// Past the stall floor on strong evidence: the coordinator's first actuator.
    Stalled(StallClass),
    /// After a stall: source frames are back but fewer than the recovery count have landed.
    Recovering,
    /// A rebuild or topology transaction owns the display — hold, count nothing.
    Rebuilding,
    SecureDesktop,
}

/// Tunables. Frame-relative where the plan says so, with absolute floors anchored to the recorded
/// field envelope (benign vendor holes run 1.6–10 s; multi-second holes recur every 20–45 s), so the
/// defaults are the values the plan fixes, not fresh guesses. Tests pin them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// The display's frame interval (1 / refresh).
    pub frame_interval: Duration,
    /// Missed source intervals before suspicion (subject to `suspect_floor`).
    pub suspect_missed_intervals: u32,
    pub suspect_floor: Duration,
    /// No actuator before this much continuously missed source (WP3b's floor carries over).
    pub stall_floor: Duration,
    /// A drain heartbeat older than this at stall time names the WORKER class.
    pub heartbeat_stale: Duration,
    /// Present→arrival past this is a reported degradation: the frames still come, late.
    /// 250 ms is ~15 refreshes at 60 Hz, far past the driver's own encode pipeline.
    pub arrival_late: Duration,
    /// Real source frames required after a stall before the class is `Healthy` again.
    pub recover_frames: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            frame_interval: Duration::from_micros(16_667),
            suspect_missed_intervals: 30,
            suspect_floor: Duration::from_millis(1_500),
            stall_floor: Duration::from_secs(15),
            heartbeat_stale: Duration::from_secs(2),
            arrival_late: Duration::from_millis(250),
            recover_frames: 3,
        }
    }
}

impl Thresholds {
    /// How long without a source frame counts as suspicious.
    pub fn suspect_after(&self) -> Duration {
        (self.frame_interval * self.suspect_missed_intervals).max(self.suspect_floor)
    }
}

/// One classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub class: HealthClass,
    /// Time since the last source frame (since the classifier started, if none yet).
    pub source_gap: Duration,
    /// The evidence the verdict rests on, if any.
    pub evidence: Option<ActivityKind>,
    /// Suspicion on weak-or-no strong evidence: the coordinator should issue a targeted canary
    /// (never move the user's cursor) so the next snapshot can carry strong evidence.
    pub wants_canary: bool,
    /// Both clocks advance and the present stamp ages past [`Thresholds::arrival_late`]: frames
    /// come late, not never. Reported on the status surface; it fires no rung.
    pub late: bool,
}

/// The stateless core: name `snap`'s class from the two driver clocks. `start` stands in for a
/// source frame that never came. Recovery hysteresis lives in [`Classifier`].
pub fn classify(th: &Thresholds, snap: &Snapshot, start: Instant) -> Verdict {
    let anchor = snap.last_source.unwrap_or(start);
    let source_gap = snap.now.saturating_duration_since(anchor);
    let evidence = snap.activity.map(|a| a.kind);
    let late = snap
        .present_to_arrival
        .is_some_and(|d| d >= th.arrival_late);
    let verdict = |class, wants_canary| Verdict {
        class,
        source_gap,
        evidence,
        wants_canary,
        late,
    };
    if snap.secure_desktop {
        return verdict(HealthClass::SecureDesktop, false);
    }
    if snap.topology_in_transaction || snap.rebuilding {
        return verdict(HealthClass::Rebuilding, false);
    }
    // A wedged encoder that two resets already had to detach threads for (`detached ≥ 2`) is not
    // a third-reset case: the resets stopped holding, so cycle the driver instead.
    let encode_class = || {
        if snap.encoder_detached >= 2 {
            StallClass::Driver
        } else {
            StallClass::Encoder
        }
    };
    let heartbeat_stale = snap
        .drain_heartbeat
        .is_none_or(|t| snap.now.saturating_duration_since(t) >= th.heartbeat_stale);
    if source_gap < th.suspect_after() {
        // The pool keeps taking composed frames, so the only leg that can still be silent is the
        // encoder: the source counter advances while the last access unit stands still.
        let au_gap = snap
            .last_au
            .map_or(source_gap, |t| snap.now.saturating_duration_since(t));
        if snap.last_source.is_some() && au_gap >= th.stall_floor {
            return verdict(HealthClass::Stalled(encode_class()), false);
        }
        return verdict(HealthClass::Healthy, false);
    }
    // The drain heartbeat ticks every pass, frames or not: silent past the floor it convicts the
    // worker by itself. A frozen driver over a still desktop has no other witness.
    if source_gap >= th.stall_floor && snap.drain_heartbeat.is_some() && heartbeat_stale {
        return verdict(HealthClass::Stalled(StallClass::Worker), false);
    }
    // Heartbeat fresh, source stopped, no access unit: DWM composed nothing. The caller counts
    // and reports it; no rung fires on an idle desktop.
    let Some(kind) = evidence else {
        return verdict(HealthClass::Idle, false);
    };
    if source_gap < th.stall_floor {
        return verdict(HealthClass::Suspect, !kind.is_strong());
    }
    if !kind.is_strong() {
        // Past the floor on cursor/input alone: a hardware-cursor desktop looks exactly like this.
        // Ask for the canary; the next snapshot decides.
        return verdict(HealthClass::Suspect, true);
    }
    let class = if heartbeat_stale {
        StallClass::Worker
    } else {
        StallClass::Presentation
    };
    verdict(HealthClass::Stalled(class), false)
}

/// [`classify`] plus the hysteresis D8 asks for: a stall clears only after `recover_frames` real
/// source frames, never on one republish or cursor regeneration.
#[derive(Clone, Debug)]
pub struct Classifier {
    th: Thresholds,
    start: Instant,
    last: Option<Verdict>,
    /// Set by a stall; cleared once `recover_frames` source frames have landed.
    recovering: Option<RecoverTrack>,
}

#[derive(Clone, Copy, Debug)]
struct RecoverTrack {
    seq_at_stall: u64,
    last_seq: u64,
    frames: u32,
}

impl Classifier {
    pub fn new(th: Thresholds, now: Instant) -> Self {
        Self {
            th,
            start: now,
            last: None,
            recovering: None,
        }
    }

    pub fn thresholds(&self) -> &Thresholds {
        &self.th
    }

    /// The previous verdict, if any.
    pub fn last(&self) -> Option<Verdict> {
        self.last
    }

    /// Classify `snap`; returns the verdict and whether the CLASS changed since the last call (one
    /// transition log per change is the WP13 budget).
    pub fn observe(&mut self, snap: &Snapshot) -> (Verdict, bool) {
        let mut v = classify(&self.th, snap, self.start);
        match (&mut self.recovering, v.class) {
            // Every stall — a relapse included — restarts the count: frames before it are no proof.
            (_, HealthClass::Stalled(_)) => {
                self.recovering = Some(RecoverTrack {
                    seq_at_stall: snap.source_seq,
                    last_seq: snap.source_seq,
                    frames: 0,
                });
            }
            (Some(track), HealthClass::Healthy) => {
                // Only a NEW source sequence counts — a republish or cursor regen keeps the seq.
                if snap.source_seq > track.last_seq {
                    track.frames += 1;
                    track.last_seq = snap.source_seq;
                }
                if track.frames >= self.th.recover_frames && snap.source_seq > track.seq_at_stall {
                    self.recovering = None;
                } else {
                    v.class = HealthClass::Recovering;
                }
            }
            _ => {}
        }
        let changed = self.last.is_none_or(|p| p.class != v.class);
        self.last = Some(v);
        (v, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// A display with the encoder open but nothing composing: the drain worker keeps its
    /// heartbeat, the pool takes no frame, the encoder publishes none.
    fn quiet(now: Instant) -> Snapshot {
        Snapshot {
            now,
            drain_heartbeat: Some(now),
            last_source: None,
            source_seq: 0,
            last_au: None,
            present_to_arrival: None,
            activity: None,
            topology_in_transaction: false,
            rebuilding: false,
            secure_desktop: false,
            encoder_detached: 0,
        }
    }

    /// A display streaming at cadence: both driver clocks fresh, access units arriving on time.
    fn flowing(start: Instant, now: Instant, seq: u64) -> Snapshot {
        Snapshot {
            last_source: Some(now),
            source_seq: seq,
            last_au: Some(now),
            present_to_arrival: Some(Duration::from_millis(20)),
            ..quiet(start)
        }
        .at(now)
    }

    impl Snapshot {
        /// Advance the clock with the drain worker alive (its heartbeat is per pass, ≤16 ms).
        fn at(mut self, now: Instant) -> Self {
            self.now = now;
            self.drain_heartbeat = Some(now);
            self
        }
        fn with(mut self, kind: ActivityKind, at: Instant) -> Self {
            self.activity = Some(Activity { at, kind });
            self
        }
    }

    const S: Duration = Duration::from_secs(1);

    /// A wedged encode thread must read as `Stalled(Encoder)`, not as an idle desktop.
    ///
    /// The wedge's shape: the drain worker keeps acquiring, so its heartbeat is fresh and the
    /// pool's DROP counter climbs, but the pool's TAKE counter is frozen at the slot the parked
    /// thread holds, and no access unit lands. Feeding takes alone reproduces the bug this
    /// guards — the source gap grows, the encoder branch is unreachable, and a session with a
    /// composited cursor (no evidence) reads `Idle` while the client sits black.
    #[test]
    fn a_wedged_encoder_is_not_an_idle_desktop() {
        let th = Thresholds::default();
        let start = t0();
        let wedged_at = start + S;
        let now = wedged_at + th.stall_floor + S;
        let t = EncoderTelemetry {
            last_au: wedged_at,
            published_total: 600,
            detached: 0,
            source_seq: 600,
            dropped_total: 1_200,
            drain_heartbeat: Some(now),
            present_to_arrival: None,
            driver_split: None,
            au_repeat: false,
            state: 0,
            backend: "nvenc",
        };
        assert_eq!(t.drain_progress(), 1_800, "takes plus drops");

        let base = Snapshot {
            last_au: Some(wedged_at),
            ..quiet(start)
        }
        .at(now);

        // The clock the capturer feeds: drops move it, so the source gap stays small.
        let live = Snapshot {
            last_source: Some(now),
            source_seq: t.drain_progress(),
            ..base
        };
        assert_eq!(
            classify(&th, &live, start).class,
            HealthClass::Stalled(StallClass::Encoder),
            "drops prove the worker is alive, so the silence is the encoder's"
        );

        // Takes alone: the clock froze with the encoder, and the wedge disappears.
        let frozen = Snapshot {
            last_source: Some(wedged_at),
            source_seq: t.source_seq,
            ..base
        };
        assert_eq!(
            classify(&th, &frozen, start).class,
            HealthClass::Idle,
            "the bug this guards: a wedged encoder hidden behind an idle verdict"
        );
    }

    #[test]
    fn defaults_are_the_plan_values() {
        let th = Thresholds::default();
        assert_eq!(
            th.stall_floor,
            Duration::from_secs(15),
            "WP3b's floor carries over"
        );
        assert_eq!(th.suspect_after(), Duration::from_millis(1_500));
        assert_eq!(th.recover_frames, 3);
        assert!(th.suspect_after() < th.stall_floor);
        assert!(th.arrival_late < th.suspect_after());
    }

    /// The heartbeat stays fresh, the source counter stops, no access unit follows: DWM composed
    /// nothing. Idle at every horizon, and never a rung.
    #[test]
    fn heartbeat_fresh_source_stopped_is_idle_forever() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let mut snap = flowing(s, last, 100);
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Healthy);
        for secs in [2u32, 15, 60, 600] {
            snap = snap.at(last + S * secs);
            let v = classify(&th, &snap, s);
            assert_eq!(v.class, HealthClass::Idle, "at +{secs}s");
            assert!(!v.wants_canary);
        }
    }

    /// The encoder wedge §2.5 names: the pool keeps taking frames, `last_au_qpc` stands still.
    #[test]
    fn source_moving_with_the_access_units_stopped_is_the_encoder() {
        let th = Thresholds::default();
        let s = t0();
        let now = s + 40 * S;
        let snap = Snapshot {
            last_au: Some(now - 16 * S),
            ..flowing(s, now, 2_000)
        };
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Encoder)
        );
        // Fourteen seconds of encoder silence is still inside the floor.
        let snap = Snapshot {
            last_au: Some(now - 14 * S),
            ..snap
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Healthy);
    }

    /// The heartbeat itself going stale is driver-level: the worker never finished a pass, so
    /// nothing downstream can be blamed. A driver that never reported one reads the same way.
    #[test]
    fn a_stale_drain_heartbeat_is_the_worker() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let now = last + 16 * S;
        let snap = Snapshot {
            drain_heartbeat: Some(last),
            ..flowing(s, last, 100).at(now)
        };
        assert_eq!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Worker)
        );
        let never = Snapshot {
            drain_heartbeat: None,
            ..snap
        };
        assert_eq!(classify(&th, &never, s).class, HealthClass::Idle);
        // With strong evidence behind it, a driver that never reported one still convicts.
        let never = never.with(ActivityKind::Canary, now - S);
        assert_eq!(
            classify(&th, &never, s).class,
            HealthClass::Stalled(StallClass::Worker)
        );
    }

    /// Both clocks advance, the OS present stamp ages past the bound: frames come late, not
    /// never. Reported on the verdict, and the class stays healthy — no rung.
    #[test]
    fn a_late_present_stamp_is_reported_not_recovered() {
        let th = Thresholds::default();
        let s = t0();
        let now = s + 10 * S;
        let snap = Snapshot {
            present_to_arrival: Some(Duration::from_millis(400)),
            ..flowing(s, now, 600)
        };
        let v = classify(&th, &snap, s);
        assert_eq!(v.class, HealthClass::Healthy);
        assert!(
            v.late,
            "400 ms past a 250 ms bound is a reported degradation"
        );
        let ok = Snapshot {
            present_to_arrival: Some(Duration::from_millis(30)),
            ..snap
        };
        assert!(!classify(&th, &ok, s).late);
    }

    /// Two detached encode threads mean the resets stopped holding: the encoder stall is named
    /// Driver instead, and one detach is still the encoder's own rung.
    #[test]
    fn two_detached_threads_turn_an_encoder_stall_into_a_driver_stall() {
        let th = Thresholds::default();
        let s = t0();
        let now = s + 40 * S;
        let wedged = Snapshot {
            last_au: Some(now - 16 * S),
            ..flowing(s, now, 2_000)
        };
        assert_eq!(
            classify(&th, &wedged, s).class,
            HealthClass::Stalled(StallClass::Encoder)
        );
        let two = Snapshot {
            encoder_detached: 2,
            ..wedged
        };
        assert_eq!(
            classify(&th, &two, s).class,
            HealthClass::Stalled(StallClass::Driver)
        );
        let one = Snapshot {
            encoder_detached: 1,
            ..wedged
        };
        assert_eq!(
            classify(&th, &one, s).class,
            HealthClass::Stalled(StallClass::Encoder)
        );
    }

    #[test]
    fn cursor_only_desktop_stops_at_suspect_and_asks_for_a_canary() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let snap = flowing(s, last, 100);
        let v = classify(
            &th,
            &snap
                .at(last + 30 * S)
                .with(ActivityKind::Input, last + 29 * S),
            s,
        );
        assert_eq!(
            v.class,
            HealthClass::Suspect,
            "a hardware cursor composes nothing"
        );
        assert!(v.wants_canary);
        // The canary came back with no source frame behind it: now it is a real stall.
        let v = classify(
            &th,
            &snap
                .at(last + 31 * S)
                .with(ActivityKind::Canary, last + 30 * S),
            s,
        );
        assert_eq!(v.class, HealthClass::Stalled(StallClass::Presentation));
        assert_eq!(v.evidence, Some(ActivityKind::Canary));
    }

    #[test]
    fn evidence_ranking_prefers_strength_then_recency() {
        let s = t0();
        let all = [
            Activity {
                at: s + 3 * S,
                kind: ActivityKind::Input,
            },
            Activity {
                at: s + S,
                kind: ActivityKind::Canary,
            },
            Activity {
                at: s + 2 * S,
                kind: ActivityKind::Canary,
            },
        ];
        let best = Activity::strongest_since(&all, Some(s)).unwrap();
        assert_eq!((best.kind, best.at), (ActivityKind::Canary, s + 2 * S));
        // Everything at or before `after` is out.
        assert!(Activity::strongest_since(&all, Some(s + 3 * S)).is_none());
    }

    #[test]
    fn topology_transaction_and_rebuilding_hold() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let base = flowing(s, last, 100)
            .at(last + 20 * S)
            .with(ActivityKind::Canary, last + 19 * S);
        let snap = Snapshot {
            topology_in_transaction: true,
            ..base
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Rebuilding);
        let snap = Snapshot {
            rebuilding: true,
            ..base
        };
        assert_eq!(classify(&th, &snap, s).class, HealthClass::Rebuilding);
    }

    #[test]
    fn secure_desktop_is_its_own_state() {
        let th = Thresholds::default();
        let s = t0();
        let snap = Snapshot {
            secure_desktop: true,
            ..quiet(s + 60 * S)
        }
        .with(ActivityKind::Canary, s + 59 * S);
        assert_eq!(classify(&th, &snap, s).class, HealthClass::SecureDesktop);
    }

    #[test]
    fn no_first_frame_yet_anchors_on_start() {
        let th = Thresholds::default();
        let s = t0();
        let v = classify(&th, &quiet(s + S), s);
        assert_eq!(v.class, HealthClass::Healthy, "inside the suspect window");
        let v = classify(&th, &quiet(s + 20 * S), s);
        assert_eq!((v.class, v.source_gap), (HealthClass::Idle, 20 * S));
    }

    #[test]
    fn recovery_needs_several_real_frames_not_regens() {
        let th = Thresholds::default();
        let s = t0();
        let mut c = Classifier::new(th, s);
        let last = s + 10 * S;
        let (v, changed) = c.observe(&flowing(s, last, 100));
        assert_eq!((v.class, changed), (HealthClass::Healthy, true));
        let stalled = flowing(s, last, 100)
            .at(last + 16 * S)
            .with(ActivityKind::Canary, last + 15 * S);
        let (v, changed) = c.observe(&stalled);
        assert_eq!(
            (v.class, changed),
            (HealthClass::Stalled(StallClass::Presentation), true)
        );
        // Same class again: no transition.
        assert!(!c.observe(&stalled.at(last + 17 * S)).1);
        // Frames come back. Seq unchanged = a republish / cursor regen: does not count.
        let back = last + 18 * S;
        let (v, _) = c.observe(&flowing(s, back, 100));
        assert_eq!(v.class, HealthClass::Recovering);
        for (i, seq) in [101u64, 102].iter().enumerate() {
            let (v, _) = c.observe(&flowing(s, back + (i as u32 + 1) * S / 10, *seq));
            assert_eq!(v.class, HealthClass::Recovering, "frame {seq}");
        }
        let (v, changed) = c.observe(&flowing(s, back + S, 103));
        assert_eq!((v.class, changed), (HealthClass::Healthy, true));
    }

    #[test]
    fn relapse_during_recovery_restarts_the_count() {
        let th = Thresholds::default();
        let s = t0();
        let mut c = Classifier::new(th, s);
        let last = s + 10 * S;
        c.observe(&flowing(s, last, 100));
        c.observe(
            &flowing(s, last, 100)
                .at(last + 16 * S)
                .with(ActivityKind::Canary, last + 15 * S),
        );
        let back = last + 17 * S;
        c.observe(&flowing(s, back, 101));
        c.observe(&flowing(s, back + S / 10, 102));
        // Relapse: another full stall.
        let (v, _) = c.observe(
            &flowing(s, back + S / 10, 102)
                .at(back + 20 * S)
                .with(ActivityKind::Canary, back + 19 * S),
        );
        assert_eq!(v.class, HealthClass::Stalled(StallClass::Presentation));
        // Two frames after the relapse are still Recovering — the earlier two do not carry over;
        // the third clears it.
        let (v, _) = c.observe(&flowing(s, back + 21 * S, 103));
        assert_eq!(v.class, HealthClass::Recovering);
        let (v, _) = c.observe(&flowing(s, back + 21 * S + S / 10, 104));
        assert_eq!(
            v.class,
            HealthClass::Recovering,
            "two new frames since the relapse"
        );
        let (v, changed) = c.observe(&flowing(s, back + 21 * S + S / 5, 105));
        assert_eq!((v.class, changed), (HealthClass::Healthy, true));
    }

    /// Non-vacuity (verification matrix): invert one expected outcome and watch it fail.
    #[test]
    fn weak_evidence_would_have_stalled_without_the_strength_gate() {
        let th = Thresholds::default();
        let s = t0();
        let last = s + 10 * S;
        let snap = flowing(s, last, 100)
            .at(last + 30 * S)
            .with(ActivityKind::Input, last + 29 * S);
        assert_ne!(
            classify(&th, &snap, s).class,
            HealthClass::Stalled(StallClass::Presentation)
        );
        assert!(!ActivityKind::Input.is_strong());
        assert!(ActivityKind::Canary.is_strong());
    }
}
