//! Per-AU H.265 conversion: one [`AuPlan`] into the `DXVA_PicParams_HEVC`,
//! `DXVA_Qmatrix_HEVC` and slice-control records
//! `ID3D11VideoContext::SubmitDecoderBuffers` takes — [`crate::pic`] one codec
//! over, and the DXVA twin of `pf_vkdecode::pic_h265`.
//!
//! HEVC decode takes no per-slice reference lists. Hardware re-derives 8.3.4
//! from the slice bits, keyed by `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr`
//! — indices into `RefPicList`. Per-slice lists are a closed check: every entry
//! must be in the current sets.
//!
//! `RefPicList` is the marked DPB ([`pf_bitstream::h265::AuPlan::dpb_refs`]):
//! current sets first (so the index arrays stay identical), then 8.3.2's *Foll*
//! pictures. Binding only the current sets would drop a long-term anchor
//! between pin and use; a driver may treat that gap as retirement. Vulkan's
//! `pReferenceSlots` is the slots this decode uses, so that rung is right to
//! bind the current sets. Surfaces are slots: see [`crate::pic`].
//!
//! A lost reference is absent from the RPS (`PlanWarning::MissingReference`);
//! the index arrays compact past it. There is no surface to point at.

use std::ops::Range;

use cros_codecs::codec::h265::parser::ScalingLists;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::PicId;
use pf_bitstream::h265::RefPic;
use pf_bitstream::h265::RefRpsIdxError;
use pf_bitstream::h265::RpsError;
use pf_bitstream::slots::Removals;
use pf_bitstream::slots::SlotError;
use pf_bitstream::slots::SlotMap;
use tracing::trace;

use crate::dxva::HevcFormatFlags;
use crate::dxva::HevcPictureFlags;
use crate::dxva::HevcToolFlags;
use crate::dxva::PicEntry;
use crate::dxva::PicParamsHevc;
use crate::dxva::QmatrixHevc;
use crate::dxva::SliceHevcShort;
use crate::dxva::UNUSED_ENTRY;

/// `RefPicList` length in `DXVA_PicParams_HEVC`. Fifteen: `CurrPic` is named
/// separately, so the array holds only the other DPB members.
const REF_PIC_LIST_LEN: usize = 15;

/// Each `RefPicSet*` index array holds eight entries — H.265 allows more;
/// beyond eight is unexpressible here and refused.
pub const RPS_LIST_SIZE: usize = 8;

// The index arrays go in as pf-bitstream builds them, so its unused marker
// must be DXVA's.
const _: () = assert!(pf_bitstream::h265::RPS_UNUSED == UNUSED_ENTRY);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxvaRefH265 {
    /// Decode texture array `ArraySlice` — the DPB slot.
    pub slot: u8,
    pub id: PicId,
    pub is_long_term: bool,
    pub pic_order_cnt: i32,
}

/// CPU-derivable half of one AU's DXVA submission.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodePlanDxvaH265 {
    /// Picture parameters. `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr` are
    /// indices into [`Self::refs`] (`0xFF` unused), laid out in the same order.
    pub pic_params: PicParamsHevc,
    /// Inverse-quantization matrices, or `None` when scaling lists are off — the
    /// buffer is then not submitted. libavcodec's `dxva2_hevc_end_frame` passes
    /// it only when `dwCodingParamToolFlags & 1`. Submitting anyway is not
    /// harmless: with lists disabled the hardware is told to ignore the
    /// matrices, and a driver that honours a buffer it was handed dequantizes
    /// against its contents.
    pub qmatrix: Option<QmatrixHevc>,
    /// Byte ranges of the AU's slice-segment NALs, start code dropped, in plan
    /// order — what [`crate::pack::pack`] takes.
    pub slice_ranges: Vec<Range<usize>>,
    pub setup_slot: u8,
    pub setup_id: PicId,
    /// Whether later pictures may reference the decoded picture. False for
    /// sub-layer non-reference NALU types.
    pub setup_is_reference: bool,
    /// Marked DPB, same order as `pic_params.RefPicList`: the three current RPS
    /// sets first (StCurrBefore, StCurrAfter, LtCurr, first appearance first),
    /// then every other marked picture.
    pub refs: Vec<DxvaRefH265>,
}

/// Conversion failures. Stream damage never lands here — pf-bitstream degrades
/// it to [`pf_bitstream::h265::PlanWarning`]s. `PlanError::RaslSkipped` is an
/// error of planning (Ok-skip upstream); no plan exists to convert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaH265Error {
    NoSlices,
    NoStoredId,
    /// An RPS entry's id holds no slot: an earlier plan never went through this
    /// [`SlotMap`].
    UnresolvedReference(PicId),
    /// A slice list names a picture outside the current RPS sets. 8.3.4 builds
    /// every list from those sets; `RefPicList` residency does not fix it,
    /// because hardware derives lists from the RPS index arrays, which only the
    /// current sets populate.
    ReferenceOutsideRps(PicId),
    Slot(SlotError),
    /// A current RPS set holds more entries than the index arrays' eight.
    RpsSetOverflow {
        set: &'static str,
        len: usize,
    },
    /// The AU references more distinct pictures than `RefPicList` holds (15).
    TooManyReferences(usize),
    /// `ucNumDeltaPocsOfRefRpsIdx` is not derivable from this plan.
    RefRpsIdx(RefRpsIdxError),
    /// Inline `st_ref_pic_set()` bit count exceeds `u16`
    /// (`wNumBitsForShortTermRPSInSlice`) — a header that large is corrupt.
    StRpsBitsOverflow(u32),
    /// The map was built for a different DPB depth than this plan's
    /// `max_dpb_frames` — an SPS renegotiation resized the DPB; rebuild decoder,
    /// pool and map.
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// A picture dimension in minimum coding blocks exceeds the `USHORT` the
    /// picture parameters carry.
    DimensionOverflow {
        width: u32,
        height: u32,
    },
    /// `separate_colour_plane_flag`. Refused upstream by pf-bitstream's envelope
    /// gate; checked again because the picture layout is a different shape.
    SeparateColourPlanes,
}

impl std::fmt::Display for PlanToDxvaH265Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaH265Error::NoSlices => write!(f, "the plan holds no slices"),
            PlanToDxvaH265Error::NoStoredId => write!(
                f,
                "the plan stores no picture (flush updates go to SlotMap::apply)"
            ),
            PlanToDxvaH265Error::UnresolvedReference(id) => {
                write!(f, "referenced picture {id} holds no DPB slot in this map")
            }
            PlanToDxvaH265Error::ReferenceOutsideRps(id) => write!(
                f,
                "a slice list names picture {id}, which is not in the AU's RPS sets"
            ),
            PlanToDxvaH265Error::Slot(err) => write!(f, "slot assignment failed: {err}"),
            PlanToDxvaH265Error::RpsSetOverflow { set, len } => {
                write!(f, "{set} holds {len} entries; DXVA's index arrays hold 8")
            }
            PlanToDxvaH265Error::TooManyReferences(count) => {
                write!(f, "{count} references exceed DXVA's RefPicList of 15")
            }
            PlanToDxvaH265Error::RefRpsIdx(err) => write!(f, "{err}"),
            PlanToDxvaH265Error::StRpsBitsOverflow(bits) => {
                write!(f, "inline st_ref_pic_set of {bits} bits exceeds u16")
            }
            PlanToDxvaH265Error::CapacityMismatch { required, capacity } => write!(
                f,
                "the plan needs {required} slots but the map holds {capacity} — \
                 an SPS renegotiation resized the DPB; rebuild decoder and map"
            ),
            PlanToDxvaH265Error::DimensionOverflow { width, height } => write!(
                f,
                "a {width}x{height} picture in min-CBs exceeds the DXVA picture parameters"
            ),
            PlanToDxvaH265Error::SeparateColourPlanes => {
                write!(f, "separate_colour_plane_flag is outside this backend")
            }
        }
    }
}

