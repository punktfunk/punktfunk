//! A decoder that returns each picture when the next AU is fed, and the tail that may end the
//! wait: the bytes ([`Tail`]) and the trial that reads whether one works ([`Trial`]). No codec:
//! `decode/async_loop.rs` feeds the tail and reports each output.

use punktfunk_core::quic::{CODEC_H264, CODEC_HEVC};

/// What follows each whole AU in the codec's input buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tail {
    None,
    /// A filler-data NAL: part of the AU, it ends the last slice for a parser that waits on the
    /// next start code.
    Fill,
    /// The next AU's delimiter, for a decoder that finishes a picture when the next one starts.
    /// A decoder that stamps an AU from the buffer it starts in then returns each picture under
    /// the pts before its own, which reads as no gain.
    Aud,
}

impl Tail {
    /// The tail `debug.punktfunk.au_tail` names: `off`, `fill` or `aud`.
    pub(crate) fn named(name: &str) -> Option<Tail> {
        match name {
            "off" => Some(Tail::None),
            "fill" => Some(Tail::Fill),
            "aud" => Some(Tail::Aud),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Tail::None => "none",
            Tail::Fill => "fill",
            Tail::Aud => "aud",
        }
    }

    /// The bit that blocks this tail from a later trial. `0` for none.
    pub(crate) fn bit(self) -> u8 {
        match self {
            Tail::None => 0,
            Tail::Fill => 1,
            Tail::Aud => 2,
        }
    }

    /// One NAL behind a four-byte start code. Empty for no tail, and for AV1.
    pub(crate) fn bytes(self, codec: u8) -> &'static [u8] {
        match (codec, self) {
            (CODEC_HEVC, Tail::Fill) => &[0, 0, 0, 1, 0x4C, 0x01, 0xFF, 0x80],
            (CODEC_HEVC, Tail::Aud) => &[0, 0, 0, 1, 0x46, 0x01, 0x50],
            (CODEC_H264, Tail::Fill) => &[0, 0, 0, 1, 0x0C, 0xFF, 0x80],
            (CODEC_H264, Tail::Aud) => &[0, 0, 0, 1, 0x09, 0xF0],
            _ => &[],
        }
    }
}

/// Outputs in one block of a reading, all fed the same way.
const BLOCK: u32 = 30;
/// Outputs skipped at the head of a block: their AUs were fed in the block before.
const SKIP: u32 = 6;
/// Blocks in a reading. A tail's reading alternates with and without it, so a rate that moves
/// meanwhile moves both halves.
const BLOCKS: u32 = 16;

/// One reading: the median codec time fed `with` the tail and `without`, over `gap_us` between
/// AUs. The first reading has no tail and its halves are one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) tail: Tail,
    pub(crate) with_us: u64,
    pub(crate) without_us: u64,
    pub(crate) gap_us: u64,
    /// The tail fed from here, and whether the trial is over.
    pub(crate) next: Tail,
    pub(crate) done: bool,
}

/// Finds the tail that ends a decoder's hold. A held picture's codec time is at least the gap
/// to the next AU, so a decoder under the gap is left alone. Over it, each tail is read in
/// turn, blocks with it against blocks without: one that takes half a gap off stays.
pub(crate) struct Trial {
    /// The tail under reading, or the one kept.
    tail: Tail,
    left: Vec<Tail>,
    seen: u32,
    with: Vec<u64>,
    without: Vec<u64>,
    gaps: Vec<u64>,
    done: bool,
}

fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

impl Trial {
    /// A trial over every tail whose [`Tail::bit`] is clear in `blocked`.
    pub(crate) fn new(blocked: u8) -> Trial {
        Trial {
            tail: Tail::None,
            left: [Tail::Fill, Tail::Aud]
                .into_iter()
                .filter(|t| t.bit() & blocked == 0)
                .collect(),
            seen: 0,
            with: Vec::new(),
            without: Vec::new(),
            gaps: Vec::new(),
            done: false,
        }
    }

    /// No trial: `tail` for the whole run.
    pub(crate) fn fixed(tail: Tail) -> Trial {
        Trial {
            tail,
            done: true,
            ..Trial::new(u8::MAX)
        }
    }

    /// Even blocks of a reading carry its tail.
    fn on(&self) -> bool {
        (self.seen / BLOCK) % 2 == 0
    }

    /// The tail to feed now.
    pub(crate) fn tail(&self) -> Tail {
        if self.done || self.on() {
            self.tail
        } else {
            Tail::None
        }
    }

