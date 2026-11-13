//! The two-slot LTR mirror AMF and QSV keep beside the hardware's long-term slots: the wire
//! index in each slot, which of those a loss tainted, the next round-robin mark, the kept pair
//! and a queued force. Policy is [`crate::rfi`]; how a mark or a force reaches the hardware stays in each
//! backend.

use crate::{rfi, Acked};

/// User LTR slots. AMD and Intel both take 2; rotating them keeps a pair, so a loss can
/// re-reference the newest mark before it.
pub const NUM_LTR_SLOTS: usize = 2;

/// AMF and QSV have no wave: a loss the mirror cannot anchor is the caller's keyframe.
const NO_WAVE: rfi::RecoveryPolicy = rfi::RecoveryPolicy {
    cycle: None,
    mid_wave: rfi::MidWave::Queue,
    abandons: true,
};

/// One frame's LTR action, decided before the surface is built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LtrStep {
    /// Mark this frame long-term into the slot.
    pub mark_slot: Option<usize>,
    /// Re-reference `(slot, wire index)`; the AU is a recovery anchor.
    pub force: Option<(usize, i64)>,
    /// The force follows the client's confirmations, not an RFI.
    pub acked: bool,
}

/// What the hardware's long-term slots hold, in wire indexes (`submit_indexed`), so they
/// compare against the client's loss reports across rebuilds.
#[derive(Debug, Default)]
pub struct LtrMirror {
    /// The wire index in each slot, `None` when never marked. A tainted slot keeps its index:
    /// QSV's RejectedRefList still names it.
    pub slots: [Option<i64>; NUM_LTR_SLOTS],
    /// The slot's picture sat in a loss the client reported: never forced. A re-mark or an
    /// IDR clears it.
    tainted: [bool; NUM_LTR_SLOTS],
    next: usize,
    /// The pin and candidate marks follow while the client confirms ([`rfi::Kept`]).
    kept: rfi::Kept,
    /// The slot the next frame force-references. That frame's step consumes it.
    pending_force: Option<usize>,
}

impl LtrMirror {
    /// The slot holds a picture a frame may reference.
    pub fn trusted(&self, slot: usize) -> bool {
        self.slots[slot].is_some() && !self.tainted[slot]
    }

    /// Each slot's wire index, `None` where untrusted.
    fn trusted_view(&self) -> [Option<i64>; NUM_LTR_SLOTS] {
        std::array::from_fn(|s| self.slots[s].filter(|_| !self.tainted[s]))
    }

    /// A re-mark replaces the slot's picture, so the tainted one leaves the DPB.
    fn mark(&mut self, slot: usize, cur_idx: i64) {
        self.slots[slot] = Some(cur_idx);
        self.tainted[slot] = false;
        self.next = (slot + 1) % NUM_LTR_SLOTS;
    }

