//! What the MediaCodec loop's feed does with the deliveries it parks and the input slots the
//! codec offers: the park and its cap, the pairing of a slot with the park's head, and what each
//! codec answer leaves behind; and how much the feeder thread hands on ahead of the loop
//! ([`Handoff`]). No codec and no clock: [`Feed::next`] names the call, the loop makes it and
//! reports the answer back. The loop is `State::feed` in `decode/async_loop.rs`; the per-piece
//! rules are [`crate::feed_rules`]. `the_feed_table` pins it.

use std::collections::VecDeque;

use punktfunk_core::session::Frame;

use crate::au_intake::hand_on;
use crate::feed_rules::{gate, spoiled, Cause, Gate, PartFeed, Piece, Spoiled};

/// Cap on whole AUs parked awaiting a free codec input slot. Matches the connector's own
/// frame-channel depth, which also counts AUs, not pieces; past it the oldest AU is dropped and a
/// keyframe requested, the recovery of a reassembler drop. In steady state the park stays
/// near-empty.
pub(crate) const FRAME_PARK_CAP: usize = 16;

/// Oversize AUs in a row that end the run as a hung codec's does. Each one asks for a keyframe;
/// past the first, those keyframes outgrew the buffer too, and only a rebuild changes it.
pub(crate) const OVERSIZE_RUN: u32 = 3;

/// The feeder's side of the hand-off to the loop: the head it last skipped, whose later parts
/// follow it out ([`hand_on`]), and the heads it discarded since it last sent one.
#[derive(Default)]
pub(crate) struct Handoff {
    skip: Option<u32>,
    discarded: u32,
}

impl Handoff {
    /// One delivery from core. `admit`: core's verdict on a whole AU or a first part, `None` on
    /// a later part. `held`: heads sent that the loop has not taken. Past [`FRAME_PARK_CAP`] of
    /// those an admitted head is discarded, a loss core never sees. `Some(n)`: send it, carrying
    /// the `n` heads discarded ahead of it to the loop. A new format always goes: it ends the
    /// run, and the next decoder is built for it.
    pub(crate) fn hand(
        &mut self,
        index: u32,
        admit: Option<bool>,
        held: usize,
        reformat: bool,
    ) -> Option<u32> {
        let full = admit == Some(true) && held >= FRAME_PARK_CAP && !reformat;
        if !hand_on(&mut self.skip, index, admit.map(|a| a && !full)) {
            self.discarded += u32::from(full);
            return None;
        }
        Some(match admit {
            Some(_) => std::mem::take(&mut self.discarded),
            None => 0,
        })
    }
}

/// The codec's input side: the slots it offered, the deliveries parked for them, and the AU open
/// in the codec. The codec offers a slot once, so one whose call did not take goes back first in
/// line: a forgotten slot is gone until a rebuild. Loop-local, like the ledger.
#[derive(Default)]
pub(crate) struct Feed {
    slots: VecDeque<usize>,
    park: VecDeque<Frame>,
    open: Option<PartFeed>,
    /// Oversize AUs since a piece last went in.
    oversize: u32,
    /// Oversize keyframes since a keyframe last went in. The frames fed between two of them
    /// say nothing: each one's replacement outgrew the buffer too.
    oversize_keys: u32,
}

/// The call [`Feed::next`] asks of the loop.
pub(crate) enum Step {
    /// Copy `frame` into `slot` and queue it. Report with [`Feed::queued`], [`Feed::no_buffer`]
    /// or [`Feed::spoil`].
    Go {
        slot: usize,
        frame: Frame,
        piece: Piece,
    },
    /// Queue an empty buffer at `open.pts_us` into `slot`: the AU open in the codec died. The
    /// park's head waits for the next slot. Report a refused call with [`Feed::close_failed`].
    Close { slot: usize, open: PartFeed },
    /// The park's head lost its AU's head upstream and is dropped. No slot is spent.
    Orphan,
}

impl Feed {
    /// The codec offered input slot `slot`.
    pub(crate) fn offer(&mut self, slot: usize) {
        self.slots.push_back(slot);
    }

    /// Park a delivery the intake admitted. [`Feed::trim`] holds the cap.
    pub(crate) fn park(&mut self, f: Frame) {
        self.park.push_back(f);
    }