impl std::error::Error for PlanToDxvaH265Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PlanToDxvaH265Error::Slot(err) => Some(err),
            PlanToDxvaH265Error::RefRpsIdx(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SlotError> for PlanToDxvaH265Error {
    fn from(err: SlotError) -> Self {
        PlanToDxvaH265Error::Slot(err)
    }
}

impl From<RpsError> for PlanToDxvaH265Error {
    fn from(err: RpsError) -> Self {
        match err {
            RpsError::SetOverflow { set, len } => PlanToDxvaH265Error::RpsSetOverflow { set, len },
            RpsError::OutsideRps(id) => PlanToDxvaH265Error::ReferenceOutsideRps(id),
        }
    }
}

impl From<RefRpsIdxError> for PlanToDxvaH265Error {
    fn from(err: RefRpsIdxError) -> Self {
        PlanToDxvaH265Error::RefRpsIdx(err)
    }
}

fn dxva_ref(slot: u8, rp: &RefPic) -> DxvaRefH265 {
    DxvaRefH265 {
        slot,
        id: rp.id,
        is_long_term: rp.is_long_term,
        pic_order_cnt: rp.pic_order_cnt,
    }
}

/// `DXVA_Qmatrix_HEVC` from the picture's active lists
/// ([`AuPlan::active_scaling_lists`]).
fn quantization_matrices(sl: &ScalingLists) -> QmatrixHevc {
    let mut qm = QmatrixHevc::zeroed();
    qm.ucScalingLists0 = sl.scaling_list_4x4;
    qm.ucScalingLists1 = sl.scaling_list_8x8;
    qm.ucScalingLists2 = sl.scaling_list_16x16;
    // sizeId 3 codes only matrixId 0 and 3 (its loop steps by three); DXVA
    // carries exactly those two — index k here is the parser's k * 3.
    qm.ucScalingLists3[0] = sl.scaling_list_32x32[0];
    qm.ucScalingLists3[1] = sl.scaling_list_32x32[3];
    // DC entries are the ScalingFactor DC value (`…_minus8 + 8`), not the coded
    // delta. Clamped rather than wrapped: the parser bounds the coded value to
    // -7..=247, so the sum is 1..=255 and the clamp is unreachable.
    for (dst, src) in qm
        .ucScalingListDCCoefSizeID2
        .iter_mut()
        .zip(sl.scaling_list_dc_coef_minus8_16x16)
    {
        *dst = (i32::from(src) + 8).clamp(0, 255) as u8;
    }
    for (k, dst) in qm.ucScalingListDCCoefSizeID3.iter_mut().enumerate() {
        let src = sl.scaling_list_dc_coef_minus8_32x32[k * 3];
        *dst = (i32::from(src) + 8).clamp(0, 255) as u8;
    }
    qm
}

/// Convert one planned AU, driving `slots` through the AU's slot lifecycle.
///
/// `status_id` becomes `StatusReportFeedbackNumber` — see [`crate::pic::plan_to_dxva`].
///
/// Atomicity: every fallible step runs before any mutation of `slots`, so an
/// error leaves the map exactly as it was.
pub fn plan_to_dxva_h265(
    plan: &AuPlan,
    slots: &mut SlotMap,
    status_id: u32,
) -> Result<DecodePlanDxvaH265, PlanToDxvaH265Error> {
    if plan.slices.is_empty() {
        return Err(PlanToDxvaH265Error::NoSlices);
    }
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaH265Error::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if sps.separate_colour_plane_flag {
        return Err(PlanToDxvaH265Error::SeparateColourPlanes);
    }

    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToDxvaH265Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }

    // The current sets lead `RefPicList`, so the index arrays' positions into
    // them are `RefPicList` indices as they stand.
    let current = plan.current_rps_refs()?;
    let mut refs: Vec<DxvaRefH265> = Vec::with_capacity(REF_PIC_LIST_LEN);
    for (rp, _) in &current.refs {
        let slot = slots
            .slot_of(rp.id)
            .ok_or(PlanToDxvaH265Error::UnresolvedReference(rp.id))?;
        // DPB snapshot is the authority for the marking: an `RefPicSetLtCurr`
        // index into a short-term-marked entry is an inconsistent DPB. The set's
        // own copy is the fallback and cannot be reached off a real plan (8.3.2).
        let marked = plan.dpb_refs.iter().find(|d| d.id == rp.id);
        if marked.is_none() {
            trace!(
                id = rp.id,
                "an RPS entry names a picture the marked DPB does not hold"
            );
        }
        refs.push(dxva_ref(slot, marked.unwrap_or(rp)));
    }
    if refs.len() > REF_PIC_LIST_LEN {
        return Err(PlanToDxvaH265Error::TooManyReferences(refs.len()));
    }
    let index_arrays = current.index;

    // Rest of the marked DPB (*Foll* pictures) in planner DPB order. Overflow
    // past the array is dropped, not refused: nothing here is referenced by this
    // picture, so the decode is unaffected. `RefPicList` holds 15 while the DPB
    // holds up to 16, so this is reachable.
    for rp in &current.foll {
        if refs.len() == REF_PIC_LIST_LEN {
            trace!(
                marked = plan.dpb_refs.len(),
                "the marked DPB exceeds RefPicList; the tail is not expressible"
            );
            break;
        }
        match slots.slot_of(rp.id) {
            Some(slot) => refs.push(dxva_ref(slot, rp)),
            None => trace!(id = rp.id, "a marked DPB picture holds no slot in this map"),
        }
    }

    // Everything else fallible, before any mutation.
    let num_delta_pocs = plan.num_delta_pocs_of_ref_rps_idx()?;
    let st_rps_bits = u16::try_from(pic.short_term_ref_pic_set_size_bits).map_err(|_| {
        PlanToDxvaH265Error::StRpsBitsOverflow(pic.short_term_ref_pic_set_size_bits)
    })?;
    let min_cb = sps.min_cb_log2_size_y;
    let width_in_min_cbs = u32::from(sps.pic_width_in_luma_samples) >> min_cb;
    let height_in_min_cbs = u32::from(sps.pic_height_in_luma_samples) >> min_cb;
    let (Ok(width_in_min_cbs), Ok(height_in_min_cbs)) = (
        u16::try_from(width_in_min_cbs),
        u16::try_from(height_in_min_cbs),
    ) else {
        return Err(PlanToDxvaH265Error::DimensionOverflow {
            width: width_in_min_cbs,
            height: height_in_min_cbs,
        });
    };

    let mut pp = PicParamsHevc::zeroed();
    pp.PicWidthInMinCbsY = width_in_min_cbs;
    pp.PicHeightInMinCbsY = height_in_min_cbs;
    pp.wFormatAndSequenceInfoFlags = HevcFormatFlags {
        chroma_format_idc: pic.chroma_format_idc,
        separate_colour_plane_flag: false, // refused above
        bit_depth_luma_minus8: pic.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: pic.bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
    }
    .pack();
    // DPB depth of the highest temporal sub-layer — the only one whose buffering
    // covers the whole stream. `max_sub_layers_minus1` indexes a seven-entry
    // array, so the read cannot go out of bounds off any parse.
    pp.sps_max_dec_pic_buffering_minus1 = sps
        .max_dec_pic_buffering_minus1
        .get(usize::from(sps.max_sub_layers_minus1))
        .copied()
        .unwrap_or(0);
    pp.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
    pp.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
    pp.log2_min_transform_block_size_minus2 = sps.log2_min_luma_transform_block_size_minus2;
    pp.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_luma_transform_block_size;
    pp.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
    pp.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
    pp.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets;
    pp.num_long_term_ref_pics_sps = sps.num_long_term_ref_pics_sps;
    pp.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    pp.init_qp_minus26 = pps.init_qp_minus26;
    pp.ucNumDeltaPocsOfRefRpsIdx = num_delta_pocs;
    // 0 when the RPS came from the SPS by index — DXVA's convention for this
    // field, and pf-bitstream's for the value it derives.
    pp.wNumBitsForShortTermRPSInSlice = st_rps_bits;
    pp.dwCodingParamToolFlags = HevcToolFlags {
        scaling_list_enabled_flag: sps.scaling_list_enabled_flag,
        amp_enabled_flag: sps.amp_enabled_flag,
        sample_adaptive_offset_enabled_flag: sps.sample_adaptive_offset_enabled_flag,
        pcm_enabled_flag: sps.pcm_enabled_flag,
        pcm_sample_bit_depth_luma_minus1: sps.pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: sps.pcm_sample_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3: sps.log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: sps
            .log2_diff_max_min_pcm_luma_coding_block_size,
        pcm_loop_filter_disabled_flag: sps.pcm_loop_filter_disabled_flag,
        long_term_ref_pics_present_flag: sps.long_term_ref_pics_present_flag,
        sps_temporal_mvp_enabled_flag: sps.temporal_mvp_enabled_flag,
        strong_intra_smoothing_enabled_flag: sps.strong_intra_smoothing_enabled_flag,
        dependent_slice_segments_enabled_flag: pps.dependent_slice_segments_enabled_flag,
        output_flag_present_flag: pps.output_flag_present_flag,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        sign_data_hiding_enabled_flag: pps.sign_data_hiding_enabled_flag,
        cabac_init_present_flag: pps.cabac_init_present_flag,
    }
    .pack();
    pp.dwCodingSettingPicturePropertyFlags = HevcPictureFlags {
        constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
        transform_skip_enabled_flag: pps.transform_skip_enabled_flag,
        cu_qp_delta_enabled_flag: pps.cu_qp_delta_enabled_flag,
        pps_slice_chroma_qp_offsets_present_flag: pps.slice_chroma_qp_offsets_present_flag,
        weighted_pred_flag: pps.weighted_pred_flag,
        weighted_bipred_flag: pps.weighted_bipred_flag,
        transquant_bypass_enabled_flag: pps.transquant_bypass_enabled_flag,
        tiles_enabled_flag: pps.tiles_enabled_flag,
        entropy_coding_sync_enabled_flag: pps.entropy_coding_sync_enabled_flag,
        uniform_spacing_flag: pps.uniform_spacing_flag,
        loop_filter_across_tiles_enabled_flag: pps.loop_filter_across_tiles_enabled_flag,
        pps_loop_filter_across_slices_enabled_flag: pps.loop_filter_across_slices_enabled_flag,
        deblocking_filter_override_enabled_flag: pps.deblocking_filter_override_enabled_flag,
        pps_deblocking_filter_disabled_flag: pps.deblocking_filter_disabled_flag,
        lists_modification_present_flag: pps.lists_modification_present_flag,
        slice_segment_header_extension_present_flag: pps
            .slice_segment_header_extension_present_flag,
        irap_pic_flag: pic.is_irap,
        idr_pic_flag: pic.is_idr,
        // HEVC states intra-ness at picture level: an IRAP is intra-only by
        // definition, and nothing else is guaranteed to be.
        intra_pic_flag: pic.is_irap,
    }
    .pack();
    pp.pps_cb_qp_offset = pps.cb_qp_offset;
    pp.pps_cr_qp_offset = pps.cr_qp_offset;
    if pps.tiles_enabled_flag {
        pp.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
        pp.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
        if !pps.uniform_spacing_flag {
            // Only the non-uniform case codes explicit widths; the uniform case
            // leaves them zero and hardware derives its own grid. `try_from` +
            // `u16::MAX`: a tile edge past 65535 min-CBs cannot come off a real
            // PPS; a clamp is better than a panic on a corrupt one.
            for (dst, src) in pp
                .column_width_minus1
                .iter_mut()
                .zip(pps.column_width_minus1)
            {
                *dst = u16::try_from(src).unwrap_or(u16::MAX);
            }
            for (dst, src) in pp.row_height_minus1.iter_mut().zip(pps.row_height_minus1) {
                *dst = u16::try_from(src).unwrap_or(u16::MAX);
            }
        }
    }
    pp.diff_cu_qp_delta_depth = if pps.cu_qp_delta_enabled_flag {
        pps.diff_cu_qp_delta_depth
    } else {
        0
    };
    pp.pps_beta_offset_div2 = pps.beta_offset_div2;
    pp.pps_tc_offset_div2 = pps.tc_offset_div2;
    pp.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
    pp.CurrPicOrderCntVal = pic.pic_order_cnt;
    pp.StatusReportFeedbackNumber = status_id;

    pp.RefPicList = [PicEntry::UNUSED; REF_PIC_LIST_LEN];
    for (i, r) in refs.iter().enumerate() {
        pp.RefPicList[i] = PicEntry::new(r.slot, r.is_long_term);
        pp.PicOrderCntValList[i] = r.pic_order_cnt;
    }
    [
        pp.RefPicSetStCurrBefore,
        pp.RefPicSetStCurrAfter,
        pp.RefPicSetLtCurr,
    ] = index_arrays;

    // Built only when scaling lists are enabled; that is the only case the
    // buffer is submitted (see `DecodePlanDxvaH265::qmatrix`).
    let qm = plan.active_scaling_lists().map(quantization_matrices);

    let slice_ranges: Vec<Range<usize>> = plan.slices.iter().map(|s| s.nal.clone()).collect();

    // Mutations last, after every fallible step. Removals first: they were real
    // regardless of this AU's fate.
    let (setup_slot, _) = slots.commit_setup(setup_id, &plan.dpb.removed, Removals::ReleaseNow)?;
    pp.CurrPic = PicEntry::new(setup_slot, false);

    Ok(DecodePlanDxvaH265 {
        pic_params: pp,
        qmatrix: qm,
        slice_ranges,
        setup_slot,
        setup_id,
        setup_is_reference: pic.is_reference,
        refs,
    })
}

