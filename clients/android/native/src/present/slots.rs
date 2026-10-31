//! The ASurfaceControl slot scheduler: each frame its own vsync slot on a grid phased on the
//! compositor's cycle.
//!
//! SurfaceFlinger defers a transaction only while `desiredPresentTime >= expectedPresentTime`, so
//! two frames applied in one period with targets before the next vsync latch together and only
//! the newer shows. Every frame therefore gets its own slot ([`pace_slot`]) and targets
//! `present − period/2`: ready for that slot, not the one before, half a period of tolerance each
//! way. Under `latency` a frame takes the slot after the last one while it can still make it, or
//! is early for it by less than the hold band; earlier than that it shares the last slot and
//! SurfaceFlinger keeps the newer. Under `smooth` the buffer bounds the hold. Until a present
//! fence phases the [`SlotClock`], with pacing off, or on a down-rated panel, every target is as
//! soon as possible.

use punktfunk_core::phase::{pace_slot, SlotClock};

use super::{Counters, PresentPriority, FALLBACK_PERIOD_NS};
use crate::apply_margin::ApplyMargin;

/// How early for the last slot's vsync a frame may be and still be held to the next slot
/// rather than shown in its predecessor's place. It covers what moves the aim between one frame
/// and the next: the grid's fence jitter, a margin step, a lead spike's decay. Not the link's
/// arrival jitter: that is the store's cushion, and a frame past it costs one repeat wherever
/// the band sits. Every millisecond here is a millisecond on each frame of a source running
/// faster than the panel, so the band is the noise and no more.
const HOLD_BAND_NS: i64 = 1_500_000;

/// The slot clock, the pacer's memory of the slots it assigned and observed, and the apply margin
/// the observed slots adapt.
pub(crate) struct Slots {
    clock: SlotClock,
    /// The source's frame interval: the latency cadence's step, in panel periods.
    frame_interval_ns: i64,
    /// The last slot assigned or observed; a held frame never shares it.
    last_slot: Option<i64>,
    /// Slots a frame may be held beyond the first reachable one: 0 under latency (1 with
    /// `debug.punktfunk.asc_hold_chain`, the one-period elastic queue), the buffer under smooth.
    max_ahead: i64,
    /// `debug.punktfunk.asc_hold_band_us` pins the hold band; `None` is [`HOLD_BAND_NS`].
    hold_band_pin: Option<i64>,
    apply_margin: ApplyMargin,
    /// Off (`debug.punktfunk.asc_pacing` = 0, or a `Skipping` verdict): as-soon-as-possible
    /// targets, two pending.
    pacing: bool,
    /// A present fence has been read at least once this session.
    fence_live: bool,
    last_latch_ns: i64,
    last_present_ns: i64,
}

impl Slots {
    /// `panel_hz` (the mode-table rate) seeds the clock; the present fences phase it.
    /// `frame_interval_ns` is the source's, the store's cadence. `hold`: the latency intent may
    /// hold a frame one slot past reachable, which on an exact-rate source chains into a
    /// permanent period of delay. `hold_band_ns` pins the hold band.
    pub(crate) fn new(
        panel_hz: i32,
        frame_interval_ns: i64,
        priority: PresentPriority,
        pacing: bool,
        hold: bool,
        hold_band_ns: Option<i64>,
    ) -> Slots {
        let max_ahead = match priority {
            PresentPriority::Latency => i64::from(hold),
            PresentPriority::Smooth { buffer } => (buffer as i64).clamp(1, 3),
        };
        Slots {
            clock: SlotClock::seeded(panel_hz),
            frame_interval_ns,
            last_slot: None,
            max_ahead,
            hold_band_pin: hold_band_ns,
            apply_margin: ApplyMargin::new(),
            pacing,
            fence_live: false,
            last_latch_ns: 0,
            last_present_ns: 0,
        }
    }

    /// When SurfaceFlinger latches a present aimed at `slot`: one lead before its vsync. An
    /// as-soon-as-possible present (no slot) latches within a period of `release_ns`.
    pub(crate) fn latch_ns_for(&self, slot: Option<i64>, release_ns: i64) -> i64 {
        match slot {
            Some(s) if self.clock.phased() => self.clock.present_ns(s) - self.clock.lead_ns(),
            _ => release_ns + self.period_ns(),
        }
    }

