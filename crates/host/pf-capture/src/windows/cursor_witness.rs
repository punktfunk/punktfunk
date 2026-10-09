//! Damage witness for the recovery classifier: did the pointer travel through a gap?
//!
//! A wedged-but-alive display and an idle desktop both just stop delivering frames. Cursor
//! travel is the evidence that separates them, so this accumulator decides whether a silent
//! stretch escalates into a reset. Pure over a `(now, position)` pair, so the rule
//! is a table test rather than a soak — the capturer owns the `GetCursorPos` call and the
//! classifier owns the verdict ([`recovery::Inputs::cursor_gap_px`]).
//!
//! `GetCursorPos` is global, not per-display: a delta of 0 proves the cursor sat still
//! EVERYWHERE, while a delta > 0 on a parallel-displays host may be a sibling's motion. Only
//! the first direction is strict, which is why demotion is safe and promotion is not.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::time::{Duration, Instant};

/// Two user32 reads; 8 ms so a ≥150 ms hole still gets many samples.
pub(crate) const SAMPLE_INTERVAL: Duration = Duration::from_millis(8);

/// Accumulated pointer travel since the last fresh frame, plus the one-call lag that keeps a
/// stall-ending frame's own move out of the gap it ended.
pub(crate) struct CursorWitness {
    last: Option<(i32, i32)>,
    gap_px: u32,
    pending_px: u32,
    sampled_at: Instant,
}

impl CursorWitness {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last: None,
            gap_px: 0,
            pending_px: 0,
            sampled_at: now,
        }
    }

    /// Fold the PREVIOUS call's pending delta into the gap, then take a fresh rate-limited
    /// sample into the pending slot.
    ///
    /// A fresh frame zeroes the pending ([`Self::fresh_frame`]), so whatever survives to be
    /// folded here belongs to the gap. `pos` is called only when the rate limit allows a
    /// sample, so the caller's `GetCursorPos` is skipped rather than discarded; `None` from it
    /// is a failed read, which leaves the accumulator alone.
    pub(crate) fn sample(&mut self, now: Instant, pos: impl FnOnce() -> Option<(i32, i32)>) {
        self.gap_px = self.gap_px.saturating_add(self.pending_px);
        self.pending_px = 0;
        if now.duration_since(self.sampled_at) < SAMPLE_INTERVAL {
            return;
        }
        self.sampled_at = now;
        if let Some((x, y)) = pos() {
            if let Some((px, py)) = self.last {
                self.pending_px = x.abs_diff(px).saturating_add(y.abs_diff(py));
            }
            self.last = Some((x, y));
        }
    }

    /// The gap accumulator the classifier reads. This call's pending — a stall-ending frame's
    /// own move — is still unfolded and so is not in it.
    pub(crate) fn gap_px(&self) -> u32 {
        self.gap_px
    }

    /// The same number for the stall report, or `None` before the first successful read: an
    /// unsampled witness must not report a confident zero.
    pub(crate) fn moved_px(&self) -> Option<u32> {
        self.last.map(|_| self.gap_px)
    }

    /// A fresh frame ended the gap. The pending sample is that frame's own move — discarded,
    /// never folded.
    pub(crate) fn fresh_frame(&mut self) {
        self.gap_px = 0;
        self.pending_px = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the witness at `SAMPLE_INTERVAL` so every call takes a sample.
    fn walk(w: &mut CursorWitness, t: &mut Instant, path: &[(i32, i32)]) {
        for &p in path {
            *t += SAMPLE_INTERVAL;
            w.sample(*t, || Some(p));
        }
    }

    /// The whole point of the one-call lag: the move that ENDS a stall belongs to the frame it
    /// produced, not to the gap it closed. Fold it and every stall reports phantom damage.
    #[test]
    fn the_stall_ending_move_stays_out_of_the_gap_it_ended() {
        let mut t = Instant::now();
        let mut w = CursorWitness::new(t);
        walk(&mut w, &mut t, &[(0, 0), (0, 0), (0, 0)]);
        assert_eq!(w.gap_px(), 0, "a still pointer damages nothing");

        // The move that wakes the desktop lands in pending, not in the gap.
        walk(&mut w, &mut t, &[(30, 40)]);
        assert_eq!(w.gap_px(), 0, "still unfolded — this call's own move");
        assert_eq!(w.moved_px(), Some(0), "and the report agrees");
        w.fresh_frame();

        // Folding it now would have charged the gap 70 px it never had.
        walk(&mut w, &mut t, &[(30, 40)]);
        assert_eq!(w.gap_px(), 0);
    }

    /// Travel through a gap accumulates across calls — this is the evidence that turns a silent
    /// stretch into a reset instead of leaving it read as an idle desktop.
    #[test]
    fn travel_through_a_gap_accumulates_until_a_fresh_frame() {
        let mut t = Instant::now();
        let mut w = CursorWitness::new(t);
        walk(&mut w, &mut t, &[(0, 0), (10, 0), (10, 5), (10, 5)]);
        // 10 px across, 5 down; the last still call folded the final delta of 0.
        assert_eq!(w.gap_px(), 15);
        assert_eq!(w.moved_px(), Some(15));

        w.fresh_frame();
        assert_eq!(w.gap_px(), 0, "a fresh frame ends the gap");
        assert_eq!(w.moved_px(), Some(0), "the anchor survives it");
    }

    /// The rate limit is what keeps this off the capture thread's hot path — and a skipped call
    /// must not silently drop travel: the next sample measures from the last anchor.
    #[test]
    fn a_rate_limited_call_takes_no_sample_and_loses_no_travel() {
        let mut t = Instant::now();
        let mut w = CursorWitness::new(t);
        walk(&mut w, &mut t, &[(0, 0)]);

        let mut asked = 0;
        t += SAMPLE_INTERVAL / 4;
        w.sample(t, || {
            asked += 1;
            Some((100, 0))
        });
        assert_eq!(asked, 0, "inside the interval the caller is never asked");

        walk(&mut w, &mut t, &[(100, 0), (100, 0)]);
        assert_eq!(
            w.gap_px(),
            100,
            "measured from the anchor, not from nothing"
        );
    }

    /// A failed `GetCursorPos` leaves the anchor alone rather than re-anchoring at a guess.
    #[test]
    fn a_failed_read_neither_moves_the_anchor_nor_the_gap() {
        let mut t = Instant::now();
        let mut w = CursorWitness::new(t);
        walk(&mut w, &mut t, &[(0, 0)]);

        t += SAMPLE_INTERVAL;
        w.sample(t, || None);
        walk(&mut w, &mut t, &[(7, 0), (7, 0)]);
        assert_eq!(w.gap_px(), 7);
    }

    /// Before the first successful read there is no evidence either way, so the report says so.
    #[test]
    fn an_unsampled_witness_reports_nothing_not_zero() {
        let t = Instant::now();
        let mut w = CursorWitness::new(t);
        assert_eq!(w.moved_px(), None);
        w.sample(t + SAMPLE_INTERVAL, || None);
        assert_eq!(w.moved_px(), None, "a failed read is still no evidence");
        w.sample(t + SAMPLE_INTERVAL * 2, || Some((1, 1)));
        assert_eq!(w.moved_px(), Some(0));
    }
}
