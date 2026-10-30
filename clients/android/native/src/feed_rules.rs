//! What the MediaCodec loop's feed decides about each piece of an access unit: whether it may
//! touch the codec, what one that cannot go in leaves behind, and the AV1 picture size and colour
//! a keyframe states. No codec and no clock: the loop describes the piece and applies the answer.
//! The loop's feed is `State::feed` in `decode/async_loop.rs`.
//!
//! A sliced AU rides `BUFFER_FLAG_PARTIAL_FRAME` piece by piece, every piece but its last.
//! [`PartFeed`] is the AU open in the codec. Any break (a gap, an orphan, an oversize or refused
//! piece) abandons that AU: it is closed with an empty buffer at its own pts, so the codec needs
//! no flush, and the next `first` piece re-syncs the feed.

use punktfunk_core::packet::FLAG_SOF;
use punktfunk_core::quic::ColorInfo;
use punktfunk_core::session::Frame;

/// H.273's unspecified transfer: the stream names no colour of its own.
const TRC_UNSPECIFIED: u8 = 2;

/// The AU open in the codec: pieces queued, the rest awaited. Loop-local: a codec rebuild ends
/// the loop, so the ledger never outlives the codec it fed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PartFeed {
    pub(crate) index: u32,
    /// The AU byte offset the next piece must carry. A mismatch means the hand-off dropped a
    /// piece (memory cap, jump-to-live clear) and the AU is unrecoverable.
    pub(crate) expected: usize,
    /// The pts of the empty close buffer. The codec then emits concealed output at that pts,
    /// which core's freeze withholds from glass until the recovery keyframe re-anchors.
    pub(crate) pts_us: u64,
}

/// One delivery as the ledger sees it. A whole AU is a first and last piece at offset 0.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Piece {
    pub(crate) index: u32,
    pub(crate) first: bool,
    pub(crate) last: bool,
    /// A piece of a sliced AU, not a whole one.
    pub(crate) partial: bool,
    pub(crate) offset: usize,
    pub(crate) pts_us: u64,
    /// The head of a keyframe: needs no reference.
    pub(crate) keyframe: bool,
}

impl Piece {
    pub(crate) fn of(f: &Frame) -> Piece {
        let (first, last, offset) = match f.part {
            None => (true, true, 0),
            Some(p) => (p.first, p.last, p.offset as usize),
        };
        Piece {
            index: f.frame_index,
            first,
            last,
            partial: f.part.is_some(),
            offset,
            pts_us: f.pts_ns / 1000,
            keyframe: first && f.flags & u32::from(FLAG_SOF) != 0,
        }
    }

    /// The ledger once `len` bytes of this piece are in the codec: open until the last one.
    pub(crate) fn queued(&self, len: usize) -> Option<PartFeed> {
        (!self.last).then_some(PartFeed {
            index: self.index,
            expected: self.offset + len,
            pts_us: self.pts_us,
        })
    }
}

/// What the ledger allows for the next piece.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// Feed it.
    Go,
    /// The open AU died mid-flight: close it, then judge this piece again.
    Close(PartFeed),
    /// A later piece whose head was lost upstream: discard it and wait for the next `first`.
    Orphan,
}

/// Judge `p` against the open AU. `Close` takes the ledger: that AU is no longer open.
pub(crate) fn gate(open: &mut Option<PartFeed>, p: &Piece) -> Gate {
    if open.is_some_and(|o| p.index == o.index && p.offset == o.expected && !p.first) {
        return Gate::Go;
    }
    match open.take() {
        Some(o) => Gate::Close(o),
        None if p.first => Gate::Go,
        None => Gate::Orphan,
    }
}

/// Why a piece did not reach the codec.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Cause {
    /// Larger than the codec's input buffer.
    Oversize,
    /// The codec refused the queue call.
    Refused,
}

/// What a piece that did not reach the codec leaves behind.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Spoiled {
    /// Queued pieces can't be unqueued: the ledger is poisoned, so the next delivery takes the
    /// [`Gate::Close`] path. `freeze_now`: core freezes at once, not at that close.
    Poison { ledger: PartFeed, freeze_now: bool },
    /// A whole AU lost, as for any other loss.
    Lost,
}

pub(crate) fn spoiled(p: &Piece, cause: Cause) -> Spoiled {
    let poisons = match cause {
        Cause::Oversize => p.partial,
        Cause::Refused => p.partial && !p.last,
    };
    if !poisons {
        return Spoiled::Lost;
    }
    Spoiled::Poison {
        ledger: PartFeed {
            index: p.index,
            expected: usize::MAX,
            pts_us: p.pts_us,
        },
        freeze_now: matches!(cause, Cause::Oversize),
    }
}

