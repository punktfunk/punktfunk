//! Per-AU H.265 conversion: one [`AuPlan`] into the `StdVideoDecodeH265*` structs,
//! slice offsets and DPB slot bindings a `vkCmdDecodeVideoKHR` call is built from.
//!
//! Vulkan H.265 decode takes no per-slice reference lists. Hardware re-derives
//! 8.3.4 from the slice bits, keyed by `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr`
//! — DPB slot indices, not positions in `pReferenceSlots`. Those two numberings
//! coincide on a fresh IDR; filling the arrays from `refs` positions is silent
//! corruption. See [`DecodePlanVkH265::std_pic`].
//!
//! The AU-level binding set is the union of the plan's three current RPS sets
//! ([`pf_bitstream::h265::RpsPlan`]), then the other marked pictures
//! ([`AuPlan::dpb_refs`]). Per-slice lists are a closed check against the
//! current sets.
//!
//! A lost reference is absent from the RPS (`PlanWarning::MissingReference`);
//! the index arrays compact past it. There is no slot to point at.

use ash::vk::native as hh;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::PicId;
use pf_bitstream::h265::RefPic;
use pf_bitstream::h265::RefRpsIdxError;
use pf_bitstream::h265::RpsError;
use pf_bitstream::h265::RPS_UNUSED;
use tracing::trace;

use crate::slots::Removals;
use crate::slots::SlotError;
use crate::slots::SlotMap;

/// `STD_VIDEO_DECODE_H265_REF_PIC_SET_LIST_SIZE`. H.265 allows 16 per side;
/// Vulkan expresses eight. Beyond that: [`PlanToVkH265Error::RpsSetOverflow`].
pub const H265_RPS_LIST_SIZE: usize = 8;

/// Std unused-entry sentinel. Real DPB slots are ≤ 17, so never `0xFF`.
const UNUSED_RPS_ENTRY: u8 = 0xFF;

/// One bound reference: DPB slot, Std info, and the planner id the backend keys images by.
#[derive(Debug, Clone)]
pub struct VkRefH265 {
    pub slot: u8,
    pub std: hh::StdVideoDecodeH265ReferenceInfo,
    pub id: PicId,
}

/// CPU-derivable half of one AU's decode submission.
#[derive(Debug, Clone)]
pub struct DecodePlanVkH265 {
    /// Picture info. `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr` hold **DPB
    /// slot indices** (`0xFF` unused) — the same numbers
    /// `VkVideoReferenceSlotInfoKHR::slotIndex` carries, not positions in
    /// [`Self::refs`] / `pDecodeInfo->pReferenceSlots`.
    ///
    /// The two numberings coincide on a fresh IDR and diverge as soon as a B
    /// picture binds slots out of `refs` order. Filling the arrays from
    /// positions is silent corruption.
    ///
    /// The backend lays `pReferenceSlots` in [`Self::refs`] order and fails
    /// closed on a reference with no bound image: a slot these arrays name
    /// that the decode op never binds is unresolvable.
    pub std_pic: hh::StdVideoDecodeH265PictureInfo,
    /// Byte offset of each slice-segment NALU in the AU as planned, start code
    /// included. AU-relative, not submission-final: the recording layer packs
    /// the slice NALUs alone into the bitstream buffer and rebases these
    /// offsets (non-VCL NALUs inside the decode range hang VCN firmware — see
    /// `crate::ring::pack_slices`). Vulkan's `pSliceSegmentOffsets` then
    /// receives the rebased offsets.
    pub slice_offsets: Vec<u32>,
    /// The slot the decoded picture activates (`pSetupReferenceSlot`).
    pub setup_slot: u8,
    /// Setup-slot reference info: this picture's POC, short-term. HEVC has no
    /// same-AU self-marking (H.264's IDR `long_term_reference_flag` / MMCO 6);
    /// C.3.4 stores every picture short-term, and it becomes long-term only when
    /// a later picture lists it in `RefPicSetLtCurr`.
    pub setup_ref: hh::StdVideoDecodeH265ReferenceInfo,
    /// Planner id of the decoded picture (`AuPlan.dpb.stored`). The backend keys
    /// image bookkeeping by it.
    pub setup_id: PicId,
    /// Whether later pictures may reference the decoded picture. False for
    /// sub-layer non-reference NALU types (`RASL_N`/`TRAIL_N` and friends): the
    /// setup slot then exists for the decode itself plus any remaining DPB
    /// residency, and must never be bound as a reference for later AUs.
    pub setup_is_reference: bool,
    /// `pReferenceSlots`: the union of the three current RPS sets in set order
    /// (StCurrBefore, StCurrAfter, LtCurr), first appearance first, then every
    /// other marked picture (the *Foll* sets) in DPB order. AMD's Windows driver
    /// decodes a later reference to an omitted *Foll* picture wrong. Every slot
    /// [`Self::std_pic`]'s index arrays name appears here exactly once.
    pub refs: Vec<VkRefH265>,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream degrades
/// it to [`pf_bitstream::h265::PlanWarning`]s. `PlanError::RaslSkipped` is an
/// error of planning (Ok-skip upstream); no plan exists to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVkH265Error {
    NoSlices,
    /// `DpbUpdate.stored` is `None`. `plan_au` always stores; only `flush()`
    /// produces such updates, and those go to [`SlotMap::apply`] directly.
    NoStoredId,
    /// An RPS entry's id holds no slot: an earlier plan never went through this
    /// [`SlotMap`].
    UnresolvedReference(PicId),
    /// A slice list names a picture outside the current RPS sets. 8.3.4 builds
    /// every list from those sets; no index array could name it, so hardware
    /// could not resolve it.
    ReferenceOutsideRps(PicId),
    Slot(SlotError),
    /// Vulkan submits slice offsets as `u32`.
    OffsetOverflow(usize),
    /// A current RPS set holds more than [`H265_RPS_LIST_SIZE`] entries —
    /// expressible in H.265, not in Vulkan.
    RpsSetOverflow {
        set: &'static str,
        len: usize,
    },
    /// `NumDeltaPocsOfRefRpsIdx` is not derivable from this plan.
    RefRpsIdx(RefRpsIdxError),
    /// Inline `st_ref_pic_set()` bit count exceeds `u16`
    /// (`NumBitsForSTRefPicSetInSlice`) — a header that large is corrupt.
    StRpsBitsOverflow(u32),
    /// The map was built for a different DPB depth than this plan's
    /// `max_dpb_frames` — an SPS renegotiation resized the DPB. Rebuild the
    /// video session and its [`SlotMap`]; converting against the stale map
    /// would hand out slot indices the session's image pool does not have.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
}

