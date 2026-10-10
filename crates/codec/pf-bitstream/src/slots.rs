//! Maps planner [`PicId`]s to the DPB slot indices a decode backend binds
//! pictures by: a Vulkan Video slot, a DXVA `Index7Bits`, a VAAPI surface index.
//!
//! The planners own which pictures live; this ledger only translates those
//! verdicts. It never evicts: [`SlotError::Full`] means a `removed` entry was
//! missed. A silent eviction would hide that behind corrupted output.

use tracing::trace;

use crate::h264::DpbUpdate;
use crate::h264::PicId;

/// Extra pictures the consumer may hold (delivered, unreleased) on top of
/// the stream's DPB depth. Pool size is `required_slots + HOLD_HEADROOM`.
/// 8 covers ~4–7 in-flight frames with one frame of slack; holding more
/// is `NoFreeSlot`.
pub const HOLD_HEADROOM: u32 = 8;

/// The H.264 slot ceiling: 16 reference frames plus the picture being decoded.
const MAX_SLOTS: usize = 17;

/// Caller bugs, not stream conditions — pf-bitstream degrades stream damage
/// to warnings long before here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotError {
    /// Sized to `max_dpb_frames + 1`, which the planner never exceeds; overflow
    /// means this map missed a `removed` entry.
    Full { capacity: usize },
    /// The id already holds a slot; ids are per-picture and never re-assigned.
    AlreadyAssigned { id: PicId, slot: u8 },
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotError::Full { capacity } => {
                write!(
                    f,
                    "all {capacity} DPB slots are held — removals were missed"
                )
            }
            SlotError::AlreadyAssigned { id, slot } => {
                write!(f, "picture {id} already holds slot {slot}")
            }
        }
    }
}

impl std::error::Error for SlotError {}

/// When [`SlotMap::commit_setup`] ends the residency of an AU's `removed` ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removals {
    /// Return them for release after the decode is issued: an H.264 AU may
    /// name a picture its own marking evicts.
    Defer,
    /// Release them before the setup slot is assigned: the H.265 RPS drops
    /// pictures before decode, and VAAPI takes its target surface up front.
    ReleaseNow,
}

/// Per-session PicId → slot index. Feed every [`DpbUpdate`] in decode order.
///
/// [`Self::apply`] releases `removed` immediately. The Vulkan and DXVA H.264
/// and AV1 converters assign the stored picture and return removals as
/// `release_after_decode` — apply that list only after the decode is issued.
/// Releasing first lets [`Self::assign`] recycle a slot this AU still names.
/// Dropping the list leaks one slot per AU.
///
/// The H.265 converters apply removals internally: `H265Planner` snapshots
/// `dpb_refs` after `decode_rps`, so a dropped picture is never in this AU's
/// reference lists. The VAAPI converters do too: VAAPI binds surfaces, not
/// slots, and takes its target surface up front.
///
/// Slots are planner bookkeeping. The backend's picture pool binds a fresh
/// image on re-activation, so a delivered image is never a decode target
/// while a consumer reads it.
#[derive(Debug, Clone)]
pub struct SlotMap {
    slots: Vec<Option<PicId>>,
}

