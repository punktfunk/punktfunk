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

    /// One present with a real vsync. `missed`: it landed after the slot it was aimed at.
    pub(crate) fn observe(&mut self, missed: bool) {
        if missed {
            self.clean = 0;
            self.ns = (self.ns + STEP_NS).min(MAX_NS);
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

    /// A session's presents replayed in runs: (missed, how many, the margin after the run).
    #[test]
    fn a_replayed_session_widens_on_misses_then_narrows_a_step_a_minute() {
        let steps = (MAX_NS / STEP_NS) as u32;
        let replay: &[(bool, u32, i64)] = &[
            (false, 300, 0),
            (true, 1, STEP_NS),
            (true, 2, 3 * STEP_NS),
            (true, steps - 3, MAX_NS),
            (true, 4, MAX_NS),
            (false, DECAY_PRESENTS - 1, MAX_NS),
            (false, 1, MAX_NS - STEP_NS),
            // A miss one present short of the decay starts the count again.
            (false, DECAY_PRESENTS - 1, MAX_NS - STEP_NS),
            (true, 1, MAX_NS),
            (false, DECAY_PRESENTS - 1, MAX_NS),
            (false, (steps - 1) * DECAY_PRESENTS + 1, 0),
            (false, 10 * DECAY_PRESENTS, 0),
        ];
        let mut m = ApplyMargin::new();
        for (i, &(missed, n, want)) in replay.iter().enumerate() {
            for _ in 0..n {
                m.observe(missed);
            }
            assert_eq!(m.ns(), want, "run {i}: {n} presents, missed={missed}");
        }
    }
}