impl std::fmt::Display for PlanToVkH265Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVkH265Error::NoSlices => write!(f, "the plan holds no slices"),
            PlanToVkH265Error::NoStoredId => {
                write!(
                    f,
                    "the plan stores no picture (flush updates go to SlotMap::apply)"
                )
            }
            PlanToVkH265Error::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToVkH265Error::ReferenceOutsideRps(id) => {
                write!(
                    f,
                    "slice list references picture {id} outside the current RPS sets"
                )
            }
            PlanToVkH265Error::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToVkH265Error::OffsetOverflow(offset) => {
                write!(f, "slice offset {offset} exceeds u32")
            }
            PlanToVkH265Error::RpsSetOverflow { set, len } => {
                write!(
                    f,
                    "{set} holds {len} entries; Vulkan expresses at most {H265_RPS_LIST_SIZE}"
                )
            }
            PlanToVkH265Error::RefRpsIdx(err) => write!(f, "{err}"),
            PlanToVkH265Error::StRpsBitsOverflow(bits) => {
                write!(f, "st_ref_pic_set bit count {bits} exceeds u16")
            }
            PlanToVkH265Error::CapacityMismatch { required, capacity } => {
                write!(
                    f,
                    "the plan needs {required} slots but the map holds {capacity} — \
                     an SPS renegotiation resized the DPB; rebuild session and map"
                )
            }
        }
    }
}

