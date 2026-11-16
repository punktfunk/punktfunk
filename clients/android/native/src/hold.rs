//! A decoder that returns each picture when the next AU is fed, and the tail that ends the
//! wait: the bytes ([`Tail`]) and the reading that keeps or drops it ([`Trial`]). No codec:
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

    /// The bit that blocks this tail from a later run. `0` for none.
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

/// Outputs in one block of the reading, all fed the same way.
const BLOCK: u32 = 30;
/// Outputs skipped at the head of a block: their AUs were fed in the block before.
const SKIP: u32 = 6;
/// Blocks in the reading. It alternates with and without the tail, so a rate that moves
/// meanwhile moves both halves.
const BLOCKS: u32 = 8;

/// The reading: the median codec time fed `with` the tail and `without`, over `gap_us` between
/// AUs, and the tail fed from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) tail: Tail,
    pub(crate) with_us: u64,
    pub(crate) without_us: u64,
    pub(crate) gap_us: u64,
    pub(crate) next: Tail,
}

/// Feeds the delimiter after every AU. One reading at the start runs blocks with it against
/// blocks without, for the log; the delimiter goes only if it adds a quarter gap. A hold comes
/// and goes within a run, so a reading that finds none keeps it.
pub(crate) struct Trial {
    /// The tail under reading, or the one kept.
    tail: Tail,
    seen: u32,
    with: Vec<u64>,
    without: Vec<u64>,
    gaps: Vec<u64>,
    done: bool,
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

impl Trial {
    /// The delimiter, read against no tail. None if `blocked` holds its [`Tail::bit`].
    pub(crate) fn new(blocked: u8) -> Trial {
        if blocked & Tail::Aud.bit() != 0 {
            return Trial::fixed(Tail::None);
        }
        Trial {
            done: false,
            ..Trial::fixed(Tail::Aud)
        }
    }

    /// No reading: `tail` for the whole run.
    pub(crate) fn fixed(tail: Tail) -> Trial {
        Trial {
            tail,
            seen: 0,
            with: Vec::new(),
            without: Vec::new(),
            gaps: Vec::new(),
            done: true,
        }
    }

    /// Even blocks of the reading carry its tail.
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
    /// `Some` closes the reading.
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
        let with_us = median(std::mem::take(&mut self.with));
        let without_us = median(std::mem::take(&mut self.without));
        let gap_us = median(std::mem::take(&mut self.gaps));
        if with_us > without_us + gap_us / 4 {
            self.tail = Tail::None;
        }
        self.done = true;
        Some(Reading {
            tail,
            with_us,
            without_us,
            gap_us,
            next: self.tail,
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

    /// The whole reading on a decoder that takes `held_us` a picture, and `aud_us` behind the
    /// delimiter.
    fn read(trial: &mut Trial, held_us: u64, aud_us: u64) -> Reading {
        (0..BLOCK * BLOCKS)
            .find_map(|_| {
                let us = if trial.tail() == Tail::Aud {
                    aud_us
                } else {
                    held_us
                };
                trial.note(us, GAP)
            })
            .expect("the reading closes")
    }

    /// 30 ms a picture, 14 ms behind the delimiter: kept on every AU from there.
    #[test]
    fn a_hold_the_delimiter_frees_keeps_it() {
        let mut trial = Trial::new(0);
        assert_eq!(trial.tail(), Tail::Aud, "the first block carries it");
        let r = read(&mut trial, 30_000, 14_000);
        assert_eq!(
            (r.tail, r.next, r.with_us, r.without_us),
            (Tail::Aud, Tail::Aud, 14_000, 30_000)
        );
        assert!((0..100).all(|_| trial.note(14_000, GAP).is_none() && trial.tail() == Tail::Aud));
    }

    /// 13 ms either way under a 16.7 ms gap: no hold now, and the delimiter stays for one that
    /// comes later.
    #[test]
    fn a_reading_without_a_hold_keeps_the_delimiter() {
        let r = read(&mut Trial::new(0), 13_000, 13_000);
        assert_eq!(r.next, Tail::Aud);
    }

    /// A delimiter that costs a picture goes, and a blocked one is never fed.
    #[test]
    fn a_delimiter_that_costs_goes() {
        let mut trial = Trial::new(0);
        assert_eq!(read(&mut trial, 14_000, 14_000 + GAP).next, Tail::None);
        assert_eq!(trial.tail(), Tail::None);

        let mut trial = Trial::new(Tail::Aud.bit());
        assert!((0..1000).all(|_| trial.note(30_000, GAP).is_none() && trial.tail() == Tail::None));
    }

    /// The reading alternates blocks with the tail and without, the first outputs of each
    /// skipped: a rate that halves the codec time mid-reading moves both halves alike.
    #[test]
    fn a_rate_that_moves_during_the_reading_costs_nothing() {
        let mut trial = Trial::new(0);
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
            .expect("the reading");
        assert_eq!(
            (r.with_us, r.without_us, r.next),
            (40_000, 40_000, Tail::Aud)
        );
        let block = |n: u32| &fed[(n * BLOCK) as usize..((n + 1) * BLOCK) as usize];
        assert!(block(0).iter().all(|&t| t == Tail::Aud));
        assert!(block(1).iter().all(|&t| t == Tail::None));
    }

    #[test]
    fn a_fixed_tail_runs_no_trial() {
        let mut trial = Trial::fixed(Tail::Fill);
        assert!((0..1000).all(|_| trial.note(30_000, GAP).is_none() && trial.tail() == Tail::Fill));
    }
}