    /// Drop the oldest parked AUs, each with its pieces, until at most [`FRAME_PARK_CAP`] whole
    /// AUs wait. A piece of an AU still arriving counts for nothing. Run after feeding, so a slot
    /// offered in the same pass is used first. Returns the AUs dropped, each a loss.
    pub(crate) fn trim(&mut self) -> u64 {
        let closes = |f: &Frame| Piece::of(f).last;
        let mut aus = self.park.iter().filter(|f| closes(f)).count();
        let mut dropped = 0;
        while aus > FRAME_PARK_CAP {
            let Some(f) = self.park.pop_front() else {
                break;
            };
            if closes(&f) {
                aus -= 1;
                dropped += 1;
            }
        }
        dropped
    }

    /// Keep the parked deliveries `keep` passes, in order.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn rejudge(&mut self, keep: impl FnMut(&Frame) -> bool) {
        self.park.retain(keep);
    }

    /// Deliveries parked.
    #[cfg_attr(not(target_os = "android"), allow(dead_code))]
    pub(crate) fn parked(&self) -> usize {
        self.park.len()
    }

    /// The park's head judged against the open AU ([`gate`]), for the first free slot. `None`
    /// with no slot free or nothing parked.
    pub(crate) fn next(&mut self) -> Option<Step> {
        if self.slots.is_empty() {
            return None;
        }
        let piece = Piece::of(self.park.front()?);
        Some(match gate(&mut self.open, &piece) {
            Gate::Go => Step::Go {
                slot: self.slots.pop_front()?,
                frame: self.park.pop_front()?,
                piece,
            },
            Gate::Close(open) => Step::Close {
                slot: self.slots.pop_front()?,
                open,
            },
            Gate::Orphan => {
                self.park.pop_front();
                Step::Orphan
            }
        })
    }

    /// The codec refused the close of `open`. The slot is still ours, and the codec still holds
    /// the AU's pieces, so the ledger stays poisoned: the next slot closes it again before any
    /// other piece goes in, which would land on the dead AU's prefix.
    pub(crate) fn close_failed(&mut self, slot: usize, open: PartFeed) {
        self.slots.push_front(slot);
        self.open = Some(PartFeed {
            expected: usize::MAX,
            ..open
        });
    }

    /// The codec handed back no buffer for `slot`. Slot and delivery stay ours, first in line:
    /// forgetting the slot leaks a codec buffer, dropping the delivery holes the reference chain.
    pub(crate) fn no_buffer(&mut self, slot: usize, frame: Frame) {
        self.slots.push_front(slot);
        self.park.push_front(frame);
    }

    /// `piece` did not reach the codec. Its slot stays ours for the next delivery, and a
    /// poisoned ledger closes at that one ([`spoiled`]).
    pub(crate) fn spoil(&mut self, slot: usize, piece: &Piece, cause: Cause) -> Spoiled {
        self.slots.push_front(slot);
        if matches!(cause, Cause::Oversize) {
            self.oversize += 1;
            self.oversize_keys += u32::from(piece.keyframe);
        }
        let left = spoiled(piece, cause);
        if let Spoiled::Poison { ledger, .. } = &left {
            self.open = Some(*ledger);
        }
        left
    }

    /// `len` bytes of `piece` went into the codec.
    pub(crate) fn queued(&mut self, piece: &Piece, len: usize) {
        self.open = piece.queued(len);
        self.oversize = 0;
        if piece.keyframe {
            self.oversize_keys = 0;
        }
    }

    /// [`OVERSIZE_RUN`] AUs in a row outgrew the codec's input buffer, or as many keyframes
    /// with no keyframe fed between them: the run ends.
    pub(crate) fn outgrown(&self) -> bool {
        self.oversize >= OVERSIZE_RUN || self.oversize_keys >= OVERSIZE_RUN
    }
}

#[cfg(test)]
mod tests {
    use super::{Feed, Handoff, Step, FRAME_PARK_CAP};
    use crate::feed_rules::{Cause, Spoiled};
    use punktfunk_core::session::{Frame, FramePart};

