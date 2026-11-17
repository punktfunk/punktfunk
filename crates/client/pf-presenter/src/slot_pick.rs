//! The paced drain's slot pick: a frame is aimed at the first latch at least a lead past
//! its due time — one frame per slot — and goes out that lead ahead of it. The present
//! then sits at one phase of the panel's grid whatever phase the source clock drifts
//! to; a present at the due time lands wherever that clock puts it, and at the
//! compositor's deadline every frame is a coin toss. Pure; `PaceState::step` hands it
//! the latch clock and the frame store.
//!
//! The lead is half a period plus a margin: as far from the previous latch as from the
//! next. Missed slots widen the margin; frames landing a slot before their aim narrow it.
//!
//! Pinned by the table test below.

/// Slot-pick margin: 0.5 ms step, 8 ms cap (KWin on NVIDIA wants a period and a half).
pub(crate) const MARGIN_STEP_NS: u64 = 500_000;
pub(crate) const MARGIN_MAX_NS: u64 = 8_000_000;

#[derive(Default)]
pub(crate) struct SlotPick {
    /// Lead past the half period, widened by missed slots.
    margin_ns: u64,
    /// The latch slot the last vended frame was aimed at.
    last_slot_ns: u64,
}

/// What a pick did with the store's front frame.
pub(crate) enum Pick<T> {
    Vend(T),
    /// Not yet: the frame goes out at this instant.
    At(u64),
    /// Nothing waits, or the store kept it.
    Empty,
}

impl SlotPick {
    pub(crate) fn margin_ns(&self) -> u64 {
        self.margin_ns
    }

    /// The latch the last vended frame was aimed at; 0 before the first.
    pub(crate) fn last_target_ns(&self) -> u64 {
        self.last_slot_ns
    }

    /// How far ahead of its latch a frame goes out: half a period plus the margin. A
    /// learned compositor need could cut that to a few ms on a fast compositor.
    pub(crate) fn lead_ns(&self, period_ns: u64) -> u64 {
        period_ns / 2 + self.margin_ns
    }

    /// The latch a frame due at `due_ns` is aimed at: the first at least a lead after
    /// it, or the slot after the last vended one — two frames never share a slot.
    fn target_ns(&self, due_ns: u64, period_ns: u64, slot_after: impl FnOnce(u64) -> u64) -> u64 {
        let t = slot_after(
            due_ns
                .saturating_add(self.lead_ns(period_ns))
                .saturating_sub(1),
        );
        if t <= self.last_slot_ns {
            self.last_slot_ns + period_ns
        } else {
            t
        }
    }

    /// Vend the front frame (due at `front_due_ns`) a lead ahead of its aim, which is
    /// never before its due time. `slot_after` is the latch clock's first slot after a
    /// time; `take` pops the store.
    pub(crate) fn pick<T>(
        &mut self,
        now_ns: u64,
        period_ns: u64,
        front_due_ns: Option<u64>,
        slot_after: impl FnOnce(u64) -> u64,
        take: impl FnOnce() -> Option<T>,
    ) -> Pick<T> {
        let Some(due) = front_due_ns else {
            return Pick::Empty;
        };
        let target = self.target_ns(due, period_ns, slot_after);
        let present = target.saturating_sub(self.lead_ns(period_ns));
        if now_ns < present {
            return Pick::At(present);
        }
        match take() {
            Some(frame) => {
                self.last_slot_ns = target;
                Pick::Vend(frame)
            }
            None => Pick::Empty,
        }
    }

    /// A vended frame went back to the store (the glass gate parked it, the presenter was
    /// busy): its slot is free again. `mark` is [`Self::last_target_ns`] from before the
    /// pick, so the retry aims at the same latch instead of one period later.
    pub(crate) fn rewind(&mut self, mark: u64) {
        self.last_slot_ns = mark;
    }

    /// A window closed: `misses` frames landed past their aimed latch — or were replaced
    /// before a latch took them, which is how a missed latch reads under MAILBOX — and
    /// `early` a slot before it. Two or more misses, outnumbering the early landings two to
    /// one, widen the margin a step; early landings outnumbering the misses two to one narrow
    /// it. A window split between the two says the stamps straddle a latch; nothing moves.
    /// `true` when it widened.
    pub(crate) fn note_window(&mut self, misses: u32, early: u32) -> bool {
        if misses > 1 && misses > early.saturating_mul(2) {
            let before = self.margin_ns;
            self.margin_ns = (self.margin_ns + MARGIN_STEP_NS).min(MARGIN_MAX_NS);
            return self.margin_ns > before;
        }
        if early > misses.saturating_mul(2) {
            self.margin_ns = self.margin_ns.saturating_sub(MARGIN_STEP_NS);
        }
        false
    }

