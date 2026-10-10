//! The [`DpbUpdate`] ledger the H.264 and H.265 planners close each AU with.

use std::collections::BTreeSet;
use std::mem;

use crate::clean::CleanLedger;
use crate::h264::DpbUpdate;
use crate::h264::PicId;

/// What the backend was last told, plus the unclean marks behind
/// `references_clean`.
#[derive(Debug, Default)]
pub(crate) struct AuReporter {
    /// Display-ready pictures. Not cleared on a failed AU: the next
    /// [`DpbUpdate`] carries them, so an error cannot swallow a frame.
    pub(crate) pending_outputs: Vec<PicId>,
    /// Ids the last [`DpbUpdate`] left alive; baseline for `removed`. Kept
    /// across failed AUs so interim evictions are still reported.
    reported_live: BTreeSet<PicId>,
    /// The picture the last plan stored: the one a wave's close mark names.
    last_stored: Option<PicId>,
    /// Resident pictures off a broken chain. Empty on a healthy stream.
    clean: CleanLedger,
}

impl AuReporter {
    /// Whether this AU predicts only from clean pictures and needed no
    /// concealment itself. Ask over the slice lists, not the DPB snapshot: a
    /// resident picture this AU does not use must not taint it.
    pub(crate) fn references_clean(
        &self,
        references: impl IntoIterator<Item = PicId>,
        concealed: bool,
    ) -> bool {
        self.clean.references_clean(references) && !concealed
    }

    /// Close an AU that stored `stored`, leaving `live_after` resident.
    ///
    /// Call after the codec's marking, so `stored` and `live_after` include it:
    /// a mark written before an eviction could survive it. `removed` is against
    /// the last reported live set, not this call's start, because a failed AU
    /// in between may have evicted pictures that still need reporting.
    pub(crate) fn close(
        &mut self,
        stored: PicId,
        live_after: BTreeSet<PicId>,
        references_clean: bool,
        concealed: bool,
    ) -> DpbUpdate {
        let mut previously_live = mem::take(&mut self.reported_live);
        previously_live.insert(stored);
        let removed = previously_live.difference(&live_after).copied().collect();
        self.reported_live = live_after;
        self.last_stored = Some(stored);

        self.clean.note_stored(stored, references_clean, concealed);
        self.clean.retain_live(self.reported_live.iter().copied());

        DpbUpdate {
            stored: Some(stored),
            outputs: mem::take(&mut self.pending_outputs),
            removed,
        }
    }

    /// Report a drained DPB. `live` is the codec's resident set read before
    /// its drain queued the last outputs.
    pub(crate) fn flush(&mut self, live: BTreeSet<PicId>) -> DpbUpdate {
        let mut removed = mem::take(&mut self.reported_live);
        removed.extend(live);
        // DPB empty; the resuming picture is an IDR/IRAP, clean by construction.
        self.clean.clear();
        DpbUpdate {
            stored: None,
            outputs: mem::take(&mut self.pending_outputs),
            removed: removed.into_iter().collect(),
        }
    }

    /// Drop the mark on the picture the last plan stored (the planners'
    /// `forgive_unclean`).
    pub(crate) fn forgive_last(&mut self) {
        if let Some(id) = self.last_stored {
            self.clean.forgive(id);
        }
    }
}