    /// What the codec answers one call.
    #[derive(Clone, Copy)]
    enum Answer {
        Takes,
        /// No buffer for the slot.
        NoBuffer,
        /// A buffer smaller than the piece.
        Small,
        /// The queue call fails.
        Refuses,
    }
    use Answer::{NoBuffer, Refuses, Small};

    #[derive(Clone, Copy)]
    enum Op {
        /// Park AU `index` whole.
        Whole(u32),
        /// Park keyframe AU `index` whole.
        Key(u32),
        /// Park a 100-byte piece of AU `index`: offset, first, last.
        Part(u32, u32, bool, bool),
        /// Park `n` whole AUs from index `from`.
        Burst(u32, u32),
        Slot(usize),
        /// One feed pass, the codec answering each call in turn and then taking everything.
        Pass(&'static [Answer]),
    }
    use Op::{Burst, Key, Part, Pass, Slot, Whole};

    /// What a spoiled piece leaves behind.
    #[derive(Debug, PartialEq)]
    enum Left {
        Lost,
        Poison,
        /// Poisoned, and core freezes at once.
        Freeze,
    }

    #[derive(Debug, PartialEq)]
    enum Out {
        /// AU index, slot.
        Fed(u32, usize),
        Closed(u32),
        CloseRefused(u32),
        Orphan,
        NoBuffer(u32),
        Oversize(u32, Left),
        Refused(u32, Left),
        /// The park dropped its oldest AU.
        Overflow,
        /// The run ends, as for a hung codec.
        Wedged,
    }
    use Out::{Closed, Fed, Orphan, Overflow};

    fn frame(index: u32, part: Option<FramePart>) -> Frame {
        Frame {
            data: vec![0; 100].into(),
            frame_index: index,
            pts_ns: u64::from(index) * 1_000_000,
            flags: 0,
            epoch: 0,
            stream: 0,
            complete: part.is_none_or(|p| p.last),
            part,
            received_ns: 0,
            new_format: None,
        }
    }

    fn left(s: Spoiled) -> Left {
        match s {
            Spoiled::Lost => Left::Lost,
            Spoiled::Poison {
                freeze_now: false, ..
            } => Left::Poison,
            Spoiled::Poison {
                freeze_now: true, ..
            } => Left::Freeze,
        }
    }

    /// One pass of `State::feed`, the codec answering from `answers`, then the park's cap.
    fn pass(feed: &mut Feed, answers: &[Answer], out: &mut Vec<Out>) {
        feed_pass(feed, answers, out);
        for _ in 0..feed.trim() {
            out.push(Overflow);
        }
        if feed.outgrown() {
            out.push(Out::Wedged);
        }
    }

    fn feed_pass(feed: &mut Feed, answers: &[Answer], out: &mut Vec<Out>) {
        let mut answers = answers.iter().copied();
        while let Some(step) = feed.next() {
            match step {
                Step::Orphan => out.push(Orphan),
                Step::Close { slot, open } => {
                    if let Some(Refuses) = answers.next() {
                        feed.close_failed(slot, open);
                        out.push(Out::CloseRefused(open.index));
                        break;
                    }
                    out.push(Closed(open.index));
                }
                Step::Go { slot, frame, piece } => match answers.next().unwrap_or(Answer::Takes) {
                    NoBuffer => {
                        feed.no_buffer(slot, frame);
                        out.push(Out::NoBuffer(piece.index));
                        break;
                    }
                    Small => {
                        let s = feed.spoil(slot, &piece, Cause::Oversize);
                        out.push(Out::Oversize(piece.index, left(s)));
                    }
                    Refuses => {
                        let s = feed.spoil(slot, &piece, Cause::Refused);
                        out.push(Out::Refused(piece.index, left(s)));
                    }
                    Answer::Takes => {
                        feed.queued(&piece, frame.data.len());
                        out.push(Fed(piece.index, slot));
                    }
                },
            }
        }
    }

    fn run(ops: &[Op]) -> Vec<Out> {
        let mut feed = Feed::default();
        let mut out = Vec::new();
        for &op in ops {
            match op {
                Whole(i) => feed.park(frame(i, None)),
                Key(i) => feed.park(Frame {
                    flags: u32::from(punktfunk_core::packet::FLAG_SOF),
                    ..frame(i, None)
                }),
                Part(i, offset, first, last) => {
                    let part = FramePart {
                        offset,
                        first,
                        last,
                    };
                    feed.park(frame(i, Some(part)));
                }
                Burst(from, n) => {
                    for i in from..from + n {
                        feed.park(frame(i, None));
                    }
                }
                Slot(s) => feed.offer(s),
                Pass(answers) => pass(&mut feed, answers, &mut out),
            }
        }
        out
    }