impl std::error::Error for PlanToVkH265Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToVkH265Error::Slot(err) => Some(err),
            PlanToVkH265Error::RefRpsIdx(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToVkH265Error {
    fn from(err: SlotError) -> Self {
        PlanToVkH265Error::Slot(err)
    }
}

impl From<RpsError> for PlanToVkH265Error {
    fn from(err: RpsError) -> Self {
        match err {
            RpsError::SetOverflow { set, len } => PlanToVkH265Error::RpsSetOverflow { set, len },
            RpsError::OutsideRps(id) => PlanToVkH265Error::ReferenceOutsideRps(id),
        }
    }
}

impl From<RefRpsIdxError> for PlanToVkH265Error {
    fn from(err: RefRpsIdxError) -> Self {
        PlanToVkH265Error::RefRpsIdx(err)
    }
}

fn ref_info(rp: &RefPic) -> hh::StdVideoDecodeH265ReferenceInfo {
    // SAFETY: StdVideoDecodeH265ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word and one integer; all-zero is a valid value for every field.
    let mut std: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_used_for_long_term_reference(u32::from(rp.is_long_term));
    // unused_for_reference stays 0: every bound picture is one the RPS keeps
    // marked, current or *Foll*.
    std.PicOrderCntVal = rp.pic_order_cnt;
    std
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// Unlike H.264 [`crate::plan_to_vk`], no `sps_id`: an H.265 [`AuPlan`] carries
/// its activated SPS and PPS.
///
/// Atomicity: every fallible step runs before any mutation of `slots`. Capacity,
/// RPS resolution (pre-removal: a current-set member survives its own AU per
/// 8.3.2), slice-list check and offsets are read-only. Then `removed` is applied
/// — those evictions were real regardless of this AU's fate — and the setup slot
/// is assigned last. A stored-and-evicted picture still gets its slot for the
/// decode itself and is released right after.
pub fn plan_to_vk_h265(
    plan: &AuPlan,
    slots: &mut SlotMap,
) -> Result<DecodePlanVkH265, PlanToVkH265Error> {
    let first_slice = plan.slices.first().ok_or(PlanToVkH265Error::NoSlices)?;
    let setup_id = plan.dpb.stored.ok_or(PlanToVkH265Error::NoStoredId)?;

    let required = plan.picture.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVkH265Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // The current sets bind once per picture. If any occurrence is long-term,
    // mark it so: hardware treats LT refs differently (no MV scaling, POC-LSB
    // matching).
    let current = plan.current_rps_refs()?;
    let mut refs: Vec<VkRefH265> = Vec::with_capacity(current.refs.len());
    for (rp, long_term) in &current.refs {
        let slot = slots
            .slot_of(rp.id)
            .ok_or(PlanToVkH265Error::UnresolvedReference(rp.id))?;
        let mut std = ref_info(rp);
        std.flags
            .set_used_for_long_term_reference(u32::from(*long_term));
        refs.push(VkRefH265 {
            slot,
            std,
            id: rp.id,
        });
    }
    // DPB slot indices, not `refs` positions. Slots ≤ 17, so never 0xFF.
    let index_arrays = current.index.map(|set| {
        set.map(|position| match position {
            RPS_UNUSED => UNUSED_RPS_ENTRY,
            position => refs[usize::from(position)].slot,
        })
    });

    // The *Foll* pictures bind after the current sets, as FFmpeg's Vulkan hwaccel
    // binds them. A picture this AU retires is no reference: skipping it keeps the
    // setup slot, assigned below from freed slots, out of the list.
    for rp in &current.foll {
        if plan.dpb.removed.contains(&rp.id) {
            continue;
        }
        match slots.slot_of(rp.id) {
            Some(slot) => refs.push(VkRefH265 {
                slot,
                std: ref_info(rp),
                id: rp.id,
            }),
            None => trace!(id = rp.id, "a marked picture holds no slot — left unbound"),
        }
    }

    let pic = &plan.picture;

    // SAFETY: StdVideoDecodeH265PictureInfo is a plain-C bindgen struct of a
    // bitfield word, integers and byte arrays; all-zero is a valid value for
    // every field.
    let mut std_pic: hh::StdVideoDecodeH265PictureInfo = unsafe { std::mem::zeroed() };
    std_pic.flags.set_IrapPicFlag(u32::from(pic.is_irap));
    std_pic.flags.set_IdrPicFlag(u32::from(pic.is_idr));
    std_pic.flags.set_IsReference(u32::from(pic.is_reference));
    std_pic.flags.set_short_term_ref_pic_set_sps_flag(u32::from(
        first_slice.header.short_term_ref_pic_set_sps_flag,
    ));
    std_pic.sps_video_parameter_set_id = plan.sps.video_parameter_set_id;
    std_pic.pps_seq_parameter_set_id = plan.pps.seq_parameter_set_id;
    std_pic.pps_pic_parameter_set_id = plan.pps.pic_parameter_set_id;
    std_pic.NumDeltaPocsOfRefRpsIdx = plan.num_delta_pocs_of_ref_rps_idx()?;
    std_pic.PicOrderCntVal = pic.pic_order_cnt;
    // 0 when the RPS came from the SPS by index — Vulkan's convention for this field.
    std_pic.NumBitsForSTRefPicSetInSlice = u16::try_from(pic.short_term_ref_pic_set_size_bits)
        .map_err(|_| PlanToVkH265Error::StRpsBitsOverflow(pic.short_term_ref_pic_set_size_bits))?;
    [
        std_pic.RefPicSetStCurrBefore,
        std_pic.RefPicSetStCurrAfter,
        std_pic.RefPicSetLtCurr,
    ] = index_arrays;

    // SAFETY: as above — all-zero is a valid StdVideoDecodeH265ReferenceInfo.
    let mut setup_ref: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
    setup_ref.PicOrderCntVal = pic.pic_order_cnt;

    let mut slice_offsets = Vec::with_capacity(plan.slices.len());
    for slice in &plan.slices {
        slice_offsets.push(
            u32::try_from(slice.data.start)
                .map_err(|_| PlanToVkH265Error::OffsetOverflow(slice.data.start))?,
        );
    }

    // Mutations last. Removals first: they were real regardless of this AU.
    let (setup_slot, _) = slots.commit_setup(setup_id, &plan.dpb.removed, Removals::ReleaseNow)?;

    Ok(DecodePlanVkH265 {
        std_pic,
        slice_offsets,
        setup_slot,
        setup_ref,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use cros_codecs::codec::h265::parser::Pps;
    use cros_codecs::codec::h265::parser::ShortTermRefPicSet;
    use cros_codecs::codec::h265::parser::Sps;
    use pf_bitstream::h265::ColourDescription;
    use pf_bitstream::h265::DisplayCrop;
    use pf_bitstream::h265::DpbUpdate;
    use pf_bitstream::h265::H265Planner;
    use pf_bitstream::h265::Level;
    use pf_bitstream::h265::NaluType;
    use pf_bitstream::h265::PicturePlan;
    use pf_bitstream::h265::RpsPlan;
    use pf_bitstream::h265::SliceHeader;
    use pf_bitstream::h265::SlicePlan;
    use pf_bitstream::testing::split_h265_aus;

    use super::*;

    // Same vendored vectors pf-bitstream's h265 tests plan.
    const TEST_25FPS: &[u8] = pf_bitstream::testing::H265_25FPS;
    const TEST_64X64_I_P_B_P: &[u8] = pf_bitstream::testing::H265_64X64_I_P_B_P;

    /// Host low-delay HEVC: 120 pictures, five-picture DPB, reorder 0 — 115 of
    /// 120 AUs retire a picture. The stream the GPU legs decode against.
    const LOWDELAY_640X480_H265: &[u8] = include_bytes!("../tests/data/lowdelay-640x480.h265");

    /// Every marked picture binds, the *Foll* ones after the current sets, and none
    /// aliases the setup slot. Also with `dpb_refs` widened to the pre-RPS marked
    /// set, the mutation that aliases the DXVA rung on 115 of these 120 AUs.
    ///
    /// A failure means a picture this AU retires reached `pReferenceSlots`: the
    /// setup slot is assigned from freed slots, so the two would alias.
    #[test]
    fn every_marked_picture_binds_and_none_aliases_the_setup_slot() {
        let aus = split_h265_aus(LOWDELAY_640X480_H265);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut prev: Option<AuPlan> = None;
        let mut converted = 0usize;
        let mut widened = 0usize;
        let mut foll_bound = 0usize;

        for au in aus {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));

            // Marked DPB as `decode_rps` found it: previous AU's snapshot plus
            // the picture it stored — `dpb_snapshot()` from above `decode_rps`.
            let mut as_if = plan.clone();
            as_if.dpb_refs = match &prev {
                None => Vec::new(),
                Some(prev) => {
                    let mut marked = prev.dpb_refs.clone();
                    if let Some(id) = prev.dpb.stored {
                        assert!(
                            prev.picture.is_reference,
                            "this stream carries no sub-layer non-reference pictures"
                        );
                        marked.push(RefPic {
                            id,
                            pic_order_cnt: prev.picture.pic_order_cnt,
                            is_long_term: false,
                        });
                    }
                    marked
                }
            };
            if as_if.dpb_refs.len() > plan.dpb_refs.len() {
                widened += 1;
            }
            let wide = plan_to_vk_h265(&as_if, &mut map.clone()).expect("conversion");
            for r in &wide.refs {
                assert_ne!(
                    r.slot, wide.setup_slot,
                    "a retired picture aliases the setup slot"
                );
            }

            let vk = plan_to_vk_h265(&plan, map).expect("conversion");
            converted += 1;
            let mut current: Vec<PicId> = Vec::new();
            let sets = plan.rps.st_curr_before.iter();
            for rp in sets.chain(&plan.rps.st_curr_after).chain(&plan.rps.lt_curr) {
                if !current.contains(&rp.id) {
                    current.push(rp.id);
                }
            }
            let bound: Vec<PicId> = vk.refs.iter().map(|r| r.id).collect();
            assert_eq!(bound[..current.len()], current[..], "the current sets lead");
            for rp in &plan.dpb_refs {
                assert!(
                    bound.contains(&rp.id),
                    "AU {converted}: marked {} unbound",
                    rp.id
                );
            }
            for r in &vk.refs {
                assert_ne!(r.slot, vk.setup_slot, "a reference aliases the setup slot");
            }
            if bound.len() > current.len() {
                foll_bound += 1;
            }
            prev = Some(plan);
        }

        assert_eq!(converted, 120);
        assert_eq!(
            widened, 115,
            "the mutation must actually widen the marked set on the access units that \
             retire a picture, else this test asserts nothing about anything"
        );
        assert_eq!(foll_bound, 118, "AUs that bind a *Foll* picture");
    }

    #[test]
    fn a_foll_picture_binds_after_the_current_set() {
        // POC 2 predicts from POC 1 and keeps POC 0 for a later anchor. AMD's
        // driver decodes that anchor wrong unless POC 0 stays bound here.
        let mut slots = SlotMap::new(4);
        slots.assign(10).unwrap(); // POC 0, slot 0
        slots.assign(11).unwrap(); // POC 1, slot 1
        let mut plan = mini_plan(
            12,
            2,
            RpsPlan {
                st_curr_before: vec![st_ref(11, 1)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            Vec::new(),
            4,
        );
        plan.dpb_refs = vec![st_ref(10, 0), st_ref(11, 1)];
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();

        let bound: Vec<(u8, i32)> = vk
            .refs
            .iter()
            .map(|r| (r.slot, r.std.PicOrderCntVal))
            .collect();
        assert_eq!(
            bound,
            vec![(1, 1), (0, 0)],
            "current first, then the *Foll* picture"
        );
        assert_eq!(
            vk.std_pic.RefPicSetStCurrBefore[0], 1,
            "the index array names slot 1"
        );
        assert_eq!(vk.std_pic.RefPicSetStCurrBefore[1], UNUSED_RPS_ENTRY);
        assert_eq!(vk.setup_slot, 2);
    }

    #[test]
    fn the_full_25fps_vector_converts_with_stable_slots_and_start_code_offsets() {
        let aus = split_h265_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        // PicId → slot assigned when that picture was decoded; drop on `removed`.
        let mut held: BTreeMap<PicId, u8> = BTreeMap::new();
        let mut converted = 0usize;
        let mut slot_order_differs = 0usize;

        for au in &aus {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let vk = plan_to_vk_h265(&plan, slots).expect("the clean vector converts");
            converted += 1;

            for r in &vk.refs {
                assert_eq!(
                    held.get(&r.id),
                    Some(&r.slot),
                    "a referenced picture's slot changed while it was referenced"
                );
                assert_ne!(r.slot, vk.setup_slot, "a reference aliases the setup slot");
            }

            // Index arrays name DPB slots of the plan's RPS sets, 0xFF beyond.
            // Every named slot must be one of `refs`.
            for (array, set) in [
                (&vk.std_pic.RefPicSetStCurrBefore, &plan.rps.st_curr_before),
                (&vk.std_pic.RefPicSetStCurrAfter, &plan.rps.st_curr_after),
                (&vk.std_pic.RefPicSetLtCurr, &plan.rps.lt_curr),
            ] {
                for (position, entry) in array.iter().enumerate() {
                    match set.get(position) {
                        Some(rp) => {
                            let r =
                                vk.refs
                                    .iter()
                                    .find(|r| r.slot == *entry)
                                    .unwrap_or_else(|| {
                                        panic!(
                                            "AU {converted}: RPS entry names DPB slot {entry}, \
                                         which this decode op does not bind ({:?}) — the \
                                         hardware cannot resolve it",
                                            vk.refs.iter().map(|r| r.slot).collect::<Vec<_>>()
                                        )
                                    });
                            assert_eq!(r.id, rp.id, "AU {converted}: the slot's picture");
                            assert_eq!(r.std.PicOrderCntVal, rp.pic_order_cnt);
                        }
                        None => assert_eq!(*entry, UNUSED_RPS_ENTRY),
                    }
                }
            }
            // Count AUs where slot ≠ position in `refs`, so a positional reading
            // cannot pass this test vacuously (on a fresh IDR they agree).
            if vk
                .refs
                .iter()
                .enumerate()
                .any(|(position, r)| usize::from(r.slot) != position)
            {
                slot_order_differs += 1;
            }

            assert_eq!(vk.slice_offsets.len(), plan.slices.len());
            for (offset, slice) in vk.slice_offsets.iter().zip(&plan.slices) {
                let offset = *offset as usize;
                assert_eq!(offset, slice.data.start);
                let at = &au[offset..];
                assert!(
                    at.starts_with(&[0, 0, 1]) || at.starts_with(&[0, 0, 0, 1]),
                    "slice offset {offset} does not sit on a start code"
                );
            }

            assert_eq!(vk.std_pic.PicOrderCntVal, plan.picture.pic_order_cnt);
            assert_eq!(
                u32::from(plan.picture.is_idr),
                vk.std_pic.flags.IdrPicFlag()
            );
            assert_eq!(
                u32::from(plan.picture.is_irap),
                vk.std_pic.flags.IrapPicFlag()
            );
            assert_eq!(
                u32::from(plan.picture.is_reference),
                vk.std_pic.flags.IsReference()
            );
            assert_eq!(vk.setup_ref.PicOrderCntVal, plan.picture.pic_order_cnt);
            assert_eq!(
                vk.std_pic.NumBitsForSTRefPicSetInSlice,
                plan.picture.short_term_ref_pic_set_size_bits as u16
            );

            let stored = plan.dpb.stored.unwrap();
            assert_eq!(vk.setup_id, stored);
            assert_eq!(vk.setup_is_reference, plan.picture.is_reference);
            held.insert(stored, vk.setup_slot);
            for id in &plan.dpb.removed {
                held.remove(id);
            }

            let ledger: BTreeMap<PicId, u8> = slots.held().map(|(slot, id)| (id, slot)).collect();
            assert_eq!(ledger, held);
        }

        assert_eq!(converted, 250, "the vector's own golden");
        // Hierarchical-B RPS puts refs out of slot order from AU 3 on (first B
        // with two StCurrAfter entries binds slots 2 then 1). 247 of 250: AUs
        // 0-2 agree, which is why a positional reading looks right at start.
        assert_eq!(
            slot_order_differs, 247,
            "the vector must distinguish DPB slots from positions in refs"
        );

        let mut slots = slots.unwrap();
        slots.apply(&planner.flush());
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn the_b_frame_vector_populates_both_current_index_arrays_around_the_picture() {
        let aus = split_h265_aus(TEST_64X64_I_P_B_P);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut b_pictures_seen = 0usize;

        for au in &aus {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let vk = plan_to_vk_h265(&plan, slots).expect("the clean vector converts");

            if plan.rps.st_curr_after.is_empty() {
                continue;
            }
            b_pictures_seen += 1;
            // 8.3.2: StCurrBefore sits below this POC, StCurrAfter above —
            // resolved through the index arrays' DPB slots.
            let bound = |slot: u8| {
                vk.refs
                    .iter()
                    .find(|r| r.slot == slot)
                    .unwrap_or_else(|| panic!("RPS entry names unbound DPB slot {slot}"))
            };
            let before = vk.std_pic.RefPicSetStCurrBefore[0];
            let after = vk.std_pic.RefPicSetStCurrAfter[0];
            assert!(bound(before).std.PicOrderCntVal < plan.picture.pic_order_cnt);
            assert!(bound(after).std.PicOrderCntVal > plan.picture.pic_order_cnt);
            assert_ne!(before, after, "distinct slots on the two sides");
        }

        assert!(b_pictures_seen > 0, "the vector must contain B pictures");
    }

    // Hand-built plans: the vendored crate has no H.265 synthesizer. Every
    // AuPlan field is public; the contract under test is the plan, not the bits.

    fn mini_sps() -> Rc<Sps> {
        Rc::new(Sps {
            video_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            chroma_format_idc: 1,
            pic_width_in_luma_samples: 64,
            pic_height_in_luma_samples: 64,
            ..Default::default()
        })
    }

    fn mini_pps(sps: &Rc<Sps>) -> Rc<Pps> {
        // Vendored Pps has no Default; only the two ids and the SPS chain matter here.
        Rc::new(Pps {
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            dependent_slice_segments_enabled_flag: false,
            output_flag_present_flag: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding_enabled_flag: false,
            cabac_init_present_flag: false,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            init_qp_minus26: 0,
            constrained_intra_pred_flag: false,
            transform_skip_enabled_flag: false,
            cu_qp_delta_enabled_flag: false,
            diff_cu_qp_delta_depth: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            slice_chroma_qp_offsets_present_flag: false,
            weighted_pred_flag: false,
            weighted_bipred_flag: false,
            transquant_bypass_enabled_flag: false,
            tiles_enabled_flag: false,
            entropy_coding_sync_enabled_flag: false,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            uniform_spacing_flag: true,
            column_width_minus1: [0; 20],
            row_height_minus1: [0; 22],
            loop_filter_across_tiles_enabled_flag: true,
            loop_filter_across_slices_enabled_flag: false,
            deblocking_filter_control_present_flag: false,
            deblocking_filter_override_enabled_flag: false,
            deblocking_filter_disabled_flag: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            scaling_list_data_present_flag: false,
            scaling_list: Default::default(),
            lists_modification_present_flag: false,
            log2_parallel_merge_level_minus2: 0,
            slice_segment_header_extension_present_flag: false,
            extension_present_flag: false,
            range_extension_flag: false,
            range_extension: Default::default(),
            scc_extension_flag: false,
            scc_extension: Default::default(),
            qp_bd_offset_y: 0,
            sps: Rc::clone(sps),
        })
    }

    fn mini_picture(poc: i32, max_dpb_frames: usize) -> PicturePlan {
        PicturePlan {
            nalu_type: if poc == 0 {
                NaluType::IdrWRadl
            } else {
                NaluType::TrailR
            },
            is_idr: poc == 0,
            is_irap: poc == 0,
            no_rasl_output_flag: poc == 0,
            is_reference: true,
            pic_order_cnt: poc,
            coded_width: 64,
            coded_height: 64,
            display_crop: DisplayCrop {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            colour: ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: false,
            },
            general_profile_idc: 1,
            level_idc: Level::L4,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            chroma_format_idc: 1,
            max_dpb_frames,
            short_term_ref_pic_set_size_bits: 0,
            recovery_point: None,
            // Healthy-stream fixture; this conversion never reads the bit.
            references_clean: true,
        }
    }

    fn mini_slice(refs0: &[RefPic], refs1: &[RefPic]) -> SlicePlan {
        SlicePlan {
            data: 0..32,
            nal: 3..32,
            header: SliceHeader::default(),
            ref_list0: refs0.to_vec(),
            ref_list1: refs1.to_vec(),
        }
    }

    /// Plan storing `stored` with the given RPS sets and one slice whose list0
    /// is the 8.3.4 concatenation of those sets.
    fn mini_plan(
        stored: PicId,
        poc: i32,
        rps: RpsPlan,
        removed: Vec<PicId>,
        max_dpb_frames: usize,
    ) -> AuPlan {
        let sps = mini_sps();
        let pps = mini_pps(&sps);
        let mut list0: Vec<RefPic> = Vec::new();
        list0.extend(rps.st_curr_before.iter().copied());
        list0.extend(rps.st_curr_after.iter().copied());
        list0.extend(rps.lt_curr.iter().copied());
        // The marked DPB is the current sets alone unless a test widens it.
        let dpb_refs = list0.clone();
        AuPlan {
            picture: mini_picture(poc, max_dpb_frames),
            rps,
            slices: vec![mini_slice(&list0, &[])],
            dpb: DpbUpdate {
                stored: Some(stored),
                outputs: vec![stored],
                removed,
            },
            dpb_refs,
            warnings: Vec::new(),
            sps,
            pps,
        }
    }

    fn st_ref(id: PicId, poc: i32) -> RefPic {
        RefPic {
            id,
            pic_order_cnt: poc,
            is_long_term: false,
        }
    }

    fn lt_ref(id: PicId, poc: i32) -> RefPic {
        RefPic {
            id,
            pic_order_cnt: poc,
            is_long_term: true,
        }
    }

    #[test]
    fn a_long_term_rps_entry_carries_the_flag_and_its_index_lands_in_lt_curr() {
        let mut slots = SlotMap::new(4);
        // Anchor (id 10, slot 0) then previous (id 11, slot 1): RPS order is
        // the reverse of slot order, so a positional reading would swap them.
        slots.assign(10).unwrap();
        slots.assign(11).unwrap();

        let plan = mini_plan(
            12,
            2,
            RpsPlan {
                st_curr_before: vec![st_ref(11, 1)],
                st_curr_after: Vec::new(),
                lt_curr: vec![lt_ref(10, 0)],
            },
            Vec::new(),
            4,
        );
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();

        assert_eq!(vk.refs.len(), 2);
        // Entries are DPB slots: id 11 took slot 1, id 10 slot 0. RPS order
        // binds id 11 first — a positional reading names slot 0 for the
        // short-term set and hits the long-term anchor instead.
        assert_eq!(vk.std_pic.RefPicSetStCurrBefore[0], 1, "id 11's DPB slot");
        assert_eq!(vk.std_pic.RefPicSetLtCurr[0], 0, "id 10's DPB slot");
        let bound = |slot: u8| vk.refs.iter().find(|r| r.slot == slot).expect("bound");
        let st = bound(vk.std_pic.RefPicSetStCurrBefore[0]);
        assert_eq!(st.id, 11);
        assert_eq!(st.std.flags.used_for_long_term_reference(), 0);
        let lt = bound(vk.std_pic.RefPicSetLtCurr[0]);
        assert_eq!(lt.id, 10);
        assert_eq!(lt.std.flags.used_for_long_term_reference(), 1);
        assert_eq!(lt.std.PicOrderCntVal, 0);
        assert_eq!(vk.std_pic.RefPicSetStCurrAfter[0], UNUSED_RPS_ENTRY);
        assert_eq!(vk.setup_ref.flags.used_for_long_term_reference(), 0);
        assert_eq!(vk.setup_ref.PicOrderCntVal, 2);
    }

    #[test]
    fn a_failed_conversion_leaves_the_slot_map_untouched_and_the_session_recovers() {
        // Right-sized map that never saw the reference's AU: fail, do not
        // fabricate a slot.
        let mut slots = SlotMap::new(4);
        slots.assign(999).unwrap();

        let plan = mini_plan(
            5,
            1,
            RpsPlan {
                st_curr_before: vec![st_ref(4, 0)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            vec![3], // must not apply on the failed path
            4,
        );
        assert_eq!(
            plan_to_vk_h265(&plan, &mut slots).unwrap_err(),
            PlanToVkH265Error::UnresolvedReference(4)
        );

        assert_eq!(slots.active(), 1);
        assert_eq!(slots.held().collect::<Vec<_>>(), vec![(0, 999)]);

        // Next valid plan (IDR restart whose `removed` names ids this map never
        // assigned — tolerated) still converts on the same map.
        let idr = mini_plan(6, 0, RpsPlan::default(), vec![4, 5], 4);
        let vk = plan_to_vk_h265(&idr, &mut slots).unwrap();
        assert_eq!(vk.setup_slot, 1, "the lowest free slot after the held one");
        assert_eq!(slots.active(), 2);
    }

    #[test]
    fn an_sps_switch_that_resizes_the_dpb_is_a_capacity_mismatch_not_a_guess() {
        // Map sized for a 6-deep DPB; a renegotiated stream plans with 16.
        // Refuse rather than hand out slots the image pool does not have.
        let mut slots = SlotMap::new(6);
        plan_to_vk_h265(
            &mini_plan(0, 0, RpsPlan::default(), Vec::new(), 6),
            &mut slots,
        )
        .unwrap();

        let renegotiated = mini_plan(1, 0, RpsPlan::default(), Vec::new(), 16);
        assert_eq!(
            plan_to_vk_h265(&renegotiated, &mut slots).unwrap_err(),
            PlanToVkH265Error::CapacityMismatch {
                required: 17,
                capacity: 7
            }
        );
        assert_eq!(slots.active(), 1);
    }

    #[test]
    fn empty_and_flush_shaped_plans_are_rejected_with_typed_errors() {
        let mut slots = SlotMap::new(4);

        let mut no_slices = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        no_slices.slices.clear();
        assert_eq!(
            plan_to_vk_h265(&no_slices, &mut slots).unwrap_err(),
            PlanToVkH265Error::NoSlices
        );

        let mut no_stored = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        no_stored.dpb.stored = None;
        assert_eq!(
            plan_to_vk_h265(&no_stored, &mut slots).unwrap_err(),
            PlanToVkH265Error::NoStoredId
        );

        let mut huge_offset = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        huge_offset.slices[0].data = (u32::MAX as usize + 1)..(u32::MAX as usize + 40);
        assert_eq!(
            plan_to_vk_h265(&huge_offset, &mut slots).unwrap_err(),
            PlanToVkH265Error::OffsetOverflow(u32::MAX as usize + 1)
        );
        assert_eq!(slots.active(), 0, "every rejection left the map untouched");
    }

    #[test]
    fn an_rps_set_deeper_than_the_std_index_arrays_is_rejected_not_truncated() {
        let mut slots = SlotMap::new(16);
        for id in 0..9u64 {
            slots.assign(id).unwrap();
        }
        let deep: Vec<RefPic> = (0..9).map(|i| st_ref(i, i as i32)).collect();
        let plan = mini_plan(
            20,
            9,
            RpsPlan {
                st_curr_before: deep,
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            Vec::new(),
            16,
        );
        assert_eq!(
            plan_to_vk_h265(&plan, &mut slots).unwrap_err(),
            PlanToVkH265Error::RpsSetOverflow {
                set: "RefPicSetStCurrBefore",
                len: 9
            }
        );
        assert_eq!(slots.active(), 9, "the rejection mutated nothing");
    }

    #[test]
    fn a_slice_list_entry_outside_the_rps_sets_fails_closed() {
        let mut slots = SlotMap::new(4);
        slots.assign(1).unwrap();
        slots.assign(2).unwrap();
        let mut plan = mini_plan(
            3,
            2,
            RpsPlan {
                st_curr_before: vec![st_ref(1, 0)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            Vec::new(),
            4,
        );
        // Id 2 holds a slot but is in no current set: it would be missing from
        // pReferenceSlots, so hardware could not resolve the list entry.
        plan.slices[0].ref_list0.push(st_ref(2, 1));
        assert_eq!(
            plan_to_vk_h265(&plan, &mut slots).unwrap_err(),
            PlanToVkH265Error::ReferenceOutsideRps(2)
        );
    }

    #[test]
    fn a_stored_and_evicted_picture_still_gets_a_slot_for_the_decode_itself() {
        // Stored id also in this plan's `removed`: slot exists for the decode
        // and is released right after, so the next picture can reuse it.
        let mut slots = SlotMap::new(1); // capacity = max_dpb_frames + 1
        let mut plan = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 1);
        plan.dpb.removed = vec![0];
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(vk.setup_slot, 0);
        assert_eq!(slots.active(), 0, "released after assignment");
        assert_eq!(slots.slot_of(0), None);
    }

    #[test]
    fn num_delta_pocs_of_ref_rps_idx_derives_from_the_predicted_inline_rps() {
        let mut slots = SlotMap::new(4);
        slots.assign(1).unwrap();

        // SPS carries two candidates; the inline slice RPS predicts from the
        // second (`delta_idx_minus1 = 0` ⇒ RefRpsIdx = 1).
        let mut sps = (*mini_sps()).clone();
        sps.num_short_term_ref_pic_sets = 2;
        sps.short_term_ref_pic_set = vec![
            ShortTermRefPicSet {
                num_delta_pocs: 3,
                ..Default::default()
            },
            ShortTermRefPicSet {
                num_delta_pocs: 5,
                ..Default::default()
            },
        ];
        let sps = Rc::new(sps);
        let pps = mini_pps(&sps);

        let header = SliceHeader {
            short_term_ref_pic_set_sps_flag: false,
            curr_rps_idx: 2, // == num_short_term_ref_pic_sets: inline
            short_term_ref_pic_set: ShortTermRefPicSet {
                inter_ref_pic_set_prediction_flag: true,
                delta_idx_minus1: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut picture = mini_picture(1, 4);
        picture.short_term_ref_pic_set_size_bits = 23;
        let plan = AuPlan {
            picture,
            rps: RpsPlan {
                st_curr_before: vec![st_ref(1, 0)],
                st_curr_after: Vec::new(),
                lt_curr: Vec::new(),
            },
            slices: vec![SlicePlan {
                data: 0..32,
                nal: 3..32,
                header,
                ref_list0: vec![st_ref(1, 0)],
                ref_list1: Vec::new(),
            }],
            dpb: DpbUpdate {
                stored: Some(2),
                outputs: vec![2],
                removed: Vec::new(),
            },
            dpb_refs: vec![st_ref(1, 0)],
            warnings: Vec::new(),
            sps: Rc::clone(&sps),
            pps: Rc::clone(&pps),
        };
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(
            vk.std_pic.NumDeltaPocsOfRefRpsIdx, 5,
            "the SOURCE set's count"
        );
        assert_eq!(vk.std_pic.flags.short_term_ref_pic_set_sps_flag(), 0);
        assert_eq!(vk.std_pic.NumBitsForSTRefPicSetInSlice, 23);

        let mut broken = plan.clone();
        {
            let header = &mut broken.slices[0].header;
            header.short_term_ref_pic_set.delta_idx_minus1 = 2; // RefRpsIdx = -1
        }
        broken.dpb.stored = Some(3);
        assert_eq!(
            plan_to_vk_h265(&broken, &mut slots).unwrap_err(),
            PlanToVkH265Error::RefRpsIdx(RefRpsIdxError::Invalid {
                curr_rps_idx: 2,
                delta_idx_minus1: 2
            })
        );
    }

    #[test]
    fn parameter_set_ids_flow_from_the_plans_activated_sets() {
        let mut slots = SlotMap::new(4);
        let mut sps = (*mini_sps()).clone();
        sps.video_parameter_set_id = 3;
        sps.seq_parameter_set_id = 7;
        let sps = Rc::new(sps);
        let mut pps = (*mini_pps(&sps)).clone();
        pps.pic_parameter_set_id = 9;
        pps.seq_parameter_set_id = 7;
        let pps = Rc::new(pps);

        let mut plan = mini_plan(0, 0, RpsPlan::default(), Vec::new(), 4);
        plan.sps = sps;
        plan.pps = pps;
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(vk.std_pic.sps_video_parameter_set_id, 3);
        assert_eq!(vk.std_pic.pps_seq_parameter_set_id, 7);
        assert_eq!(vk.std_pic.pps_pic_parameter_set_id, 9);
    }

    #[test]
    fn a_picture_referenced_by_two_sets_binds_one_slot_listed_once() {
        // Concealment can resolve an LSB-masked long-term entry and a
        // short-term entry to the same stored picture; each slot binds once,
        // with both index arrays pointing at that one entry.
        let mut slots = SlotMap::new(4);
        slots.assign(1).unwrap();
        let plan = mini_plan(
            2,
            1,
            RpsPlan {
                st_curr_before: vec![st_ref(1, 0)],
                st_curr_after: Vec::new(),
                lt_curr: vec![lt_ref(1, 0)],
            },
            Vec::new(),
            4,
        );
        let vk = plan_to_vk_h265(&plan, &mut slots).unwrap();
        assert_eq!(vk.refs.len(), 1, "one binding for one picture");
        assert_eq!(
            vk.std_pic.RefPicSetStCurrBefore[0],
            vk.std_pic.RefPicSetLtCurr[0]
        );
        // StCurrBefore bound first, but the LtCurr occurrence must still mark
        // the shared binding long-term: hardware treats LT refs differently
        // (no MV scaling, POC-LSB matching).
        assert_eq!(vk.refs[0].std.flags.used_for_long_term_reference(), 1);
    }
}
