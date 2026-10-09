//! Unclean-picture marks for one planner, shared by H.264, H.265, and AV1.
//!
//! A picture is unclean when its AU needed concealment, or when anything it
//! predicted from was unclean. Descendants inherit the mark even if their own
//! plan was clean. The host's `USER_FLAG_RECOVERY_ANCHOR` infers from what the
//! client received; this ledger is what the client decoded.
//!
//! Forgotten ids (evicted from the DPB, dropped at a flush) read as clean: they
//! cannot be referenced. A resident mark never clears, so the ledger can only
//! make a consumer more conservative.
//!
//! Keyed by the planner's `PicId` (`u64` in all three codecs). Empty on a healthy
//! stream. Tests in this module pin the propagation and reuse rules.

use std::collections::BTreeSet;

#[derive(Debug, Clone, Default)]
pub struct CleanLedger {
    unclean: BTreeSet<u64>,
}

impl CleanLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// True iff every id is clean. An empty list is vacuously true, so an IRAP/IDR
    /// (predicts from nothing) is clean by construction.
    pub fn references_clean<I>(&self, references: I) -> bool
    where
        I: IntoIterator<Item = u64>,
    {
        self.unclean.is_empty() || !references.into_iter().any(|id| self.unclean.contains(&id))
    }

    /// Concealment or an unclean reference marks `id`. Descendants inherit via
    /// [`Self::references_clean`].
    pub fn note_stored(&mut self, id: u64, references_clean: bool, concealed: bool) {
        if references_clean && !concealed {
            // Planners reuse PicIds after recycle; a stale mark would condemn the
            // fresh picture.
            self.unclean.remove(&id);
        } else {
            self.unclean.insert(id);
        }
    }

    /// Drop marks for ids not in `live`. Safe because a picture outside the DPB
    /// cannot appear in a later reference list.
    pub fn retain_live<I>(&mut self, live: I)
    where
        I: IntoIterator<Item = u64>,
    {
        if self.unclean.is_empty() {
            return;
        }
        let live: BTreeSet<u64> = live.into_iter().collect();
        self.unclean.retain(|id| live.contains(id));
    }

    /// Drop every mark: the DPB was drained (flush or stream discontinuity).
    pub fn clear(&mut self) {
        self.unclean.clear();
    }

    /// Drop one picture's mark: an intra refresh wave healed it by overwrite, which no
    /// reference chain shows. Every other resident mark stays, so a later picture that
    /// still predicts from the half-swept ones reads damaged.
    pub fn forgive(&mut self, id: u64) {
        self.unclean.remove(&id);
    }

    /// Diagnostics and tests. The plan path uses [`Self::references_clean`].
    pub fn is_unclean(&self, id: u64) -> bool {
        self.unclean.contains(&id)
    }

    pub fn unclean_count(&self) -> usize {
        self.unclean.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn damage_propagates_to_every_descendant() {
        let mut led = CleanLedger::new();
        assert!(led.references_clean([]));
        led.note_stored(0, true, false);
        assert!(!led.is_unclean(0));

        assert!(led.references_clean([0]));
        led.note_stored(1, true, false);

        let refs_clean = led.references_clean([1]);
        assert!(refs_clean, "its reference was still fine");
        led.note_stored(2, refs_clean, true);
        assert!(led.is_unclean(2));

        let refs_clean = led.references_clean([2]);
        assert!(!refs_clean, "the chain is broken from here down");
        led.note_stored(3, refs_clean, false);
        assert!(led.is_unclean(3), "3 inherited 2's damage");

        let refs_clean = led.references_clean([3]);
        assert!(!refs_clean);
        led.note_stored(4, refs_clean, false);
        assert!(led.is_unclean(4));
    }

    #[test]
    fn one_unclean_reference_is_enough() {
        let mut led = CleanLedger::new();
        led.note_stored(0, true, false);
        led.note_stored(1, true, true);
        assert!(!led.references_clean([0, 1]));
        assert!(!led.references_clean([1, 0]), "order does not matter");
        assert!(led.references_clean([0]));
    }

    #[test]
    fn a_picture_with_no_references_is_clean_however_bad_the_stream_was() {
        let mut led = CleanLedger::new();
        led.note_stored(0, true, true);
        led.note_stored(1, false, false);
        assert_eq!(led.unclean_count(), 2);
        assert!(led.references_clean([]));
        led.note_stored(2, true, false);
        assert!(!led.is_unclean(2));
    }

    #[test]
    fn marks_are_dropped_when_their_picture_leaves_the_dpb() {
        let mut led = CleanLedger::new();
        led.note_stored(7, true, true);
        led.note_stored(8, false, false);
        assert_eq!(led.unclean_count(), 2);
        led.retain_live([8, 9]);
        assert!(!led.is_unclean(7), "7 was evicted");
        assert!(led.is_unclean(8), "8 is still resident and still damaged");
        assert_eq!(led.unclean_count(), 1);
    }

    #[test]
    fn a_flush_forgets_every_mark() {
        let mut led = CleanLedger::new();
        led.note_stored(1, true, true);
        led.note_stored(2, false, false);
        led.clear();
        assert_eq!(led.unclean_count(), 0);
        assert!(led.references_clean([1, 2]));
    }

    #[test]
    fn forgiving_the_swept_picture_keeps_the_half_swept_ones_marked() {
        let mut led = CleanLedger::new();
        led.note_stored(1, true, true);
        led.note_stored(2, false, false);
        led.note_stored(3, false, false);
        led.forgive(3);
        assert!(led.references_clean([3]), "the close reads clean");
        assert!(
            !led.references_clean([2]),
            "a mid-wave picture still reads damaged"
        );
        assert!(!led.references_clean([3, 1]));
    }

    #[test]
    fn a_reused_id_is_not_condemned_by_its_predecessors_mark() {
        let mut led = CleanLedger::new();
        led.note_stored(5, true, true);
        assert!(led.is_unclean(5));
        led.note_stored(5, true, false);
        assert!(!led.is_unclean(5));
        assert!(led.references_clean([5]));
    }

    #[test]
    fn a_stream_without_loss_never_marks_a_picture() {
        let mut led = CleanLedger::new();
        for id in 0..512u64 {
            let refs = if id == 0 { vec![] } else { vec![id - 1] };
            let clean = led.references_clean(refs.iter().copied());
            assert!(clean, "picture {id} must read clean");
            led.note_stored(id, clean, false);
            led.retain_live(id.saturating_sub(3)..=id);
        }
        assert_eq!(led.unclean_count(), 0);
    }
}
