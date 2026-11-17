//! The paced drain's step: per pass, vend the store's front frame at its aim, park it
//! behind the glass gate, or wait. Pure over the slot pick and the gate; the run loop
//! samples [`PaceFacts`], calls [`PaceState::step`] and performs the [`PaceAction`].
//!
//! [`PaceState::wait`] is how long the loop sleeps after any pass, read as its wake: the
//! wake and the present decision share one rule, so neither can oversleep a frame past the
//! instant it goes out.
//!
//! Pinned by the table test below.

use crate::present_pace::{Cadence, PresentGate};
use crate::slot_pick::{Pick, SlotPick};
use crate::vk::BusyOn;
use std::time::Duration;

/// The housekeeping sleep of a loop with nothing due.
const TICK: Duration = Duration::from_millis(15);
/// Short waits sleep in slices (`wait_event`), so this is what a due time may slip by.
const FLOOR: Duration = Duration::from_micros(200);

/// What one pass knows when it decides.
pub(crate) struct PaceFacts {
    pub(crate) now_ns: u64,
    /// The latch clock's period.
    pub(crate) period_ns: u64,
    /// When the store's front frame is due; `None`: the store is empty.
    pub(crate) front_due_ns: Option<u64>,
    /// Presents in flight that the glass has not shown yet.
    pub(crate) outstanding: usize,
    /// The session paces at all; the arrival presenter does not.
    pub(crate) pacing: bool,
    /// The present mode queues (the FIFO family).
    pub(crate) fifo: bool,
    /// A glass clock reports the presents.
    pub(crate) glass: bool,
    /// Its stamps are the vblank waiter's estimate.
    pub(crate) estimated: bool,
    /// A stamp has anchored the latch clock: its slots are real latches.
    pub(crate) anchored: bool,
    pub(crate) verdict: Cadence,
    /// The intents as the pacer resolved them this pass.
    pub(crate) smoothing: bool,
    pub(crate) paces_latency: bool,
    pub(crate) free_running: bool,
}

impl PaceFacts {
    /// A paced intent on an anchored grid: frames are aimed at latch slots.
    pub(crate) fn on_grid(&self) -> bool {
        (self.smoothing || self.paces_latency) && !self.free_running && self.anchored
    }
}

/// What a pass does with the store's front frame.
pub(crate) enum PaceAction<T> {
    /// Present it now. `aim_ns` is the latch it is aimed at; 0 off the grid.
    Vend { frame: T, aim_ns: u64 },
    /// The glass gate holds it: back to the store, its aim kept.
    Park(T),
    /// Nothing goes out this pass; [`PaceState::wait`] says for how long.
    Wait,
}

/// What holds the front frame between two passes.
#[derive(Default)]
enum Hold {
    #[default]
    Free,
    Parked,
    Busy(BusyOn),
}

/// The step's memory between passes.
#[derive(Default)]
pub(crate) struct PaceState {
    /// [`SlotPick::last_target_ns`] before this pass's pick; a frame put back rewinds to it.
    mark: u64,
    hold: Hold,
    /// The instant the front frame goes out, from this pass's pick; 0 when none waits.
    present_ns: u64,
}

impl PaceState {
    /// This pass's decision. A paced intent on an anchored grid aims the frame at a latch
    /// slot, one frame per slot, and sends it out a lead ahead of it; anything else vends
    /// whatever the store releases. `slot_after` is the latch clock's next slot; `take`
    /// pops the store by its own due rule.
    pub(crate) fn step<T>(
        &mut self,
        slots: &mut SlotPick,
        gate: &mut PresentGate,
        f: &PaceFacts,
        slot_after: impl FnOnce(u64) -> u64,
        take: impl FnOnce() -> Option<T>,
    ) -> PaceAction<T> {
        self.hold = Hold::Free;
        self.present_ns = 0;
        self.mark = slots.last_target_ns();
        let on_grid = f.on_grid();
        let taken = if on_grid {
            match slots.pick(f.now_ns, f.period_ns, f.front_due_ns, slot_after, take) {
                Pick::Vend(frame) => Some(frame),
                Pick::At(present_ns) => {
                    self.present_ns = present_ns;
                    None
                }
                Pick::Empty => None,
            }
        } else {
            take()
        };
        let Some(frame) = taken else {
            return PaceAction::Wait;
        };
        if gated(f) && !gate.open(f.outstanding, f.now_ns) {
            slots.rewind(self.mark);
            self.hold = Hold::Parked;
            return PaceAction::Park(frame);
        }
        let aim_ns = if on_grid { slots.last_target_ns() } else { 0 };
        PaceAction::Vend { frame, aim_ns }
    }

