//! The PyroWave lane's present rules, with no Vulkan or NDK type: the `ASurfaceControl` buffer
//! ring ([`Ring`]), the swapchain's size ([`target_extent`]), the cropped viewport
//! ([`crop_viewport`]) and what a Vulkan result means for the picture in hand ([`verdict`]). The
//! shell in `pyro/` makes the calls these name and reports each result back.
//!
//! A ring slot is free, on screen, or leaving: replaced on the layer and free once that
//! transaction completes. The completion frees the slot its transaction replaced; the shell
//! imports the release fence it carried, and the next draw into that slot waits on it.
//!
//! Tables pin each rule. Design: `design/android-native-overhaul.md` F7 (planning repo).

use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// `armed`: its release fence is imported, and the next draw into it waits on it.
    Free { armed: bool },
    /// Drawn and on the layer.
    OnScreen,
    /// Replaced on the layer; free once that transaction completes.
    Leaving,
}

/// The buffer ring's bookkeeping. The shell holds the buffers, indexed alike.
#[derive(Default)]
pub(crate) struct Ring {
    slots: Vec<State>,
    /// The picture size the slots hold; `(0, 0)` with none.
    extent: (u32, u32),
    /// The last transaction's seq. Seqs only grow, across rebuilds too.
    seq: u64,
    /// Applied transactions not yet completed, each with the slot it replaced.
    pending: VecDeque<(u64, Option<usize>)>,
}

impl Ring {
    /// Whether a `width`×`height` picture needs the ring rebuilt.
    pub(crate) fn outgrown(&self, width: u32, height: u32) -> bool {
        self.extent != (width, height)
    }

    /// The picture size the slots hold.
    pub(crate) fn extent(&self) -> (u32, u32) {
        self.extent
    }

    /// The slots are gone: nothing to draw into, nothing pending, and every picture outgrows it.
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.pending.clear();
        self.extent = (0, 0);
    }

    /// `slots` free slots hold `width`×`height` pictures.
    pub(crate) fn fill(&mut self, slots: usize, width: u32, height: u32) {
        self.slots = vec![State::Free { armed: false }; slots];
        self.extent = (width, height);
    }

    /// Transaction `seq` completed: the slot it replaced, now free. The shell imports the release
    /// fence the completion carried and reports it with [`Ring::arm`].
    pub(crate) fn complete(&mut self, seq: u64) -> Option<usize> {
        let at = self.pending.iter().position(|&(s, _)| s == seq)?;
        let slot = self.pending.remove(at)?.1?;
        self.slots[slot] = State::Free { armed: false };
        Some(slot)
    }

    /// The release fence of `slot`, freed by [`Ring::complete`], is imported.
    pub(crate) fn arm(&mut self, slot: usize) {
        if let State::Free { armed } = &mut self.slots[slot] {
            *armed = true;
        }
    }

    /// The slot the next picture draws into, and whether the draw waits on its release fence;
    /// `None` while the compositor holds every one.
    pub(crate) fn next_draw(&mut self) -> Option<(usize, bool)> {
        self.slots
            .iter_mut()
            .enumerate()
            .find_map(|(i, s)| match s {
                State::Free { armed } => Some((i, std::mem::take(armed))),
                _ => None,
            })
    }

    /// The next transaction's seq, whether or not it is made.
    pub(crate) fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Transaction `seq` put `slot` on the layer; the slot on screen before it leaves.
    pub(crate) fn shown(&mut self, slot: usize, seq: u64) {
        let replaced = self.slots.iter().position(|&s| s == State::OnScreen);
        if let Some(i) = replaced {
            self.slots[i] = State::Leaving;
        }
        self.slots[slot] = State::OnScreen;
        self.pending.push_back((seq, replaced));
    }
}

/// The size the swapchain should have: Kotlin's report of the view's size (`followed`, packed by
/// `pack_surface_size`, `0` before the first) once it holds one, else the window's `current`,
/// within the surface's `min`..=`max`.
pub(crate) fn target_extent(
    followed: u64,
    current: (u32, u32),
    min: (u32, u32),
    max: (u32, u32),
) -> (u32, u32) {
    let (w, h) = match crate::session::unpack_surface_size(followed) {
        Some((w, h)) if w > 0 && h > 0 => (w as u32, h as u32),
        _ => current,
    };
    (w.clamp(min.0, max.0), h.clamp(min.1, max.1))
}

