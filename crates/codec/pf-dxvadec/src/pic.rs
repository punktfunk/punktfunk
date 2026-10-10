//! Per-AU H.264 conversion: one [`AuPlan`] into the `DXVA_PicParams_H264`,
//! `DXVA_Qmatrix_H264` and slice-control records
//! `ID3D11VideoContext::SubmitDecoderBuffers` is built from —
//! `pf_vkdecode::pic`'s job, one hardware API over.
//!
//! Surfaces are slots. A `DXVA_PicEntry_H264` carries the uncompressed surface
//! index, so DPB slot and decode-texture `ArraySlice` are the same number and
//! this module drives the shared [`SlotMap`] unchanged.
//!
//! `RefFrameList` is the marked DPB ([`AuPlan::dpb_refs`]), not the AU's
//! reference set — DXVA asks for every picture currently used for reference;
//! Vulkan's `pReferenceSlots` is only the slots THIS decode uses. AU
//! references first, then the rest; a DPB deeper than 16 can only lose a
//! picture no slice names. The snapshot is the authority for marking, POC
//! pair and `FrameNumList` key (concealment can relabel a long-term
//! substitute as short-term in the slice lists).
//!
//! Progressive envelope: pf-bitstream rejects interlaced streams, so field
//! flags are written for a frame. Top/bottom PicOrderCnt pairs still ride
//! through; they differ when the PPS codes `bottom_field_pic_order_in_frame_present_flag`.

use std::ops::Range;

use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::RefPic;
use pf_bitstream::slots::Removals;
use pf_bitstream::slots::SlotError;
use pf_bitstream::slots::SlotMap;
use tracing::trace;

use crate::dxva::H264BitFields;
use crate::dxva::PicEntry;
use crate::dxva::PicParamsH264;
use crate::dxva::QmatrixH264;
use crate::dxva::SliceH264Short;

/// DXVA `RefFrameList` length, and H.264's own reference ceiling.
const REF_FRAME_LIST_LEN: usize = 16;

/// One `RefFrameList` entry: a marked DPB picture resolved to its surface.
/// Kept beside the packed params so tests can assert the mapping, not the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxvaRef {
    /// DPB slot = decode texture `ArraySlice`.
    pub slot: u8,
    pub id: PicId,
    pub is_long_term: bool,
    /// 8.2.1 field order counts — `FieldOrderCntList[i]`.
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// `FrameNumList[i]`: `frame_num` for a short-term reference,
    /// `LongTermFrameIdx` for a long-term one.
    pub frame_num_or_lt_idx: u16,
}

impl DxvaRef {
    fn new(slot: u8, rp: &RefPic) -> DxvaRef {
        DxvaRef {
            slot,
            id: rp.id,
            is_long_term: rp.is_long_term,
            top_field_order_cnt: rp.top_field_order_cnt,
            bottom_field_order_cnt: rp.bottom_field_order_cnt,
            frame_num_or_lt_idx: rp.frame_num_or_lt_idx,
        }
    }
}

/// CPU-derivable half of one AU's DXVA submission. The Windows layer adds the
/// decoder, mapped buffers and output view.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodePlanDxva {
    pub pic_params: PicParamsH264,
    pub qmatrix: QmatrixH264,
    /// Slice NAL byte ranges, start code dropped — what [`crate::pack::pack`] takes.
    pub slice_ranges: Vec<Range<usize>>,
    /// Decode target surface (`CreateVideoDecoderOutputView` / `DecoderBeginFrame`).
    pub setup_slot: u8,
    /// Planner id of the decoded picture (`AuPlan.dpb.stored`).
    pub setup_id: PicId,
    /// Whether later AUs may reference this picture. When `false` the surface
    /// exists for this decode plus remaining DPB residency, and this AU's
    /// `removed` may already have released it.
    pub setup_is_reference: bool,
    /// Pictures this AU's end-of-picture bookkeeping retires while the
    /// submission still names them. Release once the decode op is issued —
    /// never inside the conversion.
    ///
    /// [`SlotMap::assign`] takes the lowest free slot. Release here and the
    /// setup assignment reuses the surface, so `CurrPic` and a `RefFrameList`
    /// entry alias. [`AuPlan::dpb_refs`] is snapshotted in `begin_picture`,
    /// before 8.2.5 marking and C.4.5.3 bump, so a sliding-window unmark plus
    /// bump can land the same picture in both `dpb_refs` and `dpb.removed`.
    ///
    /// The surfaces must outlive the conversion, not the decode: one AU is
    /// planned, converted and submitted before the next. The whole `removed`
    /// list is deferred — `refs` is also built from the slice lists (a
    /// `dpb_refs` filter would miss a concealment substitute), and
    /// [`SlotMap::new`]'s spare slot always leaves `assign` a free slot.
    pub release_after_decode: Vec<PicId>,
    /// Marked DPB, resolved to surfaces — AU references first, then the rest
    /// (module docs). Same order as `pic_params.RefFrameList`.
    pub refs: Vec<DxvaRef>,
    /// `MbWidth * MbHeight`. The picture parameters already carry both numbers,
    /// but the Windows layer sees only packed bytes and a driver that validates
    /// the descriptor rejects `NumMBsInBuffer == 0` at `SubmitDecoderBuffers`.
    pub mb_count: u32,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream
