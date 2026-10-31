//! The ASurfaceControl presenter's apply margin: how far ahead of the learned lead a transaction
//! is applied, adapted to the slot misses the present fences report.

/// Where the margin starts: zero was enough on the A024.
const START_NS: i64 = 0;
/// One slot miss widens the margin by this much.
const STEP_NS: i64 = 500_000;
/// The widest margin: the desktop presenter's cap. A Shield TV's apply can take over 2.5 ms to
/// reach SurfaceFlinger's latch; the A024 never needed that.
const MAX_NS: i64 = 8_000_000;
/// Clean presents in a row that narrow the margin one step: a minute at 60 Hz. Narrowing is a
/// probe, and a probe that fails costs a repeated frame and a dropped one, so it runs no more
/// often than the clock beat's own drop.
const DECAY_PRESENTS: u32 = 3600;

/// Apply-ahead beyond the learned lead: the binder hop plus SurfaceFlinger's wakeup-to-latch gap.
/// A miss widens it one step, up to [`MAX_NS`]; [`DECAY_PRESENTS`] clean presents narrow it one.
pub(crate) struct ApplyMargin {
    ns: i64,
    /// Presents since the last slot miss.
    clean: u32,
}

impl ApplyMargin {
    pub(crate) const fn new() -> Self {
        Self {
            ns: START_NS,
            clean: 0,
        }
    }

    /// The margin now, in nanoseconds.
    pub(crate) const fn ns(&self) -> i64 {
        self.ns
    }

    /// One present with a real vsync. `missed`: SurfaceFlinger latched it a cycle after the one
    /// it was aimed at. `slack_ns`: how far ahead of that aimed latch the apply was. Only a miss
    /// with the apply within a step of the margin widens it; one with more slack is the
    /// compositor's own slip (a display queue changing depth, on a Shield TV every half minute),
    /// which no apply-ahead covers and must not ratchet the margin to its cap.
    pub(crate) fn observe(&mut self, missed: bool, slack_ns: i64) {
        if missed {
            if slack_ns < self.ns + STEP_NS {
                self.clean = 0;
                self.ns = (self.ns + STEP_NS).min(MAX_NS);
            }
            return;
        }
        self.clean += 1;
        if self.clean >= DECAY_PRESENTS {
            self.clean = 0;
            self.ns = (self.ns - STEP_NS).max(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyMargin, DECAY_PRESENTS, MAX_NS, STEP_NS};

    /// A session's presents replayed in runs: (missed, the apply's slack, how many, the margin
    /// after the run). A tight miss has the apply right at the margin; a slip has it 5 ms ahead.
    #[test]
    fn a_replayed_session_widens_on_its_own_misses_then_narrows_a_step_a_minute() {
        let steps = (MAX_NS / STEP_NS) as u32;
        const SLIP: i64 = 5_000_000;
        let replay: &[(bool, i64, u32, i64)] = &[
            (false, 0, 300, 0),
            (true, 0, 1, STEP_NS),
            (true, STEP_NS, 2, 3 * STEP_NS),
            // The compositor's own slips, the apply well ahead: nothing moves, not even the
            // clean count.
            (true, 3 * STEP_NS + SLIP, 50, 3 * STEP_NS),
            (true, 3 * STEP_NS, steps - 3, MAX_NS),
            (true, 0, 4, MAX_NS),
            (false, 0, DECAY_PRESENTS - 1, MAX_NS),
            (false, 0, 1, MAX_NS - STEP_NS),
            // A miss one present short of the decay starts the count again.
            (false, 0, DECAY_PRESENTS - 1, MAX_NS - STEP_NS),
            (true, 0, 1, MAX_NS),
            (false, 0, DECAY_PRESENTS - 1, MAX_NS),
            (true, MAX_NS + SLIP, 1, MAX_NS),
            (false, 0, 1, MAX_NS - STEP_NS),
            (false, 0, (steps - 1) * DECAY_PRESENTS, 0),
            (false, 0, 10 * DECAY_PRESENTS, 0),
        ];
        let mut m = ApplyMargin::new();
        for (i, &(missed, slack, n, want)) in replay.iter().enumerate() {
            for _ in 0..n {
                m.observe(missed, slack);
            }
            assert_eq!(m.ns(), want, "run {i}: {n} presents, missed={missed}");
        }
    }
}