    /// This frame's mark and force, ranked by [`rfi::plan_frame`]. An IDR empties the mirror
    /// and drops a queued force. A queued force resolves now: an untrusted slot ships a plain P
    /// with no recovery anchor, because that tag lifts the client's post-loss freeze. With
    /// `drop_unforced` the force leaves only its own slot trusted (AMF's force discards the
    /// rest); without, the other marks stay trusted (QSV rejects them for this frame only). A
    /// force takes the frame's mark, which would overwrite it.
    ///
    /// Under a `floor` each frame forces the newest confirmed slot ([`rfi::ltr_acked_step`]).
    /// With `keep` the driver keeps the other slot and the frame marks only a free one;
    /// without, the frame marks the other slot. Otherwise a frame the kept pair takes as its
    /// candidate marks, or every `interval` frames while the client sends no `confirmed`;
    /// first an untrusted slot, never the pin's.
    #[allow(clippy::too_many_arguments)]
    pub fn step(
        &mut self,
        forced: bool,
        cur_idx: i64,
        interval: i64,
        confirmed: Option<&Acked>,
        floor: Option<Acked>,
        keep: bool,
        drop_unforced: bool,
    ) -> LtrStep {
        if forced {
            *self = Self::default();
        }
        let queued = self.pending_force.take().filter(|&s| self.trusted(s));
        let pending = rfi::Pending {
            idr: forced,
            anchor: queued.is_some(),
            acked: floor.is_some_and(|f| {
                rfi::ltr_acked_step(&self.trusted_view(), &f, cur_idx, self.next)
                    .1
                    .is_some()
            }),
            loss: false,
        };
        let refs = rfi::plan_frame(rfi::Recovery::default(), pending, &NO_WAVE).refs;
        let mut step = LtrStep::default();
        match (refs, queued, floor) {
            (rfi::FrameRef::Anchor, Some(slot), _) => {
                step.force = self.slots[slot].map(|w| (slot, w));
                if drop_unforced {
                    for (s, tainted) in self.tainted.iter_mut().enumerate() {
                        *tainted |= s != slot;
                    }
                }
            }
            // A floor with no confirmed slot resident still places the frame's mark.
            (rfi::FrameRef::Acked | rfi::FrameRef::Chain, _, Some(floor)) => {
                return self.step_acked(&floor, cur_idx, keep);
            }
            _ if forced || self.marks(cur_idx, interval, confirmed) => {
                let trusted: [bool; NUM_LTR_SLOTS] = std::array::from_fn(|s| self.trusted(s));
                let slot = rfi::mark_slot(&trusted, self.next);
                let pinned = trusted[slot] && self.slots[slot] == self.kept.pin;
                let slot = if pinned {
                    (slot + 1) % NUM_LTR_SLOTS
                } else {
                    slot
                };
                self.mark(slot, cur_idx);
                if forced {
                    // The IDR is the next pin, so a late loss right after it still finds it.
                    self.kept.candidate = Some(cur_idx);
                }
                step.mark_slot = Some(slot);
            }
            _ => {}
        }
        step
    }

    /// Whether frame `cur_idx` takes a mark outside confirmed references: as the kept
    /// candidate while the client confirms, else every `interval` frames. A pair that starts
    /// from nothing pins the newest mark the client confirmed.
    fn marks(&mut self, cur_idx: i64, interval: i64, confirmed: Option<&Acked>) -> bool {
        let Some(acked) = confirmed else {
            self.kept = rfi::Kept::default();
            return cur_idx % interval == 0;
        };
        let view = self.trusted_view();
        if self.kept == rfi::Kept::default() {
            self.kept.pin = view
                .iter()
                .flatten()
                .copied()
                .filter(|&w| acked.contains(w))
                .max();
        }
        self.kept
            .step(cur_idx, true, Some(acked), |w| view.contains(&Some(w)))
    }

    /// [`Self::step`] under confirmed references.
    fn step_acked(&mut self, floor: &Acked, cur_idx: i64, keep: bool) -> LtrStep {
        let (mark, force) = rfi::ltr_acked_step(&self.trusted_view(), floor, cur_idx, self.next);
        let mark = match force {
            Some((f, _)) if !keep => Some((f + 1) % NUM_LTR_SLOTS),
            _ => mark,
        };
        if let Some(m) = mark {
            self.mark(m, cur_idx);
        }
        LtrStep {
            mark_slot: mark,
            force,
            acked: true,
        }
    }

    /// A loss from `first` ([`rfi::plan_slot_recovery`]): taint every trusted slot from it on
    /// and queue a force of the newest trusted slot before it. `None` queues nothing. A taint
    /// outlives later losses.
    pub fn invalidate(&mut self, first: i64, floor: Option<&Acked>) -> Option<(usize, i64)> {
        let view: Vec<(usize, i64)> = self
            .trusted_view()
            .iter()
            .enumerate()
            .filter_map(|(s, w)| w.map(|w| (s, w)))
            .collect();
        let plan = rfi::plan_slot_recovery(&view, first, floor);
        for (s, tainted) in self.tainted.iter_mut().enumerate() {
            *tainted |= plan.tainted & (1 << s) != 0;
        }
        self.pending_force = plan.anchor.map(|(s, _)| s);
        plan.anchor
    }

