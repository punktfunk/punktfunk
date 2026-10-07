//! Slot-family RFI recovery **policy** for the three backends that answer a loss
//! with a re-reference to a known-good older frame instead of an IDR: native AMF
//! (user-LTR bitfield), native QSV (`mfxExtRefListCtrl` LTR), and Vulkan Video
//! (app-owned DPB slot table).
//!
//! Policy only. Callers feed **currently-trusted** `(slot, wire)` pairs and apply
//! the returned taints through their own persistent marker. How a force is applied
//! and how distrust is stored (AMF clears the mirror slot, QSV sets `ltr_tainted`,
//! Vulkan blanks `slot_wire` to `-1`) stays in the backend; the caller-side filter
//! is what makes those schemes equivalent under one pure function.
//!
//! Decline is also the backend's: AMF/QSV drop an un-consumed `pending_force`;
//! Vulkan leaves `pending_loss` armed so frame-build can re-pick or force an IDR.
//! Do not harmonize them here. NVENC's range policy is
//! [`super::nvenc_core::plan_range_recovery`].

pub struct SlotPlan {
    /// Slots with `wire >= loss_first`. Persist the distrust in the backend marker:
    /// without it, the next loss treats these as pre-loss anchors.
    pub tainted: u32,
    /// Newest trusted `(slot, wire)` strictly older than the loss. `None` → the
    /// caller declines and recovers via its keyframe path.
    pub anchor: Option<(usize, i64)>,
}

/// The newest trusted `(slot, wire)` at or below `floor`, the newest frame the client
/// confirmed: what a frame references while the link loses packets
/// ([`crate::codec::Encoder::set_reference_floor`]). `None` when none is resident; the
/// caller keeps its ordinary chain, never an IDR.
pub fn pick_acked(refs: &[(usize, i64)], floor: i64) -> Option<(usize, i64)> {
    pick_anchor(refs, floor.saturating_add(1))
}

/// Taint and pick from one snapshot of currently-trusted `(slot, wire)` pairs
/// (caller already dropped previously-distrusted entries). `wire >= loss_first`
/// taints; `wire < loss_first` is the only eligible anchor, so this call cannot
/// pick a slot it just tainted. A `floor` also bounds the anchor to `wire <= floor`,
/// the frames the client confirmed; the taint still covers every slot from the loss on.
pub fn plan_slot_recovery(refs: &[(usize, i64)], loss_first: i64, floor: Option<i64>) -> SlotPlan {
    // Callers gate `first < 0` before they get here; `-1`/`None` sentinels are
    // "untrusted". Plain `assert`: `--release` lint runs, and a compiled-out
    // check would drop taints instead of failing.
    assert!(
        loss_first >= 0,
        "loss_first must be validity-gated by the caller"
    );
    let mut tainted = 0u32;
    for &(slot, wire) in refs {
        if wire >= loss_first {
            assert!(slot < 32, "slot table exceeds the u32 taint mask");
            tainted |= 1 << slot;
        }
    }
    SlotPlan {
        tainted,
        anchor: pick_recovery(refs, loss_first, floor),
    }
}

/// The anchor for a loss from `loss_first`: the newest trusted frame before it, and at or
/// below `floor` while the encoder holds confirmed references.
pub fn pick_recovery(
    refs: &[(usize, i64)],
    loss_first: i64,
    floor: Option<i64>,
) -> Option<(usize, i64)> {
    pick_anchor(
        refs,
        floor.map_or(loss_first, |f| loss_first.min(f.saturating_add(1))),
    )
}

/// Newest trusted `wire` strictly older than the loss. Ties keep the first
/// `refs` entry (callers feed ascending slot order; the backends used `>`).
/// Vulkan re-picks at frame-build against the table as it stands then.
pub fn pick_anchor(refs: &[(usize, i64)], loss_first: i64) -> Option<(usize, i64)> {
    let mut best: Option<(usize, i64)> = None;
    for &(slot, wire) in refs {
        if wire < loss_first && best.is_none_or(|(_, b)| wire > b) {
            best = Some((slot, wire));
        }
    }
    best
}

