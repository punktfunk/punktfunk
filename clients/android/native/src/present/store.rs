//! The held-frame store between decode and present, one per sink.
//!
//! Every frame carries a due time from the source's own timestamps ([`CadenceClock`]): the due
//! time paces, the capacity only bounds a burst (`design/presenter-cadence-rework.md` §4.3).
//! Under `latency` the FIFO is two deep on the [`CadenceTuning::vrr_latency`] cushion — one MAD
//! of measured arrival jitter, floor 0.5 ms — widened by the late tail: the desktop presenter's
//! grid-latency rule, so a frame costs the jitter this link shows, up to one period where the
//! tail is one. Under `smooth` it is 1–3 deep on the snapping cushion.

use punktfunk_core::phase::{CadenceClock, CadenceHealth, CadenceTuning};
use std::collections::VecDeque;

use super::{Counters, PresentPriority};

/// Frames the latency FIFO holds: the due one and one early; a third evicts the oldest.
const LATENCY_DEPTH: usize = 2;

/// One frame held for presentation.
pub(crate) struct Held<F> {
    pub(crate) frame: F,
    /// When the source says the frame is due (`CLOCK_MONOTONIC`). [`Store::push`] always sets
    /// it; the budgets read `None` as due now.
    pub(crate) due_ns: Option<i64>,
}

/// The held frames. `F` is the sink's handle: a codec buffer index or a reader image.
pub(crate) struct Store<F> {
    /// The FIFO's bound against a burst: [`LATENCY_DEPTH`] under latency, the buffer under smooth.
    capacity: usize,
    frames: VecDeque<Held<F>>,
    /// The source-cadence loop.
    cadence: CadenceClock,
    /// The source's frame interval, the loop's cushion ceiling ([`super::frame_interval_ns`]).
    frame_interval_ns: i64,
}

impl<F> Store<F> {
    /// Latency: two deep on the one-MAD cushion, plus the late tail when `tail` (a spike then
    /// costs its own height for ~128 frames instead of a dropped frame). Smooth: `buffer` deep
    /// on the `snapping()` cushion, since every present lands on a vsync grid whose snap-up
    /// carries about half a refresh of implicit slack.
    pub(crate) fn new(priority: PresentPriority, frame_interval_ns: i64, tail: bool) -> Store<F> {
        let (capacity, cadence) = match priority {
            PresentPriority::Latency => {
                let mut clock = CadenceClock::new(CadenceTuning::vrr_latency());
                clock.set_tail_cushion(tail);
                (LATENCY_DEPTH, clock)
            }
            PresentPriority::Smooth { buffer } => {
                (buffer, CadenceClock::new(CadenceTuning::snapping()))
            }
        };
        Store {
            capacity,
            frames: VecDeque::new(),
            cadence,
            frame_interval_ns,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Frames held.
    pub(crate) fn fifo_len(&self) -> usize {
        self.frames.len()
    }

    /// Admit one decoded frame; past capacity the oldest goes to `evict` and counts in
    /// `skipped`. Returns the evictions.
    ///
    /// `pts_us` is the host's own stamp as the codec echoes it; `ready_mono_ns` is when the frame
    /// became presentable. They fold into the due time, fed and read in `CLOCK_MONOTONIC` only:
    /// the loop's offset estimator absorbs the constant between the host's clock and ours, and
    /// smooths the offset, never the timestamps.
    ///
    /// An overflow while frames arrive more than a source interval ahead of the loop's estimate
    /// is no burst: the path got shorter than the offset still says, as when a decoder backlog
    /// clears. Every held frame is then due now and the loop re-anchors on the next one, where
    /// slewing would evict a frame a refresh for a second.
    pub(crate) fn push(
        &mut self,
        frame: F,
        pts_us: u64,
        ready_mono_ns: i64,
        c: &mut Counters,
        mut evict: impl FnMut(F),
    ) -> u64 {
        let due_ns = self.cadence.due_ns(
            pts_us.saturating_mul(1_000),
            ready_mono_ns,
            self.frame_interval_ns,
        );
        // How far ahead of the estimate this frame became presentable. A burst is late frames.
        let ahead_ns = due_ns - ready_mono_ns - self.cadence.cushion_ns();
        self.frames.push_back(Held {
            frame,
            due_ns: Some(due_ns),
        });
        let mut evicted = 0u64;
        let stale = self.frames.len() > self.capacity && ahead_ns > self.frame_interval_ns;
        if stale {
            self.cadence.reset();
            for held in &mut self.frames {
                held.due_ns = None;
            }
        }
        while self.frames.len() > self.capacity {
            if let Some(old) = self.frames.pop_front() {
                evict(old.frame);
                evicted += 1;
            }
        }
        c.skipped += evicted;
        evicted
    }

    /// Whether the head may leave by `reach_ns`: due by then, or carrying no due time. The
    /// store holds what is not due yet; the sink sets the reach.
    pub(crate) fn head_is_releasable(&self, reach_ns: i64) -> bool {
        self.frames
            .front()
            .is_some_and(|f| f.due_ns.is_none_or(|due| due <= reach_ns))
    }

    /// The oldest held frame.
    pub(crate) fn take(&mut self) -> Option<Held<F>> {
        self.frames.pop_front()
    }

    /// Put back the frame [`Store::take`] returned when the budget was closed.
    pub(crate) fn park(&mut self, held: Held<F>) {
        self.frames.push_front(held);
    }

    /// Every held frame, oldest first, for the teardown.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = F> + '_ {
        self.frames.drain(..).map(|h| h.frame)
    }

    /// Re-anchor the cadence loop on the next frame. The decode loop calls it when the re-anchor
    /// gate arms: past a loss the frames come through a decoder that just recovered, so the
    /// source→presentable delay the loop measured no longer holds.
    pub(crate) fn reset_cadence(&mut self) {
        self.cadence.reset();
    }

    /// The cadence loop's health for the 1 Hz line.
    pub(crate) fn cadence_health(&self) -> CadenceHealth {
        self.cadence.health()
    }
}

#[cfg(test)]
mod tests {
    use super::super::budget::TimelineBudget;
    use super::super::frame_interval_ns;
    use super::*;