    /// One picture out: its codec time, and the gap between its AU and the one before.
    /// `Some` closes a reading.
    pub(crate) fn note(&mut self, codec_us: u64, gap_us: u64) -> Option<Reading> {
        if self.done {
            return None;
        }
        if self.seen % BLOCK >= SKIP {
            if self.on() {
                self.with.push(codec_us);
            } else {
                self.without.push(codec_us);
            }
            self.gaps.push(gap_us);
        }
        self.seen += 1;
        if self.seen < BLOCK * BLOCKS {
            return None;
        }
        let tail = self.tail;
        let (with_us, without_us) = (median(&mut self.with), median(&mut self.without));
        let gap_us = median(&mut self.gaps);
        let keep = match tail {
            // No tail on either half: under the gap nothing is held.
            Tail::None => with_us.max(without_us) < gap_us,
            _ => with_us + gap_us / 2 <= without_us,
        };
        if keep || self.left.is_empty() {
            self.done = true;
            if !keep {
                self.tail = Tail::None;
            }
        } else {
            self.tail = self.left.remove(0);
        }
        self.seen = 0;
        self.with.clear();
        self.without.clear();
        self.gaps.clear();
        Some(Reading {
            tail,
            with_us,
            without_us,
            gap_us,
            next: self.tail,
            done: self.done,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::CODEC_AV1;

    const GAP: u64 = 16_667;

    /// Each tail is one NAL of the type its name says: HEVC filler is type 38 and its
    /// delimiter 35, H.264's are 12 and 9.
    #[test]
    fn a_tail_is_one_filler_or_delimiter_nal() {
        let hevc = |tail: Tail| {
            let t = tail.bytes(CODEC_HEVC);
            assert_eq!((&t[..4], t[5] & 7), (&[0, 0, 0, 1][..], 1), "{tail:?}");
            (t[4] >> 1) & 0x3F
        };
        assert_eq!((hevc(Tail::Fill), hevc(Tail::Aud)), (38, 35));
        let h264 = |tail: Tail| {
            let t = tail.bytes(CODEC_H264);
            assert_eq!(&t[..4], &[0, 0, 0, 1], "{tail:?}");
            t[4] & 0x1F
        };
        assert_eq!((h264(Tail::Fill), h264(Tail::Aud)), (12, 9));
        assert!(Tail::None.bytes(CODEC_HEVC).is_empty());
        assert!(Tail::Aud.bytes(CODEC_AV1).is_empty());
        for tail in [Tail::None, Tail::Fill, Tail::Aud] {
            let name = if tail == Tail::None {
                "off"
            } else {
                tail.label()
            };
            assert_eq!(Tail::named(name), Some(tail));
        }
        assert_eq!(Tail::named("on"), None);
    }

    /// One whole reading on a decoder that takes `held_us` a picture, and `freed_us` when fed
    /// the tail in `frees`.
    fn read(trial: &mut Trial, held_us: u64, freed_us: u64, frees: Tail) -> Reading {
        (0..BLOCK * BLOCKS)
            .find_map(|_| {
                let fed = trial.tail();
                let us = if fed == frees && fed != Tail::None {
                    freed_us
                } else {
                    held_us
                };
                trial.note(us, GAP)
            })
            .expect("a reading closes")
    }

    /// 9 ms a picture against a 16.7 ms gap: nothing is held, and nothing is tried.
    #[test]
    fn a_decoder_under_the_gap_is_left_alone() {
        let mut trial = Trial::new(0);
        let r = read(&mut trial, 9_000, 9_000, Tail::None);
        assert_eq!((r.tail, r.next, r.done), (Tail::None, Tail::None, true));
        assert_eq!(trial.note(40_000, GAP), None, "over");
        assert_eq!(trial.tail(), Tail::None);
    }

    /// 30 ms a picture, 14 ms under the delimiter: the filler is read and dropped, the
    /// delimiter read and kept, on every AU from there.
    #[test]
    fn a_hold_tries_each_tail_and_keeps_the_one_that_frees_it() {
        let mut trial = Trial::new(0);
        let r = read(&mut trial, 30_000, 14_000, Tail::Aud);
        assert_eq!((r.tail, r.next, r.done), (Tail::None, Tail::Fill, false));
        let r = read(&mut trial, 30_000, 14_000, Tail::Aud);
        assert_eq!((r.tail, r.next, r.done), (Tail::Fill, Tail::Aud, false));
        assert_eq!((r.with_us, r.without_us), (30_000, 30_000));
        let r = read(&mut trial, 30_000, 14_000, Tail::Aud);
        assert_eq!((r.tail, r.next, r.done), (Tail::Aud, Tail::Aud, true));
        assert_eq!((r.with_us, r.without_us), (14_000, 30_000));
        assert!((0..100).all(|_| {
            trial.note(14_000, GAP);
            trial.tail() == Tail::Aud
        }));
    }

    /// A reading alternates blocks with the tail and without, the first outputs of each
    /// skipped: a rate that halves the codec time mid-reading moves both halves alike.
    #[test]
    fn a_rate_that_moves_during_a_reading_frees_nothing() {
        let mut trial = Trial::new(0);
        read(&mut trial, 30_000, 30_000, Tail::None);
        let mut fed = Vec::new();
        let r = (0..BLOCK * BLOCKS)
            .find_map(|i| {
                fed.push(trial.tail());
                let us = if i < BLOCK * BLOCKS / 2 {
                    40_000
                } else {
                    20_000
                };
                trial.note(us, GAP)
            })
            .expect("a reading");
        assert_eq!((r.tail, r.next), (Tail::Fill, Tail::Aud), "not kept");
        let block = |n: u32| &fed[(n * BLOCK) as usize..((n + 1) * BLOCK) as usize];
        assert!(block(0).iter().all(|&t| t == Tail::Fill));
        assert!(block(1).iter().all(|&t| t == Tail::None));
    }

    /// No tail frees it, or the ones that might are blocked: back to none.
    #[test]
    fn a_hold_no_tail_frees_ends_on_none() {
        let mut trial = Trial::new(0);
        for _ in 0..2 {
            assert!(!read(&mut trial, 30_000, 30_000, Tail::None).done);
        }
        let r = read(&mut trial, 30_000, 30_000, Tail::None);
        assert_eq!((r.tail, r.next, r.done), (Tail::Aud, Tail::None, true));

        let mut trial = Trial::new(Tail::Fill.bit() | Tail::Aud.bit());
        let r = read(&mut trial, 30_000, 30_000, Tail::None);
        assert_eq!((r.next, r.done), (Tail::None, true));
    }

    #[test]
    fn a_fixed_tail_runs_no_trial() {
        let mut trial = Trial::fixed(Tail::Fill);
        assert!((0..1000).all(|_| trial.note(30_000, GAP).is_none() && trial.tail() == Tail::Fill));
    }
}