/// Frames back a confirmed long-term reference may reach on a two-slot LTR backend (AMF,
/// QSV): Vulkan Video's DPB depth. Past it the frame takes the chain.
pub const LTR_ACKED_REACH: i64 = 8;

/// One frame's long-term step while the encoder holds confirmed references: force the newest
/// slot the client confirmed within [`LTR_ACKED_REACH`], and mark this frame into a free slot,
/// round robin from `next`, so a later frame has a newer candidate once the client confirms
/// it. A slot is free when empty, out of reach, or confirmed and older than the forced one; a
/// frame still awaiting its confirmation keeps its slot. `slots` holds each slot's wire, `None`
/// when empty or tainted. Returns the slot to mark and the `(slot, wire)` to force.
pub fn ltr_acked_step(
    slots: &[Option<i64>],
    floor: i64,
    cur: i64,
    next: usize,
) -> (Option<usize>, Option<(usize, i64)>) {
    let reach = |w: i64| cur - w <= LTR_ACKED_REACH;
    let refs: Vec<(usize, i64)> = slots
        .iter()
        .enumerate()
        .filter_map(|(s, w)| w.filter(|&w| reach(w)).map(|w| (s, w)))
        .collect();
    let force = pick_acked(&refs, floor);
    let free = |s: usize| match slots[s] {
        None => true,
        Some(w) => !reach(w) || (w <= floor && force.is_none_or(|(f, _)| f != s)),
    };
    let n = slots.len();
    let mark = (0..n).map(|k| (next + k) % n).find(|&s| free(s));
    (mark, force)
}

/// Slot for the next LTR mark: the first one holding no trusted picture, else `next`, the
/// round robin. Marking over a slot a loss emptied keeps the last clean LTR for the next loss.
pub fn mark_slot(trusted: &[bool], next: usize) -> usize {
    trusted.iter().position(|&t| !t).unwrap_or(next)
}

/// What a wave frame's AU tells the client. Both ends carry the recovery point; the close
/// also carries the close bit, so a client that knows it lifts on a close after a start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaveMark {
    None,
    Start,
    Close,
}

impl WaveMark {
    /// `EncodedFrame::recovery_point`: either end of the wave.
    pub fn point(self) -> bool {
        self != WaveMark::None
    }

    /// `EncodedFrame::recovery_close`: the close alone.
    pub fn close(self) -> bool {
        self == WaveMark::Close
    }
}

/// An on-demand intra refresh wave in flight, the rung between an RFI anchor and the IDR:
/// where no anchor survives, the picture heals over `cycle` frames with no bitrate spike.
/// `index` is the frame about to be encoded. The start AU and the close AU carry
/// `recovery_point`, which the client counts as its two-mark lift; every wave picture but the
/// close is part dirty and never an RFI anchor. The backend places the stripe (Vulkan, VAAPI)
/// or the driver does (NVENC); the bookkeeping here is the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wave {
    pub cycle: u32,
    pub index: u32,
}

impl Wave {
    pub fn start(cycle: u32) -> Wave {
        Wave { cycle, index: 0 }
    }

    /// This frame closes the wave: the picture is fully swept once it is encoded.
    pub fn closes(self) -> bool {
        self.index + 1 >= self.cycle
    }

    /// This frame's AU carries `recovery_point`: the start and the close.
    pub fn marks(self) -> bool {
        self.index == 0 || self.closes()
    }

    /// The mark this frame's AU carries. A `spoiled` wave (a loss inside its sweep) closes
    /// unmarked: its close is not whole, and the wave queued behind it carries the lift.
    pub fn mark(self, spoiled: bool) -> WaveMark {
        if self.index == 0 {
            WaveMark::Start
        } else if self.closes() && !spoiled {
            WaveMark::Close
        } else {
            WaveMark::None
        }
    }