    const SOURCE_HZ: u32 = 120;
    const P: i64 = 8_333_333; // one 120 Hz source interval, ns
    /// A host stamping realtime against a monotonic present clock: two eras, which is why the
    /// loop estimates the offset rather than being told it.
    const PTS0_NS: i64 = 1_786_000_000_000_000_000;
    const MONO0: i64 = 987_000_000_000;

    /// `k`'s pts as the codec echoes it (µs, truncated as the feed queues it) and the monotonic
    /// instant it became presentable, with `jitter_ns` of transport noise on the arrival only.
    fn frame(k: i64, jitter_ns: i64) -> (u64, i64) {
        (((PTS0_NS + k * P) / 1000) as u64, MONO0 + k * P + jitter_ns)
    }

    fn store(priority: PresentPriority, tail: bool) -> Store<i64> {
        Store::new(priority, frame_interval_ns(SOURCE_HZ * 1000), tail)
    }

    /// Push `k` through `s` and take it back: the due time it was given.
    fn due_of(s: &mut Store<i64>, c: &mut Counters, k: i64, jitter_ns: i64) -> (i64, i64) {
        let (pts_us, ready) = frame(k, jitter_ns);
        s.push(k, pts_us, ready, c, drop);
        let held = s.take().expect("held");
        (held.due_ns.expect("every frame is due sometime"), ready)
    }

    #[test]
    fn the_latency_store_cushions_each_frame_by_the_measured_jitter_alone() {
        let mut s = store(PresentPriority::Latency, true);
        let mut c = Counters::default();
        for k in 0..600 {
            let (due, ready) = due_of(&mut s, &mut c, k, (k % 7) * 200_000);
            assert!(due >= ready, "frame {k} is cushioned ahead of ready");
            assert!(
                due - ready < 3_000_000,
                "frame {k} waits {} ns",
                due - ready
            );
        }
        let h = s.cadence_health();
        assert_eq!((h.frames, h.reanchors, h.late), (600, 1, 0));
        assert!(
            (500_000..2_000_000).contains(&h.cushion_ns),
            "cushion {} ns: a MAD, never a period",
            h.cushion_ns
        );
    }

    #[test]
    fn a_late_frame_rides_its_own_tail_and_the_tail_decays() {
        let mut s = store(PresentPriority::Latency, true);
        let mut c = Counters::default();
        for k in 0..300 {
            due_of(&mut s, &mut c, k, (k % 3) * 200_000);
        }
        // Six milliseconds late: shown, not dropped; the next frames carry its tail.
        let (due, ready) = due_of(&mut s, &mut c, 300, 6_000_000);
        assert!(due >= ready, "the spike is cushioned, not late");
        assert!(s.cadence_health().cushion_ns >= 6_000_000);
        for k in 301..700 {
            due_of(&mut s, &mut c, k, 0);
        }
        let h = s.cadence_health();
        assert_eq!(h.late, 0);
        assert!(
            h.cushion_ns < 2_000_000,
            "the tail decays: {} ns",
            h.cushion_ns
        );
        // Tail off: the spike reads as late and the cushion stays a MAD.
        let mut s = store(PresentPriority::Latency, false);
        for k in 0..300 {
            due_of(&mut s, &mut c, k, (k % 3) * 200_000);
        }
        let (due, ready) = due_of(&mut s, &mut c, 300, 6_000_000);
        assert!(due < ready);
        let h = s.cadence_health();
        assert_eq!(h.late, 1);
        assert!(h.cushion_ns < 2_000_000);
    }

    #[test]
    fn the_smooth_store_puts_every_decoded_frame_through_the_loop() {
        let mut s = store(PresentPriority::Smooth { buffer: 2 }, true);
        let mut c = Counters::default();
        for k in 0..600 {
            let (pts_us, ready) = frame(k, (k % 11) * 400_000);
            s.push(k, pts_us, ready, &mut c, drop);
            assert!(s.frames.back().is_some_and(|h| h.due_ns.is_some()));
        }
        let h = s.cadence_health();
        assert_eq!(h.frames, 600);
        assert_eq!(
            h.reanchors, 1,
            "only the cold start anchors on a clean trace"
        );
        s.reset_cadence();
        let (pts_us, ready) = frame(600, 0);
        s.push(600, pts_us, ready, &mut c, drop);
        assert_eq!(s.cadence_health().reanchors, 2);
    }