    /// Slot scheduling is live: pacing wanted and the clock has a real present behind it.
    fn paced(&self) -> bool {
        self.pacing && self.clock.phased()
    }

    /// Pacing wants slots and no present fence has phased the clock yet: the budget holds one.
    pub(crate) fn bootstrapping(&self) -> bool {
        self.pacing && !self.clock.phased()
    }

    /// The choreographer's panel period (0 = none yet): the compositor's own statement of the
    /// grid, which a source below the panel rate cannot bias the way present spacings can.
    pub(crate) fn set_period(&mut self, panel_period_ns: i64) {
        if panel_period_ns > 0 {
            self.clock.set_period(panel_period_ns);
        }
    }

    /// Pacing off for the rest of the run: the `Skipping` verdict.
    pub(crate) fn pacing_off(&mut self) {
        self.pacing = false;
    }

    /// The panel period, or one 120 Hz frame while none has been learned.
    pub(crate) fn period_ns(&self) -> i64 {
        match self.clock.period_ns() {
            0 => FALLBACK_PERIOD_NS,
            p => p,
        }
    }

    /// The clock's own period, 0 while unknown, for the 1 Hz line.
    pub(crate) fn clock_period_ns(&self) -> i64 {
        self.clock.period_ns()
    }

    pub(crate) fn lead_ns(&self) -> i64 {
        self.clock.lead_ns()
    }

    pub(crate) fn margin_ns(&self) -> i64 {
        self.apply_margin.ns()
    }

    pub(crate) fn fence_live(&self) -> bool {
        self.fence_live
    }

    /// The desired present time for a frame whose earliest presentable instant is `earliest`
    /// (`CLOCK_MONOTONIC`: its due time, or `now_ns` when that is past), and the slot it aims
    /// at. `0` = as soon as possible: the bootstrap before the first present sample, a device
    /// without present fences, pacing switched off, or a `downrated` panel, where
    /// SurfaceFlinger coalesces.
    pub(crate) fn next_target(
        &mut self,
        earliest: i64,
        now_ns: i64,
        downrated: bool,
        c: &mut Counters,
    ) -> (i64, Option<i64>) {
        if downrated || !self.paced() {
            return (0, None);
        }
        let ahead = self.clock.lead_ns() + self.apply_margin.ns();
        let reachable = self.clock.slot_after(earliest + ahead);
        let slot = match self.last_slot {
            Some(last) if self.max_ahead == 0 => {
                self.own_slot(last, reachable, earliest + ahead, now_ns + ahead)
            }
            last => pace_slot(reachable, last, self.max_ahead),
        };
        if slot > reachable {
            c.held += 1;
        }
        self.last_slot = Some(slot);
        (
            self.clock.present_ns(slot) - self.clock.period_ns() / 2,
            Some(slot),
        )
    }

    /// Latency: the slot one source interval after the last one (60 on 120: two slots on). The
    /// cadence estimate `ready_ns` (due + lead + margin) wobbles, so near a vsync the slot it
    /// names flips: two frames on one, none on the next. The cadence slot is kept while the
    /// frame can still make it from the apply now (`apply_ns` = now + lead + margin) or is early
    /// for the slot before it by less than the hold band. Earlier than the band it takes the
    /// reachable slot: the last one, on a source at the panel rate, which SurfaceFlinger keeps
    /// the newer of, one drop per beat of the source and panel clocks. A source interval that is
    /// no whole number of periods (24 on 60) has no cadence slot and takes the reachable one.
    fn own_slot(&self, last: i64, reachable: i64, ready_ns: i64, apply_ns: i64) -> i64 {
        let Some(step) = self.cadence_step() else {
            return reachable;
        };
        let next = last + step;
        if reachable > next {
            // Late for `next` on the cadence estimate; the apply is now, so the latch decides.
            if self.clock.slot_after(apply_ns) <= next {
                next
            } else {
                reachable
            }
        } else if reachable == next
            || self.clock.present_ns(next - 1) - ready_ns < self.hold_band_ns()
        {
            next
        } else {
            reachable
        }
    }

    /// The source interval in panel periods when that is a whole number within a tenth: 1 for a
    /// source at or above the panel rate, 2 for 60 on 120. `None` on a ratio like 24 on 60.
    fn cadence_step(&self) -> Option<i64> {
        let p = self.period_ns();
        if self.frame_interval_ns <= p {
            return Some(1);
        }
        let k = (self.frame_interval_ns + p / 2) / p;
        ((self.frame_interval_ns - k * p).abs() * 10 <= p).then_some(k)
    }