/// Slice-control records for a packed AU — [`crate::pic::slice_control`]'s
/// HEVC twin.
pub fn slice_control_h265(records: &[crate::pack::SliceRecord]) -> Vec<SliceHevcShort> {
    records
        .iter()
        .map(|r| SliceHevcShort {
            BSNALunitDataLocation: r.location,
            SliceBytesInBuffer: r.bytes,
            wBadSliceChopping: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {

    use pf_bitstream::h265::H265Planner;
    use pf_bitstream::testing::split_h265_aus;

    use super::*;

    /// Vendored vectors pf-bitstream's and pf-vkdecode's h265 tests plan.
    const TEST_25FPS: &[u8] = pf_bitstream::testing::H265_25FPS;
    const TEST_64X64_I_P_B_P: &[u8] = pf_bitstream::testing::H265_64X64_I_P_B_P;

    /// Host HEVC: the only stream in this repository that reaches the DPB pressure
    /// HEVC's no-aliasing exemption is claimed against — low-delay IPPP,
    /// `sps_max_num_reorder_pics = 0`, five-picture DPB against the four pictures
    /// 8.3.2 keeps marked. Vendored beside the goldens the GPU legs decode it
    /// against, same path as `lowdelay-640x480.h264`.
    const LOWDELAY_640X480_H265: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h265");

    fn convert_stream(stream: &[u8]) -> Vec<(AuPlan, DecodePlanDxvaH265)> {
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut out = Vec::new();
        for (i, au) in split_h265_aus(stream).into_iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            if map.capacity() != plan.picture.max_dpb_frames + 1 {
                *map = SlotMap::new(plan.picture.max_dpb_frames);
            }
            let dxva = plan_to_dxva_h265(&plan, map, i as u32 + 1).expect("conversion");
            out.push((plan, dxva));
        }
        out
    }

    #[test]
    fn the_whole_vendored_25fps_vector_converts_without_a_refusal() {
        let converted = convert_stream(TEST_25FPS);
        assert_eq!(converted.len(), 250);
    }

    #[test]
    fn the_index_arrays_point_at_the_ref_pic_list_entries_the_rps_sets_name() {
        for (plan, dxva) in convert_stream(TEST_25FPS) {
            for (array, set) in [
                (
                    &dxva.pic_params.RefPicSetStCurrBefore,
                    &plan.rps.st_curr_before,
                ),
                (
                    &dxva.pic_params.RefPicSetStCurrAfter,
                    &plan.rps.st_curr_after,
                ),
                (&dxva.pic_params.RefPicSetLtCurr, &plan.rps.lt_curr),
            ] {
                for (position, entry) in array.iter().enumerate() {
                    match set.get(position) {
                        Some(rp) => {
                            let index = usize::from(*entry);
                            assert_eq!(dxva.refs[index].id, rp.id);
                            assert_eq!(dxva.pic_params.PicOrderCntValList[index], rp.pic_order_cnt);
                            assert_eq!(
                                dxva.pic_params.RefPicList[index].index(),
                                dxva.refs[index].slot
                            );
                        }
                        None => assert_eq!(*entry, UNUSED_ENTRY),
                    }
                }
            }
        }
    }

    /// Unique pictures the plan's three current sets name, in the order this
    /// conversion binds them (set order, first appearance first).
    fn current_set_ids(plan: &AuPlan) -> Vec<PicId> {
        let mut ids = Vec::new();
        for rp in plan
            .rps
            .st_curr_before
            .iter()
            .chain(&plan.rps.st_curr_after)
            .chain(&plan.rps.lt_curr)
        {
            if !ids.contains(&rp.id) {
                ids.push(rp.id);
            }
        }
        ids
    }

    #[test]
    fn the_reference_list_holds_every_marked_picture_and_the_current_sets_lead_it() {
        for (plan, dxva) in convert_stream(TEST_25FPS) {
            let mut listed: Vec<PicId> = dxva.refs.iter().map(|r| r.id).collect();
            let mut marked: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            listed.sort_unstable();
            marked.sort_unstable();
            assert_eq!(listed, marked, "RefPicList must be the marked DPB");
            // Current sets lead, so the index arrays never point past them —
            // which is what makes appending the rest of the DPB safe.
            let current = current_set_ids(&plan);
            assert_eq!(
                dxva.refs
                    .iter()
                    .take(current.len())
                    .map(|r| r.id)
                    .collect::<Vec<_>>(),
                current
            );
        }
    }

    #[test]
    fn a_marked_picture_no_current_set_names_is_appended_after_them() {
        // 8.3.2 *Foll* shape: a picture the DPB keeps marked for a later picture.
        // Injected into the snapshot rather than synthesised as a bitstream — the
        // conversion, not the planner, is under test. Vendored vectors never produce
        // this (their RPS names every marked picture they hold).
        let aus = split_h265_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let plans: Vec<AuPlan> = aus
            .iter()
            .take(3)
            .map(|au| planner.plan_au(au).expect("plan"))
            .collect();
        let mut slots = SlotMap::new(plans[0].picture.max_dpb_frames);
        let baseline: Vec<DecodePlanDxvaH265> = plans
            .iter()
            .enumerate()
            .map(|(i, plan)| plan_to_dxva_h265(plan, &mut slots, i as u32 + 1).expect("convert"))
            .collect();
        let last = baseline.last().expect("three conversions");
        let current_sets = last.refs.len();
        assert!(current_sets > 0, "the third AU must reference something");

        let mut planner = H265Planner::new();
        let mut plans: Vec<AuPlan> = aus
            .iter()
            .take(3)
            .map(|au| planner.plan_au(au).expect("plan"))
            .collect();
        let mut slots = SlotMap::new(plans[0].picture.max_dpb_frames);
        for (i, plan) in plans.iter().take(2).enumerate() {
            plan_to_dxva_h265(plan, &mut slots, i as u32 + 1).expect("convert");
        }
        // A picture the map already holds and the third AU does not name, else a
        // fresh id parked in a free slot — either is a marked DPB entry with a
        // surface, which is all `RefPicList` needs.
        let setup = plans[2].dpb.stored.expect("stored");
        let named = current_set_ids(&plans[2]);
        let existing = slots
            .held()
            .map(|(_, id)| id)
            .find(|id| *id != setup && !named.contains(id));
        let foll = match existing {
            Some(id) => id,
            None => {
                let id = 9_999;
                slots
                    .assign(id)
                    .expect("a free slot for the synthetic entry");
                id
            }
        };
        plans[2].dpb_refs.push(RefPic {
            id: foll,
            pic_order_cnt: -4242,
            is_long_term: true,
        });
        let dxva = plan_to_dxva_h265(&plans[2], &mut slots, 3).expect("convert");

        assert_eq!(dxva.refs.len(), current_sets + 1, "appended, not merged");
        let appended = &dxva.refs[current_sets];
        assert_eq!(appended.id, foll);
        assert!(appended.is_long_term);
        assert_eq!(appended.pic_order_cnt, -4242);
        assert_eq!(
            dxva.pic_params.RefPicList[current_sets],
            PicEntry::new(appended.slot, true)
        );
        assert_eq!(dxva.pic_params.PicOrderCntValList[current_sets], -4242);
        // Index arrays are untouched by the append: every live index still points
        // inside the current sets, which is what makes the whole-DPB `RefPicList`
        // a safe superset.
        for array in [
            dxva.pic_params.RefPicSetStCurrBefore,
            dxva.pic_params.RefPicSetStCurrAfter,
            dxva.pic_params.RefPicSetLtCurr,
        ] {
            for entry in array {
                assert!(
                    entry == UNUSED_ENTRY || usize::from(entry) < current_sets,
                    "an index array reached the appended entry"
                );
            }
        }
    }

    #[test]
    fn unused_ref_pic_list_entries_are_the_sentinel_with_a_zero_poc() {
        for (_, dxva) in convert_stream(TEST_25FPS) {
            for i in dxva.refs.len()..REF_PIC_LIST_LEN {
                assert_eq!(dxva.pic_params.RefPicList[i], PicEntry::UNUSED);
                assert_eq!(dxva.pic_params.PicOrderCntValList[i], 0);
            }
        }
    }

    /// HEVC is free of the slot aliasing that cost AV1 and H.264 a deferral.
    ///
    /// Those codecs release a removed picture's slot and then let
    /// [`SlotMap::assign`] hand it to the decode target, so `CurrPic` and a
    /// `RefPicList` entry name one surface. This conversion still releases its
    /// whole `removed` list inline, and is safe because `H265Planner` snapshots
    /// `dpb_refs` after `decode_rps` has updated the DPB: a picture this AU's RPS
    /// dropped is never in the set `RefPicList` is built from. `H264Planner` and
    /// `Av1Planner` snapshot before their marking, and both needed the deferral.
    ///
    /// The second assertion makes that argument falsifiable. The first is only
    /// the consequence — it would hold even if the snapshot moved. Moving
    /// `dpb_snapshot()` above `decode_rps` would leave the first passing and break
    /// the second on the first AU whose RPS drops a picture.
    ///
    /// This vector reorders and cannot reach the DPB pressure the exemption is
    /// claimed against. The stream that can is [`LOWDELAY_640X480_H265`].
    #[test]
    fn the_current_picture_is_named_by_curr_pic_and_never_aliases_a_reference() {
        let mut aus_with_removals = 0usize;
        let mut both = 0usize;
        for (plan, dxva) in convert_stream(TEST_25FPS) {
            assert_eq!(dxva.pic_params.CurrPic.index(), dxva.setup_slot);
            assert!(!dxva.pic_params.CurrPic.associated());
            assert_eq!(
                dxva.pic_params.CurrPicOrderCntVal,
                plan.picture.pic_order_cnt
            );
            for r in &dxva.refs {
                assert_ne!(r.slot, dxva.setup_slot, "a reference aliases the target");
            }
            if !plan.dpb.removed.is_empty() {
                aus_with_removals += 1;
            }
            both += plan
                .dpb
                .removed
                .iter()
                .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                .count();
        }
        assert!(
            aus_with_removals > 0,
            "no AU of this vector removed anything, so the zero below would be empty \
             for a reason that has nothing to do with the property being asserted"
        );
        assert_eq!(
            both, 0,
            "{both} picture(s) are in an AU's own reference set AND removed by it. \
             That is the H.264/AV1 aliasing precondition, and HEVC is supposed to be \
             structurally incapable of it — so the snapshot in `H265Planner` has moved \
             ahead of `decode_rps`. Restore the ordering, or give this conversion the \
             `release_after_decode` deferral the other two carry; do NOT relax this \
             number"
        );
    }

    /// Marked DPB as an access unit's `decode_rps` finds it — the set
    /// `dpb_snapshot()` would return from the other side of that call.
    ///
    /// `H265Planner::begin_picture` runs `decode_rps` →
    /// `update_dpb_before_decoding` → `dpb_snapshot`. Between AU N-1's snapshot
    /// and AU N's `decode_rps` only `finish_picture(N-1)` stores its picture
    /// marked used for short-term reference. So the pre-RPS marked set is exactly
    /// `dpb_refs(N-1) ∪ {stored(N-1)}` — no DPB replay, no dependence on planner
    /// internals staying reachable from a test.
    ///
    /// A sub-layer non-reference picture is stored but not marked, so it is
    /// excluded; callers assert their streams contain none.
    fn pre_rps_marked(prev: Option<&AuPlan>) -> Vec<RefPic> {
        let Some(prev) = prev else {
            return Vec::new();
        };
        let mut marked = prev.dpb_refs.clone();
        if let Some(id) = prev.dpb.stored {
            if prev.picture.is_reference {
                marked.push(RefPic {
                    id,
                    pic_order_cnt: prev.picture.pic_order_cnt,
                    is_long_term: false,
                });
            }
        }
        marked
    }

    /// The exemption, measured on the stream that can falsify it.
    ///
    /// [`the_current_picture_is_named_by_curr_pic_and_never_aliases_a_reference`]
    /// runs over `test-25fps.h265`, which reorders: a picture the RPS drops stays
    /// alive for output past the access unit that dropped it, so eviction and
    /// unmarking never land together. This stream reaches the precondition.
    ///
    /// Three numbers: nearly every AU retires a picture; none of those
    /// retirements intersect the AU's own `dpb_refs`; all of them intersect the
    /// pre-RPS marked set, so a snapshot one call earlier would alias on every
    /// one. [`the_low_delay_stream_would_alias_if_the_snapshot_moved_ahead_of_the_rps`]
    /// drives that counterfactual through the conversion.
    #[test]
    fn the_low_delay_stream_reaches_the_dpb_pressure_and_hevc_still_does_not_alias() {
        let converted = convert_stream(LOWDELAY_640X480_H265);
        assert_eq!(converted.len(), 120, "the low-delay stream is 120 pictures");

        let mut with_removals = 0usize;
        let mut both = 0usize;
        let mut would_alias = 0usize;
        for (i, (plan, dxva)) in converted.iter().enumerate() {
            assert_eq!(dxva.pic_params.CurrPic.index(), dxva.setup_slot);
            assert!(!dxva.pic_params.CurrPic.associated());
            for r in &dxva.refs {
                assert_ne!(
                    r.slot, dxva.setup_slot,
                    "AU {i}: reference picture {} shares surface {} with the decode \
                     target — HEVC has acquired the H.264/AV1 defect",
                    r.id, r.slot
                );
            }

            assert!(
                plan.picture.is_reference,
                "AU {i}: this stream carries no sub-layer non-reference pictures, \
                 which is what makes `pre_rps_marked` exact"
            );
            if !plan.dpb.removed.is_empty() {
                with_removals += 1;
            }
            both += plan
                .dpb
                .removed
                .iter()
                .filter(|id| plan.dpb_refs.iter().any(|r| r.id == **id))
                .count();
            let before = pre_rps_marked(i.checked_sub(1).map(|prev| &converted[prev].0));
            would_alias += plan
                .dpb
                .removed
                .iter()
                .filter(|id| before.iter().any(|r| r.id == **id))
                .count();
        }

        assert_eq!(
            with_removals, 115,
            "the stream must still retire a picture on nearly every access unit; \
             without that both numbers below are trivially zero"
        );
        assert_eq!(
            both, 0,
            "{both} picture(s) are in an access unit's own marked DPB AND removed by \
             it — the H.264/AV1 aliasing precondition, which HEVC is supposed to be \
             structurally incapable of. `H265Planner`'s snapshot has moved ahead of \
             `decode_rps`. Restore the ordering, or give this conversion the \
             `release_after_decode` deferral the other two carry; do NOT relax this"
        );
        assert_eq!(
            would_alias, 115,
            "the fixture must stay CAPABLE of exposing the defect it rules out. A \
             regenerated stream that reordered, or whose DPB was deeper than its \
             reference count, would report 0 here — and the zero above would then \
             prove exactly as much as `test-25fps.h264`'s zero proved, which was nothing"
        );
    }

    /// Counterfactual driven through the conversion, not just the planner's
    /// arithmetic.
    ///
    /// `H265Planner` snapshotting one call earlier is where `H264Planner` and
    /// `Av1Planner` both snapshot, and both needed `release_after_decode`. This
    /// test hands `plan_to_dxva_h265` the pre-RPS marked set as `dpb_refs` and
    /// nothing else changed, then asserts the alias: the dropped picture enters
    /// `RefPicList` as a *Foll* entry with its slot resolved before the removals
    /// are released, and `SlotMap::assign` hands that freed slot to `CurrPic`.
    #[test]
    fn the_low_delay_stream_would_alias_if_the_snapshot_moved_ahead_of_the_rps() {
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut plans: Vec<AuPlan> = Vec::new();
        let mut aliased = 0usize;
        let mut converted = 0usize;

        for (i, au) in split_h265_aus(LOWDELAY_640X480_H265)
            .into_iter()
            .enumerate()
        {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));

            // The one mutation: the marked DPB as it stood before this AU's RPS ran.
            let mut as_if = plan.clone();
            as_if.dpb_refs = pre_rps_marked(plans.last());

            // Reconstruction validates itself: a planner change that broke
            // `pre_rps_marked` fails here rather than quietly turning the count
            // below into a different measurement. Marking only ever grows between
            // two AUs' RPS derivations, so the pre-RPS set is a strict superset.
            for rp in plan.dpb_refs.iter().chain(
                as_if
                    .rps
                    .st_curr_before
                    .iter()
                    .chain(&as_if.rps.st_curr_after)
                    .chain(&as_if.rps.lt_curr),
            ) {
                assert!(
                    as_if.dpb_refs.iter().any(|r| r.id == rp.id),
                    "AU {i}: picture {} is in the post-RPS marked DPB (or a current \
                     set) but not in the reconstructed pre-RPS one — marking is no \
                     longer monotone across an access unit boundary, and this test is \
                     measuring a different mutation than the one it documents",
                    rp.id
                );
            }

            let dxva = plan_to_dxva_h265(&as_if, map, i as u32 + 1).expect("conversion");
            converted += 1;
            if dxva.refs.iter().any(|r| r.slot == dxva.setup_slot) {
                aliased += 1;
            }
            plans.push(plan);
        }

        assert_eq!(converted, 120);
        assert_eq!(
            aliased, 115,
            "the pre-RPS snapshot must alias on every access unit that retires a \
             picture. {aliased} of 120 did — if this is 0, the stream no longer \
             reaches the shape and the exemption asserted by the test above is \
             unfalsifiable again; regenerate the fixture rather than relaxing this"
        );
    }

    #[test]
    fn the_irap_picture_sets_all_three_picture_type_flags_and_the_others_set_none() {
        let converted = convert_stream(TEST_25FPS);
        let (plan, first) = &converted[0];
        assert!(plan.picture.is_irap && plan.picture.is_idr);
        let flags = first.pic_params.dwCodingSettingPicturePropertyFlags;
        assert_ne!(flags & (1 << 16), 0, "IrapPicFlag");
        assert_ne!(flags & (1 << 17), 0, "IdrPicFlag");
        assert_ne!(flags & (1 << 18), 0, "IntraPicFlag");
        assert!(first.refs.is_empty());

        let (plan, second) = &converted[1];
        assert!(!plan.picture.is_irap);
        let flags = second.pic_params.dwCodingSettingPicturePropertyFlags;
        assert_eq!(flags & (0b111 << 16), 0);
        assert!(!second.refs.is_empty());
    }

    #[test]
    fn the_picture_parameters_carry_the_active_sps_and_pps_verbatim() {
        let converted = convert_stream(TEST_25FPS);
        let (plan, dxva) = &converted[0];
        let pp = &dxva.pic_params;
        let sps = &plan.sps;
        let pps = &plan.pps;
        assert_eq!(
            u32::from(pp.PicWidthInMinCbsY) << sps.min_cb_log2_size_y,
            u32::from(sps.pic_width_in_luma_samples)
        );
        assert_eq!(
            u32::from(pp.PicHeightInMinCbsY) << sps.min_cb_log2_size_y,
            u32::from(sps.pic_height_in_luma_samples)
        );
        assert_eq!(
            pp.log2_min_luma_coding_block_size_minus3,
            sps.log2_min_luma_coding_block_size_minus3
        );
        assert_eq!(
            pp.log2_diff_max_min_luma_coding_block_size,
            sps.log2_diff_max_min_luma_coding_block_size
        );
        assert_eq!(
            pp.log2_min_transform_block_size_minus2,
            sps.log2_min_luma_transform_block_size_minus2
        );
        assert_eq!(
            pp.log2_diff_max_min_transform_block_size,
            sps.log2_diff_max_min_luma_transform_block_size
        );
        assert_eq!(
            pp.max_transform_hierarchy_depth_inter,
            sps.max_transform_hierarchy_depth_inter
        );
        assert_eq!(
            pp.max_transform_hierarchy_depth_intra,
            sps.max_transform_hierarchy_depth_intra
        );
        assert_eq!(
            pp.num_short_term_ref_pic_sets,
            sps.num_short_term_ref_pic_sets
        );
        assert_eq!(
            pp.num_long_term_ref_pics_sps,
            sps.num_long_term_ref_pics_sps
        );
        assert_eq!(pp.init_qp_minus26, pps.init_qp_minus26);
        assert_eq!(pp.pps_cb_qp_offset, pps.cb_qp_offset);
        assert_eq!(pp.pps_cr_qp_offset, pps.cr_qp_offset);
        assert_eq!(pp.pps_beta_offset_div2, pps.beta_offset_div2);
        assert_eq!(pp.pps_tc_offset_div2, pps.tc_offset_div2);
        assert_eq!(
            pp.log2_parallel_merge_level_minus2,
            pps.log2_parallel_merge_level_minus2
        );
        assert_eq!(
            pp.sps_max_dec_pic_buffering_minus1,
            sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)]
        );
        assert_eq!(pp.StatusReportFeedbackNumber, 1);
        // Reserved fields stay zero, as both the spec and every driver expect.
        assert_eq!(pp.ReservedBits2, 0);
        assert_eq!(pp.ReservedBits5, 0);
        assert_eq!(pp.ReservedBits6, 0);
        assert_eq!(pp.ReservedBits7, 0);
    }

    #[test]
    fn the_format_word_carries_the_streams_chroma_format_and_bit_depths() {
        let converted = convert_stream(TEST_25FPS);
        let (plan, dxva) = &converted[0];
        let expected = HevcFormatFlags {
            chroma_format_idc: plan.picture.chroma_format_idc,
            separate_colour_plane_flag: false,
            bit_depth_luma_minus8: plan.picture.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: plan.picture.bit_depth_chroma_minus8,
            log2_max_pic_order_cnt_lsb_minus4: plan.sps.log2_max_pic_order_cnt_lsb_minus4,
        }
        .pack();
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags, expected);
        // 8-bit 4:2:0: chroma_format_idc 1 at bit 0, both depths zero.
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags & 0x3, 1);
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags >> 3 & 0x7, 0);
        assert_eq!(dxva.pic_params.wFormatAndSequenceInfoFlags >> 6 & 0x7, 0);
    }

    /// H.265 Table 7-6's default 8x8 list for intra prediction (matrixId 0..2),
    /// in the up-right diagonal order both the coded syntax and DXVA use.
    ///
    /// Transcribed from the specification rather than imported from the vendored
    /// parser's own constant: a test that reads the same array the code reads
    /// cannot tell the defaults from whatever that array happens to hold, and the
    /// shape this guards — enabled, nothing coded — is where a wrong default
    /// drifts the picture to flat grey.
    const DEFAULT_INTRA_8X8: [u8; 64] = [
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19,
        20, 21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35,
        35, 31, 29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
    ];
    /// Table 7-6's default 8x8 list for INTER prediction (matrixId 3..5).
    const DEFAULT_INTER_8X8: [u8; 64] = [
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20,
        20, 20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28,
        28, 28, 28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
    ];

    /// The vector's first plan with its parameter sets rewritten to a chosen
    /// scaling-list shape.
    ///
    /// The parameter sets are the real ones the parser produced; only the
    /// scaling-list fields move. That is what makes the "coded nowhere" shape
    /// meaningful: `pps.scaling_list` still holds the parser's default fill, and
    /// `sps.scaling_list` still holds the all-zero `ScalingLists::default()` an
    /// uncoded SPS is left with.
    fn plan_with_scaling_lists(
        enabled: bool,
        sps_coded: Option<u8>,
        pps_coded: Option<u8>,
    ) -> (AuPlan, DecodePlanDxvaH265) {
        let mut planner = H265Planner::new();
        let aus = split_h265_aus(TEST_25FPS);
        let mut plan = planner.plan_au(aus[0]).expect("plan");

        let mut sps = (*plan.sps).clone();
        sps.scaling_list_enabled_flag = enabled;
        sps.scaling_list_data_present_flag = sps_coded.is_some();
        if let Some(fill) = sps_coded {
            sps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
            sps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
            sps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
            sps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
            sps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
            sps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
        }
        let mut pps = (*plan.pps).clone();
        pps.scaling_list_data_present_flag = pps_coded.is_some();
        if let Some(fill) = pps_coded {
            pps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
            pps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
            pps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
            pps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
            pps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
            pps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
        }
        plan.sps = std::rc::Rc::new(sps);
        plan.pps = std::rc::Rc::new(pps);

        let mut slots = SlotMap::new(plan.picture.max_dpb_frames);
        let dxva = plan_to_dxva_h265(&plan, &mut slots, 1).expect("convert");
        (plan, dxva)
    }

    #[test]
    fn a_sequence_that_disables_scaling_lists_submits_no_quantization_matrix_at_all() {
        // The shape every punktfunk HEVC stream is in. libavcodec's
        // `dxva2_hevc_end_frame` passes the buffer only when
        // `dwCodingParamToolFlags & 1`; handing a driver a matrix it was told to
        // ignore is a bet on the driver ignoring it.
        let converted = convert_stream(TEST_25FPS);
        let (plan, dxva) = &converted[0];
        assert!(!plan.sps.scaling_list_enabled_flag);
        assert_eq!(dxva.pic_params.dwCodingParamToolFlags & 1, 0);
        assert_eq!(dxva.qmatrix, None);
        for (_, dxva) in &converted {
            assert_eq!(dxva.qmatrix, None);
        }
    }

    #[test]
    fn an_enabled_sequence_that_codes_no_list_anywhere_gets_the_table_7_5_and_7_6_defaults() {
        // The bug this guards: the vendored parser leaves an SPS that codes no
        // scaling list data with an all-zero `ScalingLists` (unlike libavcodec's,
        // which seeds the defaults), so selecting the SPS here would dequantize
        // every residual to nothing.
        let (plan, dxva) = plan_with_scaling_lists(true, None, None);
        assert!(plan.sps.scaling_list.scaling_list_4x4[0]
            .iter()
            .all(|&v| v == 0));
        assert_eq!(dxva.pic_params.dwCodingParamToolFlags & 1, 1);
        let qm = dxva
            .qmatrix
            .expect("an enabled sequence submits the matrix");

        for list in qm.ucScalingLists0 {
            assert_eq!(list, [16u8; 16]);
        }
        for (m, list) in qm.ucScalingLists1.iter().enumerate() {
            let want = if m < 3 {
                DEFAULT_INTRA_8X8
            } else {
                DEFAULT_INTER_8X8
            };
            assert_eq!(*list, want, "8x8 matrixId {m}");
        }
        for (m, list) in qm.ucScalingLists2.iter().enumerate() {
            let want = if m < 3 {
                DEFAULT_INTRA_8X8
            } else {
                DEFAULT_INTER_8X8
            };
            assert_eq!(*list, want, "16x16 matrixId {m}");
        }
        assert_eq!(qm.ucScalingLists3[0], DEFAULT_INTRA_8X8);
        assert_eq!(qm.ucScalingLists3[1], DEFAULT_INTER_8X8);
        // The inferred DC is 8, and DXVA takes the value — 8 + 8.
        assert_eq!(qm.ucScalingListDCCoefSizeID2, [16u8; 6]);
        assert_eq!(qm.ucScalingListDCCoefSizeID3, [16u8; 2]);
    }

    #[test]
    fn a_coded_pps_list_wins_and_a_coded_sps_list_is_taken_only_when_the_pps_codes_none() {
        // 7.4.5 activation order, with a distinct fill per source so the
        // selection is visible rather than inferred.
        let (_, pps_wins) = plan_with_scaling_lists(true, Some(7), Some(9));
        let qm = pps_wins.qmatrix.expect("enabled");
        assert_eq!(qm.ucScalingLists0[0], [9u8; 16]);
        assert_eq!(qm.ucScalingLists1[2], [9u8; 64]);
        assert_eq!(qm.ucScalingLists2[5], [9u8; 64]);
        assert_eq!(qm.ucScalingLists3[0], [9u8; 64]);
        assert_eq!(qm.ucScalingLists3[1], [9u8; 64]);
        // DC entries are the coded delta plus 8, and sizeId 3 takes the parser's
        // matrixId 0 and 3 rather than 0 and 1.
        assert_eq!(qm.ucScalingListDCCoefSizeID2, [17u8; 6]);
        assert_eq!(qm.ucScalingListDCCoefSizeID3, [17u8; 2]);

        let (_, sps_wins) = plan_with_scaling_lists(true, Some(7), None);
        let qm = sps_wins.qmatrix.expect("enabled");
        assert_eq!(qm.ucScalingLists0[0], [7u8; 16]);
        assert_eq!(qm.ucScalingLists1[2], [7u8; 64]);
        assert_eq!(qm.ucScalingListDCCoefSizeID2, [15u8; 6]);

        // Neither source reaches the driver when the sequence disables scaling
        // lists, however much data the parameter sets carry.
        let (_, disabled) = plan_with_scaling_lists(false, Some(7), Some(9));
        assert_eq!(disabled.qmatrix, None);
    }

    #[test]
    fn the_sizeid_3_entries_take_the_parsers_matrix_ids_0_and_3() {
        // Index arithmetic (`k * 3`) that would otherwise be invisible: with
        // matrixId 0 and 3 given different values, taking 0 and 1 reads the wrong
        // matrix for the inter slot.
        let (_, dxva) = {
            let mut planner = H265Planner::new();
            let aus = split_h265_aus(TEST_25FPS);
            let mut plan = planner.plan_au(aus[0]).expect("plan");
            let mut sps = (*plan.sps).clone();
            sps.scaling_list_enabled_flag = true;
            let mut pps = (*plan.pps).clone();
            pps.scaling_list_data_present_flag = true;
            let mut lists = [[0u8; 64]; 6];
            let mut dc = [0i16; 6];
            for (m, list) in lists.iter_mut().enumerate() {
                *list = [(m as u8 + 1) * 10; 64];
                dc[m] = m as i16 + 1;
            }
            pps.scaling_list.scaling_list_32x32 = lists;
            pps.scaling_list.scaling_list_dc_coef_minus8_32x32 = dc;
            plan.sps = std::rc::Rc::new(sps);
            plan.pps = std::rc::Rc::new(pps);
            let mut slots = SlotMap::new(plan.picture.max_dpb_frames);
            let dxva = plan_to_dxva_h265(&plan, &mut slots, 1).expect("convert");
            (plan, dxva)
        };
        let qm = dxva.qmatrix.expect("enabled");
        assert_eq!(qm.ucScalingLists3[0], [10u8; 64], "matrixId 0");
        assert_eq!(qm.ucScalingLists3[1], [40u8; 64], "matrixId 3");
        assert_eq!(qm.ucScalingListDCCoefSizeID3, [1 + 8, 4 + 8]);
    }

    #[test]
    fn the_b_frame_vector_converts_and_binds_both_directions_of_its_rps() {
        let converted = convert_stream(TEST_64X64_I_P_B_P);
        assert!(!converted.is_empty());
        // The B picture of I-P-B-P has references on both sides, so at least one
        // AU must populate both StCurrBefore and StCurrAfter.
        let both = converted.iter().any(|(plan, _)| {
            !plan.rps.st_curr_before.is_empty() && !plan.rps.st_curr_after.is_empty()
        });
        assert!(both, "the I-P-B-P vector must exercise bidirectional RPS");
        for (plan, dxva) in &converted {
            for slice in &plan.slices {
                for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
                    assert!(dxva.refs.iter().any(|r| r.id == rp.id));
                }
            }
        }
    }

    #[test]
    fn slice_ranges_ride_through_in_plan_order_on_start_code_boundaries() {
        let aus = split_h265_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        for (i, au) in aus.iter().enumerate() {
            let plan = planner.plan_au(au).expect("plan");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let dxva = plan_to_dxva_h265(&plan, map, i as u32 + 1).expect("convert");
            assert_eq!(dxva.slice_ranges.len(), plan.slices.len());
            for range in &dxva.slice_ranges {
                assert_eq!(au[range.start - 3..range.start], [0, 0, 1]);
            }
        }
    }

    #[test]
    fn a_capacity_mismatch_is_refused_and_leaves_the_map_untouched() {
        let aus = split_h265_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let plan = planner.plan_au(aus[0]).expect("plan");
        let mut slots = SlotMap::new(plan.picture.max_dpb_frames + 1);
        assert_eq!(
            plan_to_dxva_h265(&plan, &mut slots, 1),
            Err(PlanToDxvaH265Error::CapacityMismatch {
                required: plan.picture.max_dpb_frames + 1,
                capacity: plan.picture.max_dpb_frames + 2,
            })
        );
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn a_reference_the_map_never_saw_is_refused_and_leaves_the_map_untouched() {
        let aus = split_h265_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let first = planner.plan_au(aus[0]).expect("plan 0");
        let second = planner.plan_au(aus[1]).expect("plan 1");
        let mut slots = SlotMap::new(second.picture.max_dpb_frames);
        let missing = second.rps.st_curr_before[0].id;
        assert_eq!(first.dpb.stored, Some(missing));
        assert_eq!(
            plan_to_dxva_h265(&second, &mut slots, 1),
            Err(PlanToDxvaH265Error::UnresolvedReference(missing))
        );
        assert_eq!(slots.active(), 0);
    }

    #[test]
    fn slice_control_records_carry_the_packers_locations_verbatim() {
        let records = [
            crate::pack::SliceRecord {
                location: 0,
                bytes: 128,
            },
            crate::pack::SliceRecord {
                location: 128,
                bytes: 256,
            },
        ];
        let control = slice_control_h265(&records);
        // Read by value, in braces: the record is `#[repr(C, packed)]` (ten bytes),
        // so a reference to a `u32` member would be unaligned — and `assert_eq!`
        // takes references. See `dxva.rs`'s alignment section.
        assert_eq!({ control[0].BSNALunitDataLocation }, 0);
        assert_eq!({ control[0].SliceBytesInBuffer }, 128);
        assert_eq!({ control[0].wBadSliceChopping }, 0);
        // A second record, because the ten-vs-twelve byte defect is invisible on a
        // single-record buffer — the vendored HEVC vector is one slice segment per
        // picture, which is the shape that hid it.
        assert_eq!({ control[1].BSNALunitDataLocation }, 128);
        let bytes = crate::dxva::slice_bytes(&control);
        assert_eq!(bytes.len(), 20);
        assert_eq!(&bytes[10..14], &128u32.to_le_bytes());
        assert_eq!(&bytes[14..18], &256u32.to_le_bytes());
    }

    #[test]
    fn a_slot_is_reused_only_after_its_picture_leaves_the_dpb() {
        let aus = split_h265_aus(TEST_25FPS);
        let mut planner = H265Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut live: Vec<(PicId, u8)> = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            let Ok(plan) = planner.plan_au(au) else {
                continue;
            };
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            let removed = plan.dpb.removed.clone();
            let dxva = plan_to_dxva_h265(&plan, map, i as u32 + 1).expect("convert");
            live.retain(|&(id, _)| !removed.contains(&id));
            assert!(
                live.iter().all(|&(_, slot)| slot != dxva.setup_slot),
                "AU {i} decodes into a surface a live picture still holds"
            );
            live.push((dxva.setup_id, dxva.setup_slot));
        }
    }
}