/// degrades it to [`pf_bitstream::h264::PlanWarning`]s upstream; these are
/// caller bugs or features this backend does not submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaError {
    NoSlices,
    /// `DpbUpdate.stored` is `None`. `plan_au` always stores; only `flush()`
    /// produces this, and those updates go to [`SlotMap::apply`] directly.
    NoStoredId,
    /// A reference id holds no slot — an earlier plan never went through this map.
    UnresolvedReference(PicId),
    Slot(SlotError),
    /// Map DPB depth differs from this plan's `max_dpb_frames` — SPS
    /// renegotiation. Rebuild decoder, surface pool and [`SlotMap`]; converting
    /// against the stale map would hand out surface indices the pool lacks.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// More distinct references than `RefFrameList` holds. Unreachable from a
    /// spec-conformant stream (16 is H.264's ceiling too). An error, not a
    /// truncation: a dropped reference decodes to a wrong picture.
    TooManyReferences(usize),
    /// FMO (`num_slice_groups_minus1 != 0`). This backend does not build
    /// `SliceGroupMap`; refusing hands the session to the FFmpeg rung.
    SliceGroups {
        count: u32,
    },
    /// `separate_colour_plane_flag`. Refused upstream; checked again because
    /// the picture-parameters layout for it is a different shape.
    SeparateColourPlanes,
    /// A picture dimension exceeds the `USHORT` the picture parameters carry
    /// (65536 macroblocks a side).
    DimensionOverflow {
        width_mbs: u32,
        height_mbs: u32,
    },
}

impl std::fmt::Display for PlanToDxvaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaError::NoSlices => write!(f, "the plan holds no slices"),
            PlanToDxvaError::NoStoredId => write!(
                f,
                "the plan stores no picture (flush updates go to SlotMap::apply)"
            ),
            PlanToDxvaError::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToDxvaError::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToDxvaError::CapacityMismatch { required, capacity } => write!(
                f,
                "the plan needs {required} slots but the map holds {capacity} — \
                 an SPS renegotiation resized the DPB; rebuild decoder and map"
            ),
            PlanToDxvaError::TooManyReferences(count) => {
                write!(f, "{count} references exceed DXVA's RefFrameList of 16")
            }
            PlanToDxvaError::SliceGroups { count } => write!(
                f,
                "the PPS codes {count} slice groups (FMO), which this backend does not submit"
            ),
            PlanToDxvaError::SeparateColourPlanes => {
                write!(f, "separate_colour_plane_flag is outside this backend")
            }
            PlanToDxvaError::DimensionOverflow {
                width_mbs,
                height_mbs,
            } => write!(
                f,
                "a {width_mbs}x{height_mbs}-macroblock picture exceeds the DXVA picture parameters"
            ),
        }
    }
}