/// The picture size after a fed keyframe: what an AV1 keyframe's `render_size` states, which
/// it parses only on an AV1 session. A keyframe that states none clears it.
pub(crate) fn render_after(
    prev: Option<(i32, i32)>,
    av1: bool,
    stated: impl FnOnce() -> Option<(u32, u32)>,
) -> Option<(i32, i32)> {
    if !av1 {
        return prev;
    }
    stated().map(|(w, h)| (w as i32, h as i32))
}

/// The stream's colour after a fed keyframe: what an AV1 sequence header states, which it parses
/// only on an AV1 session. A keyframe that names none keeps the last word.
pub(crate) fn colour_after(
    prev: Option<ColorInfo>,
    av1: bool,
    stated: impl FnOnce() -> Option<ColorInfo>,
) -> Option<ColorInfo> {
    if !av1 {
        return prev;
    }
    stated().filter(|c| c.transfer != TRC_UNSPECIFIED).or(prev)
}

#[cfg(test)]
mod tests {
    use super::{colour_after, gate, render_after, spoiled, Cause, Gate, PartFeed, Piece, Spoiled};
    use punktfunk_core::packet::FLAG_SOF;
    use punktfunk_core::quic::ColorInfo;
    use punktfunk_core::session::{Frame, FramePart};

    /// A header states PQ or a flip back to SDR; one that names nothing keeps the last word, and
    /// only AV1 parses for it.
    #[test]
    fn a_keyframe_states_the_stream_colour() {
        let pq = ColorInfo::HDR10_BT2020_PQ;
        let sdr = ColorInfo::SDR_BT709;
        let unnamed = ColorInfo { transfer: 2, ..sdr };
        assert_eq!(colour_after(None, true, || Some(pq)), Some(pq));
        assert_eq!(colour_after(Some(pq), true, || Some(sdr)), Some(sdr));
        assert_eq!(colour_after(Some(pq), true, || Some(unnamed)), Some(pq));
        assert_eq!(colour_after(Some(pq), true, || None), Some(pq));
        assert_eq!(colour_after(None, true, || Some(unnamed)), None);
        assert_eq!(colour_after(None, false, || unreachable!()), None);
    }

    /// A piece of AU 7 at `offset`.
    fn piece(first: bool, last: bool, offset: usize) -> Piece {
        Piece {
            index: 7,
            first,
            last,
            partial: true,
            offset,
            pts_us: 70,
            keyframe: false,
        }
    }

    fn whole() -> Piece {
        Piece {
            partial: false,
            ..piece(true, true, 0)
        }
    }

    fn open(index: u32, expected: usize) -> Option<PartFeed> {
        Some(PartFeed {
            index,
            expected,
            pts_us: 10,
        })
    }

    fn frame(flags: u32, part: Option<FramePart>) -> Frame {
        Frame {
            data: vec![0; 4].into(),
            frame_index: 7,
            pts_ns: 70_000,
            flags,
            epoch: 0,
            stream: 0,
            complete: true,
            part,
            received_ns: 0,
            new_format: None,
        }
    }

    #[test]
    fn a_frame_becomes_a_piece() {
        let sof = u32::from(FLAG_SOF);
        let p = Piece::of(&frame(sof, None));
        assert!(p.first && p.last && !p.partial && p.keyframe);
        assert_eq!((p.index, p.offset, p.pts_us), (7, 0, 70));
        let part = |first, last| FramePart {
            offset: 300,
            first,
            last,
        };
        let p = Piece::of(&frame(0, Some(part(false, true))));
        assert!(!p.first && p.last && p.partial && !p.keyframe);
        assert_eq!(p.offset, 300);
        // The flag rides every piece of a keyframe; only its head is one.
        assert!(!Piece::of(&frame(sof, Some(part(false, false)))).keyframe);
        assert!(Piece::of(&frame(sof, Some(part(true, false)))).keyframe);
    }

