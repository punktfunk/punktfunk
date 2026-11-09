//! The ASC retire rule: which presents a transaction's completion frees from SurfaceFlinger, and
//! behind which release fence. Generic over the sink's present record and its fence; the sink
//! owns the reader images and does each release.
//!
//! A completion's release fence frees the buffer its transaction replaced: the present before it
//! in seq order. The sink deletes that image behind the fence, so the reader reuses it only once
//! SurfaceFlinger is done with it; an image freed without one returns at once. Completions take
//! effect in seq order, so one that arrives early waits for the older one. SurfaceFlinger holds
//! its own reference from `setBuffer`, so an early free tears at worst and never faults.
//!
//! `CASES` replays completion traces and pins each outcome. Design:
//! `design/android-native-overhaul.md` F3 (planning repo).

use std::collections::VecDeque;

/// One present SurfaceFlinger holds.
struct Entry<I, F> {
    seq: u64,
    present: I,
    /// `Some` once its completion arrived, with the release fence it carried until that fence
    /// frees the present before it.
    done: Option<Option<F>>,
}

/// The presents SurfaceFlinger holds, oldest first. A done front is the one on glass.
pub(crate) struct Retire<I, F> {
    held: VecDeque<Entry<I, F>>,
}

impl<I, F> Default for Retire<I, F> {
    fn default() -> Self {
        Retire {
            held: VecDeque::new(),
        }
    }
}

impl<I, F> Retire<I, F> {
    /// One transaction applied; seqs only grow.
    pub(crate) fn push(&mut self, seq: u64, present: I) {
        self.held.push_back(Entry {
            seq,
            present,
            done: None,
        });
    }

    /// The held presents, oldest first.
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = &I> + DoubleEndedIterator {
        self.held.iter().map(|e| &e.present)
    }

    /// The present a completion for `seq` reports on, while it is held.
    pub(crate) fn get_mut(&mut self, seq: u64) -> Option<&mut I> {
        self.held
            .iter_mut()
            .find(|e| e.seq == seq)
            .map(|e| &mut e.present)
    }

    /// One completion for `seq` and the release fence it carried. Completions take effect in seq
    /// order: each frees the present before it behind its fence, at once without one. One that
    /// arrives before an older one waits for it; an older one still missing once two newer ones
    /// arrived never comes and takes effect without a fence, so at most one completed present
    /// waits behind a hole. Returns each freed present with the fence to delete it behind.
    pub(crate) fn complete(&mut self, seq: u64, fence: Option<F>) -> Vec<(I, Option<F>)> {
        if let Some(e) = self
            .held
            .iter_mut()
            .find(|e| e.seq == seq && e.done.is_none())
        {
            e.done = Some(fence);
        }
        let mut freed = Vec::new();
        loop {
            let done = |i: usize| self.held.get(i).is_some_and(|e| e.done.is_some());
            if done(0) && done(1) {
                let fence = self.held[1].done.as_mut().and_then(Option::take);
                freed.extend(self.held.pop_front().map(|e| (e.present, fence)));
            } else if let Some(hole) = self.lost() {
                self.held[hole].done = Some(None);
            } else {
                return freed;
            }
        }
    }

    /// The oldest present whose completion never comes: two newer ones already arrived.
    fn lost(&self) -> Option<usize> {
        let hole = self.held.iter().position(|e| e.done.is_none())?;
        let newer = self.held.iter().skip(hole + 1);
        (newer.filter(|e| e.done.is_some()).count() >= 2).then_some(hole)
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
            "R2-2: a reordered coalesced pair frees as it does in order",
            &[Fenced(0), Bare(2), Fenced(1)],
            &[Behind(0, 1), Now(1)],
            &[2, 3],
        ),
        (
            "R2-2: a reordered pair across frames frees as it does in order",
            &[Fenced(0), Fenced(2), Fenced(1)],
            &[Behind(0, 1), Behind(1, 2)],
            &[2, 3],
        ),
        (
            "R2-2: a completion ahead of an older one waits for it",
            &[Fenced(0), Fenced(2)],
            &[],
            &[0, 1, 2, 3],
        ),
        (
            "a missing completion ages out once two newer ones arrived, without a fence",
            &[Fenced(0), Fenced(2), Fenced(3)],
            &[Now(0), Behind(1, 2), Behind(2, 3)],
            &[3],
        ),
        (
            "a completion after its age-out frees nothing",
            &[Fenced(0), Fenced(2), Fenced(3), Fenced(1)],
            &[Now(0), Behind(1, 2), Behind(2, 3), Lost(1)],
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
