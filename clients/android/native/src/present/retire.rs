//! The ASC retire rule: which presents a transaction's completion frees from SurfaceFlinger, and
//! behind which release fence. Generic over the sink's present record and its fence; the sink
//! owns the reader images and does each release.
//!
//! A completion's release fence frees the buffer its transaction replaced. The sink deletes that
//! image behind the fence, so the reader reuses it only once SurfaceFlinger is done with it; an
//! image freed without one returns at once. SurfaceFlinger holds its own reference from
//! `setBuffer`, so an early free tears at worst and never faults.
//!
//! `CASES` replays completion traces and pins each outcome. Design:
//! `design/android-native-overhaul.md` F3 (planning repo).

use std::collections::VecDeque;

/// The presents SurfaceFlinger holds, oldest first, each under its transaction's seq.
pub(crate) struct Retire<I> {
    held: VecDeque<(u64, I)>,
}

impl<I> Default for Retire<I> {
    fn default() -> Self {
        Retire {
            held: VecDeque::new(),
        }
    }
}

impl<I> Retire<I> {
    /// One transaction applied; seqs only grow.
    pub(crate) fn push(&mut self, seq: u64, present: I) {
        self.held.push_back((seq, present));
    }

    /// The held presents, oldest first.
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = &I> + DoubleEndedIterator {
        self.held.iter().map(|(_, p)| p)
    }

    /// The present a completion for `seq` reports on, while it is held.
    pub(crate) fn get_mut(&mut self, seq: u64) -> Option<&mut I> {
        self.held
            .iter_mut()
            .find(|(s, _)| *s == seq)
            .map(|(_, p)| p)
    }

    /// One completion for `seq` and its release fence. Every older present is replaced: the
    /// newest of them is freed behind `fence`, any older straggler at once. Returns each freed
    /// present with the fence to delete it behind (`None`: at once).
    pub(crate) fn complete<F>(&mut self, seq: u64, fence: Option<F>) -> Vec<(I, Option<F>)> {
        let older = self.held.iter().take_while(|(s, _)| *s < seq).count();
        let mut freed: Vec<_> = self.held.drain(..older).map(|(_, p)| (p, None)).collect();
        if let Some(newest) = freed.last_mut() {
            newest.1 = fence;
        }
        freed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One completion. A fence is named by the seq whose completion carried it.
    #[derive(Clone, Copy)]
    enum Ev {
        /// With a release fence.
        Fenced(u64),
        /// Without one.
        Bare(u64),
    }

    /// What the sink does.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Out {
        /// Image `.0` freed behind the fence completion `.1` carried.
        Behind(u64, u64),
        /// Image freed at once.
        Now(u64),
        /// A completion that found no present: its sample is lost.
        Lost(u64),
    }

    use Ev::{Bare, Fenced};
    use Out::{Behind, Lost, Now};

    /// Presents 0..=3 applied, then `script` replayed the way the sink calls the model: the
    /// sample, then the frees. Returns what the sink did and the presents still held.
    fn replay(script: &[Ev]) -> (Vec<Out>, Vec<u64>) {
        let mut r = Retire::default();
        (0..=3).for_each(|seq| r.push(seq, seq));
        let mut out = Vec::new();
        for &ev in script {
            let (seq, fence) = match ev {
                Fenced(seq) => (seq, Some(seq)),
                Bare(seq) => (seq, None),
            };
            if r.get_mut(seq).is_none() {
                out.push(Lost(seq));
            }
            out.extend(r.complete(seq, fence).into_iter().map(|f| match f {
                (image, Some(by)) => Behind(image, by),
                (image, None) => Now(image),
            }));
        }
        (out, r.iter().copied().collect())
    }

    /// Name, completions, what the sink does, presents still held.
    type Case = (&'static str, &'static [Ev], &'static [Out], &'static [u64]);

    const CASES: &[Case] = &[
        (
            "the first completion has nothing older to free",
            &[Fenced(0)],
            &[],
            &[0, 1, 2, 3],
        ),
        (
            "in order, each completion frees its predecessor behind its fence",
            &[Fenced(0), Fenced(1), Fenced(2), Fenced(3)],
            &[Behind(0, 1), Behind(1, 2), Behind(2, 3)],
            &[3],
        ),
        (
            "in order, a completion without a fence frees its predecessor at once",
            &[Fenced(0), Bare(1)],
            &[Now(0)],
            &[1, 2, 3],
        ),
        (
            "in order, a coalesced pair: the first frees the on-glass image, the second its twin",
            &[Fenced(0), Fenced(1), Bare(2)],
            &[Behind(0, 1), Now(1)],
            &[2, 3],
        ),
        (
            "R2-2 today: a reordered coalesced pair frees the on-glass image at once, loses 1",
            &[Fenced(0), Bare(2), Fenced(1)],
            &[Now(0), Now(1), Lost(1)],
            &[2, 3],
        ),
        (
            "R2-2 today: a reordered pair across frames frees 0 at once, loses 1",
            &[Fenced(0), Fenced(2), Fenced(1)],
            &[Now(0), Behind(1, 2), Lost(1)],
            &[2, 3],
        ),
        (
            "a missing completion: the next one frees its predecessor at once",
            &[Fenced(0), Fenced(2), Fenced(3)],
            &[Now(0), Behind(1, 2), Behind(2, 3)],
            &[3],
        ),
    ];

    #[test]
    fn completions_free_what_the_table_says() {
        for &(name, script, frees, held) in CASES {
            assert_eq!(replay(script), (frees.to_vec(), held.to_vec()), "{name}");
        }
    }
}