impl std::error::Error for PlanToDxvaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToDxvaError::Slot(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToDxvaError {
    fn from(err: SlotError) -> Self {
        PlanToDxvaError::Slot(err)
    }
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// `status_id` becomes `StatusReportFeedbackNumber`. libavcodec starts at 1;
/// 0 is the value a driver reads out of a buffer nobody wrote.
///
/// Atomicity matches `pf_vkdecode::plan_to_vk`: every fallible step runs
/// before any mutation of `slots`. Envelope and capacity first (read-only);
/// references resolve against the pre-removal state (this AU's marking can
/// evict a picture its slices still name); setup is assigned last, and
/// `removed` leaves as [`DecodePlanDxva::release_after_decode`].
pub fn plan_to_dxva(
    plan: &AuPlan,
    slots: &mut SlotMap,
    status_id: u32,
) -> Result<DecodePlanDxva, PlanToDxvaError> {
    let first_slice = plan.slices.first().ok_or(PlanToDxvaError::NoSlices)?;
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaError::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if pps.num_slice_groups_minus1 != 0 {
        return Err(PlanToDxvaError::SliceGroups {
            count: pps.num_slice_groups_minus1 + 1,
        });
    }
    if sps.separate_colour_plane_flag {
        return Err(PlanToDxvaError::SeparateColourPlanes);
    }

    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToDxvaError::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // Height is FRAME macroblocks, not a field count. The map-units count
    // doubles for a non-frame-only SPS — unreachable under the progressive
    // envelope, written so the expression matches the spec.
    let width_mbs = u32::from(sps.pic_width_in_mbs_minus1) + 1;
    let height_mbs = (u32::from(sps.pic_height_in_map_units_minus1) + 1)
        * (2 - u32::from(sps.frame_mbs_only_flag));
    let (Ok(width_minus1), Ok(height_minus1)) = (
        u16::try_from(width_mbs.saturating_sub(1)),
        u16::try_from(height_mbs.saturating_sub(1)),
    ) else {
        return Err(PlanToDxvaError::DimensionOverflow {
            width_mbs,
            height_mbs,
        });
    };

    // Marked DPB, AU references first (module docs). Per-slice `ref_idx` order
    // lives in the slice headers the hardware parses, not here.
    let mut refs: Vec<DxvaRef> = Vec::new();
    for slice in &plan.slices {
        for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
            if refs.iter().any(|existing| existing.id == rp.id) {
                continue;
            }
            // Snapshot is the authority for marking and pair-key (module docs).
            // A list naming an unmarked picture is a planner-contract violation;
            // the entry's own copy is the fallback if it ever happens.
            let marked = plan.dpb_refs.iter().find(|d| d.id == rp.id);
            if marked.is_none() {
                trace!(
                    id = rp.id,
                    "a slice list names a picture the marked DPB does not hold"
                );
            }
            let slot = slots
                .slot_of(rp.id)
                .ok_or(PlanToDxvaError::UnresolvedReference(rp.id))?;
            refs.push(DxvaRef::new(slot, marked.unwrap_or(rp)));
        }
    }
    if refs.len() > REF_FRAME_LIST_LEN {
        return Err(PlanToDxvaError::TooManyReferences(refs.len()));
    }
    // Rest of the marked DPB. Overflow is dropped, not refused: these pictures
    // are not referenced, so a missing tail does not change the decode. An
    // unseen slot is skipped for the same reason — AU refs already resolved.
    for rp in &plan.dpb_refs {
        if refs.len() == REF_FRAME_LIST_LEN {
            trace!(
                marked = plan.dpb_refs.len(),
                "the marked DPB exceeds RefFrameList; the tail is not expressible"
            );
            break;
        }
        if refs.iter().any(|existing| existing.id == rp.id) {
            continue;
        }
        match slots.slot_of(rp.id) {
            Some(slot) => refs.push(DxvaRef::new(slot, rp)),
            None => trace!(id = rp.id, "a marked DPB picture holds no slot in this map"),
        }
    }

    let mut pp = PicParamsH264::zeroed();
    pp.wFrameWidthInMbsMinus1 = width_minus1;
    pp.wFrameHeightInMbsMinus1 = height_minus1;
    pp.num_ref_frames = sps.max_num_ref_frames;
    pp.bit_depth_luma_minus8 = pic.bit_depth_luma_minus8;
    pp.bit_depth_chroma_minus8 = pic.bit_depth_chroma_minus8;
    // Reserved, not free: libavcodec and the Microsoft reference decoder write
    // 3 for every standard profile. 0 is the Intel ClearVideo / ATI zigzag
    // workaround, which this backend does not use.
    pp.Reserved16Bits = 3;
    pp.StatusReportFeedbackNumber = status_id;
    pp.CurrFieldOrderCnt = [pic.top_field_order_cnt, pic.bottom_field_order_cnt];
    pp.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
    pp.chroma_qp_index_offset = pps.chroma_qp_index_offset;
    pp.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;
    // Fields after ContinuationFlag are present. Always: we never truncate.
    pp.ContinuationFlag = 1;
    pp.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
    // PPS defaults, not a slice override. Each slice header carries its own
    // `num_ref_idx_active_override_flag` for the hardware to apply.
    pp.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    pp.frame_num = pic.frame_num;
    pp.log2_max_frame_num_minus4 = sps.log2_max_frame_num_minus4;
    pp.pic_order_cnt_type = sps.pic_order_cnt_type;
    // Each POC type reads exactly one of these; the other stays 0.
    if sps.pic_order_cnt_type == 0 {
        pp.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
    } else if sps.pic_order_cnt_type == 1 {
        pp.delta_pic_order_always_zero_flag = u8::from(sps.delta_pic_order_always_zero_flag);
    }
    pp.direct_8x8_inference_flag = u8::from(sps.direct_8x8_inference_flag);
    pp.entropy_coding_mode_flag = u8::from(pps.entropy_coding_mode_flag);
    pp.pic_order_present_flag = u8::from(pps.bottom_field_pic_order_in_frame_present_flag);
    // FMO fields stay 0: slice groups were refused above.
    pp.deblocking_filter_control_present_flag =
        u8::from(pps.deblocking_filter_control_present_flag);
    pp.redundant_pic_cnt_present_flag = u8::from(pps.redundant_pic_cnt_present_flag);

    let is_intra = plan
        .slices
        .iter()
        .all(|slice| slice.header.slice_type.is_i() || slice.header.slice_type.is_si());
    pp.wBitFields = H264BitFields {
        chroma_format_idc: pic.chroma_format_idc,
        ref_pic_flag: pic.nal_ref_idc != 0,
        constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
        weighted_pred_flag: pps.weighted_pred_flag,
        weighted_bipred_idc: pps.weighted_bipred_idc,
        frame_mbs_only_flag: sps.frame_mbs_only_flag,
        transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
        // DXVA: MinLumaBipredSize8x8Flag is `level_idc >= 31` (8x8 is then
        // the smallest bi-predicted luma block). libavcodec writes the same.
        min_luma_bipred_size_8x8: pic.level_idc as u8 >= 31,
        intra_pic_flag: is_intra,
    }
    .pack();

    // Unused entries are 0xFF with zeroed counts — spec and shipping-decoder padding.
    pp.RefFrameList = [PicEntry::UNUSED; REF_FRAME_LIST_LEN];
    for (i, r) in refs.iter().enumerate() {
        pp.RefFrameList[i] = PicEntry::new(r.slot, r.is_long_term);
        // Both field counts of a progressive frame. TOP first — a swapped pair
        // is invisible when the PPS does not code a distinct bottom count.
        pp.FieldOrderCntList[i] = [r.top_field_order_cnt, r.bottom_field_order_cnt];
        pp.FrameNumList[i] = r.frame_num_or_lt_idx;
        // Two bits per entry: top at 2i, bottom at 2i+1. A progressive frame
        // is marked for both. `i < 16` bounds the shift.
        pp.UsedForReferenceFlags |= 0b11 << (2 * i);
    }
    // NonExistingFrameFlags stays 0: pf-bitstream substitutes a lost reference
    // and warns; it never hands over a placeholder for the hardware to invent.

    // Coded (zig-zag) order, both the parser and DXVA. PPS lists are
    // authoritative: Table 7-2 fallbacks already applied (SPS or flat default).
    let mut qm = QmatrixH264::zeroed();
    qm.bScalingLists4x4 = pps.scaling_lists_4x4;
    // Only Intra-Y and Inter-Y: DXVA has two 8x8 slots because 8x8 chroma
    // lists exist only in 4:4:4, refused above. Parser stores the 4:2:0 pair
    // at 0 and 1, not 0 and 3 (libavcodec's `scaling_matrix8` indexing).
    qm.bScalingLists8x8[0] = pps.scaling_lists_8x8[0];
    qm.bScalingLists8x8[1] = pps.scaling_lists_8x8[1];

    let slice_ranges: Vec<Range<usize>> = plan.slices.iter().map(|s| s.nal.clone()).collect();

    // Mutations last (fn docs). Removals are deferred, not applied here.
    let (setup_slot, release_after_decode) =
        slots.commit_setup(setup_id, &plan.dpb.removed, Removals::Defer)?;
    // AssociatedFlag is the bottom-field flag; 0 under the progressive envelope.
    pp.CurrPic = PicEntry::new(setup_slot, false);

    // DXVA picture parameters name no PPS id: the parameter set travels in
    // the picture parameters. `first_slice` is only this cross-check.
    debug_assert_eq!(
        first_slice.header.pic_parameter_set_id, pps.pic_parameter_set_id,
        "the plan's activated PPS must be the first slice's"
    );

    Ok(DecodePlanDxva {
        pic_params: pp,
        qmatrix: qm,
        slice_ranges,
        setup_slot,
        setup_id,
        setup_is_reference: pic.is_reference,
        release_after_decode,
        refs,
        mb_count: width_mbs * height_mbs,
    })
}

/// Slice-control records for a packed AU. Split from [`plan_to_dxva`]
/// because the byte locations exist only after the bitstream buffer is
/// mapped and packed — conversion is per-plan, this is per-submission.
pub fn slice_control(records: &[crate::pack::SliceRecord]) -> Vec<SliceH264Short> {
    records
        .iter()
        .map(|r| SliceH264Short {
            BSNALunitDataLocation: r.location,
            SliceBytesInBuffer: r.bytes,
            // Whole slice in one buffer; this backend never chops.
            wBadSliceChopping: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use cros_codecs::codec::h264::parser::Pps;
    use cros_codecs::codec::h264::parser::Sps;
    use cros_codecs::codec::h264::synthesizer::Synthesizer;
    use pf_bitstream::h264::H264Planner;
    use pf_bitstream::testing::h264::authored_sps_pps;
    use pf_bitstream::testing::h264::write_idr_slice;
    use pf_bitstream::testing::h264::write_p_slice;
    use pf_bitstream::testing::split_h264_aus;

    use super::*;

    const TEST_25FPS: &[u8] = pf_bitstream::testing::H264_25FPS;

    /// Host-emitted low-delay IPPP: `max_num_reorder_frames = 0` and a DPB
    /// as deep as its reference count — the shape `release_after_decode`
    /// exists for. Shared with `pf-vkdecode` and `pf-client-core` GPU legs.
    const LOWDELAY_640X480: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h264");

    fn convert_stream() -> Vec<(AuPlan, DecodePlanDxva)> {
        convert(TEST_25FPS)
    }

    fn convert_low_delay() -> Vec<(AuPlan, DecodePlanDxva)> {
        convert(LOWDELAY_640X480)
    }

    /// Plan and convert a stream the way a caller does: apply
    /// `release_after_decode` once the (notional) decode op is issued.
    fn convert(stream: &[u8]) -> Vec<(AuPlan, DecodePlanDxva)> {
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut out = Vec::new();
        for (i, au) in split_h264_aus(stream).into_iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            if map.capacity() != plan.picture.max_dpb_frames + 1 {
                *map = SlotMap::new(plan.picture.max_dpb_frames);
            }
            let dxva = plan_to_dxva(&plan, map, i as u32 + 1).expect("conversion");
            for &id in &dxva.release_after_decode {
                assert!(map.release(id), "AU {i}: deferred id {id} held no slot");
            }
            out.push((plan, dxva));
        }
        out
    }

    /// Unique pictures the AU's own slice lists name, in first-appearance order.
    fn au_reference_ids(plan: &AuPlan) -> Vec<PicId> {
        let mut ids = Vec::new();
        for slice in &plan.slices {
            for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
                if !ids.contains(&rp.id) {
                    ids.push(rp.id);
                }
            }
        }
        ids
    }

    #[test]
    fn the_reference_list_is_the_marked_dpb_led_by_the_pictures_this_au_names() {
        let converted = convert_stream();
        let mut wider_than_the_au = 0usize;
        for (plan, dxva) in &converted {
            let au = au_reference_ids(plan);
            // AU references lead: truncation at 16 can only drop a picture no slice names.
            assert_eq!(
                dxva.refs
                    .iter()
                    .take(au.len())
                    .map(|r| r.id)
                    .collect::<Vec<_>>(),
                au
            );
            let mut listed: Vec<PicId> = dxva.refs.iter().map(|r| r.id).collect();
            let mut marked: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            listed.sort_unstable();
            marked.sort_unstable();
            assert_eq!(listed, marked);
            if dxva.refs.len() > au.len() {
                wider_than_the_au += 1;
            }
        }
        // Not a degenerate vector: many AUs name a proper subset of the marked DPB.
        assert!(
            wider_than_the_au >= 100,
            "only {wider_than_the_au} of {} AUs exercised the difference",
            converted.len()
        );
    }

    #[test]
    fn a_long_term_reference_no_slice_names_still_reaches_the_reference_list() {
        // Long-term anchor that the current (truncated) list never names.
        // Authored: no vendored vector carries MMCO 6.
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        // MMCO 4 admits index 0; MMCO 6 assigns it (pin long-term).
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(4, 1), (6, 0)]));
        // One active reference: 8.2.4.2.1 makes that the short-term IDR.
        let au2 = write_p_slice(2, 4, 1, 1, None);

        let mut planner = H264Planner::new();
        let plans: Vec<AuPlan> = [au0.as_slice(), au1.as_slice(), au2.as_slice()]
            .into_iter()
            .map(|au| planner.plan_au(au).expect("plan"))
            .collect();
        let mut slots = SlotMap::new(plans[0].picture.max_dpb_frames);
        let converted: Vec<DecodePlanDxva> = plans
            .iter()
            .enumerate()
            .map(|(i, plan)| plan_to_dxva(plan, &mut slots, i as u32 + 1).expect("convert"))
            .collect();

        let idr_id = plans[0].dpb.stored.unwrap();
        let lt_id = plans[1].dpb.stored.unwrap();
        assert_eq!(au_reference_ids(&plans[2]), vec![idr_id]);

        // AU2's named ref plus the unnamed long-term. Absence may be a retirement.
        let dxva = &converted[2];
        assert_eq!(
            dxva.refs
                .iter()
                .map(|r| (r.id, r.is_long_term, r.frame_num_or_lt_idx))
                .collect::<Vec<_>>(),
            vec![(idr_id, false, 0), (lt_id, true, 0)]
        );
        assert!(
            dxva.pic_params.RefFrameList[1].associated(),
            "AssociatedFlag"
        );
        assert_eq!(dxva.pic_params.FrameNumList[1], 0, "LongTermFrameIdx");
        assert_eq!(dxva.pic_params.UsedForReferenceFlags & 0b1111, 0b1111);
        assert_eq!(dxva.pic_params.RefFrameList[2], PicEntry::UNUSED);
    }

    #[test]
    fn an_unequal_field_order_count_pair_rides_through_top_first() {
        // Vendored vectors have equal top/bottom counts, so a swapped pair is
        // invisible. Drive one AU with distinct counts.
        let mut planner = H264Planner::new();
        let aus = split_h264_aus(TEST_25FPS);
        let first = planner.plan_au(aus[0]).expect("plan 0");
        let mut second = planner.plan_au(aus[1]).expect("plan 1");
        let ref_id = first.dpb.stored.unwrap();

        for rp in second
            .dpb_refs
            .iter_mut()
            .filter(|rp| rp.id == ref_id)
            .chain(
                second.slices[0]
                    .ref_list0
                    .iter_mut()
                    .filter(|rp| rp.id == ref_id),
            )
        {
            rp.top_field_order_cnt = 4;
            rp.bottom_field_order_cnt = 6;
        }

        let mut slots = SlotMap::new(first.picture.max_dpb_frames);
        plan_to_dxva(&first, &mut slots, 1).expect("convert 0");
        let dxva = plan_to_dxva(&second, &mut slots, 2).expect("convert 1");
        assert_eq!(dxva.refs[0].id, ref_id);
        assert_eq!(
            dxva.pic_params.FieldOrderCntList[0],
            [4, 6],
            "TopFieldOrderCnt occupies index 0"
        );
    }

    #[test]
    fn the_macroblock_count_is_the_coded_picture_in_macroblocks() {
        // libavcodec's `NumMBsInBuffer` is `mb_width * mb_height`. This vector
        // is 320x240 → 20×15 macroblocks.
        for (plan, dxva) in convert_stream() {
            let width = u32::from(plan.sps.pic_width_in_mbs_minus1) + 1;
            let height = (u32::from(plan.sps.pic_height_in_map_units_minus1) + 1)
                * (2 - u32::from(plan.sps.frame_mbs_only_flag));
            assert_eq!(dxva.mb_count, width * height);
            assert_eq!(dxva.mb_count, 20 * 15);
            assert_eq!(
                u32::from(dxva.pic_params.wFrameWidthInMbsMinus1 + 1)
                    * u32::from(dxva.pic_params.wFrameHeightInMbsMinus1 + 1),
                dxva.mb_count
            );
        }
    }

    #[test]
    fn the_whole_vendored_stream_converts_without_a_single_refusal() {
        let converted = convert_stream();
        // pf-bitstream's golden for this vector is 250 planned AUs.
        assert_eq!(converted.len(), 250);
    }

    /// `removed ∩ dpb_refs` is the aliasing precondition: a picture this AU
    /// retires while `RefFrameList` still names it. Release it inside the
    /// conversion and [`SlotMap::assign`] hands its surface to `CurrPic`.
    ///
    /// `test-25fps.h264` is blind: level 1.3 with no VUI restriction (DPB 7
    /// vs 2 refs) and it reorders, so an unmarked picture stays for output.
    /// `lowdelay-640x480.h264` is host output with DPB depth equal to the
    /// reference count and `max_num_reorder_frames = 0`, so the window unmarks
    /// the oldest reference in the AU whose bump evicts it.
    ///
    /// HEVC needs no such test: `H265Planner` snapshots `dpb_refs` after
    /// `decode_rps`, so an RPS-dropped picture is never in the snapshot.
    #[test]
    fn the_low_delay_stream_removes_pictures_its_own_reference_list_names_and_the_vector_never_does(
    ) {
        fn intersections(stream: &[u8]) -> (usize, usize, usize) {
            let mut planner = H264Planner::new();
            let (mut both, mut with_removals, mut planned) = (0usize, 0usize, 0usize);
            for au in split_h264_aus(stream) {
                let Ok(plan) = planner.plan_au(au) else {
                    continue;
                };
                planned += 1;
                if !plan.dpb.removed.is_empty() {
                    with_removals += 1;
                }
                both += plan
                    .dpb
                    .removed
                    .iter()
                    .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                    .count();
            }
            (planned, with_removals, both)
        }

        let (planned, with_removals, both) = intersections(TEST_25FPS);
        assert_eq!(planned, 250);
        assert!(
            with_removals > 0,
            "no access unit of the vendored vector removed anything, so the zero below \
             would be empty for a reason that has nothing to do with the hazard"
        );
        assert_eq!(
            both, 0,
            "the vendored vector is supposed to be BLIND to this shape — a non-zero \
             here means the reordering/DPB-depth reasoning above is wrong, and the \
             low-delay numbers below need re-deriving before they mean anything"
        );

        let (planned, with_removals, both) = intersections(LOWDELAY_640X480);
        assert_eq!(planned, 120);
        assert_eq!(with_removals, 117);
        assert_eq!(
            both, 117,
            "the low-delay stream must still exercise the aliasing precondition on \
             nearly every access unit — if this ever drops to zero the deferral below \
             is no longer being TESTED by anything, whatever else still passes"
        );
    }

    /// No submission names its decode surface as a reference — the same
    /// invariant as the vendored-vector test, on the stream that would alias
    /// without `release_after_decode`.
    #[test]
    fn the_low_delay_stream_never_aliases_its_decode_surface_with_a_reference() {
        let converted = convert_low_delay();
        assert_eq!(converted.len(), 120);
        let mut deferred_total = 0usize;
        for (i, (_, dxva)) in converted.iter().enumerate() {
            assert_eq!(dxva.pic_params.CurrPic.index(), dxva.setup_slot);
            deferred_total += dxva.release_after_decode.len();
            for r in &dxva.refs {
                assert_ne!(
                    r.slot, dxva.setup_slot,
                    "AU {i}: reference picture {} shares surface {} with the decode \
                     target — the deferral is not holding",
                    r.id, r.slot
                );
            }
        }
        assert_eq!(
            deferred_total, 117,
            "every access unit that removes a picture must defer it; a zero here with \
             the assertions above still passing would mean the stream stopped \
             exercising the shape"
        );
    }

    /// Holding every removal through setup is free because [`SlotMap::new`]
    /// allocates `max_dpb_frames + 1` and the DPB never exceeds
    /// `max_dpb_frames`.
    #[test]
    fn deferring_every_removal_still_fits_the_ledger() {
        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut peak = 0usize;
        let mut capacity = 0usize;
        for (i, au) in split_h264_aus(LOWDELAY_640X480).into_iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            if map.capacity() != plan.picture.max_dpb_frames + 1 {
                *map = SlotMap::new(plan.picture.max_dpb_frames);
            }
            let dxva = plan_to_dxva(&plan, map, i as u32 + 1).expect("conversion");
            // Peak before deferred releases: the moment `assign` had to find a slot.
            peak = peak.max(map.held().count());
            capacity = map.capacity();
            for &id in &dxva.release_after_decode {
                assert!(map.release(id), "AU {i}: deferred id {id} held no slot");
            }
        }
        assert_eq!(capacity, 4, "max_dec_frame_buffering 3 + the spare slot");
        assert_eq!(
            peak, 4,
            "the deferral is expected to USE the spare slot — a peak of 3 would mean \
             the removals are being released early after all"
        );
    }

    #[test]
    fn the_setup_surface_is_the_current_picture_entry_and_is_never_also_a_reference_entry() {
        for (_, dxva) in convert_stream() {
            assert_eq!(dxva.pic_params.CurrPic.index(), dxva.setup_slot);
            assert!(
                !dxva.pic_params.CurrPic.associated(),
                "progressive: CurrPic's AssociatedFlag is the bottom-field flag"
            );
            // A reference that aliases the target is a surface read and written in one op.
            for r in &dxva.refs {
                assert_ne!(r.slot, dxva.setup_slot, "a reference aliases the target");
            }
        }
    }

    #[test]
    fn reference_entries_carry_their_pictures_frame_num_poc_pair_and_used_flags() {
        for (plan, dxva) in convert_stream() {
            for (i, r) in dxva.refs.iter().enumerate() {
                // DPB snapshot is the authority, not the slice lists' copy.
                let rp = plan
                    .dpb_refs
                    .iter()
                    .find(|d| d.id == r.id)
                    .expect("every entry is a marked DPB picture");
                assert_eq!(dxva.pic_params.RefFrameList[i].index(), r.slot);
                assert_eq!(dxva.pic_params.RefFrameList[i].associated(), r.is_long_term);
                assert_eq!(r.is_long_term, rp.is_long_term);
                assert_eq!(dxva.pic_params.FrameNumList[i], rp.frame_num_or_lt_idx);
                assert_eq!(
                    dxva.pic_params.FieldOrderCntList[i],
                    [rp.top_field_order_cnt, rp.bottom_field_order_cnt]
                );
                // A progressive reference is used for both fields.
                assert_eq!(
                    dxva.pic_params.UsedForReferenceFlags >> (2 * i) & 0b11,
                    0b11
                );
            }
            // Past the marked DPB: 0xFF sentinel, never a stale surface index.
            for i in dxva.refs.len()..REF_FRAME_LIST_LEN {
                assert_eq!(dxva.pic_params.RefFrameList[i], PicEntry::UNUSED);
                assert_eq!(dxva.pic_params.FrameNumList[i], 0);
                assert_eq!(dxva.pic_params.FieldOrderCntList[i], [0, 0]);
                assert_eq!(dxva.pic_params.UsedForReferenceFlags >> (2 * i) & 0b11, 0);
            }
            // pf-bitstream never emits a gap placeholder as an id.
            assert_eq!(dxva.pic_params.NonExistingFrameFlags, 0);
        }
    }

    #[test]
    fn the_idr_is_intra_and_the_inter_pictures_are_not() {
        let converted = convert_stream();
        let (_, first) = &converted[0];
        assert_ne!(first.pic_params.wBitFields & (1 << 15), 0, "IDR is intra");
        assert!(first.refs.is_empty(), "an IDR references nothing");
        // Vector is IPPP: the second picture is inter and references the first.
        let (_, second) = &converted[1];
        assert_eq!(second.pic_params.wBitFields & (1 << 15), 0);
        assert_eq!(second.refs.len(), 1);
    }

    #[test]
    fn the_picture_parameters_carry_the_active_sps_and_pps_verbatim() {
        let converted = convert_stream();
        let (plan, dxva) = &converted[0];
        let pp = &dxva.pic_params;
        assert_eq!(
            pp.wFrameWidthInMbsMinus1, plan.sps.pic_width_in_mbs_minus1,
            "320-wide stream: 20 macroblocks"
        );
        assert_eq!(
            pp.wFrameHeightInMbsMinus1,
            plan.sps.pic_height_in_map_units_minus1
        );
        assert_eq!(pp.num_ref_frames, plan.sps.max_num_ref_frames);
        assert_eq!(pp.frame_num, plan.picture.frame_num);
        assert_eq!(
            pp.log2_max_frame_num_minus4,
            plan.sps.log2_max_frame_num_minus4
        );
        assert_eq!(pp.pic_order_cnt_type, plan.sps.pic_order_cnt_type);
        assert_eq!(pp.pic_init_qp_minus26, plan.pps.pic_init_qp_minus26);
        assert_eq!(pp.pic_init_qs_minus26, plan.pps.pic_init_qs_minus26);
        assert_eq!(pp.chroma_qp_index_offset, plan.pps.chroma_qp_index_offset);
        assert_eq!(
            pp.second_chroma_qp_index_offset,
            plan.pps.second_chroma_qp_index_offset
        );
        assert_eq!(
            pp.entropy_coding_mode_flag,
            u8::from(plan.pps.entropy_coding_mode_flag)
        );
        assert_eq!(
            pp.num_ref_idx_l0_active_minus1,
            plan.pps.num_ref_idx_l0_default_active_minus1
        );
        assert_eq!(pp.ContinuationFlag, 1);
        assert_eq!(pp.Reserved16Bits, 3);
        assert_eq!(pp.StatusReportFeedbackNumber, 1);
        // FMO is refused, so its descriptor block stays zero.
        assert_eq!(pp.num_slice_groups_minus1, 0);
        assert_eq!(pp.slice_group_map_type, 0);
        assert_eq!(pp.slice_group_change_rate_minus1, 0);
        assert!(pp.SliceGroupMap.iter().all(|&b| b == 0));
    }

    #[test]
    fn the_quantization_matrices_are_the_parsers_lists_in_coded_order() {
        let converted = convert_stream();
        let (plan, dxva) = &converted[0];
        assert_eq!(dxva.qmatrix.bScalingLists4x4, plan.pps.scaling_lists_4x4);
        assert_eq!(
            dxva.qmatrix.bScalingLists8x8[0],
            plan.pps.scaling_lists_8x8[0]
        );
        assert_eq!(
            dxva.qmatrix.bScalingLists8x8[1],
            plan.pps.scaling_lists_8x8[1]
        );
    }

    #[test]
    fn slice_ranges_ride_through_in_plan_order() {
        for (plan, dxva) in convert_stream() {
            assert_eq!(dxva.slice_ranges.len(), plan.slices.len());
            for (range, slice) in dxva.slice_ranges.iter().zip(&plan.slices) {
                assert_eq!(*range, slice.nal);
            }
        }
    }

    #[test]
    fn poc_type_specific_fields_are_written_only_for_the_type_that_codes_them() {
        // POC type 0: the type-1 field must stay 0 even if the SPS has a default.
        let converted = convert_stream();
        let (plan, dxva) = &converted[0];
        assert_eq!(plan.sps.pic_order_cnt_type, 0);
        assert_eq!(
            dxva.pic_params.log2_max_pic_order_cnt_lsb_minus4,
            plan.sps.log2_max_pic_order_cnt_lsb_minus4
        );
        assert_eq!(dxva.pic_params.delta_pic_order_always_zero_flag, 0);
    }

    #[test]
    fn a_capacity_mismatch_is_refused_and_leaves_the_map_untouched() {
        let mut planner = H264Planner::new();
        let au = split_h264_aus(TEST_25FPS).into_iter().next().unwrap();
        let plan = planner.plan_au(au).expect("plan");
        // Map sized for a different DPB depth (SPS renegotiation).
        let mut slots = SlotMap::new(plan.picture.max_dpb_frames + 1);
        let before = slots.active();
        assert_eq!(
            plan_to_dxva(&plan, &mut slots, 1),
            Err(PlanToDxvaError::CapacityMismatch {
                required: plan.picture.max_dpb_frames + 1,
                capacity: plan.picture.max_dpb_frames + 2,
            })
        );
        assert_eq!(slots.active(), before, "a refusal must not mutate the map");
    }

    #[test]
    fn a_reference_the_map_never_saw_is_refused_and_leaves_the_map_untouched() {
        // Plan two AUs; feed only the second through the map so its ref is missing.
        let aus = split_h264_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let first = planner.plan_au(aus[0]).expect("plan 0");
        let second = planner.plan_au(aus[1]).expect("plan 1");
        let mut slots = SlotMap::new(second.picture.max_dpb_frames);
        assert!(!second.slices[0].ref_list0.is_empty());
        let missing = second.slices[0].ref_list0[0].id;
        assert_eq!(first.dpb.stored, Some(missing));
        assert_eq!(
            plan_to_dxva(&second, &mut slots, 1),
            Err(PlanToDxvaError::UnresolvedReference(missing))
        );
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn slice_control_records_carry_the_packers_locations_verbatim() {
        let records = [
            crate::pack::SliceRecord {
                location: 0,
                bytes: 40,
            },
            crate::pack::SliceRecord {
                location: 40,
                bytes: 216,
            },
        ];
        let control = slice_control(&records);
        assert_eq!(control.len(), 2);
        // Read by value, in braces: `DXVA_Slice_H264_Short` is packed (ten
        // bytes); a reference to a `u32` member is unaligned and will not compile.
        assert_eq!({ control[0].BSNALunitDataLocation }, 0);
        assert_eq!({ control[0].SliceBytesInBuffer }, 40);
        assert_eq!({ control[1].BSNALunitDataLocation }, 40);
        assert_eq!({ control[1].SliceBytesInBuffer }, 216);
        assert!(control.iter().all(|c| { c.wBadSliceChopping } == 0));
        // Records are ten bytes apart: the second location is at byte 10, not 12.
        let bytes = crate::dxva::slice_bytes(&control);
        assert_eq!(bytes.len(), 20);
        assert_eq!(&bytes[10..14], &40u32.to_le_bytes());
    }

    /// No two live pictures share a surface index.
    ///
    /// Both streams: the vendored vector's DPB is far deeper than its
    /// reference count, so it rarely reuses a surface; the low-delay stream
    /// cycles all four slots every four pictures.
    ///
    /// The loop applies `release_after_decode` because the caller does. A
    /// loop that skips it holds a surface per AU and dies of `SlotError::Full`.
    #[test]
    fn a_slot_is_reused_only_after_its_picture_leaves_the_dpb() {
        for (label, stream) in [("vendored", TEST_25FPS), ("low-delay", LOWDELAY_640X480)] {
            let mut planner = H264Planner::new();
            let mut slots: Option<SlotMap> = None;
            let mut live: Vec<(PicId, u8)> = Vec::new();
            for (i, au) in split_h264_aus(stream).into_iter().enumerate() {
                let Ok(plan) = planner.plan_au(au) else {
                    continue;
                };
                let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
                let removed = plan.dpb.removed.clone();
                let dxva = plan_to_dxva(&plan, map, i as u32 + 1).expect("conversion");
                // Before deferred releases: at submission every removed picture is still live.
                assert!(
                    live.iter().all(|&(_, slot)| slot != dxva.setup_slot),
                    "{label} AU {i} decodes into a surface a live picture still holds"
                );
                for &id in &dxva.release_after_decode {
                    assert!(
                        map.release(id),
                        "{label} AU {i}: deferred id {id} held no slot"
                    );
                }
                live.retain(|&(id, _)| !removed.contains(&id));
                live.push((dxva.setup_id, dxva.setup_slot));
            }
        }
    }
}