    /// The presenter found `on` busy for the vended frame. `returned`: the frame went back
    /// to the store, so its slot is free again.
    pub(crate) fn note_busy(&mut self, slots: &mut SlotPick, on: BusyOn, returned: bool) {
        if returned {
            slots.rewind(self.mark);
        }
        self.hold = Hold::Busy(on);
    }

    /// The presenter holds this pass's frame for a retry.
    pub(crate) fn is_busy(&self) -> bool {
        matches!(self.hold, Hold::Busy(_))
    }

    /// The loop's sleep. The presenter's fence wait is the pace, so a busy fence turns
    /// straight around; a full swapchain needs a refresh to pass. A parked frame sleeps to
    /// the tick: the waiter's wake reopens the gate, the stale force-open the tick. A paced
    /// front frame sleeps to the instant its pick named, else its due time, at least the
    /// floor; anything else, the tick.
    pub(crate) fn wait(&self, now_ns: u64, front_due_ns: Option<u64>, paced: bool) -> Duration {
        match self.hold {
            Hold::Busy(BusyOn::Fence) => Duration::ZERO,
            Hold::Busy(BusyOn::Acquire) => Duration::from_millis(1),
            Hold::Parked => TICK,
            Hold::Free => match front_due_ns {
                Some(due) if paced => {
                    let at = if self.present_ns > 0 {
                        self.present_ns
                    } else {
                        due
                    };
                    Duration::from_nanos(at.saturating_sub(now_ns)).clamp(FLOOR, TICK)
                }
                _ => TICK,
            },
        }
    }
}