    #[test]
    fn the_ledger_judges_each_piece_against_the_open_au() {
        // (open, piece, verdict, ledger after)
        let a = open(7, 100);
        let rows = [
            // Nothing open: a first piece or a whole AU goes in, a later piece lost its head.
            (None, piece(true, false, 0), Gate::Go, None),
            (None, whole(), Gate::Go, None),
            (None, piece(false, false, 100), Gate::Orphan, None),
            // The exact next piece continues the AU.
            (a, piece(false, false, 100), Gate::Go, a),
            (a, piece(false, true, 100), Gate::Go, a),
            // A gap, a repeat or another AU's piece abandons it.
            (a, piece(false, false, 200), Gate::Close(a.unwrap()), None),
            (a, piece(false, false, 0), Gate::Close(a.unwrap()), None),
            (
                open(6, 100),
                piece(false, false, 100),
                Gate::Close(open(6, 100).unwrap()),
                None,
            ),
            // A first piece never continues, even at the expected offset of the same index.
            (a, piece(true, false, 100), Gate::Close(a.unwrap()), None),
            (a, whole(), Gate::Close(a.unwrap()), None),
        ];
        for (mut ledger, p, verdict, after) in rows {
            assert_eq!(gate(&mut ledger, &p), verdict, "{p:?}");
            assert_eq!(ledger, after, "{p:?}");
        }
    }

    #[test]
    fn a_piece_leaves_the_ledger_open_until_the_last() {
        assert_eq!(whole().queued(50), None);
        assert_eq!(piece(false, true, 100).queued(50), None);
        assert_eq!(
            piece(true, false, 0).queued(50),
            Some(PartFeed {
                index: 7,
                expected: 50,
                pts_us: 70
            })
        );
        assert_eq!(piece(false, false, 50).queued(50).unwrap().expected, 100);
    }

    #[test]
    fn a_piece_that_cannot_go_in_poisons_the_ledger_only_when_pieces_are_queued() {
        let poison = |freeze_now| Spoiled::Poison {
            ledger: PartFeed {
                index: 7,
                expected: usize::MAX,
                pts_us: 70,
            },
            freeze_now,
        };
        // A whole AU is a plain loss either way.
        assert_eq!(spoiled(&whole(), Cause::Oversize), Spoiled::Lost);
        assert_eq!(spoiled(&whole(), Cause::Refused), Spoiled::Lost);
        // Oversize poisons for any piece, and freezes at once.
        for (first, last) in [(true, false), (false, false), (false, true)] {
            assert_eq!(
                spoiled(&piece(first, last, 0), Cause::Oversize),
                poison(true)
            );
        }
        // A refused first piece poisons a ledger nothing was queued under: the close at the next
        // delivery is an empty buffer the codec never needed, and arms the freeze this loss owes.
        assert_eq!(
            spoiled(&piece(true, false, 0), Cause::Refused),
            poison(false)
        );
        // A refused middle piece poisons, and the close at the next delivery freezes.
        assert_eq!(
            spoiled(&piece(false, false, 100), Cause::Refused),
            poison(false)
        );
        // A refused last piece leaves the ledger as it was: that close abandons the AU.
        assert_eq!(
            spoiled(&piece(false, true, 100), Cause::Refused),
            Spoiled::Lost
        );
    }

    /// The codec refuses piece 2 of 4. Piece 3 closes the AU and is then an orphan with the rest,
    /// until the next AU's first piece goes in.
    #[test]
    fn a_refused_piece_costs_its_au_and_the_feed_resyncs_at_the_next_first() {
        let mut ledger = None;
        let mut verdicts = Vec::new();
        let pieces = [
            piece(true, false, 0),
            piece(false, false, 100),
            piece(false, false, 200),
            piece(false, true, 300),
            Piece {
                index: 8,
                ..piece(true, false, 0)
            },
        ];
        for p in &pieces {
            // A closed AU hands the piece back to be judged again.
            loop {
                match gate(&mut ledger, p) {
                    Gate::Go if p.offset == 100 => {
                        verdicts.push("go");
                        if let Spoiled::Poison { ledger: l, .. } = spoiled(p, Cause::Refused) {
                            ledger = Some(l);
                        }
                    }
                    Gate::Go => {
                        verdicts.push("go");
                        ledger = p.queued(100);
                    }
                    Gate::Close(_) => {
                        verdicts.push("close");
                        continue;
                    }
                    Gate::Orphan => verdicts.push("orphan"),
                }
                break;
            }
        }
        assert_eq!(verdicts, ["go", "go", "close", "orphan", "orphan", "go"]);
    }

    #[test]
    fn an_av1_keyframe_states_the_visible_picture() {
        let old = Some((1280, 720));
        // A fed AV1 keyframe replaces what the last one said.
        assert_eq!(
            render_after(old, true, || Some((1920, 1080))),
            Some((1920, 1080))
        );
        // Another codec has none, and never reads one.
        assert_eq!(render_after(None, false, || unreachable!()), None);
        // pins today: a keyframe whose header the parser cannot read drops the crop, so the
        // padding rows show until the next keyframe states one.
        assert_eq!(render_after(old, true, || None), None);
    }
}