    /// How far a latch's vsync (one lead on) sits from the grid as the presents before it phased
    /// it: the compositor's own wake jitter, read before the clock takes the sample. `None`
    /// without a latch or a phased clock.
    pub(crate) fn latch_deviation_ns(&self, latch_ns: i64) -> Option<i64> {
        if latch_ns == 0 || !self.clock.phased() {
            return None;
        }
        let anchor = latch_ns + self.clock.lead_ns();
        Some(anchor - self.clock.present_ns(self.clock.slot_nearest(anchor)))
    }

    /// [`HOLD_BAND_NS`] or the pin, never past half a period.
    fn hold_band_ns(&self) -> i64 {
        self.hold_band_pin
            .unwrap_or(HOLD_BAND_NS)
            .min(self.clock.period_ns() / 2)
    }

    /// One completion's latch (0 = none: `unlatched`). Two transactions latched in one commit
    /// share the instant and SurfaceFlinger showed only the newer; the interval histogram sees
    /// the same pair as a 0-slot spacing.
    pub(crate) fn note_latch(&mut self, latch_ns: i64, c: &mut Counters) {
        if latch_ns == 0 {
            c.unlatched += 1;
            return;
        }
        if latch_ns == self.last_latch_ns {
            c.coalesced += 1;
        }
        self.last_latch_ns = latch_ns;
    }

    /// One frame on glass at `present_ns`, aimed at `slot`. With a `real` vsync (a present
    /// fence) phase the clock and correct the pacer by what happened; a latch stands in where no
    /// fence exists and never phases the clock, so such a device keeps the one-pending ASAP
    /// budget. `release_ns`: when the transaction was applied. Scores the glass interval either
    /// way, fence to fence in whole periods. Returns the phase error, µs, when there is one.
    pub(crate) fn on_present(
        &mut self,
        present_ns: i64,
        latch_ns: i64,
        slot: Option<i64>,
        real: bool,
        release_ns: i64,
        c: &mut Counters,
    ) -> Option<u64> {
        c.displays += 1;
        self.fence_live |= real;
        let mut phase_err_us = None;
        if present_ns == self.last_present_ns {
            c.intervals.note(0); // the coalesced pair's second half
        } else if present_ns > self.last_present_ns {
            if self.last_present_ns > 0 {
                let p = self.period_ns();
                c.intervals
                    .note((present_ns - self.last_present_ns + p / 2).div_euclid(p));
            }
            if real {
                phase_err_us = self.observe_present(present_ns, latch_ns, slot, release_ns, c);
            }
            self.last_present_ns = present_ns;
        }
        phase_err_us
    }