/// The FIFO glass budget applies: one undisplayed present in flight, so the swapchain's
/// own FIFO never becomes a standing queue. An estimate counts only at the mode rate,
/// where that queue stands two deep; under variable refresh nothing queues.
fn gated(f: &PaceFacts) -> bool {
    f.pacing
        && f.fifo
        && f.glass
        && (!f.estimated || f.verdict == Cadence::Fixed)
        && f.verdict != Cadence::Variable
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::present_pace::{FrameStore, SourcePacer};

    const US: u64 = 1_000;
    /// A 100 Hz grid anchored at 0, as `LatchClock::next_slot_after` reckons it.
    const PERIOD: u64 = 10_000 * US;

    fn slot_after(t: u64) -> u64 {
        (t / PERIOD + 1) * PERIOD
    }

    /// The session's intent, resolved by a real pacer from the verdict and the stamps.
    #[derive(Clone, Copy)]
    enum Intent {
        /// Smoothness on a measured grid.
        Smooth,
        /// Smoothness on a measured VRR panel.
        SmoothVrr,
        /// Smoothness on the vblank waiter's estimate, under this verdict.
        SmoothEstimated(Cadence),
        /// Smoothness before the first glass stamp: no grid yet.
        SmoothUnanchored,
        /// Latency on a fixed grid.
        Latency,
        /// Latency on a measured VRR panel.
        LatencyVrr,
        /// Latency with no glass clock at all.
        LatencyNoGrid,
    }

    /// A frame is `(id, due)`: µs in the table, ns in the store.
    type Frame = (u32, i64);
    const fn in_ns((id, due_us): Frame) -> Frame {
        (id, due_us * 1_000)
    }

    #[derive(Debug, PartialEq)]
    enum Want {
        /// Frame id, aim µs.
        Vend(u32, u64),
        Park,
        Wait,
    }

    struct Pass {
        now_us: u64,
        outstanding: usize,
        /// Taken in before the pass.
        lands: Option<Frame>,
        /// The presenter comes back busy on the vended frame.
        busy: Option<BusyOn>,
        want: Want,
        /// The loop's sleep after the pass, µs.
        wake_us: u64,
    }

    fn pass(now_us: u64, outstanding: usize, want: Want, wake_us: u64) -> Pass {
        Pass {
            now_us,
            outstanding,
            lands: None,
            busy: None,
            want,
            wake_us,
        }
    }

    struct Row {
        name: &'static str,
        intent: Intent,
        /// The store before the first pass.
        frames: &'static [Frame],
        /// Overrides the gate's facts: `(pacing, fifo)`.
        gate: (bool, bool),
        passes: Vec<Pass>,
    }

    /// Drive the step through a row's passes and perform each action as the run loop does.
    fn run(row: Row) {
        let smoothing = !matches!(
            row.intent,
            Intent::Latency | Intent::LatencyVrr | Intent::LatencyNoGrid
        );
        let (verdict, estimated) = match row.intent {
            Intent::Smooth | Intent::Latency => (Cadence::Fixed, false),
            Intent::SmoothVrr | Intent::LatencyVrr => (Cadence::Variable, false),
            Intent::SmoothEstimated(v) => (v, true),
            Intent::SmoothUnanchored | Intent::LatencyNoGrid => (Cadence::Unknown, false),
        };
        let glass = !matches!(row.intent, Intent::LatencyNoGrid);
        let anchored = glass && !matches!(row.intent, Intent::SmoothUnanchored);
        let mut pacer = SourcePacer::new(false, true);
        // The shell's grid: glass stamps that are not an estimate.
        pacer.follow(verdict, glass && !estimated, smoothing);
        let mut store: FrameStore<Frame> = FrameStore::new(if smoothing { 2 } else { 0 });
        store.set_paced(pacer.paces_latency());
        for &f in row.frames {
            store.submit(in_ns(f));
        }
        let (mut pace, mut slots) = (PaceState::default(), SlotPick::default());
        let mut gate = PresentGate::default();
        let paced = smoothing || pacer.paces_latency();
        let front_due = |s: &FrameStore<Frame>| s.front().map(|f| f.1.max(0) as u64);
        for (i, p) in row.passes.into_iter().enumerate() {
            if let Some(f) = p.lands {
                store.submit(in_ns(f));
            }
            let now = p.now_us * US;
            let facts = PaceFacts {
                now_ns: now,
                period_ns: PERIOD,
                front_due_ns: front_due(&store),
                outstanding: p.outstanding,
                pacing: row.gate.0,
                fifo: row.gate.1,
                glass,
                estimated,
                anchored,
                verdict,
                smoothing,
                paces_latency: pacer.paces_latency(),
                free_running: pacer.free_running(),
            };
            // The run loop's rule: newest-wins at a slot keeps one frame of backlog.
            let due = move |f: &Frame| f.1 <= now as i64;
            let take = || {
                if facts.on_grid() && !facts.smoothing {
                    store.take_due_bounded(due)
                } else {
                    store.take(due)
                }
            };
            let got = match pace.step(&mut slots, &mut gate, &facts, slot_after, take) {
                PaceAction::Vend { frame, aim_ns } => {
                    if let Some(on) = p.busy {
                        store.put_back(frame);
                        pace.note_busy(&mut slots, on, true);
                    } else {
                        gate.note_present(now);
                    }
                    Want::Vend(frame.0, aim_ns / US)
                }
                PaceAction::Park(frame) => {
                    store.put_back(frame);
                    Want::Park
                }
                PaceAction::Wait => Want::Wait,
            };
            assert_eq!(got, p.want, "{}: pass {i}", row.name);
            assert_eq!(pace.is_busy(), p.busy.is_some(), "{}: pass {i}", row.name);
            let wake = pace.wait(now, front_due(&store), paced);
            assert_eq!(
                wake.as_micros() as u64,
                p.wake_us,
                "{}: the sleep after pass {i}",
                row.name
            );
        }
    }

    /// Each row names what it pins.
    #[test]
    fn the_paced_drain_steps_by_the_table() {
        use Intent::*;
        use Want::*;
        const GATED: (bool, bool) = (true, true);
        let tick = TICK.as_micros() as u64;
        let rows = [
            Row {
                name: "a frame sleeps to its present instant, a lead ahead of its aim",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: GATED,
                // Due 12 + lead 5 = 17 → the latch at 20; out at 15.
                passes: vec![
                    pass(12_000, 0, Wait, 3_000),
                    pass(15_000, 0, Vend(0, 20_000), tick),
                ],
            },
            Row {
                name: "no anchor yet: the frame goes out at its due time, unaimed",
                intent: SmoothUnanchored,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    pass(11_000, 0, Wait, 1_000),
                    pass(12_000, 0, Vend(0, 0), tick),
                ],
            },
            Row {
                name: "a far due time sleeps one tick",
                intent: Smooth,
                frames: &[(0, 40_000)],
                gate: GATED,
                passes: vec![pass(12_000, 0, Wait, tick)],
            },
            Row {
                name: "past its present instant the open gate vends it at once",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![pass(17_000, 0, Vend(0, 20_000), tick)],
            },
            Row {
                name: "one frame per slot; the next sleeps to its own instant",
                intent: Smooth,
                frames: &[(0, 12_000), (1, 13_000)],
                gate: GATED,
                // Frame 1 would aim at 20 too: it takes 30 and goes out at 25.
                passes: vec![
                    pass(15_000, 0, Vend(0, 20_000), 200),
                    pass(15_200, 0, Wait, 9_800),
                    pass(25_000, 0, Vend(1, 30_000), tick),
                ],
            },
            Row {
                name: "R4-1: the closed gate parks it, keeps its aim, sleeps to the tick",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    pass(15_000, 1, Park, tick),
                    pass(15_200, 1, Park, tick),
                    pass(18_000, 0, Vend(0, 20_000), tick),
                ],
            },
            Row {
                name: "busy on the fence: it goes back, its aim kept, the loop turns around",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    Pass {
                        busy: Some(BusyOn::Fence),
                        ..pass(15_000, 0, Vend(0, 20_000), 0)
                    },
                    pass(15_100, 0, Vend(0, 20_000), tick),
                ],
            },
            Row {
                name: "busy on acquire: the loop waits a millisecond",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    Pass {
                        busy: Some(BusyOn::Acquire),
                        ..pass(15_000, 0, Vend(0, 20_000), 1_000)
                    },
                    pass(16_000, 0, Vend(0, 20_000), tick),
                ],
            },
            Row {
                name: "R4-7: estimated stamps vend at the due time, no aim, no gate",
                intent: SmoothEstimated(Cadence::Unknown),
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    pass(11_000, 1, Wait, 1_000),
                    pass(12_000, 1, Vend(0, 0), tick),
                ],
            },
            Row {
                name: "estimated stamps on a fixed panel gate, still with no aim",
                intent: SmoothEstimated(Cadence::Fixed),
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    pass(12_000, 1, Park, tick),
                    pass(12_500, 0, Vend(0, 0), tick),
                ],
            },
            Row {
                name: "free-running on measured VRR vends at the due time, ungated",
                intent: SmoothVrr,
                frames: &[(0, 12_345)],
                gate: GATED,
                passes: vec![
                    pass(12_000, 1, Wait, 345),
                    pass(12_345, 1, Vend(0, 0), tick),
                ],
            },
            Row {
                name: "no frame sleeps the tick",
                intent: Smooth,
                frames: &[],
                gate: GATED,
                passes: vec![pass(12_000, 0, Wait, tick)],
            },
            Row {
                name: "latency with no glass clock vends on arrival, whatever its due time",
                intent: LatencyNoGrid,
                frames: &[(0, 90_000)],
                gate: GATED,
                passes: vec![pass(12_000, 0, Vend(0, 0), tick)],
            },
            Row {
                name: "latency on a fixed grid aims like smoothing, a lead ahead of the latch",
                intent: Latency,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    pass(12_000, 0, Wait, 3_000),
                    pass(15_000, 0, Vend(0, 20_000), tick),
                ],
            },
            Row {
                name: "a successor early by jitter waits for its own slot",
                intent: Latency,
                frames: &[(0, 12_000), (1, 13_000)],
                gate: GATED,
                passes: vec![
                    pass(15_000, 0, Vend(0, 20_000), 200),
                    pass(25_000, 0, Vend(1, 30_000), tick),
                ],
            },
            Row {
                name: "a stream above the panel rate replaces the oldest of three due",
                intent: Latency,
                frames: &[(0, 12_000), (1, 13_000), (2, 14_000)],
                gate: GATED,
                passes: vec![pass(15_000, 0, Vend(1, 20_000), 200)],
            },
            Row {
                name: "a parked frame goes out ahead of its successor",
                intent: Latency,
                frames: &[(0, 0)],
                gate: GATED,
                // Due 0 + lead 5 → the latch at 10; out at 5.
                passes: vec![
                    pass(5_000, 1, Park, tick),
                    Pass {
                        lands: Some((1, 0)),
                        ..pass(6_000, 0, Vend(0, 10_000), 200)
                    },
                ],
            },
            Row {
                name: "paced latency holds a lone frame to its due time; a successor frees it",
                intent: LatencyVrr,
                frames: &[(0, 12_000)],
                gate: GATED,
                passes: vec![
                    pass(11_000, 0, Wait, 1_000),
                    Pass {
                        lands: Some((1, 20_000)),
                        ..pass(11_500, 0, Vend(0, 0), 8_500)
                    },
                    pass(20_000, 0, Vend(1, 0), tick),
                ],
            },
            Row {
                name: "an arrival session never gates",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: (false, true),
                passes: vec![pass(15_000, 1, Vend(0, 20_000), tick)],
            },
            Row {
                name: "a mode that cannot queue never gates",
                intent: Smooth,
                frames: &[(0, 12_000)],
                gate: (true, false),
                passes: vec![pass(15_000, 1, Vend(0, 20_000), tick)],
            },
        ];
        for row in rows {
            run(row);
        }
    }
}