    const CAP: u32 = FRAME_PARK_CAP as u32;

    /// (name, script, what the loop did)
    const CASES: &[(&str, &[Op], &[Out])] = &[
        (
            "go: a whole AU takes the slot the codec offered",
            &[Whole(1), Whole(2), Slot(4), Pass(&[])],
            &[Fed(1, 4)],
        ),
        (
            "go: the next piece continues the open AU",
            &[
                Part(1, 0, true, false),
                Part(1, 100, false, false),
                Part(1, 200, false, true),
                Slot(0),
                Slot(1),
                Slot(2),
                Pass(&[]),
            ],
            &[Fed(1, 0), Fed(1, 1), Fed(1, 2)],
        ),
        (
            "close: a delivery that breaks the open AU spends a slot closing it, then goes in",
            &[
                Part(1, 0, true, false),
                Whole(2),
                Slot(0),
                Slot(1),
                Slot(2),
                Pass(&[]),
            ],
            &[Fed(1, 0), Closed(1), Fed(2, 2)],
        ),
        (
            "orphan: a piece whose head was lost is dropped, its slot kept",
            &[Part(1, 100, false, true), Whole(2), Slot(0), Pass(&[])],
            &[Orphan, Fed(2, 0)],
        ),
        (
            "no buffer: slot and AU wait for the next pass, which stops here",
            &[
                Whole(1),
                Whole(2),
                Slot(0),
                Slot(1),
                Pass(&[NoBuffer]),
                Pass(&[]),
            ],
            &[Out::NoBuffer(1), Fed(1, 0), Fed(2, 1)],
        ),
        (
            "oversize: a whole AU is lost, its slot kept for the next",
            &[Whole(1), Whole(2), Slot(0), Pass(&[Small])],
            &[Out::Oversize(1, Left::Lost), Fed(2, 0)],
        ),
        (
            "oversize: a piece freezes at once, the next delivery closes its AU",
            &[
                Part(1, 0, true, false),
                Part(1, 100, false, true),
                Whole(2),
                Slot(0),
                Slot(1),
                Pass(&[Small]),
            ],
            &[Out::Oversize(1, Left::Freeze), Closed(1), Orphan, Fed(2, 1)],
        ),
        (
            "refused: a whole AU is lost, its slot kept for the next",
            &[Whole(1), Whole(2), Slot(0), Pass(&[Refuses])],
            &[Out::Refused(1, Left::Lost), Fed(2, 0)],
        ),
        (
            "refused: a middle piece poisons, the next delivery closes its AU",
            &[
                Part(1, 0, true, false),
                Part(1, 100, false, false),
                Part(1, 200, false, true),
                Slot(0),
                Slot(1),
                Slot(2),
                Pass(&[Answer::Takes, Refuses]),
            ],
            &[Fed(1, 0), Out::Refused(1, Left::Poison), Closed(1), Orphan],
        ),
        (
            "R1-5: a refused close is tried again before the next AU goes in",
            &[
                Part(1, 0, true, false),
                Whole(2),
                Slot(0),
                Slot(1),
                Pass(&[Answer::Takes, Refuses]),
                Slot(2),
                Pass(&[]),
            ],
            &[Fed(1, 0), Out::CloseRefused(1), Closed(1), Fed(2, 2)],
        ),
        (
            "R1-4: three oversize AUs in a row end the run",
            &[Burst(1, 3), Slot(0), Pass(&[Small, Small, Small])],
            &[
                Out::Oversize(1, Left::Lost),
                Out::Oversize(2, Left::Lost),
                Out::Oversize(3, Left::Lost),
                Out::Wedged,
            ],
        ),
        (
            "R1-4: three oversize keyframes end the run, whatever was fed between them",
            &[
                Key(1),
                Whole(2),
                Key(3),
                Whole(4),
                Key(5),
                Slot(0),
                Slot(1),
                Slot(2),
                Pass(&[Small, Answer::Takes, Small, Answer::Takes, Small]),
            ],
            &[
                Out::Oversize(1, Left::Lost),
                Fed(2, 0),
                Out::Oversize(3, Left::Lost),
                Fed(4, 1),
                Out::Oversize(5, Left::Lost),
                Out::Wedged,
            ],
        ),
        (
            "R1-4: a keyframe that goes in restarts the keyframe count",
            &[
                Key(1),
                Key(2),
                Key(3),
                Key(4),
                Slot(0),
                Slot(1),
                Pass(&[Small, Small, Answer::Takes, Small]),
            ],
            &[
                Out::Oversize(1, Left::Lost),
                Out::Oversize(2, Left::Lost),
                Fed(3, 0),
                Out::Oversize(4, Left::Lost),
            ],
        ),
        (
            "R1-4: a piece that goes in restarts the oversize count",
            &[
                Burst(1, 4),
                Slot(0),
                Slot(1),
                Pass(&[Small, Small, Answer::Takes, Small]),
            ],
            &[
                Out::Oversize(1, Left::Lost),
                Out::Oversize(2, Left::Lost),
                Fed(3, 0),
                Out::Oversize(4, Left::Lost),
            ],
        ),
        (
            "park: past the cap the oldest AU is dropped",
            &[Burst(0, CAP + 1), Pass(&[]), Slot(0), Pass(&[])],
            &[Overflow, Fed(1, 0)],
        ),
        (
            "park: the oldest AU goes with its pieces",
            &[
                Part(0, 0, true, false),
                Part(0, 100, false, true),
                Burst(1, CAP),
                Pass(&[]),
                Slot(0),
                Pass(&[]),
            ],
            &[Overflow, Fed(1, 0)],
        ),
        (
            "R1-2: a piece never trips the cap while its AU is open",
            &[
                Burst(0, CAP),
                Part(CAP, 0, true, false),
                Part(CAP, 100, false, false),
                Pass(&[]),
                Part(CAP, 200, false, true),
                Pass(&[]),
                Slot(0),
                Pass(&[]),
            ],
            &[Overflow, Fed(1, 0)],
        ),
        (
            "R1-2: a slot offered in the same pass feeds before the cap judges",
            &[Burst(0, CAP + 1), Slot(0), Pass(&[])],
            &[Fed(0, 0)],
        ),
    ];