    /// Past the frame just encoded; `None` once the wave closed.
    pub fn next(self) -> Option<Wave> {
        (!self.closes()).then_some(Wave {
            cycle: self.cycle,
            index: self.index + 1,
        })
    }

    /// A loss reported mid-wave spoils the close when a lost frame sits at or past the
    /// start frame: stripes swept before it still reference it. A loss before the start is
    /// swept away by the close. `span_start` is the start frame's timestamp once submitted;
    /// until then the start is `next_ts`, after every frame that could have been lost.
    pub fn spoiled_by(self, span_start: Option<i64>, next_ts: i64, last_lost: i64) -> bool {
        let start = span_start.filter(|_| self.index > 0).unwrap_or(next_ts);
        last_lost >= start
    }

    /// Row-based stripe for this frame: `(first_row, rows)`, one region per frame plus one
    /// row of overlap for the deblocking filter, clipped to the picture. `rows` is the
    /// picture height in the driver's row unit.
    pub fn stripe(self, rows: u32) -> (u32, u32) {
        let region = rows.div_ceil(self.cycle.max(1));
        let first = (region * self.index).min(rows);
        (first, (region + 1).min(rows - first))
    }
}

/// Frames per wave: a quarter second at most (the freeze the client holds during the wave),
/// one row per frame at most, the driver's ceiling, never below 2, and then the number of
/// whole regions the rows make at that size: a cycle that does not divide the rows would
/// refresh nothing on its last indices and close late. `pinned` is [`pinned_cycle`].
pub fn wave_cycle(rows: u32, fps: u32, max_cycle: u32, pinned: Option<u32>) -> u32 {
    let rows = rows.max(2);
    let wanted = pinned
        .unwrap_or((fps / 4).max(2))
        .min(rows)
        .min(max_cycle)
        .max(2);
    rows.div_ceil(rows.div_ceil(wanted)).max(2)
}

/// `PUNKTFUNK_INTRA_REFRESH=0` keeps the IDR on every backend. The same knob at `1` opts the
/// Windows periodic wave in (`policy::intra_refresh_requested`); unset is this wave alone.
pub fn wave_enabled() -> bool {
    crate::knobs::get().intra_refresh != 2
}