    #[test]
    fn the_latency_fifo_holds_two_and_evicts_the_oldest() {
        let mut s = store(PresentPriority::Latency, true);
        let mut c = Counters::default();
        let mut evicted = Vec::new();
        for k in 0..3 {
            let (pts_us, ready) = frame(k, 0);
            let n = s.push(k, pts_us, ready, &mut c, |f| evicted.push(f));
            assert_eq!(n, u64::from(k > 1));
        }
        assert_eq!(
            (evicted.as_slice(), c.skipped, s.fifo_len()),
            ([0].as_slice(), 1, 2)
        );
        let head = s.take().expect("head");
        assert_eq!(head.frame, 1, "oldest first");
        // A parked head stays the head; a fourth evicts it.
        s.park(head);
        let (pts_us, ready) = frame(3, 0);
        assert_eq!(s.push(3, pts_us, ready, &mut c, drop), 1);
        assert_eq!(s.drain().collect::<Vec<_>>(), [2, 3]);
        assert!(s.is_empty());
    }

    #[test]
    fn frames_ahead_of_their_due_times_reanchor_and_cost_one_frame() {
        // The loop settles on a path 60 ms long, as a backed-up decoder makes it.
        let mut s = store(PresentPriority::Smooth { buffer: 2 }, true);
        let mut c = Counters::default();
        for k in 0..120 {
            due_of(&mut s, &mut c, k, 60_000_000);
        }
        // The backlog clears: every frame is now 60 ms ahead of its due time.
        let mut evicted = 0;
        for k in 120..240 {
            let (pts_us, ready) = frame(k, 0);
            evicted += s.push(k, pts_us, ready, &mut c, drop);
            while s.head_is_releasable(ready + P) {
                s.take();
            }
        }
        assert_eq!(evicted, 1, "the overflow that showed it");
        assert_eq!(s.cadence_health().reanchors, 2);
        // A burst is late frames: three land together, the first two behind their cadence. The
        // oldest goes and the anchor stays.
        let mut s = store(PresentPriority::Latency, true);
        for k in 0..120 {
            due_of(&mut s, &mut c, k, 0);
        }
        let (_, together) = frame(122, 0);
        let mut evicted = 0;
        for k in 120..123 {
            let (pts_us, _) = frame(k, 0);
            evicted += s.push(k, pts_us, together, &mut c, drop);
        }
        assert_eq!(
            (evicted, s.fifo_len(), s.cadence_health().reanchors),
            (1, 2, 1)
        );
    }

    #[test]
    fn the_smooth_fifo_is_bounded_and_drains_oldest_first() {
        let mut s = store(PresentPriority::Smooth { buffer: 2 }, true);
        let mut c = Counters::default();
        let mut evicted = Vec::new();
        for k in 0..4 {
            let (pts_us, ready) = frame(k, 0);
            s.push(k, pts_us, ready, &mut c, |f| evicted.push(f));
        }
        assert_eq!(evicted, [0, 1]);
        assert_eq!((c.skipped, s.fifo_len()), (2, 2));
        let head = s.take().expect("head");
        assert_eq!(head.frame, 2);
        s.park(head);
        assert_eq!(s.drain().collect::<Vec<_>>(), [2, 3]);
        assert!(s.is_empty());
    }

    #[test]
    fn the_store_holds_a_frame_that_is_not_due_yet_and_releases_it_within_reach_of_its_slot() {
        let mut s = store(PresentPriority::Smooth { buffer: 2 }, true);
        let b = TimelineBudget::new(None);
        let due = MONO0 + 4 * P;
        s.frames.push_back(Held {
            frame: 0,
            due_ns: Some(due),
        });
        assert!(
            !s.head_is_releasable(b.reach_ns(MONO0, P)),
            "four refreshes early"
        );
        assert!(
            !s.head_is_releasable(b.reach_ns(due - P - b.margin_ns() - 1, P)),
            "one nanosecond outside the submit reach of its own slot"
        );
        assert!(
            s.head_is_releasable(b.reach_ns(due - P - b.margin_ns(), P)),
            "exactly one panel period of reach, the earliest that still buys nothing to wait"
        );
        assert!(
            s.head_is_releasable(b.reach_ns(due + 10 * P, P)),
            "late frames go at once"
        );
    }

    #[test]
    fn a_frame_carrying_no_due_time_is_always_releasable() {
        // The predicate drains a frame it cannot answer for rather than wedge behind it.
        let mut s = store(PresentPriority::Smooth { buffer: 2 }, true);
        s.frames.push_back(Held {
            frame: 0,
            due_ns: None,
        });
        assert!(s.head_is_releasable(MONO0));
        assert!(!store(PresentPriority::Latency, true).head_is_releasable(MONO0));
    }
}