    /// Taint every slot and drop a queued force, which would otherwise re-reference a slot
    /// this just distrusted. Returns how many slots were trusted.
    pub fn distrust(&mut self) -> usize {
        let live = (0..NUM_LTR_SLOTS).filter(|&s| self.trusted(s)).count();
        self.tainted = [true; NUM_LTR_SLOTS];
        self.pending_force = None;
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AMF's step at interval 8: a queued force drops the other slot.
    fn amf(m: &mut LtrMirror, cur: i64, floor: Option<Acked>, keep: bool) -> LtrStep {
        m.step(false, cur, 8, None, floor, keep, true)
    }

    fn floor(last: i64) -> Option<Acked> {
        Some(Acked { last, mask: 0xffff })
    }

    /// While the client confirms, marks follow the kept pair: a loss reported 40 frames late
    /// still forces the confirmed pin, where 8-frame marks kept nothing older than the loss.
    #[test]
    fn a_late_loss_forces_the_kept_pin() {
        let lost = 100;
        let run = |confirms: bool| {
            let mut m = LtrMirror::default();
            for cur in 0..lost + 40 {
                let acked = Acked {
                    last: (cur - 4).min(lost - 1),
                    mask: 0xffff,
                };
                let confirmed = confirms.then_some(&acked);
                m.step(cur == 0, cur, 8, confirmed, None, false, true);
            }
            m.invalidate(lost, None)
        };
        let (_, pin) = run(true).expect("the confirmed pin answers");
        assert!(pin < lost, "{pin}");
        assert_eq!(run(false), None);
    }

    /// Frame 1 is lost and reported 20 frames late; the client confirms from frame 4 on, frame 0
    /// only. The IDR is the confirmed pin and answers.
    #[test]
    fn a_late_loss_after_the_opening_idr_forces_it() {
        let mut m = LtrMirror::default();
        let acked = Acked { last: 0, mask: 0 };
        for cur in 0..21 {
            let confirmed = (cur >= 4).then_some(&acked);
            m.step(cur == 0, cur, 30, confirmed, None, false, true);
        }
        assert_eq!(m.invalidate(1, None), Some((0, 0)));
    }

    /// Confirmations flow, an IDR at 40, and its first P is lost; the client's confirmations
    /// stall before the IDR. The IDR answers the late report.
    #[test]
    fn a_late_loss_after_a_forced_idr_forces_it() {
        let mut m = LtrMirror::default();
        for cur in 0..61 {
            let acked = Acked {
                last: (cur - 4).min(39),
                mask: 0xffff,
            };
            m.step(
                cur == 0 || cur == 40,
                cur,
                30,
                Some(&acked),
                None,
                false,
                true,
            );
        }
        assert_eq!(m.invalidate(41, None).map(|(_, w)| w), Some(40));
    }

    /// An IDR empties the mirror, its taint and a queued force, and marks slot 0.
    #[test]
    fn an_idr_resets_the_ltr_mirror_and_marks_slot_zero() {
        let mut m = LtrMirror {
            slots: [Some(3), Some(5)],
            tainted: [true, false],
            next: 1,
            pending_force: Some(1),
            ..LtrMirror::default()
        };
        let step = m.step(true, 9, 8, None, None, false, true);
        assert_eq!(
            step,
            LtrStep {
                mark_slot: Some(0),
                ..LtrStep::default()
            }
        );
        assert_eq!(
            (m.slots, m.tainted, m.next, m.pending_force),
            ([Some(9), None], [false, false], 1, None)
        );
    }

    /// A queued force on a trusted slot re-references it and takes the frame's mark. AMF's
    /// drops the other slot, QSV's keeps it. On a tainted slot it ships a plain P.
    #[test]
    fn a_queued_force_needs_a_trusted_slot() {
        for (drop_unforced, other_trusted) in [(true, false), (false, true)] {
            let mut m = LtrMirror {
                slots: [Some(0), Some(8)],
                pending_force: Some(0),
                ..LtrMirror::default()
            };
            let step = m.step(false, 16, 8, None, None, false, drop_unforced);
            assert_eq!(
                step,
                LtrStep {
                    force: Some((0, 0)),
                    ..LtrStep::default()
                }
            );
            assert_eq!((m.trusted(0), m.trusted(1)), (true, other_trusted));
            assert_eq!(m.slots, [Some(0), Some(8)], "a taint keeps the wire index");
            m.tainted = [true, false];
            m.pending_force = Some(0);
            let step = m.step(false, 17, 8, None, None, false, drop_unforced);
            assert_eq!(step, LtrStep::default());
            assert_eq!(m.pending_force, None, "a force is consumed either way");
        }
    }

    /// Under confirmed references a frame forces the newest confirmed slot and marks the
    /// other. With none confirmed and both awaiting it, nothing.
    #[test]
    fn confirmed_references_force_the_newest_confirmed_slot() {
        let mut m = LtrMirror {
            slots: [Some(9), Some(10)],
            ..LtrMirror::default()
        };
        assert_eq!(
            amf(&mut m, 11, floor(10), false),
            LtrStep {
                mark_slot: Some(0),
                force: Some((1, 10)),
                acked: true
            }
        );
        assert_eq!((m.slots, m.next), ([Some(11), Some(10)], 1));
        let step = amf(&mut m, 12, floor(9), false);
        assert_eq!((step.mark_slot, step.force), (None, None));
    }

    /// A driver that keeps unforced slots keeps a mark awaiting confirmation: the frame
    /// marks only a free slot, and the next one forces the newer confirmed frame.
    #[test]
    fn kept_slots_hold_a_mark_until_it_is_confirmed() {
        let mut m = LtrMirror {
            slots: [Some(9), Some(10)],
            ..LtrMirror::default()
        };
        let step = amf(&mut m, 11, floor(9), true);
        assert_eq!((step.mark_slot, step.force), (None, Some((0, 9))));
        assert_eq!(m.slots, [Some(9), Some(10)], "10 awaits its confirmation");
        let step = amf(&mut m, 12, floor(10), true);
        assert_eq!((step.mark_slot, step.force), (Some(0), Some((1, 10))));
        assert_eq!(m.slots, [Some(12), Some(10)]);
    }

    /// Marks land on the interval, first on a slot holding no trusted picture, else round
    /// robin. A re-mark clears the taint.
    #[test]
    fn a_mark_prefers_a_slot_without_a_trusted_picture() {
        let mut m = LtrMirror {
            slots: [Some(0), Some(8)],
            tainted: [false, true],
            ..LtrMirror::default()
        };
        assert_eq!(
            amf(&mut m, 15, None, false),
            LtrStep::default(),
            "off the interval"
        );
        assert_eq!(amf(&mut m, 16, None, false).mark_slot, Some(1));
        assert_eq!(m.slots, [Some(0), Some(16)]);
        assert!(m.trusted(1), "a re-mark clears the taint");
        let step = amf(&mut m, 24, None, false);
        assert_eq!(step.mark_slot, Some(0), "both trusted: the round robin");
        assert_eq!(m.slots, [Some(24), Some(16)]);
    }

    /// A loss taints from its first frame on and queues the newest clean slot before it. The
    /// taint outlives the next loss, which then finds no anchor.
    #[test]
    fn a_loss_queues_the_newest_clean_slot_before_it() {
        let mut m = LtrMirror {
            slots: [Some(8), Some(16)],
            ..LtrMirror::default()
        };
        assert_eq!(m.invalidate(12, None), Some((0, 8)));
        assert_eq!((m.trusted(0), m.trusted(1)), (true, false));
        assert_eq!(m.pending_force, Some(0));
        assert_eq!(m.invalidate(4, None), None);
        assert_eq!((m.trusted(0), m.trusted(1)), (false, false));
        assert_eq!(m.pending_force, None);
    }

    /// Distrust taints every slot and drops a queued force; it counts the trusted slots.
    #[test]
    fn distrust_withdraws_every_slot_and_the_queued_force() {
        let mut m = LtrMirror {
            slots: [Some(8), Some(16)],
            pending_force: Some(1),
            ..LtrMirror::default()
        };
        assert_eq!(m.distrust(), 2);
        assert_eq!(
            (m.trusted(0), m.trusted(1), m.pending_force),
            (false, false, None)
        );
        assert_eq!(m.distrust(), 0);
    }

    /// The mirror's own order, checked frame by frame against [`rfi::plan_frame`] fed the
    /// candidates the mirror holds. Events: `f` a frame, `k` a forced IDR, `a` the newest
    /// frame lost, `l` every frame lost, `d` distrust, `+` / `-` confirmations one frame
    /// behind, on and off. Returns the frames: `I`, `A` anchor, `K` confirmed, `.` chain.
    fn drive(events: &[u8], keep: bool, drop_unforced: bool) -> String {
        let mut m = LtrMirror::default();
        let (mut cur, mut forced, mut confirming) = (0i64, true, false);
        let mut frames = String::new();
        for &e in events {
            let floor = confirming.then_some(Acked {
                last: cur - 1,
                mask: 0xffff,
            });
            match e {
                b'k' => forced = true,
                b'+' | b'-' => confirming = e == b'+',
                b'd' => _ = m.distrust(),
                // The backends refuse a range below zero before the mirror sees it.
                b'a' if cur == 0 => {}
                b'a' => _ = m.invalidate(cur - 1, floor.as_ref()),
                b'l' => _ = m.invalidate(0, floor.as_ref()),
                _ => {
                    let pending = rfi::Pending {
                        idr: forced,
                        anchor: m.pending_force.is_some_and(|s| m.trusted(s)),
                        acked: floor.is_some_and(|f| {
                            rfi::ltr_acked_step(&m.trusted_view(), &f, cur, m.next)
                                .1
                                .is_some()
                        }),
                        loss: false,
                    };
                    let plan = rfi::plan_frame(rfi::Recovery::default(), pending, &NO_WAVE);
                    let step = m.step(forced, cur, 4, None, floor, keep, drop_unforced);
                    let (was, letter) = match (forced, step.force, step.acked) {
                        (true, ..) => (rfi::FrameRef::Idr, 'I'),
                        (_, Some(_), false) => (rfi::FrameRef::Anchor, 'A'),
                        (_, Some(_), true) => (rfi::FrameRef::Acked, 'K'),
                        (_, None, _) => (rfi::FrameRef::Chain, '.'),
                    };
                    let seen = String::from_utf8_lossy(events);
                    assert_eq!(plan.refs, was, "frame {cur} of {seen}");
                    assert_eq!((plan.wave, plan.next), (None, rfi::Recovery::default()));
                    frames.push(letter);
                    (forced, cur) = (false, cur + 1);
                }
            }
        }
        frames
    }

    /// The order AMF and QSV follow today: an IDR, then a queued force, then the newest
    /// confirmed slot, then the chain.
    #[test]
    fn ltr_recovery_order() {
        for (events, frames, rule) in [
            ("ff", "I.", "the first frame is an IDR"),
            ("ffaff", "I.A.", "a loss with a clean slot before it"),
            ("fflff", "I...", "a loss with none is declined"),
            ("ffakff", "I.I.", "an IDR outranks the queued force"),
            ("ff+ff-f", "I.KK.", "a confirmed reference"),
            ("ff+aff", "I.AK", "a queued force outranks it"),
            ("ffadf", "I..", "distrust drops the queued force"),
            ("ffalf", "I..", "a later loss taints the queued slot"),
        ] {
            assert_eq!(drive(events.as_bytes(), false, true), frames, "{rule}");
        }
        // Every sequence of up to six events, as AMF in both LTR modes and as QSV.
        const EVENTS: &[u8] = b"fkald+-";
        for (keep, drop_unforced) in [(false, true), (true, true), (false, false)] {
            for len in 1..=6u32 {
                for n in 0..EVENTS.len().pow(len) {
                    let mut seq = [0u8; 6];
                    let mut digits = n;
                    for e in seq.iter_mut().take(len as usize) {
                        *e = EVENTS[digits % EVENTS.len()];
                        digits /= EVENTS.len();
                    }
                    drive(&seq[..len as usize], keep, drop_unforced);
                }
            }
        }
    }
}