/// `PUNKTFUNK_IR_PERIOD_FRAMES=<frames>` pins the cycle for a measurement: the one wave-length
/// knob, shared with the periodic wave's length (`policy::intra_refresh_period`).
pub fn pinned_cycle() -> Option<u32> {
    match crate::knobs::get().ir_period_frames {
        n if n >= 2 => Some(u32::from(n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ltr_acked_step, mark_slot, pick_acked, pick_anchor, plan_slot_recovery, wave_cycle, Wave,
    };

    /// Two LTR slots marked at 840 and 870. A loss at 869 anchors on 840 and empties 870's
    /// slot, so the mark at 900 lands there and a loss at 900 still anchors on 840.
    #[test]
    fn a_mark_refills_the_slot_a_loss_emptied() {
        let mut wires = [840i64, 870];
        let plan = plan_slot_recovery(&view(&wires), 869, None);
        assert_eq!(plan.anchor, Some((0, 840)));
        apply(&mut wires, plan.tainted);
        let slot = mark_slot(&wires.map(|w| w >= 0), 0);
        assert_eq!(slot, 1, "the round robin would overwrite 840");
        wires[slot] = 900;
        assert_eq!(
            plan_slot_recovery(&view(&wires), 900, None).anchor,
            Some((0, 840))
        );
        assert_eq!(
            mark_slot(&[true, true], 1),
            1,
            "every slot trusted: the round robin"
        );
    }

    /// Rows cap the cycle (1080p = 17 CTB rows), a quarter second caps it at 60 fps, the
    /// driver ceiling and the pin override, never below 2.
    #[test]
    fn wave_cycle_caps() {
        // 17 rows in two-row regions: 9 frames, not the 15 a quarter second would allow.
        assert_eq!(wave_cycle(17, 60, 256, None), 9);
        assert_eq!(wave_cycle(17, 120, 256, None), 17);
        assert_eq!(wave_cycle(34, 120, 256, None), 17);
        assert_eq!(wave_cycle(17, 60, 8, None), 6);
        assert_eq!(wave_cycle(17, 60, 256, Some(4)), 4);
        assert_eq!(wave_cycle(17, 60, 256, Some(0)), 2);
        assert_eq!(wave_cycle(1, 60, 256, None), 2);
        assert_eq!(wave_cycle(4, 60, 256, None), 4);
        assert_eq!(wave_cycle(16, 60, 256, None), 8);
    }

    /// A loss at or past the start frame spoils the close; one before it does not. Before
    /// the start frame is submitted every already-sent frame is before the start.
    #[test]
    fn wave_spoiled_by_a_loss_inside_its_span_only() {
        let pending = Wave::start(23);
        assert!(!pending.spoiled_by(None, 120, 119));
        assert!(
            !pending.spoiled_by(Some(90), 120, 119),
            "a stale span is not this wave"
        );
        let running = Wave {
            cycle: 23,
            index: 2,
        };
        assert!(!running.spoiled_by(Some(120), 122, 119));
        assert!(running.spoiled_by(Some(120), 122, 120));
        assert!(running.spoiled_by(Some(120), 122, 121));
    }

    /// Marks on the start and the close only, dirty until the close, stripes that walk the
    /// picture with one overlap row and never past its end.
    #[test]
    fn wave_marks_and_stripes() {
        let mut w = Some(Wave::start(4));
        let mut seen = Vec::new();
        while let Some(wave) = w {
            seen.push((wave.index, wave.marks(), wave.closes(), wave.stripe(4)));
            w = wave.next();
        }
        assert_eq!(
            seen,
            [
                (0, true, false, (0, 2)),
                (1, false, false, (1, 2)),
                (2, false, false, (2, 2)),
                (3, true, true, (3, 1)),
            ]
        );
        // 17 rows over 9 frames: two rows per region plus the overlap, the last one clipped.
        assert_eq!(Wave { cycle: 9, index: 0 }.stripe(17), (0, 3));
        assert_eq!(Wave { cycle: 9, index: 7 }.stripe(17), (14, 3));
        assert_eq!(Wave { cycle: 9, index: 8 }.stripe(17), (16, 1));
        // A two-frame wave both starts and closes within two frames.
        assert!(Wave::start(2).marks() && !Wave::start(2).closes());
        assert!(Wave::start(2).next().unwrap().closes());
    }

    fn view(wires: &[i64]) -> Vec<(usize, i64)> {
        wires
            .iter()
            .enumerate()
            .filter_map(|(s, &w)| (w >= 0).then_some((s, w)))
            .collect()
    }

    fn apply(wires: &mut [i64], tainted: u32) {
        for (s, w) in wires.iter_mut().enumerate() {
            if tainted & (1 << s) != 0 {
                *w = -1;
            }
        }
    }

    /// Under a floor the reference is the newest resident frame the client confirmed,
    /// and a loss's anchor is one too, while the taint still covers the loss.
    #[test]
    fn a_floor_picks_the_newest_confirmed_frame() {
        // Slots hold 15..=22; 21 and 22 are in flight, 20 is the newest confirmed.
        let wires = [15i64, 16, 17, 18, 19, 20, 21, 22];
        assert_eq!(pick_acked(&view(&wires), 20), Some((5, 20)));
        let tainted = [15i64, 16, 17, 18, 19, -1, 21, 22];
        assert_eq!(pick_acked(&view(&tainted), 20), Some((4, 19)));
        assert_eq!(
            pick_acked(&view(&[30, 31]), 20),
            None,
            "nothing confirmed resident"
        );
        let plan = plan_slot_recovery(&view(&wires), 19, Some(17));
        assert_eq!(
            plan.anchor,
            Some((2, 17)),
            "older than the loss and confirmed"
        );
        assert_eq!(plan.tainted, 0b1111_0000);
        assert_eq!(
            plan_slot_recovery(&view(&wires), 19, Some(25)).anchor,
            Some((3, 18))
        );
    }

    /// Under confirmed references a frame forces the newest confirmed slot in reach and marks
    /// only a free slot: a frame still awaiting its confirmation keeps its own.
    #[test]
    fn a_confirmed_ltr_is_forced_and_a_pending_one_is_kept() {
        assert_eq!(
            ltr_acked_step(&[Some(9), Some(10)], 9, 11, 0),
            (None, Some((0, 9))),
            "10 awaits its confirmation"
        );
        assert_eq!(
            ltr_acked_step(&[Some(9), Some(10)], 10, 11, 1),
            (Some(0), Some((1, 10)))
        );
        assert_eq!(
            ltr_acked_step(&[Some(9), None], 9, 18, 0),
            (Some(0), None),
            "past the reach: the chain"
        );
        assert_eq!(
            ltr_acked_step(&[Some(12), Some(13)], 9, 14, 1),
            (None, None)
        );
    }

    /// Two slots, confirmations two frames behind: once warm, every frame references one the
    /// client confirmed, two or three frames back.
    #[test]
    fn two_slots_hold_a_two_frame_round_trip() {
        let (mut slots, mut next) = ([None; 2], 0);
        for i in 0..40i64 {
            let (mark, force) = ltr_acked_step(&slots, i - 2, i, next);
            if i >= 6 {
                let (_, w) = force.expect("a confirmed slot in reach");
                assert!((2..=3).contains(&(i - w)), "frame {i} reaches {}", i - w);
            }
            if let Some(m) = mark {
                slots[m] = Some(i);
                next = (m + 1) % 2;
            }
        }
    }

    #[test]
    fn picks_newest_pre_loss() {
        let wires = [8i64, 9, 10, 11, 12, 5, 6, 7];
        assert_eq!(pick_anchor(&view(&wires), 9), Some((0, 8)));
        assert_eq!(pick_anchor(&view(&wires), 5), None);
        assert_eq!(pick_anchor(&view(&[-1, 3, -1, 4]), 5), Some((3, 4)));
        assert_eq!(pick_anchor(&view(&[-1; 8]), 5), None);
        // `wire == loss_first` is inside the corrupt window: strictly older only.
        assert_eq!(pick_anchor(&view(&[9, 8]), 9), Some((1, 8)));
        // Tie keeps the first `refs` entry — the backends used `>`, not `>=`.
        assert_eq!(pick_anchor(&[(2, 7), (5, 7)], 9), Some((2, 7)));
        assert_eq!(pick_anchor(&[], 9), None);
    }

    /// A slot from an earlier unrepaired loss must not become a later loss's
    /// "known-good" anchor: without persisted distrust it is still resident and
    /// below the second start, so the picker would serve it as `recovery_anchor`.
    #[test]
    fn taint_sweep_excludes_slots_from_an_earlier_loss() {
        // Loss at 4 taints 4..7; a second report at 6 still sees them resident.
        let tainted_wires = [4i64, 5, 6, 7];

        let unswept = [0i64, 1, 2, 3, 4, 5, 6, 7];
        let (_, picked_wire) = pick_anchor(&view(&unswept), 6).expect("unswept picks something");
        assert!(
            tainted_wires.contains(&picked_wire),
            "precondition: without the sweep the anchor comes from the earlier loss window"
        );

        let mut wires = unswept;
        let plan = plan_slot_recovery(&view(&wires), 4, None);
        assert_eq!(plan.tainted, 0b1111_0000);
        assert_eq!(plan.anchor, Some((3, 3)));
        apply(&mut wires, plan.tainted);
        assert_eq!(wires, [0, 1, 2, 3, -1, -1, -1, -1]);
        let (slot, wire) = pick_anchor(&view(&wires), 6).expect("clean wires remain");
        assert_eq!((slot, wire), (3, 3), "newest clean survivor is wire 3");

        // Post-recovery refill: a later loss at 10 may anchor on 9; do not over-taint.
        wires[4] = 8;
        wires[5] = 9;
        wires[6] = 10;
        wires[7] = 11;
        let plan = plan_slot_recovery(&view(&wires), 10, None);
        assert_eq!(plan.anchor, Some((5, 9)), "wire 9 is post-recovery, clean");
        apply(&mut wires, plan.tainted);

        let mut all = [5i64, 6, 7, 8, 9, 10, 11, 12];
        let plan = plan_slot_recovery(&view(&all), 5, None);
        assert_eq!(plan.tainted, 0b1111_1111);
        assert_eq!(plan.anchor, None);
        apply(&mut all, plan.tainted);
        assert_eq!(pick_anchor(&view(&all), 5), None);
    }

    /// Wholesale withdrawal (`Encoder::distrust_references`) has no loss range,
    /// so every resident ref is dropped. The next pick must decline rather than
    /// serve an anchor over unrepaired damage.
    #[test]
    fn distrusting_every_reference_makes_the_next_anchor_pick_decline() {
        let mut wires = [4i64, 5, 6, 7, -1, -1, -1, -1];
        assert_eq!(
            pick_anchor(&view(&wires), 9),
            Some((3, 7)),
            "precondition: this table would happily anchor"
        );

        apply(&mut wires, u32::MAX);
        assert_eq!(
            pick_anchor(&view(&wires), 9),
            None,
            "every reference withdrawn → no anchor, caller falls through to its keyframe path"
        );
        // Persisted: any later loss, not only this one, still finds nothing.
        assert_eq!(pick_anchor(&view(&wires), 100), None);
    }

    /// Withdrawal is per-slot, not the session: a slot re-marked with a fresh
    /// frame (after the IDR flush that emptied the table) is a legal anchor again.
    #[test]
    fn a_re_marked_slot_restores_anchor_trust_after_a_full_withdrawal() {
        let mut wires = [4i64, 5, 6, 7, -1, -1, -1, -1];
        apply(&mut wires, u32::MAX);
        assert_eq!(pick_anchor(&view(&wires), 20), None);

        wires[0] = 14;
        wires[1] = 15;
        assert_eq!(
            pick_anchor(&view(&wires), 20),
            Some((1, 15)),
            "a re-marked slot is trusted again — the suppression is a few frames, not the session"
        );
    }

    /// A report never arrives at the loss: the client waits for the next frame to
    /// spot the gap, and the ask crosses the link. The ring keeps rolling. A ring
    /// shallower than that latency has overwritten every pre-loss picture by the
    /// time the ask lands, so it declines every loss and the session pays an IDR.
    #[test]
    fn the_ring_must_outlast_the_loss_report() {
        // Slot `w % depth` holds wire `w`, the newest `depth` frames.
        let ring = |depth: usize, newest: i64| -> Vec<(usize, i64)> {
            (0..depth)
                .map(|back| {
                    let w = newest - back as i64;
                    ((w as usize) % depth, w)
                })
                .collect()
        };
        // 1440p100 over Wi-Fi: frames 10213 and 10214 are lost, the client sees the
        // gap at 10215, and the ask reaches the encoder around 10217.
        let (loss, when_asked) = (10_213i64, 10_217i64);
        assert_eq!(
            pick_anchor(&ring(4, when_asked), loss),
            None,
            "four slots hold 10214..10217 — the anchor is already evicted"
        );
        assert_eq!(
            pick_anchor(&ring(8, when_asked), loss),
            Some(((10_212usize) % 8, 10_212)),
            "eight slots still hold the picture before the loss"
        );
    }
}