impl SlotMap {
    /// `max_dpb_frames + 1` so the setup slot coexists with a full reference window.
    ///
    /// Larger than the H.264 16-frame ceiling is a caller bug (the envelope gate
    /// already rejected it). Debug-asserted, never clamped: a clamp would become
    /// silent evictions later.
    pub fn new(max_dpb_frames: usize) -> Self {
        debug_assert!(
            max_dpb_frames < MAX_SLOTS,
            "a {max_dpb_frames}-frame DPB exceeds the H.264 ceiling pf-bitstream's \
             envelope gate enforces"
        );
        Self {
            slots: vec![None; max_dpb_frames + 1],
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    pub fn active(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    pub fn held(&self) -> impl Iterator<Item = (u8, PicId)> + '_ {
        self.slots
            .iter()
            .enumerate()
            // The envelope-gated capacity (<= 17) keeps every index within u8.
            .filter_map(|(index, slot)| slot.map(|id| (index as u8, id)))
    }

    pub fn assign(&mut self, id: PicId) -> Result<u8, SlotError> {
        if let Some(slot) = self.slot_of(id) {
            return Err(SlotError::AlreadyAssigned { id, slot });
        }
        let free = self
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(SlotError::Full {
                capacity: self.slots.len(),
            })?;
        self.slots[free] = Some(id);
        // The envelope-gated capacity (<= 17) keeps every index within u8.
        Ok(free as u8)
    }

    pub fn slot_of(&self, id: PicId) -> Option<u8> {
        self.slots
            .iter()
            .position(|slot| *slot == Some(id))
            // The envelope-gated capacity (<= 17) keeps every index within u8.
            .map(|index| index as u8)
    }

    /// End DPB residency for `id`. A picture holds its slot while the planner
    /// holds it as a reference or as a decoded picture awaiting output; only a
    /// [`DpbUpdate::removed`] entry ends that. The Vulkan and DXVA H.264 and
    /// AV1 converters defer this via `release_after_decode` until the decode
    /// is issued.
    ///
    /// The slot becomes assignable immediately. Keeping the IMAGE out of reuse
    /// until in-flight decodes complete is the backend's job, not this ledger's.
    pub fn release(&mut self, id: PicId) -> bool {
        match self.slots.iter().position(|slot| *slot == Some(id)) {
            Some(index) => {
                self.slots[index] = None;
                true
            }
            None => false,
        }
    }

    /// A converter's last step: end `removed` per `removals`, then give `setup`
    /// its slot. Returns that slot and the ids left for release after decode
    /// (empty for [`Removals::ReleaseNow`]).
    ///
    /// `setup` itself can be in `removed`: a non-reference picture with no free
    /// frame buffer is stored and evicted in one plan. It still gets a slot for
    /// the decode and is released at once, so it is never handed back as a
    /// deferred id.
    pub fn commit_setup(
        &mut self,
        setup: PicId,
        removed: &[PicId],
        removals: Removals,
    ) -> Result<(u8, Vec<PicId>), SlotError> {
        let others = removed.iter().copied().filter(|&id| id != setup);
        let release_after_decode = match removals {
            Removals::Defer => others.collect(),
            Removals::ReleaseNow => {
                for id in others {
                    if !self.release(id) {
                        // Reachable only if the caller skipped an AU's plan.
                        trace!(id, "DpbUpdate removed an id this SlotMap never assigned");
                    }
                }
                Vec::new()
            }
        };
        let slot = self.assign(setup)?;
        if removed.contains(&setup) {
            self.release(setup);
        }
        Ok((slot, release_after_decode))
    }

    /// Release every `removed` id. `outputs` is display order, not residency:
    /// a display-ready picture can still be a reference, so its slot stays
    /// until a later `removed` entry.
    pub fn apply(&mut self, update: &DpbUpdate) {
        for &id in &update.removed {
            if !self.release(id) {
                // Reachable only if the caller skipped an AU's plan.
                trace!(id, "DpbUpdate removed an id this SlotMap never assigned");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pic_id_keeps_its_slot_until_released_and_the_slot_is_then_reusable() {
        let mut slots = SlotMap::new(3);
        let s0 = slots.assign(10).unwrap();
        let s1 = slots.assign(11).unwrap();
        assert_ne!(s0, s1);

        assert_eq!(slots.slot_of(10), Some(s0));
        slots.release(11);
        assert_eq!(slots.slot_of(10), Some(s0));
        assert_eq!(slots.slot_of(11), None);

        let s2 = slots.assign(12).unwrap();
        assert_eq!(s2, s1, "the lowest free slot is the released one");
        assert_eq!(slots.slot_of(10), Some(s0));
        assert_eq!(slots.active(), 2);
    }

    #[test]
    fn assigning_past_capacity_is_an_error_never_a_silent_eviction() {
        let mut slots = SlotMap::new(1);
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        assert_eq!(slots.assign(3), Err(SlotError::Full { capacity: 2 }));
        assert_eq!(slots.slot_of(1), Some(0));
        assert_eq!(slots.slot_of(2), Some(1));
    }

    #[test]
    fn re_assigning_a_held_id_is_an_error_not_a_move() {
        let mut slots = SlotMap::new(2);
        let s = slots.assign(7).unwrap();
        assert_eq!(
            slots.assign(7),
            Err(SlotError::AlreadyAssigned { id: 7, slot: s })
        );
        assert_eq!(slots.active(), 1);
    }

    #[test]
    fn capacity_is_dpb_frames_plus_one_for_the_setup_slot() {
        assert_eq!(SlotMap::new(16).capacity(), 17);
        assert_eq!(SlotMap::new(4).capacity(), 5);
    }

    #[test]
    #[should_panic(expected = "envelope")]
    fn a_dpb_past_the_h264_ceiling_is_a_debug_panic_not_a_clamp() {
        // pf-bitstream's envelope gate makes this unreachable from a real stream;
        // reaching it means a caller bypassed the planner.
        let _ = SlotMap::new(17);
    }

    #[test]
    fn held_lists_slot_id_pairs_in_slot_order() {
        let mut slots = SlotMap::new(3);
        slots.assign(10).unwrap();
        slots.assign(11).unwrap();
        slots.assign(12).unwrap();
        slots.release(11);
        assert_eq!(slots.held().collect::<Vec<_>>(), vec![(0, 10), (2, 12)]);
    }

    #[test]
    fn apply_releases_removed_ids_and_ignores_outputs() {
        let mut slots = SlotMap::new(3);
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        slots.apply(&DpbUpdate {
            stored: None,
            outputs: vec![1], // still a reference — slot must stay
            removed: vec![2],
        });
        assert_eq!(slots.slot_of(1), Some(0));
        assert_eq!(slots.slot_of(2), None);
    }

    #[test]
    fn commit_setup_defers_or_releases_removals_and_never_defers_the_setup() {
        let mut slots = SlotMap::new(3);
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        let (slot, deferred) = slots.commit_setup(3, &[1, 3], Removals::Defer).unwrap();
        assert_eq!(slot, 2);
        assert_eq!(
            deferred,
            vec![1],
            "a stored-and-evicted setup is not deferred"
        );
        assert_eq!(slots.slot_of(1), Some(0), "deferred ids keep their slot");
        assert_eq!(slots.slot_of(3), None, "the evicted setup leaves at once");

        let (slot, deferred) = slots.commit_setup(4, &[1], Removals::ReleaseNow).unwrap();
        assert!(deferred.is_empty());
        assert_eq!(
            slot, 0,
            "ReleaseNow frees the removed slot before assigning"
        );
        assert_eq!(slots.slot_of(1), None);
    }

    #[test]
    fn a_hundred_synthetic_dpb_updates_churn_without_aliasing_a_slot() {
        let mut slots = SlotMap::new(4);
        let mut recorded: Vec<(PicId, u8)> = Vec::new();
        for id in 0u64..100 {
            let slot = slots.assign(id).unwrap();
            assert!(
                recorded.iter().all(|&(_, held)| held != slot),
                "assign handed out a slot a live picture still holds"
            );
            recorded.push((id, slot));

            let removed = if id >= 4 { vec![id - 4] } else { Vec::new() };
            slots.apply(&DpbUpdate {
                stored: Some(id),
                outputs: vec![id],
                removed: removed.clone(),
            });
            for gone in removed {
                recorded.retain(|&(held_id, _)| held_id != gone);
            }
            for &(live, slot) in &recorded {
                assert_eq!(slots.slot_of(live), Some(slot));
            }
            assert!(slots.active() <= 5);
        }
    }
}