/// The viewport `[x, y, width, height]` that shows `crop` (frame fractions) across the whole
/// `surface`. The SurfaceView is laid out at the picture's rect, so the cropped-away edges fall
/// outside the surface and the scissor drops them.
pub(crate) fn crop_viewport(
    [left, top, right, bottom]: [f32; 4],
    (sw, sh): (u32, u32),
) -> [f32; 4] {
    let (w, h) = (
        sw as f32 / (right - left).max(1e-3),
        sh as f32 / (bottom - top).max(1e-3),
    );
    [-left * w, -top * h, w, h]
}

/// A Vulkan call the lane makes for a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    /// The wait on the frame's fence before its command buffer is reused.
    FenceWait,
    Acquire,
    Present,
    /// A rebuild of the swapchain or the buffer ring.
    Rebuild,
}

/// How a call ended, as the lane reads its `VkResult`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    OutOfDate,
    /// `VK_TIMEOUT` or `VK_NOT_READY`.
    Timeout,
    DeviceLost,
    /// Any other error.
    Failed,
}

/// What the lane does with the picture in hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Carry on.
    Go,
    /// Drop this picture; the next one tries again.
    Drop,
    /// Rebuild the swapchain, then drop this picture.
    Rebuild,
    /// End the lane.
    Fatal,
}

/// What `outcome` of `call` means for the picture in hand. A failed rebuild leaves the ring or
/// swapchain outgrown, so the next picture rebuilds again; a fence wait that timed out leaves
/// the fence for the next picture to wait on.
pub(crate) fn verdict(call: Call, outcome: Outcome) -> Verdict {
    match (call, outcome) {
        (_, Outcome::Done) => Verdict::Go,
        (_, Outcome::DeviceLost) => Verdict::Fatal,
        (Call::Acquire | Call::Present, Outcome::OutOfDate) => Verdict::Rebuild,
        (Call::Acquire | Call::FenceWait, Outcome::Timeout) | (Call::Rebuild, _) => Verdict::Drop,
        _ => Verdict::Fatal,
    }
}

/// Failed rebuilds in a row that end the lane: half a second of pictures at 60 Hz.
pub(crate) const MAX_FAILED_REBUILDS: u32 = 30;

