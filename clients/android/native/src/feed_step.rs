//! What the MediaCodec loop's feed does with the deliveries it parks and the input slots the
//! codec offers: the park and its cap, the pairing of a slot with the park's head, and what each
//! codec answer leaves behind. No codec and no clock: [`Feed::next`] names the call, the loop
//! makes it and reports the answer back. The loop is `State::feed` in `decode/async_loop.rs`;
//! the per-piece rules are [`crate::feed_rules`]. `the_feed_table` pins it.

use std::collections::VecDeque;

use punktfunk_core::session::Frame;

use crate::feed_rules::{gate, spoiled, Cause, Gate, PartFeed, Piece, Spoiled};

/// Cap on deliveries parked awaiting a free codec input slot. Matches the connector's own
/// frame-channel depth; past it the oldest is dropped and a keyframe requested, the recovery of
/// a reassembler drop. In steady state the park stays near-empty.
pub(crate) const FRAME_PARK_CAP: usize = 16;

/// The codec's input side: the slots it offered, the deliveries parked for them, and the AU open
/// in the codec. Loop-local, like the ledger.
#[derive(Default)]
pub(crate) struct Feed {
    slots: VecDeque<usize>,
    park: VecDeque<Frame>,
    open: Option<PartFeed>,
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
    /// park's head waits for the next slot. Report with [`Feed::closed`].
    Close { slot: usize, open: PartFeed },
    /// The park's head lost its AU's head upstream and is dropped. No slot is spent.
    Orphan,
}

impl Feed {
    /// The codec offered input slot `slot`.
    pub(crate) fn offer(&mut self, slot: usize) {
        self.slots.push_back(slot);
    }

    /// Park a delivery the intake admitted. `true`: the park passed [`FRAME_PARK_CAP`] and its
    /// oldest entry is dropped, a loss.
    pub(crate) fn park(&mut self, f: Frame) -> bool {
        self.park.push_back(f);
        let over = self.park.len() > FRAME_PARK_CAP;
        if over {
            self.park.pop_front();
        }
        over
    }

    /// Keep the parked deliveries `keep` passes, in order.
    pub(crate) fn rejudge(&mut self, keep: impl FnMut(&Frame) -> bool) {
        self.park.retain(keep);
    }

    /// Deliveries parked.
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

    /// The close [`Step::Close`] asked for went in (`taken`), or its slot is still ours.
    pub(crate) fn closed(&mut self, slot: usize, taken: bool) {
        if !taken {
            self.slots.push_front(slot);
        }
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
        let left = spoiled(piece, cause);
        if let Spoiled::Poison { ledger, .. } = &left {
            self.open = Some(*ledger);
        }
        left
    }

    /// `len` bytes of `piece` went into the codec.
    pub(crate) fn queued(&mut self, piece: &Piece, len: usize) {
        self.open = piece.queued(len);
    }
}

#[cfg(test)]
mod tests {
    use super::{Feed, Step, FRAME_PARK_CAP};
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
        /// Park a 100-byte piece of AU `index`: offset, first, last.
        Part(u32, u32, bool, bool),
        /// Park `n` whole AUs from index `from`.
        Burst(u32, u32),
        Slot(usize),
        /// One feed pass, the codec answering each call in turn and then taking everything.
        Pass(&'static [Answer]),
    }
    use Op::{Burst, Part, Pass, Slot, Whole};

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
        /// The park dropped its oldest entry.
        Overflow,
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

    fn park(feed: &mut Feed, f: Frame, out: &mut Vec<Out>) {
        if feed.park(f) {
            out.push(Overflow);
        }
    }

    /// One pass of `State::feed`, the codec answering from `answers`.
    fn pass(feed: &mut Feed, answers: &[Answer], out: &mut Vec<Out>) {
        let mut answers = answers.iter().copied();
        while let Some(step) = feed.next() {
            match step {
                Step::Orphan => out.push(Orphan),
                Step::Close { slot, open } => {
                    let taken = !matches!(answers.next(), Some(Refuses));
                    feed.closed(slot, taken);
                    out.push(if taken {
                        Closed(open.index)
                    } else {
                        Out::CloseRefused(open.index)
                    });
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
                Whole(i) => park(&mut feed, frame(i, None), &mut out),
                Part(i, offset, first, last) => {
                    let part = FramePart {
                        offset,
                        first,
                        last,
                    };
                    park(&mut feed, frame(i, Some(part)), &mut out);
                }
                Burst(from, n) => {
                    for i in from..from + n {
                        park(&mut feed, frame(i, None), &mut out);
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
            "park: past the cap the oldest delivery is dropped",
            &[Burst(0, CAP + 1), Slot(0), Pass(&[])],
            &[Overflow, Fed(1, 0)],
        ),
    ];

    #[test]
    fn the_feed_table() {
        for &(name, ops, want) in CASES {
            assert_eq!(run(ops), want, "{name}");
        }
    }
}
