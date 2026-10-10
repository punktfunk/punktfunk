//! Per-AU conversion: one [`AuPlan`] into the `StdVideoDecodeH264*` structs, slice
//! offsets and DPB slot bindings a `vkCmdDecodeVideoKHR` call is built from.
//!
//! Progressive envelope: pf-bitstream rejects interlaced streams before a plan
//! exists, so every field/bottom FLAG here is written 0. Top/bottom PicOrderCnt
//! pairs still ride through; they differ when the PPS codes
//! `bottom_field_pic_order_in_frame_present_flag`.

use ash::vk::native as hh;
use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::RefPic;

use crate::slots::Removals;
use crate::slots::SlotError;
use crate::slots::SlotMap;

/// Active reference. `id` is kept so the backend can map the slot to its image.
#[derive(Debug, Clone)]
pub struct VkRef {
    pub slot: u8,
    pub std: hh::StdVideoDecodeH264ReferenceInfo,
    pub id: PicId,
}

/// CPU-derivable half of one AU's decode submission. The recording layer adds
/// the bitstream buffer, DPB images, session and command recording.
#[derive(Debug, Clone)]
pub struct DecodePlanVk {
    pub std_pic: hh::StdVideoDecodeH264PictureInfo,
    /// Slice NALU byte offsets in the AU as planned, start code included.
    /// AU-relative, not submission-final: the recording layer packs SLICE
    /// NALUs only (non-VCL in the decode range hangs VCN) and rebases these
    /// for Vulkan's `pSliceOffsets`.
    pub slice_offsets: Vec<u32>,
    /// The slot the decoded picture activates (`pSetupReferenceSlot`).
    pub setup_slot: u8,
    /// Setup-slot identity: this picture's FrameNum/POC, already long-term when
    /// the AU self-marks (IDR `long_term_reference_flag` or MMCO 6).
    pub setup_ref: hh::StdVideoDecodeH264ReferenceInfo,
    /// Planner id of the decoded picture (`AuPlan.dpb.stored`).
    pub setup_id: PicId,
    /// Whether later AUs may reference this picture. When `false` the setup slot
    /// exists for this decode plus remaining DPB residency, and this AU's
    /// `removed` may already have released it.
    pub setup_is_reference: bool,
    /// The unique referenced pictures across all slices, in first-appearance order.
    pub refs: Vec<VkRef>,
    /// Pictures this AU's end-of-picture bookkeeping retires while the decode
    /// op still binds them. Release once that op is recorded — never inside
    /// the conversion, and never dropped: a leak per AU reaches
    /// `SlotError::Full`. Apply on failure paths too.
    ///
    /// [`SlotMap::assign`] takes the lowest free slot. Release here and setup
    /// reuses a slot this AU still references. `H264Planner` snapshots
    /// `dpb_refs` in `begin_picture`, before 8.2.5 marking and C.4.5.3 bump,
    /// so a sliding-window unmark plus bump can land the same picture in both
    /// `dpb_refs` and `dpb.removed`.
    pub release_after_decode: Vec<PicId>,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream degrades it to
/// [`pf_bitstream::h264::PlanWarning`]s upstream; these are caller/session bugs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVkError {
    NoSlices,
    /// The plan's `DpbUpdate.stored` is `None`. `plan_au` always stores; only
    /// `flush()` produces such updates, and those go to [`SlotMap::apply`] directly.
    NoStoredId,
    /// A reference id holds no slot — an earlier plan never went through this map.
    UnresolvedReference(PicId),
    Slot(SlotError),
    OffsetOverflow(usize),
    /// Map DPB depth differs from this plan's `max_dpb_frames` — SPS
    /// renegotiation. Rebuild the video session and its [`SlotMap`]; converting
    /// against the stale map would hand out slot indices the image pool lacks.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
}