    /// A new panel: the margin was sized by the old one's compositor.
    pub(crate) fn reset_margin(&mut self) {
        self.margin_ns = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const MS: u64 = 1_000_000;
    /// A 100 Hz grid anchored at 0, as `LatchClock::next_slot_after` reckons it.
    const PERIOD: u64 = 10 * MS;

    fn slot_after(t: u64) -> u64 {
        (t / PERIOD + 1) * PERIOD
    }

    fn vended<T>(p: Pick<T>) -> Option<T> {
        match p {
            Pick::Vend(f) => Some(f),
            _ => None,
        }
    }

    fn at<T>(p: Pick<T>) -> Option<u64> {
        match p {
            Pick::At(t) => Some(t),
            _ => None,
        }
    }

    /// A frame goes out a lead ahead of its aim — half a period before the latch — and
    /// never before its due time; until then the pick names the instant.
    #[test]
    fn a_frame_goes_out_a_lead_ahead_of_its_aim() {
        let mut s = SlotPick::default();
        let mut store: VecDeque<u64> = [12 * MS, 29 * MS].into();
        // (now ms, instant ms the pick names, vended due ms)
        for (now, want_at, want) in [
            (0, Some(15), None),
            (12, Some(15), None),
            (15, None, Some(12)),
            (20, Some(35), None),
            (34, Some(35), None),
            (35, None, Some(29)),
            (40, None, None),
        ] {
            let front = store.front().copied();
            let got = s.pick(now * MS, PERIOD, front, slot_after, || store.pop_front());
            let (got_at, got) = match got {
                Pick::Vend(f) => (None, Some(f)),
                Pick::At(t) => (Some(t), None),
                Pick::Empty => (None, None),
            };
            assert_eq!(got_at, want_at.map(|t: u64| t * MS), "pick at {now} ms");
            assert_eq!(got, want.map(|d: u64| d * MS), "pick at {now} ms");
        }
    }

    /// The aim is the first latch at least a lead after the due time, and two frames
    /// never share one: the second aims a slot later.
    #[test]
    fn the_aim_is_the_first_latch_a_lead_after_due_one_per_slot() {
        let mut s = SlotPick::default();
        assert_eq!(s.lead_ns(PERIOD), PERIOD / 2, "half a period to start with");
        let mut store: VecDeque<u64> = [12 * MS, 13 * MS, 45 * MS].into();
        let mut pick = |s: &mut SlotPick, now: u64| {
            vended(
                s.pick(now * MS, PERIOD, store.front().copied(), slot_after, || {
                    store.pop_front()
                }),
            )
        };
        assert_eq!(pick(&mut s, 15), Some(12 * MS));
        assert_eq!(
            s.last_target_ns(),
            20 * MS,
            "due 12 + lead 5 = 17 → the latch at 20"
        );
        assert_eq!(pick(&mut s, 24), None, "the next slot's frame waits for 25");
        assert_eq!(pick(&mut s, 25), Some(13 * MS));
        assert_eq!(
            s.last_target_ns(),
            30 * MS,
            "the slot after the one just taken"
        );
        assert_eq!(pick(&mut s, 45), Some(45 * MS));
        assert_eq!(
            s.last_target_ns(),
            50 * MS,
            "due 45 + 5 = 50; exactly on a latch counts"
        );
    }

    /// Misses widen the margin a step per window up to the cap; early landings narrow it
    /// down to nothing; a window split between the two leaves it.
    #[test]
    fn the_margin_follows_the_window() {
        let mut s = SlotPick::default();
        // (misses, early, widened, margin after) in µs
        let table: &[(u32, u32, bool, u64)] = &[
            (0, 0, false, 0),
            (1, 0, false, 0),
            (2, 0, true, 500),
            (9, 4, true, 1_000),
            (5, 3, false, 1_000),
            (0, 1, false, 500),
            (0, 1, false, 0),
            (0, 5, false, 0),
        ];
        for &(misses, early, widened, margin_us) in table {
            assert_eq!(s.note_window(misses, early), widened, "{misses}/{early}");
            assert_eq!(s.margin_ns(), margin_us * 1000, "{misses}/{early}");
        }
        for _ in 0..20 {
            s.note_window(3, 0);
        }
        assert_eq!(s.margin_ns(), MARGIN_MAX_NS);
        assert!(!s.note_window(3, 0), "capped");
        assert_eq!(s.lead_ns(PERIOD), PERIOD / 2 + MARGIN_MAX_NS);
        s.reset_margin();
        assert_eq!(s.lead_ns(PERIOD), PERIOD / 2);
    }

    /// A frame the gate parks is picked again on the next pass; given its mark back, the
    /// retry aims at the same latch, not one period later per retry.
    #[test]
    fn a_parked_frame_keeps_its_aim_across_retries() {
        let mut s = SlotPick::default();
        let mut store: VecDeque<u64> = [12 * MS].into();
        for retry in 0..3u64 {
            let mark = s.last_target_ns();
            let now = 15 * MS + retry * 200_000;
            let got = vended(s.pick(now, PERIOD, store.front().copied(), slot_after, || {
                store.pop_front()
            }));
            assert_eq!(got, Some(12 * MS), "retry {retry} vends the frame");
            assert_eq!(
                s.last_target_ns(),
                20 * MS,
                "retry {retry} aims at the latch at 20"
            );
            store.push_front(12 * MS);
            s.rewind(mark);
        }
        assert_eq!(
            at(s.pick(14 * MS, PERIOD, Some(12 * MS), slot_after, || None::<u64>)),
            Some(15 * MS),
            "rewound, the frame still goes out at 15"
        );
    }
}