    /// A real vsync for a frame aimed at `assigned`: phase the clock on the compositor's cycle
    /// and correct the pacer when it latched the frame for a later vsync than aimed.
    fn observe_present(
        &mut self,
        present_ns: i64,
        latch_ns: i64,
        assigned: Option<i64>,
        release_ns: i64,
        c: &mut Counters,
    ) -> Option<u64> {
        let phase_err_us = assigned
            .filter(|_| self.clock.phased())
            .map(|a| (present_ns - self.clock.present_ns(a)).unsigned_abs() / 1000);
        // The compositor presented this frame on `taken`, one lead after its latch. Past the
        // aimed slot, the apply landed after its wakeup and the next frame must not aim at the
        // vsync it took. The fence trailing by refreshes is the display's own queue (a Shield TV:
        // one or two, for minutes): `phaseErr` shows it, and no aim of ours moves it.
        let taken = self.clock.observe(present_ns, latch_ns);
        let Some(assigned) = assigned else {
            // An as-soon-as-possible frame's vsync is taken all the same.
            self.last_slot = self.last_slot.max(Some(taken));
            return None;
        };
        let missed = taken > assigned;
        if missed {
            c.slot_miss += 1;
            self.last_slot = self.last_slot.max(Some(taken));
        }
        // How far ahead of the aimed slot's latch the apply was: the margin's evidence.
        let aimed_latch = self.clock.present_ns(assigned) - self.clock.lead_ns();
        self.apply_margin.observe(missed, aimed_latch - release_ns);
        phase_err_us
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame_interval_ns;
    use super::*;

    const P: i64 = 8_333_333; // 120 Hz
    const LEAD: i64 = 4_000_000; // present − latch
    const T0: i64 = 1_000_000_000_000; // the first fenced present, monotonic

    /// A scheduler for a 120 Hz source, phased by one fenced present at `T0` (slot 0).
    fn phased(priority: PresentPriority, hold: bool) -> (Slots, Counters) {
        phased_at(120, priority, hold)
    }

    /// [`phased`] for a source at `source_hz`.
    fn phased_at(source_hz: u32, priority: PresentPriority, hold: bool) -> (Slots, Counters) {
        let mut s = Slots::new(
            120,
            frame_interval_ns(source_hz),
            priority,
            true,
            hold,
            None,
        );
        let mut c = Counters::default();
        assert!(s.bootstrapping());
        assert_eq!(s.next_target(T0 - P, T0 - P, false, &mut c), (0, None));
        s.note_latch(T0 - LEAD, &mut c);
        assert_eq!(s.on_present(T0, T0 - LEAD, None, true, 0, &mut c), None);
        assert!(!s.bootstrapping() && s.fence_live());
        (s, c)
    }

    /// Aim a frame presentable at `earliest`, in hand just then; its target is half a period
    /// before its slot.
    fn aim(s: &mut Slots, c: &mut Counters, earliest: i64) -> i64 {
        aim_from(s, c, earliest, earliest)
    }

    /// Aim a frame due at `earliest` that has been in hand since `now`.
    fn aim_from(s: &mut Slots, c: &mut Counters, earliest: i64, now: i64) -> i64 {
        let (target, slot) = s.next_target(earliest, now, false, c);
        let slot = slot.expect("phased");
        assert_eq!(target, s.clock.present_ns(slot) - P / 2);
        slot
    }

    /// Land a frame aimed at `slot` on vsync `on`, fenced, as a device does, applied 0.1 ms
    /// before the aimed slot's latch: a miss is the apply's.
    fn land(s: &mut Slots, c: &mut Counters, slot: i64, on: i64) -> Option<u64> {
        let release = s.clock.present_ns(slot) - LEAD - 100_000;
        land_from(s, c, slot, on, release)
    }

    /// [`land`] for a transaction applied at `release`.
    fn land_from(s: &mut Slots, c: &mut Counters, slot: i64, on: i64, release: i64) -> Option<u64> {
        let present = s.clock.present_ns(on);
        s.note_latch(present - LEAD, c);
        s.on_present(present, present - LEAD, Some(slot), true, release, c)
    }

    #[test]
    fn quiet_120_on_120_gives_every_frame_its_own_slot_and_holds_none() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        for k in 0..120 {
            let slot = aim(&mut s, &mut c, T0 + k * P + P / 10);
            assert_eq!(slot, k + 1);
            assert_eq!(land(&mut s, &mut c, slot, slot), Some(0));
        }
        assert_eq!((c.held, c.slot_miss, c.coalesced), (0, 0, 0));
        assert_eq!((c.intervals.n0, c.intervals.n1), (0, 120));
        assert_eq!((c.intervals.mode(), c.intervals.judder_permille()), (1, 0));
        assert_eq!((c.displays, s.margin_ns(), s.lead_ns()), (121, 0, LEAD));
        assert_eq!((s.period_ns(), s.clock_period_ns()), (P, P));
    }