/// The [`verdict`] on one rebuild, with `failed` counting the failures in a row.
pub(crate) fn after_rebuild(failed: &mut u32, outcome: Outcome) -> Verdict {
    let v = verdict(Call::Rebuild, outcome);
    *failed = match v {
        Verdict::Drop => *failed + 1,
        _ => 0,
    };
    if *failed >= MAX_FAILED_REBUILDS {
        Verdict::Fatal
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::pack_surface_size;

    /// Slots per ring, as the shell builds it.
    const SLOTS: usize = 4;

    #[derive(Clone, Copy)]
    enum Op {
        /// `clear`, then `fill` at this size.
        Rebuild(u32, u32),
        /// `clear` alone.
        Clear,
        /// `next_draw`, and what it returns.
        Draw(Option<(usize, bool)>),
        /// A transaction made for this slot: `next_seq`, then `shown`.
        Show(usize),
        /// A transaction not made: `next_seq` alone.
        Unmade,
        /// `complete(seq)`, and the slot it frees.
        Complete(u64, Option<usize>),
        /// The freed slot's release fence imported.
        Arm(usize),
        /// `outgrown` at this size, and the answer.
        Outgrown(u32, u32, bool),
    }

    use Op::{Arm, Clear, Complete, Draw, Outgrown, Rebuild, Show, Unmade};

    const HD: (u32, u32) = (1920, 1080);

    /// Name and steps, from a fresh ring. Seqs count from 1.
    const RING: &[(&str, &[Op])] = &[
        (
            "a fresh ring holds no slot and every picture outgrows it",
            &[Outgrown(1920, 1080, true), Draw(None), Complete(1, None)],
        ),
        (
            "a rebuilt ring fits its size alone and draws its first slot unfenced",
            &[
                Rebuild(HD.0, HD.1),
                Outgrown(1920, 1080, false),
                Outgrown(1280, 720, true),
                Draw(Some((0, false))),
            ],
        ),
        (
            "a shown slot stays on screen; the next picture takes the next free one",
            &[
                Rebuild(HD.0, HD.1),
                Draw(Some((0, false))),
                Show(0),
                Draw(Some((1, false))),
            ],
        ),
        (
            "a completion frees the slot its transaction replaced",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Complete(1, None),
                Complete(2, Some(0)),
                Draw(Some((0, false))),
            ],
        ),
        (
            "a freed slot whose release fence was imported is waited on by one draw",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Complete(2, Some(0)),
                Arm(0),
                Draw(Some((0, true))),
                Draw(Some((0, false))),
            ],
        ),
        (
            "with every slot on screen or leaving, there is nothing to draw into",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Show(2),
                Show(3),
                Draw(None),
                Complete(2, Some(0)),
                Draw(Some((0, false))),
            ],
        ),
        (
            "a transaction not made leaves its slot free and still takes a seq",
            &[
                Rebuild(HD.0, HD.1),
                Draw(Some((0, false))),
                Unmade,
                Draw(Some((0, false))),
                Show(0),
                Show(1),
                Complete(1, None),
                Complete(3, Some(0)),
            ],
        ),
        (
            "completions out of order free what each replaced",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Show(2),
                Complete(3, Some(1)),
                Complete(2, Some(0)),
                Complete(1, None),
            ],
        ),
        (
            "a completion not pending frees nothing",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Complete(7, None),
                Complete(2, Some(0)),
                Complete(2, None),
            ],
        ),
        (
            "a rebuild frees every slot, and a completion from before it frees nothing",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Show(2),
                Show(3),
                Rebuild(1280, 720),
                Outgrown(1920, 1080, true),
                Complete(2, None),
                Draw(Some((0, false))),
                Show(0),
                Show(1),
                Complete(6, Some(0)),
            ],
        ),
        (
            "a cleared ring has nothing to draw into, frees nothing, and every picture outgrows it",
            &[
                Rebuild(HD.0, HD.1),
                Show(0),
                Show(1),
                Clear,
                Outgrown(1920, 1080, true),
                Draw(None),
                Complete(2, None),
            ],
        ),
    ];

    #[test]
    fn the_ring_steps_as_the_table_says() {
        for &(name, ops) in RING {
            let mut ring = Ring::default();
            for (i, &op) in ops.iter().enumerate() {
                match op {
                    Rebuild(w, h) => {
                        ring.clear();
                        ring.fill(SLOTS, w, h);
                        assert_eq!(ring.extent(), (w, h), "{name}: step {i}");
                    }
                    Clear => ring.clear(),
                    Draw(want) => assert_eq!(ring.next_draw(), want, "{name}: step {i}"),
                    Show(slot) => {
                        let seq = ring.next_seq();
                        ring.shown(slot, seq);
                    }
                    Unmade => {
                        ring.next_seq();
                    }
                    Complete(seq, want) => {
                        assert_eq!(ring.complete(seq), want, "{name}: step {i}")
                    }
                    Arm(slot) => ring.arm(slot),
                    Outgrown(w, h, want) => {
                        assert_eq!(ring.outgrown(w, h), want, "{name}: step {i}")
                    }
                }
            }
        }
    }

    #[test]
    fn the_swapchain_follows_kotlins_size_within_the_surfaces_limits() {
        let any = ((1, 1), (4096, 4096));
        // (name, followed, current, (min, max), size)
        let rows = [
            ("before Kotlin reports a size, the window's", 0, HD, any, HD),
            (
                "Kotlin's size once it reports one",
                pack_surface_size(1280, 720),
                HD,
                any,
                (1280, 720),
            ),
            (
                "a report without a width leaves the window's",
                720,
                HD,
                any,
                HD,
            ),
            (
                "a size past the surface's limits is clamped to them",
                pack_surface_size(8000, 100),
                HD,
                ((1, 200), (4096, 4096)),
                (4096, 200),
            ),
            (
                "the window's size is clamped too",
                0,
                (u32::MAX, u32::MAX),
                ((1, 1), (2400, 1080)),
                (2400, 1080),
            ),
        ];
        for (name, followed, current, (min, max), want) in rows {
            assert_eq!(target_extent(followed, current, min, max), want, "{name}");
        }
    }

    #[test]
    fn the_viewport_spreads_the_crop_across_the_surface() {
        // (name, crop, surface, viewport rounded)
        let rows = [
            (
                "the whole frame fills the surface",
                [0.0, 0.0, 1.0, 1.0],
                HD,
                [0.0, 0.0, 1920.0, 1080.0],
            ),
            (
                "a centred half is doubled, its edges off the surface",
                [0.25, 0.25, 0.75, 0.75],
                (1000, 500),
                [-500.0, -250.0, 2000.0, 1000.0],
            ),
            (
                "a left crop shifts the picture left",
                [0.25, 0.0, 1.0, 1.0],
                (750, 1000),
                [-250.0, 0.0, 1000.0, 1000.0],
            ),
            (
                "an empty crop is held to a thousandth of the frame",
                [0.5, 0.5, 0.5, 0.5],
                (100, 100),
                [-50000.0, -50000.0, 100000.0, 100000.0],
            ),
        ];
        for (name, crop, surface, want) in rows {
            assert_eq!(crop_viewport(crop, surface).map(f32::round), want, "{name}");
        }
    }

    use Call::{Acquire, FenceWait, Present};
    use Outcome::{DeviceLost, Done, Failed, OutOfDate, Timeout};

    /// Name, call, how it ended, what the lane does.
    const VERDICTS: &[(&str, Call, Outcome, Verdict)] = &[
        ("a fence that signals goes on", FenceWait, Done, Verdict::Go),
        ("an acquired image goes on", Acquire, Done, Verdict::Go),
        ("a present goes on", Present, Done, Verdict::Go),
        ("a rebuild goes on", Call::Rebuild, Done, Verdict::Go),
        (
            "an acquire that is out of date rebuilds",
            Acquire,
            OutOfDate,
            Verdict::Rebuild,
        ),
        (
            "a present that is out of date rebuilds",
            Present,
            OutOfDate,
            Verdict::Rebuild,
        ),
        (
            "an acquire that timed out drops the picture",
            Acquire,
            Timeout,
            Verdict::Drop,
        ),
        (
            "R4-2: a fence wait that timed out drops the picture",
            FenceWait,
            Timeout,
            Verdict::Drop,
        ),
        (
            "R4-1: a failed rebuild drops the picture",
            Call::Rebuild,
            Failed,
            Verdict::Drop,
        ),
        (
            "a lost device ends the lane at the fence",
            FenceWait,
            DeviceLost,
            Verdict::Fatal,
        ),
        (
            "a lost device ends the lane at the acquire",
            Acquire,
            DeviceLost,
            Verdict::Fatal,
        ),
        (
            "a lost device ends the lane at the present",
            Present,
            DeviceLost,
            Verdict::Fatal,
        ),
        (
            "a lost device ends the lane at a rebuild",
            Call::Rebuild,
            DeviceLost,
            Verdict::Fatal,
        ),
        (
            "a fence wait that failed ends the lane",
            FenceWait,
            Failed,
            Verdict::Fatal,
        ),
        (
            "an acquire that failed ends the lane",
            Acquire,
            Failed,
            Verdict::Fatal,
        ),
        (
            "a present that failed ends the lane",
            Present,
            Failed,
            Verdict::Fatal,
        ),
    ];

    #[test]
    fn each_result_means_what_the_table_says() {
        for &(name, call, outcome, want) in VERDICTS {
            assert_eq!(verdict(call, outcome), want, "{name}");
        }
    }

    /// Rebuild outcomes through one count, as `Present` reports them.
    fn rebuilds(outcomes: &[Outcome]) -> Vec<Verdict> {
        let mut failed = 0;
        outcomes
            .iter()
            .map(|&o| after_rebuild(&mut failed, o))
            .collect()
    }

    #[test]
    fn r4_1_thirty_failed_rebuilds_in_a_row_end_the_lane() {
        let failures = [Failed; MAX_FAILED_REBUILDS as usize];
        let mut want = vec![Verdict::Drop; failures.len() - 1];
        want.push(Verdict::Fatal);
        assert_eq!(rebuilds(&failures), want);
    }

    #[test]
    fn a_rebuild_that_works_restarts_the_count() {
        let short = MAX_FAILED_REBUILDS as usize - 1;
        let mut outcomes = vec![Failed; short];
        outcomes.push(Done);
        outcomes.extend(vec![Failed; short]);
        assert!(!rebuilds(&outcomes).contains(&Verdict::Fatal));
    }

    #[test]
    fn a_lost_device_ends_the_lane_at_once() {
        assert_eq!(
            rebuilds(&[Failed, DeviceLost]),
            [Verdict::Drop, Verdict::Fatal]
        );
    }
}