impl std::fmt::Display for PlanToVkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVkError::NoSlices => write!(f, "the plan holds no slices"),
            PlanToVkError::NoStoredId => {
                write!(
                    f,
                    "the plan stores no picture (flush updates go to SlotMap::apply)"
                )
            }
            PlanToVkError::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToVkError::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToVkError::OffsetOverflow(offset) => {
                write!(f, "slice offset {offset} exceeds u32")
            }
            PlanToVkError::CapacityMismatch { required, capacity } => {
                write!(
                    f,
                    "the plan needs {required} slots but the map holds {capacity} — \
                     an SPS renegotiation resized the DPB; rebuild session and map"
                )
            }
        }
    }
}

impl std::error::Error for PlanToVkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToVkError::Slot(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToVkError {
    fn from(err: SlotError) -> Self {
        PlanToVkError::Slot(err)
    }
}

fn ref_info(rp: &RefPic) -> hh::StdVideoDecodeH264ReferenceInfo {
    // SAFETY: StdVideoDecodeH264ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word and integers; all-zero is a valid value for every field.
    let mut std: hh::StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_used_for_long_term_reference(u32::from(rp.is_long_term));
    // FrameNum is the Std pair-key: frame_num short-term, LongTermFrameIdx
    // long-term. Field flags and is_non_existing stay 0 (module docs).
    std.FrameNum = rp.frame_num_or_lt_idx;
    std.PicOrderCnt = [rp.top_field_order_cnt, rp.bottom_field_order_cnt];
    std
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// `sps_id` is the active SPS: an [`AuPlan`] names the PPS, not the SPS that
/// PPS references — the caller resolves it through its parameter-set table.
///
/// Atomicity: every fallible step runs before any mutation of `slots`.
/// Capacity first (read-only); references resolve against the pre-removal
/// state (this AU's 8.2.5 marking can evict a picture its slices still name);
/// setup is assigned last, and `removed` leaves as
/// [`DecodePlanVk::release_after_decode`].
pub fn plan_to_vk(
    plan: &AuPlan,
    slots: &mut SlotMap,
    sps_id: u8,
) -> Result<DecodePlanVk, PlanToVkError> {
    let first_slice = plan.slices.first().ok_or(PlanToVkError::NoSlices)?;
    let setup_id = plan.dpb.stored.ok_or(PlanToVkError::NoStoredId)?;

    let required = plan.picture.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVkError::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // Unique pictures across both slice lists, first appearance first.
    // Per-slice `ref_idx` order lives in the SlicePlans, not here.
    let mut refs: Vec<VkRef> = Vec::new();
    for slice in &plan.slices {
        for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
            if refs.iter().any(|existing| existing.id == rp.id) {
                continue;
            }
            let slot = slots
                .slot_of(rp.id)
                .ok_or(PlanToVkError::UnresolvedReference(rp.id))?;
            refs.push(VkRef {
                slot,
                std: ref_info(rp),
                id: rp.id,
            });
        }
    }

    let pic = &plan.picture;

    // SAFETY: StdVideoDecodeH264PictureInfo is a plain-C bindgen struct of a bitfield
    // word and integers; all-zero is a valid value for every field.
    let mut std_pic: hh::StdVideoDecodeH264PictureInfo = unsafe { std::mem::zeroed() };
    let is_intra = plan
        .slices
        .iter()
        .all(|slice| slice.header.slice_type.is_i() || slice.header.slice_type.is_si());
    std_pic.flags.set_is_intra(u32::from(is_intra));
    std_pic.flags.set_is_reference(u32::from(pic.is_reference));
    std_pic.flags.set_IdrPicFlag(u32::from(pic.is_idr));
    std_pic.seq_parameter_set_id = sps_id;
    std_pic.pic_parameter_set_id = first_slice.header.pic_parameter_set_id;
    std_pic.frame_num = pic.frame_num;
    std_pic.idr_pic_id = if pic.is_idr {
        first_slice.header.idr_pic_id
    } else {
        0
    };
    std_pic.PicOrderCnt = [pic.top_field_order_cnt, pic.bottom_field_order_cnt];

    // SAFETY: as above — all-zero is a valid StdVideoDecodeH264ReferenceInfo.
    let mut setup_ref: hh::StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
    setup_ref.PicOrderCnt = [pic.top_field_order_cnt, pic.bottom_field_order_cnt];
    // Self-marking (IDR long_term_reference_flag or MMCO 6) keys the slot by
    // LongTermFrameIdx, matching `ref_info`. MMCO 5 is spec-legal and not
    // rejected: these are decoded 8.2.1 values; 8.2.5.4.5 rebases stored
    // frame_num/POC to 0 (`PlanWarning::Mmco5Rebase`).
    let marking = &first_slice.header.dec_ref_pic_marking;
    let self_lt_idx = if pic.is_idr {
        marking.long_term_reference_flag.then_some(0u32)
    } else if marking.adaptive_ref_pic_marking_mode_flag {
        marking
            .inner
            .iter()
            .find(|op| op.memory_management_control_operation == 6)
            .map(|op| op.long_term_frame_idx)
    } else {
        None
    };
    match self_lt_idx {
        Some(idx) => {
            setup_ref.flags.set_used_for_long_term_reference(1);
            // Same saturation as pf-bitstream's frame_num_or_lt_idx: the spec bounds
            // the ue(v)-coded index at 15, the parser does not.
            setup_ref.FrameNum = u16::try_from(idx).unwrap_or(u16::MAX);
        }
        None => setup_ref.FrameNum = pic.frame_num,
    }

    let mut slice_offsets = Vec::with_capacity(plan.slices.len());
    for slice in &plan.slices {
        slice_offsets.push(
            u32::try_from(slice.data.start)
                .map_err(|_| PlanToVkError::OffsetOverflow(slice.data.start))?,
        );
    }

    // Mutations last (fn docs). Removals are deferred, not applied here.
    let (setup_slot, release_after_decode) =
        slots.commit_setup(setup_id, &plan.dpb.removed, Removals::Defer)?;

    Ok(DecodePlanVk {
        std_pic,
        slice_offsets,
        setup_slot,
        setup_ref,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
        release_after_decode,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use cros_codecs::codec::h264::parser::Pps;
    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::Sps;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use cros_codecs::codec::h264::synthesizer::Synthesizer;
    use pf_bitstream::h264::H264Planner;
    use pf_bitstream::h264::Level;
    use pf_bitstream::testing::h264::authored_sps_pps;
    use pf_bitstream::testing::h264::write_idr_slice;
    use pf_bitstream::testing::h264::write_p_slice;
    use pf_bitstream::testing::h264::write_slice;
    use pf_bitstream::testing::h264::SliceSpec;
    use pf_bitstream::testing::split_h264_aus;

    use super::*;

    const TEST_25FPS: &[u8] = pf_bitstream::testing::H264_25FPS;

    /// Picture-pool occupancy (`bound`/`pending`/`held`) over the vendored
    /// vector, with a consumer that holds `hold` delivered frames before
    /// releasing the oldest. Returns the first starved AU index, if any.
    fn simulate_pool_occupancy(pool_size: usize, hold: usize) -> Option<usize> {
        use std::collections::VecDeque;

        #[derive(Clone, Default)]
        struct SimPicture {
            bound: bool,
            pending: bool,
            held: u32,
        }

        let aus = split_h264_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut pictures = vec![SimPicture::default(); pool_size];
        let mut slot_image: Vec<Option<usize>> = Vec::new();
        let mut pending: BTreeMap<PicId, usize> = BTreeMap::new();
        let mut consumer: VecDeque<usize> = VecDeque::new();

        for (index, au) in aus.iter().enumerate() {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| {
                slot_image = vec![None; plan.picture.max_dpb_frames + 1];
                SlotMap::new(plan.picture.max_dpb_frames)
            });
            let vk = plan_to_vk(&plan, slots, 0).expect("the clean vector converts");

            // Binding sync before deferred releases: releasing first would
            // unbind images this AU's own references still read.
            let setup = usize::from(vk.setup_slot);
            let mut held_slots = vec![false; slot_image.len()];
            for (slot, _id) in slots.held() {
                held_slots[usize::from(slot)] = true;
            }
            for (slot, binding) in slot_image.iter_mut().enumerate() {
                if let Some(picture) = *binding {
                    if !held_slots[slot] || slot == setup {
                        pictures[picture].bound = false;
                        *binding = None;
                    }
                }
            }

            let Some(dst) = pictures
                .iter()
                .position(|p| !p.bound && !p.pending && p.held == 0)
            else {
                return Some(index);
            };
            pictures[dst].pending = true;
            pictures[dst].bound = true;
            slot_image[setup] = Some(dst);
            pending.insert(vk.setup_id, dst);

            // Deferred releases, as `Decoder::decode` applies them.
            for id in &vk.release_after_decode {
                assert!(slots.release(*id), "a deferred id held no slot");
            }

            for id in &plan.dpb.outputs {
                if let Some(picture) = pending.remove(id) {
                    pictures[picture].pending = false;
                    pictures[picture].held += 1;
                    consumer.push_back(picture);
                }
            }
            for id in &plan.dpb.removed {
                if let Some(picture) = pending.remove(id) {
                    pictures[picture].pending = false;
                }
            }
            // Releases only once the consumer holds more than `hold`.
            while consumer.len() > hold {
                let released = consumer.pop_front().expect("nonempty");
                pictures[released].held -= 1;
            }
        }
        None
    }

    /// The pool must absorb DPB residency (`max_dpb_frames + 1`) and four
    /// held delivered frames at once. `required_slots + HOLD_HEADROOM` never
    /// starves; an under-headroomed pool on the same stream does.
    #[test]
    fn the_full_vector_with_a_hold_four_consumer_never_starves_the_picture_pool() {
        // This vector: max_dpb_frames = 7 → required_slots = 8.
        let required_slots = 8;
        let headroom = crate::images::HOLD_HEADROOM as usize;
        assert_eq!(
            simulate_pool_occupancy(required_slots + headroom, 4),
            None,
            "the shipped sizing must survive the whole vector with 4 held frames"
        );
        assert!(
            simulate_pool_occupancy(required_slots + 2, 4).is_some(),
            "an under-headroomed pool must starve (else this regression proves nothing)"
        );
    }

    #[test]
    fn the_full_25fps_vector_converts_with_stable_slots_and_start_code_offsets() {
        let aus = split_h264_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut held: BTreeMap<PicId, u8> = BTreeMap::new();
        let mut converted = 0usize;

        for au in &aus {
            let plan = planner.plan_au(au).expect("the clean vector plans");
            let slots = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let vk = plan_to_vk(&plan, slots, 0).expect("the clean vector converts");
            converted += 1;

            for r in &vk.refs {
                assert_eq!(
                    held.get(&r.id),
                    Some(&r.slot),
                    "a referenced picture's slot changed while it was referenced"
                );
                assert_ne!(r.slot, vk.setup_slot, "a reference aliases the setup slot");
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

            assert_eq!(vk.std_pic.frame_num, plan.picture.frame_num);
            assert_eq!(
                u32::from(plan.picture.is_idr),
                vk.std_pic.flags.IdrPicFlag()
            );
            assert_eq!(
                vk.setup_ref.PicOrderCnt[0],
                plan.picture.top_field_order_cnt
            );

            // Drop `removed` only after the deferred releases run — that is
            // when the map drops them.
            let stored = plan.dpb.stored.unwrap();
            assert_eq!(vk.setup_id, stored);
            assert_eq!(vk.setup_is_reference, plan.picture.is_reference);
            held.insert(stored, vk.setup_slot);
            assert_eq!(
                vk.release_after_decode,
                plan.dpb
                    .removed
                    .iter()
                    .copied()
                    .filter(|id| *id != stored)
                    .collect::<Vec<_>>(),
                "the deferral is the plan's whole `removed` list less the stored id"
            );
            for id in &vk.release_after_decode {
                assert!(slots.release(*id), "a deferred id held no slot");
            }
            for id in &plan.dpb.removed {
                held.remove(id);
            }

            let ledger: BTreeMap<PicId, u8> = slots.held().map(|(slot, id)| (id, slot)).collect();
            assert_eq!(ledger, held);
        }

        assert_eq!(converted, 250, "the vector's own golden");

        let mut slots = slots.unwrap();
        slots.apply(&planner.flush());
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn an_mmco_self_marking_sets_the_setup_lt_flag_and_later_refs_carry_it() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_slice(&SliceSpec {
            idr: true,
            ref_idc: 3,
            idr_pic_id: 7,
            ..Default::default()
        }));
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(4, 1), (6, 0)]));
        let au2 = write_p_slice(2, 4, 1, 2, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);

        let idr_id = p0.dpb.stored.unwrap();
        let vk0 = plan_to_vk(&p0, &mut slots, 0).unwrap();
        assert_eq!(vk0.std_pic.flags.IdrPicFlag(), 1);
        assert_eq!(vk0.std_pic.flags.is_intra(), 1);
        assert_eq!(vk0.std_pic.idr_pic_id, 7, "from the authored slice header");
        assert_eq!(vk0.std_pic.PicOrderCnt, [0, 0]);
        assert_eq!(vk0.setup_ref.flags.used_for_long_term_reference(), 0);
        assert_eq!(vk0.setup_id, idr_id);
        assert!(vk0.setup_is_reference, "an IDR is a reference");
        assert!(vk0.refs.is_empty());

        let p1 = planner.plan_au(&au1).unwrap();
        let lt_id = p1.dpb.stored.unwrap();
        let vk1 = plan_to_vk(&p1, &mut slots, 0).unwrap();
        assert_eq!(vk1.std_pic.flags.is_intra(), 0);
        assert_eq!(vk1.std_pic.PicOrderCnt, [2, 2]);
        assert_eq!(vk1.setup_ref.flags.used_for_long_term_reference(), 1);
        assert_eq!(vk1.setup_ref.FrameNum, 0, "LongTermFrameIdx, not frame_num");
        assert_eq!(vk1.refs.len(), 1);
        assert_eq!(vk1.refs[0].id, idr_id);
        assert_eq!(vk1.refs[0].std.flags.used_for_long_term_reference(), 0);

        let p2 = planner.plan_au(&au2).unwrap();
        let vk2 = plan_to_vk(&p2, &mut slots, 0).unwrap();
        let by_id: BTreeMap<PicId, &VkRef> = vk2.refs.iter().map(|r| (r.id, r)).collect();
        let idr_ref = by_id[&idr_id];
        assert_eq!(idr_ref.std.flags.used_for_long_term_reference(), 0);
        assert_eq!(idr_ref.std.FrameNum, 0);
        assert_eq!(idr_ref.slot, vk0.setup_slot);
        let lt_ref = by_id[&lt_id];
        assert_eq!(lt_ref.std.flags.used_for_long_term_reference(), 1);
        assert_eq!(lt_ref.std.FrameNum, 0, "LongTermFrameIdx, not frame_num");
        assert_eq!(
            lt_ref.std.PicOrderCnt,
            [2, 2],
            "the stored top/bottom pair (equal here: no delta_pic_order_cnt_bottom)"
        );
        assert_eq!(lt_ref.slot, vk1.setup_slot);
        assert_ne!(vk2.setup_slot, idr_ref.slot);
        assert_ne!(vk2.setup_slot, lt_ref.slot);
    }

    #[test]
    fn a_failed_conversion_leaves_the_slot_map_untouched_and_the_session_recovers() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        let au1 = write_p_slice(1, 2, 1, 1, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&au1).unwrap();

        // Right-sized map that never saw AU0: the reference must fail, not
        // resolve to a fabricated slot.
        let mut fresh = SlotMap::new(p1.picture.max_dpb_frames);
        fresh.assign(999).unwrap();
        assert_eq!(
            plan_to_vk(&p1, &mut fresh, 0).unwrap_err(),
            PlanToVkError::UnresolvedReference(idr_id)
        );

        assert_eq!(fresh.active(), 1);
        assert_eq!(fresh.held().collect::<Vec<_>>(), vec![(0, 999)]);

        // Next valid AU (IDR restart) still converts. Its `removed` names ids
        // this map never assigned — tolerated by design.
        let p2 = planner.plan_au(&write_idr_slice()).unwrap();
        let vk2 = plan_to_vk(&p2, &mut fresh, 0).unwrap();
        assert_eq!(vk2.setup_slot, 1, "the lowest free slot after the held one");
        assert_eq!(fresh.active(), 2);
    }

    #[test]
    fn an_sps_switch_that_resizes_the_dpb_is_a_capacity_mismatch_not_a_guess() {
        // Level 1 at 320x240: MaxDpbMbs 396/300 = 1 frame. Level 4: 16.
        let authored = |level: Level, max_refs: u8| -> (Rc<Sps>, Rc<Pps>) {
            let sps = SpsBuilder::new()
                .seq_parameter_set_id(0)
                .profile_idc(Profile::Main)
                .level_idc(level)
                .frame_mbs_only_flag(true)
                .direct_8x8_inference_flag(true)
                .max_num_ref_frames(max_refs)
                .log2_max_frame_num_minus4(0)
                .pic_order_cnt_type(0)
                .log2_max_pic_order_cnt_lsb_minus4(0)
                .resolution(320, 240)
                .build();
            let pps = PpsBuilder::new(Rc::clone(&sps))
                .pic_parameter_set_id(0)
                .pic_init_qp(26)
                .build();
            (sps, pps)
        };
        let (sps_a, pps_a) = authored(Level::L1, 1);
        let (sps_b, pps_b) = authored(Level::L4, 4);

        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps_a, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps_a, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        let mut au1 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps_b, &mut au1, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps_b, &mut au1, true).unwrap();
        au1.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert_eq!(p0.picture.max_dpb_frames, 1);
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);
        plan_to_vk(&p0, &mut slots, 0).unwrap();

        let p1 = planner.plan_au(&au1).unwrap();
        assert_eq!(p1.picture.max_dpb_frames, 16);
        assert_eq!(
            plan_to_vk(&p1, &mut slots, 0).unwrap_err(),
            PlanToVkError::CapacityMismatch {
                required: 17,
                capacity: 2
            }
        );
    }

    #[test]
    fn a_full_dpb_bump_reuses_the_slot_but_the_pool_model_binds_a_fresh_image() {
        // Depth-1 DPB (Level 1 at 320x240 → max_dpb_frames 1, capacity 2):
        // every stored P evicts the previous picture into both `outputs` and
        // `removed` of the same plan. AU1 still references that picture, so
        // setup must take the spare; the pool must not rebind a held image.
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L1)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(1)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .resolution(320, 240)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);
        let vk0 = plan_to_vk(&p0, &mut slots, 0).unwrap();

        // Image 0 hosts picture 0; the consumer holds the delivered frame.
        let pool = 2 + 1; // required_slots + 1 of headroom is enough here
        let mut bound = vec![false; pool];
        let mut held = vec![0u32; pool];
        let mut slot_image: Vec<Option<usize>> = vec![None; slots.capacity()];
        let free = |bound: &[bool], held: &[u32]| (0..pool).find(|&i| !bound[i] && held[i] == 0);

        let img0 = free(&bound, &held).unwrap();
        bound[img0] = true;
        slot_image[usize::from(vk0.setup_slot)] = Some(img0);

        let p1 = planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        assert!(p1.dpb.outputs.contains(&vk0.setup_id) && p1.dpb.removed.contains(&vk0.setup_id));
        let vk1 = plan_to_vk(&p1, &mut slots, 0).unwrap();

        // AU1 still references the picture it evicts, so the eviction is
        // deferred and setup takes the spare slot.
        assert!(
            vk1.refs.iter().any(|r| r.id == vk0.setup_id),
            "AU1 must reference the picture it evicts, or this proves nothing"
        );
        assert_ne!(
            vk1.setup_slot, vk0.setup_slot,
            "the setup must not take the slot of a picture this AU still references"
        );
        for r in &vk1.refs {
            assert_ne!(r.slot, vk1.setup_slot, "a reference aliases the setup slot");
        }
        assert_eq!(
            vk1.release_after_decode,
            vec![vk0.setup_id],
            "the eviction is handed back, not applied"
        );

        // Slot still held through the submit, so picture 0's image stays bound
        // as a reference while the consumer holds the delivered frame.
        assert!(
            slots
                .held()
                .any(|(slot, id)| slot == vk0.setup_slot && id == vk0.setup_id),
            "the referenced picture must still hold its slot through the submission"
        );
        bound[img0] = false;
        held[img0] += 1;

        let img1 = free(&bound, &held).expect("headroom guarantees a free image");
        assert_ne!(
            img1, img0,
            "the held (delivered, unreleased) image must never be re-bound as a \
             decode target — the pool decoupling IS the overwrite fix"
        );
        bound[img1] = true;
        slot_image[usize::from(vk1.setup_slot)] = Some(img1);

        // Deferred release lands after submit; only then is the evicted slot free.
        for id in &vk1.release_after_decode {
            assert!(slots.release(*id));
        }
        assert!(
            !slots.held().any(|(slot, _)| slot == vk0.setup_slot),
            "the deferred release must actually free the slot"
        );

        held[img0] -= 1;
        assert_eq!(free(&bound, &held), Some(img0));
    }

    #[test]
    fn a_nonzero_delta_bottom_reaches_setup_and_reference_poc_pairs_distinctly() {
        // Builder has no setter for bottom_field_pic_order_in_frame_present_flag,
        // so the Pps is constructed directly (fields are public) and synthesized.
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .resolution(64, 64)
            .build();
        let pps = Pps {
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            entropy_coding_mode_flag: false,
            bottom_field_pic_order_in_frame_present_flag: true,
            num_slice_groups_minus1: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            weighted_pred_flag: false,
            weighted_bipred_idc: 0,
            pic_init_qp_minus26: 0,
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            deblocking_filter_control_present_flag: false,
            constrained_intra_pred_flag: false,
            redundant_pic_cnt_present_flag: false,
            transform_8x8_mode_flag: false,
            pic_scaling_matrix_present_flag: false,
            scaling_lists_4x4: [[0; 16]; 6],
            scaling_lists_8x8: [[0; 64]; 6],
            second_chroma_qp_index_offset: 0,
            sps: Rc::clone(&sps),
        };

        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        // top 0, bottom 0 + 2
        au0.extend(write_slice(&SliceSpec {
            idr: true,
            ref_idc: 3,
            bottom_delta: Some(2),
            ..Default::default()
        }));
        // top 4, bottom 4 + 1
        let au1 = write_slice(&SliceSpec {
            frame_num: 1,
            poc_lsb: 4,
            bottom_delta: Some(1),
            ..Default::default()
        });

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert_eq!(
            (
                p0.picture.top_field_order_cnt,
                p0.picture.bottom_field_order_cnt
            ),
            (0, 2)
        );
        let mut slots = SlotMap::new(p0.picture.max_dpb_frames);
        let vk0 = plan_to_vk(&p0, &mut slots, 0).unwrap();
        // Both structs, both orders: a swap or a collapse to one value must fail.
        assert_eq!(vk0.std_pic.PicOrderCnt, [0, 2]);
        assert_eq!(vk0.setup_ref.PicOrderCnt, [0, 2]);

        let p1 = planner.plan_au(&au1).unwrap();
        let vk1 = plan_to_vk(&p1, &mut slots, 0).unwrap();
        assert_eq!(vk1.std_pic.PicOrderCnt, [4, 5]);
        assert_eq!(vk1.setup_ref.PicOrderCnt, [4, 5]);
        // Stored IDR pair through RefPic, not a fabricated bottom.
        assert_eq!(vk1.refs.len(), 1);
        assert_eq!(vk1.refs[0].id, p0.dpb.stored.unwrap());
        assert_eq!(vk1.refs[0].std.PicOrderCnt, [0, 2]);
    }
}