    #[test]
    fn the_feed_table() {
        for &(name, ops, want) in CASES {
            assert_eq!(run(ops), want, "{name}");
        }
    }

    const FULL: usize = FRAME_PARK_CAP;

    /// One feeder step: index, core's verdict or `None` for a later part, heads the loop has
    /// not taken, a new format, and `Some(discards it carries)` when it is sent.
    type Hand = (u32, Option<bool>, usize, bool, Option<u32>);

    /// (name, steps through one feeder)
    const HANDOFFS: &[(&str, &[Hand])] = &[
        (
            "a head goes while the loop keeps up, its parts however full",
            &[
                (1, Some(true), 0, false, Some(0)),
                (1, None, FULL, false, Some(0)),
            ],
        ),
        (
            "R1-1: past the bound heads are discarded, and the next head sent carries them",
            &[
                (2, Some(true), FULL, false, None),
                (2, None, 0, false, None),
                (3, Some(true), FULL, false, None),
                (4, Some(true), FULL - 1, false, Some(2)),
                (4, None, FULL, false, Some(0)),
                (5, Some(true), 0, false, Some(0)),
            ],
        ),
        (
            "core's refusal drops a head and its parts, no loss",
            &[
                (3, Some(false), 0, false, None),
                (3, None, 0, false, None),
                (4, Some(true), 0, false, Some(0)),
            ],
        ),
        (
            "a new format goes past the bound",
            &[(4, Some(true), FULL, true, Some(0))],
        ),
    ];

    #[test]
    fn the_feeder_hands_on_a_bounded_backlog() {
        for &(name, steps) in HANDOFFS {
            let mut handoff = Handoff::default();
            for (i, &(index, admit, held, reformat, want)) in steps.iter().enumerate() {
                let got = handoff.hand(index, admit, held, reformat);
                assert_eq!(got, want, "{name}: step {i}");
            }
        }
    }
}