    #[test]
    fn two_frames_due_for_one_slot_share_it_under_latency_and_queue_under_smooth() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        let a = aim(&mut s, &mut c, T0 + P / 10);
        let b = aim(&mut s, &mut c, T0 + P / 4);
        assert_eq!(
            (a, b, c.held),
            (1, 1, 0),
            "shared: SurfaceFlinger keeps the newer"
        );
        land(&mut s, &mut c, a, a);
        land(&mut s, &mut c, b, b);
        assert_eq!((c.coalesced, c.intervals.n0, c.intervals.n1), (1, 1, 1));
        // Smooth with a one-frame buffer holds the second one slot; a third while two are
        // pending clamps to the buffer, re-targets the held slot, and the newer of that pair shows.
        let (mut s, mut c) = phased(PresentPriority::Smooth { buffer: 1 }, false);
        let a = aim(&mut s, &mut c, T0 + P / 10);
        let b = aim(&mut s, &mut c, T0 + P / 4);
        assert_eq!((a, b, c.held), (1, 2, 1));
        let third = aim(&mut s, &mut c, T0 + P / 2);
        assert_eq!((third, c.held), (b, 2));
    }

    #[test]
    fn the_hold_knob_restores_the_one_slot_chain() {
        let (mut s, mut c) = phased(PresentPriority::Latency, true);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 10), 1);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 4), 2, "the first hold");
        assert_eq!(aim(&mut s, &mut c, T0 + P + P / 4), 3, "and it chains");
        assert_eq!(c.held, 2);
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 10), 1);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 4), 1);
        assert_eq!(aim(&mut s, &mut c, T0 + P + P / 4), 2);
        assert_eq!(c.held, 0);
    }

    #[test]
    fn a_presents_latch_is_one_lead_before_its_slot_and_a_period_after_an_asap_apply() {
        let (s, _) = phased(PresentPriority::Latency, false);
        assert_eq!(s.latch_ns_for(Some(3), T0), s.clock.present_ns(3) - LEAD);
        assert_eq!(s.latch_ns_for(None, T0), T0 + P);
        let unphased = Slots::new(120, P, PresentPriority::Latency, true, false, None);
        assert_eq!(unphased.latch_ns_for(Some(3), T0), T0 + P);
        // A latch's distance from the grid, signed; nothing without a latch or a phased clock.
        let latch = s.clock.present_ns(3) - LEAD;
        assert_eq!(s.latch_deviation_ns(latch + 300_000), Some(300_000));
        assert_eq!(s.latch_deviation_ns(latch - 300_000), Some(-300_000));
        assert_eq!(s.latch_deviation_ns(0), None);
        assert_eq!(unphased.latch_deviation_ns(latch), None);
    }

    #[test]
    fn an_early_frame_inside_the_hold_band_is_held_and_beyond_it_shares() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 10), 1);
        // Ready 1 ms before slot 1's vsync: inside the band, held to slot 2, not shared.
        let inside = T0 + P - LEAD - 1_000_000;
        assert_eq!(aim(&mut s, &mut c, inside), 2);
        // Ready 2 ms before slot 2's vsync: beyond the band, it shares and the newer shows.
        assert_eq!(aim(&mut s, &mut c, T0 + 2 * P - LEAD - 2_000_000), 2);
        assert_eq!(c.held, 1);
        // A pinned band of zero is the plain shared slot.
        let mut pinned = Slots::new(120, P, PresentPriority::Latency, true, false, Some(0));
        pinned.on_present(T0, T0 - LEAD, None, true, 0, &mut c);
        assert_eq!(aim(&mut pinned, &mut c, T0 + P / 10), 1);
        assert_eq!(aim(&mut pinned, &mut c, inside), 1);
    }

    #[test]
    fn a_frame_late_on_the_cadence_but_in_hand_in_time_keeps_its_slot() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 10), 1);
        // Due half a millisecond too late for slot 2 by the estimate, in hand 2 ms before that.
        let due = T0 + 2 * P - LEAD + 500_000;
        assert_eq!(aim_from(&mut s, &mut c, due, due - 2_000_000), 2);
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        assert_eq!(aim(&mut s, &mut c, T0 + P / 10), 1);
        assert_eq!(aim(&mut s, &mut c, due), 3, "in hand only at its due time");
        assert_eq!(c.held, 0);
    }

    #[test]
    fn sixty_on_120_latency_presents_every_second_vsync() {
        // In hand at its due time or up to a panel period before it (the store's reach), the
        // due time anywhere in the period: every frame two slots after the last, never the
        // slot between (1,3,1,3).
        for in_hand in [0, 1_000_000, 6_000_000, P] {
            for phase in [P / 10, P / 2, 3 * P / 4] {
                let (mut s, mut c) = phased_at(60, PresentPriority::Latency, false);
                for k in 0..60 {
                    let due = T0 + (2 * k + 1) * P + phase;
                    let slot = aim_from(&mut s, &mut c, due, due - in_hand);
                    land(&mut s, &mut c, slot, slot);
                }
                let i = c.intervals;
                assert_eq!(
                    (c.held, c.coalesced, i.n0, i.n1),
                    (0, 0, 0, 0),
                    "in hand {in_hand} ns early, phase {phase}: {i:?}"
                );
                assert!(i.n2 >= 59, "{i:?}");
            }
        }
    }

    #[test]
    fn a_source_at_no_whole_number_of_periods_takes_the_reachable_slot() {
        // 48 on 120: two and a half periods a frame, so no cadence slot; the slot after the
        // cadence estimate, 3,3,2,3,2, however early the frame is in hand.
        let (mut s, mut c) = phased_at(48, PresentPriority::Latency, false);
        for k in 1..=20 {
            let due = T0 + k * 5 * P / 2 - LEAD + 100_000;
            let slot = aim_from(&mut s, &mut c, due, due - 6_000_000);
            assert_eq!(slot, s.clock.slot_after(due + LEAD), "frame {k}");
            land(&mut s, &mut c, slot, slot);
        }
        assert_eq!((c.held, c.coalesced, c.intervals.n1), (0, 0, 0));
        assert_eq!((c.intervals.n2, c.intervals.n3p), (9, 11));
    }

    #[test]
    fn a_display_that_shows_frames_late_leaves_the_lead_and_the_aim_alone() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        for k in 0..30 {
            let slot = aim(&mut s, &mut c, T0 + k * P + P / 10);
            assert_eq!(slot, k + 1, "frame {k}");
            // Latched one lead before its own vsync, on glass two refreshes later.
            let latch = s.clock.present_ns(slot) - LEAD;
            let present = s.clock.present_ns(slot + 2);
            s.note_latch(latch, &mut c);
            let err = s.on_present(present, latch, Some(slot), true, 0, &mut c);
            assert_eq!(err, Some((2 * P / 1000) as u64));
        }
        assert_eq!((c.slot_miss, s.margin_ns(), s.lead_ns()), (0, 0, LEAD));
        assert_eq!((c.held, c.intervals.n1, c.intervals.n3p), (0, 29, 1));
    }

    /// One beat of a 60 fps source against a 60 Hz panel, the source `drift_ns` off per frame
    /// (550 ns ≈ 33 ppm), the due time wobbling ±1.5 ms, the band's whole width, every frame
    /// on glass on its slot. Returns (slots shared, slots skipped, misses).
    fn beat(drift_ns: i64) -> (u32, u32, u64) {
        const P60: i64 = 16_666_667;
        let mut s = Slots::new(60, P60, PresentPriority::Latency, true, false, None);
        let mut c = Counters::default();
        // The Shield latches 0.3 ms after the vsync before the one it presents on.
        s.on_present(T0, T0 - P60 + 300_000, None, true, 0, &mut c);
        let lead = s.lead_ns();
        let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
        let (mut last, mut shared, mut skipped) = (None, 0, 0);
        for k in 1..=P60 / 550 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let wobble = (rng % 3_000_001) as i64 - 1_500_000;
            let arrival = T0 + k * (P60 + drift_ns);
            let due = arrival + 2_000_000 + wobble;
            let (_, slot) = s.next_target(due, arrival, false, &mut c);
            let slot = slot.expect("phased");
            match last {
                Some(l) if slot == l => shared += 1,
                Some(l) if slot > l + 1 => skipped += 1,
                _ => {}
            }
            last = Some(slot);
            let present = s.clock.present_ns(slot);
            s.note_latch(present - lead, &mut c);
            s.on_present(present, present - lead, Some(slot), true, 0, &mut c);
        }
        (shared, skipped, c.slot_miss)
    }

    #[test]
    fn a_source_a_hair_off_the_panel_rate_costs_one_slot_a_beat() {
        let (shared, skipped, misses) = beat(-550);
        assert!(
            shared <= 1 && skipped == 0,
            "faster: {shared} shared, {skipped} skipped"
        );
        assert_eq!(misses, 0);
        let (shared, skipped, misses) = beat(550);
        assert!(
            shared == 0 && skipped <= 1,
            "slower: {shared} shared, {skipped} skipped"
        );
        assert_eq!(misses, 0);
    }

    #[test]
    fn an_unphased_clock_or_pacing_off_aims_as_soon_as_possible() {
        let mut c = Counters::default();
        let mut s = Slots::new(120, P, PresentPriority::Latency, true, false, None);
        // A latch stands in for a vsync but never phases the clock.
        s.on_present(T0, T0 - LEAD, None, false, 0, &mut c);
        assert!(s.bootstrapping() && !s.fence_live());
        assert_eq!(s.next_target(T0, T0, false, &mut c), (0, None));
        let mut off = Slots::new(120, P, PresentPriority::Latency, false, false, None);
        off.on_present(T0, T0 - LEAD, None, true, 0, &mut c);
        assert!(
            !off.bootstrapping(),
            "pacing off: two pending from the start"
        );
        assert_eq!(off.next_target(T0, T0, false, &mut c), (0, None));
        assert_eq!(c.held, 0);
        // No seed: the fallback period until the choreographer states one; 0 states nothing.
        let mut unseeded = Slots::new(0, P, PresentPriority::Latency, true, false, None);
        unseeded.set_period(0);
        assert_eq!(
            (unseeded.period_ns(), unseeded.clock_period_ns()),
            (FALLBACK_PERIOD_NS, 0)
        );
        unseeded.set_period(2 * P);
        assert_eq!(unseeded.period_ns(), 2 * P);
    }

    #[test]
    fn a_downrated_panel_or_a_skipping_verdict_aims_as_soon_as_possible() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        let t = T0 + P / 10;
        assert_eq!(s.next_target(t, t, true, &mut c), (0, None));
        assert_eq!(aim(&mut s, &mut c, t), 1, "pacing resumes on the grid");
        s.pacing_off();
        assert!(!s.bootstrapping(), "two pending");
        let t = T0 + P + P / 10;
        assert_eq!(s.next_target(t, t, false, &mut c), (0, None));
        assert_eq!(c.held, 0);
    }

    #[test]
    fn sixty_on_120_smooth_presents_every_second_vsync() {
        let (mut s, mut c) = phased(PresentPriority::Smooth { buffer: 2 }, false);
        for k in 0..60 {
            let slot = aim(&mut s, &mut c, T0 + (2 * k + 1) * P + P / 10);
            assert_eq!(slot, 2 * k + 2);
            land(&mut s, &mut c, slot, slot);
        }
        assert_eq!((c.held, c.intervals.n2, c.intervals.mode()), (0, 60, 2));
        assert_eq!(c.intervals.judder_permille(), 0);
    }

    #[test]
    fn a_frame_that_lands_a_slot_late_widens_the_margin_and_frees_its_slot() {
        let (mut s, mut c) = phased(PresentPriority::Smooth { buffer: 2 }, false);
        let a = aim(&mut s, &mut c, T0 + P / 10);
        assert_eq!(land(&mut s, &mut c, a, a + 1), Some((P / 1000) as u64));
        assert_eq!((c.slot_miss, s.margin_ns()), (1, 500_000));
        // Reachable is the slot the late frame took; the pacer moves past it.
        let next = aim(&mut s, &mut c, T0 + P + P / 10);
        assert_eq!(next, a + 2, "never the slot the late frame took");
    }

    #[test]
    fn a_compositor_slip_frees_its_slot_but_leaves_the_margin_alone() {
        let (mut s, mut c) = phased(PresentPriority::Latency, false);
        let a = aim(&mut s, &mut c, T0 + P / 10);
        // Applied 5 ms before the aimed latch and still latched a cycle late: the compositor's.
        let release = s.clock.present_ns(a) - LEAD - 5_000_000;
        assert_eq!(
            land_from(&mut s, &mut c, a, a + 1, release),
            Some((P / 1000) as u64)
        );
        assert_eq!((c.slot_miss, s.margin_ns()), (1, 0));
        // The next frame is early beyond the band for the slot after the one the slip took:
        // under latency it shares that slot and the newer shows.
        assert_eq!(aim(&mut s, &mut c, T0 + P + P / 10), a + 1);
    }

    #[test]
    fn latch_only_presents_score_the_spacing_off_the_period() {
        let mut c = Counters::default();
        let mut s = Slots::new(120, P, PresentPriority::Latency, true, false, None);
        s.note_latch(0, &mut c);
        assert_eq!(c.unlatched, 1);
        for k in [0, 1, 3, 3] {
            s.on_present(T0 + k * P, T0 + k * P - LEAD, None, false, 0, &mut c);
        }
        let i = c.intervals;
        assert_eq!((i.n0, i.n1, i.n2, i.n3p), (1, 1, 1, 0));
    }
}
