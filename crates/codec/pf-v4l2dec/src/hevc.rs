//! One HEVC access-unit plan into the stateless decoder's controls. Twin of
//! `pf_vaapi::pic_h265`, for frame-based decoding: every slice of the picture
//! in one buffer, one `slice_params` record each.
//!
//! What differs from the VA conversion, and mis-wires a driver if carried over:
//!
//! * References are named by the **timestamp** of the CAPTURE buffer that
//!   holds them, in nanoseconds. There is no surface or slot.
//! * The three current sets and the slice lists are indices into
//!   `decode_params.dpb`, which is the whole marked DPB. Unused is `0xff`.
//! * `data_byte_offset` counts raw buffer bytes: the start code when one is
//!   sent, the NAL and slice header, and the emulation-prevention bytes inside
//!   that header.
//! * Scaling lists go in raster order; the parser holds them in coded
//!   (up-right diagonal) order.
//! * `chroma_offset_lN` is the coded delta, not the derived offset.

use cros_codecs::codec::h265::parser::ScalingLists;
use cros_codecs::codec::h265::parser::SliceHeader;
use cros_codecs::codec::h265::parser::SliceType;
use cros_codecs::codec::h265::parser::Sps;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::PicId;
use pf_bitstream::h265::RefPic;
use pf_bitstream::h265::RefRpsIdxError;

use crate::uapi_stateless as uapi;
use crate::uapi_stateless::V4l2CtrlHevcDecodeParams;
use crate::uapi_stateless::V4l2CtrlHevcExtSpsLtRps;
use crate::uapi_stateless::V4l2CtrlHevcExtSpsStRps;
use crate::uapi_stateless::V4l2CtrlHevcPps;
use crate::uapi_stateless::V4l2CtrlHevcScalingMatrix;
use crate::uapi_stateless::V4l2CtrlHevcSliceParams;
use crate::uapi_stateless::V4l2CtrlHevcSps;
use crate::uapi_stateless::V4l2HevcDpbEntry;
use crate::uapi_stateless::V4L2_HEVC_DPB_ENTRIES_NUM_MAX;

/// A list position that names no picture.
pub const UNUSED: u8 = 0xff;

const START_CODE: [u8; 3] = [0, 0, 1];

/// Everything one decode request carries.
#[derive(Debug, Clone)]
pub struct Request {
    pub sps: V4l2CtrlHevcSps,
    pub pps: V4l2CtrlHevcPps,
    pub decode: V4l2CtrlHevcDecodeParams,
    pub slices: Vec<V4l2CtrlHevcSliceParams>,
    /// Flat when the stream has no scaling lists; drivers that require the
    /// control then get the values the standard infers.
    pub scaling: V4l2CtrlHevcScalingMatrix,
    /// `entry_point_offset_minus1` of every slice, in slice order.
    pub entry_points: Vec<u32>,
    pub st_rps: Vec<V4l2CtrlHevcExtSpsStRps>,
    pub lt_rps: Vec<V4l2CtrlHevcExtSpsLtRps>,
    /// The slices as the driver reads them, back to back.
    pub data: Vec<u8>,
}

/// Why a plan cannot be expressed as stateless controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillError {
    NoSlices,
    SeparateColourPlanes,
    /// More marked references than `dpb[16]` holds.
    TooManyReferences(usize),
    /// A reference this decoder holds no buffer for.
    UnresolvedReference(PicId),
    RefListTooLong {
        slice: usize,
        len: usize,
    },
    SliceRange {
        slice: usize,
    },
    UnalignedSliceHeader {
        slice: usize,
        bits: u32,
    },
    /// More entry points than the parser keeps.
    TooManyEntryPoints {
        slice: usize,
        count: u32,
    },
    /// `num_delta_pocs_of_ref_rps_idx` is not derivable from this plan.
    RefRpsIdx(RefRpsIdxError),
    /// The inline `st_ref_pic_set()` bit count exceeds the control's `u16`;
    /// a header that large is corrupt.
    StRpsBitsOverflow(u32),
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FillError::NoSlices => write!(f, "the access unit planned no slices"),
            FillError::SeparateColourPlanes => {
                write!(f, "separate colour planes are outside the envelope")
            }
            FillError::TooManyReferences(n) => write!(f, "{n} marked references exceed dpb[16]"),
            FillError::UnresolvedReference(id) => {
                write!(f, "picture {id} has no decoded buffer")
            }
            FillError::RefListTooLong { slice, len } => {
                write!(f, "slice {slice}: reference list of {len} exceeds 16")
            }
            FillError::SliceRange { slice } => {
                write!(f, "slice {slice}: byte range lies outside the access unit")
            }
            FillError::UnalignedSliceHeader { slice, bits } => {
                write!(f, "slice {slice}: a {bits}-bit header is not byte-aligned")
            }
            FillError::TooManyEntryPoints { slice, count } => {
                write!(
                    f,
                    "slice {slice}: {count} entry points exceed the parser's 32"
                )
            }
            FillError::RefRpsIdx(err) => write!(f, "{err}"),
            FillError::StRpsBitsOverflow(bits) => {
                write!(f, "inline st_ref_pic_set of {bits} bits exceeds u16")
            }
        }
    }
}

impl std::error::Error for FillError {}

/// Convert one planned access unit. `timestamp_of` answers with the CAPTURE
/// timestamp (ns) of a stored picture; `annex_b` prefixes each slice with a
/// start code, for drivers that want one.
pub fn fill(
    plan: &AuPlan,
    au: &[u8],
    timestamp_of: impl Fn(PicId) -> Option<u64>,
    annex_b: bool,
) -> Result<Request, FillError> {
    if plan.slices.is_empty() {
        return Err(FillError::NoSlices);
    }
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;
    if sps.separate_colour_plane_flag {
        return Err(FillError::SeparateColourPlanes);
    }
    if plan.dpb_refs.len() > V4L2_HEVC_DPB_ENTRIES_NUM_MAX {
        return Err(FillError::TooManyReferences(plan.dpb_refs.len()));
    }

    // Built first: the sets and the slice lists are indices into it.
    let mut dpb = [V4l2HevcDpbEntry::default(); V4L2_HEVC_DPB_ENTRIES_NUM_MAX];
    for (entry, rp) in dpb.iter_mut().zip(&plan.dpb_refs) {
        *entry = V4l2HevcDpbEntry {
            timestamp: timestamp_of(rp.id).ok_or(FillError::UnresolvedReference(rp.id))?,
            flags: if rp.is_long_term {
                uapi::V4L2_HEVC_DPB_ENTRY_LONG_TERM_REFERENCE
            } else {
                0
            },
            field_pic: 0,
            reserved: 0,
            pic_order_cnt_val: rp.pic_order_cnt,
        };
    }
    let index_of = |id: PicId| -> Result<u8, FillError> {
        plan.dpb_refs
            .iter()
            .position(|rp| rp.id == id)
            .map(|i| i as u8)
            .ok_or(FillError::UnresolvedReference(id))
    };
    let indices = |set: &[RefPic]| -> Result<([u8; 16], u8), FillError> {
        let mut out = [UNUSED; 16];
        for (slot, rp) in out.iter_mut().zip(set) {
            *slot = index_of(rp.id)?;
        }
        Ok((out, set.len().min(16) as u8))
    };

    let first = &plan.slices[0].header;
    let lt_bits = long_term_bits(first, sps);
    let st_bits = u16::try_from(pic.short_term_ref_pic_set_size_bits)
        .map_err(|_| FillError::StRpsBitsOverflow(pic.short_term_ref_pic_set_size_bits))?;
    let num_delta_pocs = plan
        .num_delta_pocs_of_ref_rps_idx()
        .map_err(FillError::RefRpsIdx)?;

    let mut slices = Vec::with_capacity(plan.slices.len());
    let mut entry_points = Vec::new();
    let mut data = Vec::with_capacity(au.len() + plan.slices.len() * START_CODE.len());
    for (index, sp) in plan.slices.iter().enumerate() {
        let hdr = &sp.header;
        let nal = au
            .get(sp.nal.clone())
            .filter(|n| n.len() > 2)
            .ok_or(FillError::SliceRange { slice: index })?;
        if hdr.header_bit_size % 8 != 0 {
            return Err(FillError::UnalignedSliceHeader {
                slice: index,
                bits: hdr.header_bit_size,
            });
        }
        let prefix = if annex_b { START_CODE.len() } else { 0 };
        if annex_b {
            data.extend_from_slice(&START_CODE);
        }
        data.extend_from_slice(nal);

        let mut rec = V4l2CtrlHevcSliceParams {
            bit_size: ((prefix + nal.len()) * 8) as u32,
            // The parser's size is of the unescaped header; the buffer holds
            // the escaped one.
            data_byte_offset: prefix as u32
                + hdr.header_bit_size / 8
                + hdr.n_emulation_prevention_bytes,
            num_entry_point_offsets: hdr.num_entry_point_offsets,
            nal_unit_type: (nal[0] >> 1) & 0x3f,
            nuh_temporal_id_plus1: nal[1] & 0x07,
            // H.265's own numbering (B=0, P=1, I=2) is the kernel's.
            slice_type: hdr.type_ as u8,
            colour_plane_id: hdr.colour_plane_id,
            slice_pic_order_cnt: pic.pic_order_cnt,
            num_ref_idx_l0_active_minus1: hdr.num_ref_idx_l0_active_minus1,
            num_ref_idx_l1_active_minus1: hdr.num_ref_idx_l1_active_minus1,
            collocated_ref_idx: hdr.collocated_ref_idx,
            five_minus_max_num_merge_cand: hdr.five_minus_max_num_merge_cand,
            slice_qp_delta: hdr.qp_delta,
            slice_cb_qp_offset: hdr.cb_qp_offset,
            slice_cr_qp_offset: hdr.cr_qp_offset,
            slice_act_y_qp_offset: hdr.slice_act_y_qp_offset,
            slice_act_cb_qp_offset: hdr.slice_act_cb_qp_offset,
            slice_act_cr_qp_offset: hdr.slice_act_cr_qp_offset,
            slice_beta_offset_div2: hdr.beta_offset_div2,
            slice_tc_offset_div2: hdr.tc_offset_div2,
            slice_segment_addr: hdr.segment_address,
            ref_idx_l0: [UNUSED; 16],
            ref_idx_l1: [UNUSED; 16],
            short_term_ref_pic_set_size: st_bits,
            long_term_ref_pic_set_size: lt_bits,
            ..Default::default()
        };

        for (list_out, list_in) in [
            (&mut rec.ref_idx_l0, &sp.ref_list0),
            (&mut rec.ref_idx_l1, &sp.ref_list1),
        ] {
            if list_in.len() > list_out.len() {
                return Err(FillError::RefListTooLong {
                    slice: index,
                    len: list_in.len(),
                });
            }
            for (slot, rp) in list_out.iter_mut().zip(list_in) {
                *slot = index_of(rp.id)?;
            }
        }

        // 7.3.6.1: a weight table is coded only for P+weighted_pred or
        // B+weighted_bipred; elsewhere the parser holds defaults.
        let weighted = (pps.weighted_pred_flag && hdr.type_ == SliceType::P)
            || (pps.weighted_bipred_flag && hdr.type_ == SliceType::B);
        if weighted {
            let pwt = &hdr.pred_weight_table;
            let out = &mut rec.pred_weight_table;
            out.luma_log2_weight_denom = pwt.luma_log2_weight_denom;
            out.delta_chroma_log2_weight_denom = pwt.delta_chroma_log2_weight_denom;
            out.delta_luma_weight_l0[..15].copy_from_slice(&pwt.delta_luma_weight_l0);
            out.luma_offset_l0[..15].copy_from_slice(&pwt.luma_offset_l0);
            out.delta_chroma_weight_l0[..15].copy_from_slice(&pwt.delta_chroma_weight_l0);
            out.chroma_offset_l0 = narrow_offsets(&pwt.delta_chroma_offset_l0);
            out.delta_luma_weight_l1[..15].copy_from_slice(&pwt.delta_luma_weight_l1);
            out.luma_offset_l1[..15].copy_from_slice(&pwt.luma_offset_l1);
            out.delta_chroma_weight_l1[..15].copy_from_slice(&pwt.delta_chroma_weight_l1);
            out.chroma_offset_l1 = narrow_offsets(&pwt.delta_chroma_offset_l1);
        }

        let count = hdr.num_entry_point_offsets;
        let kept = hdr.entry_point_offset_minus1.get(..count as usize).ok_or(
            FillError::TooManyEntryPoints {
                slice: index,
                count,
            },
        )?;
        entry_points.extend_from_slice(kept);

        rec.flags = flags(&[
            (
                hdr.sao_luma_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_SAO_LUMA,
            ),
            (
                hdr.sao_chroma_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_SAO_CHROMA,
            ),
            (
                hdr.temporal_mvp_enabled_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_TEMPORAL_MVP_ENABLED,
            ),
            (
                hdr.mvd_l1_zero_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_MVD_L1_ZERO,
            ),
            (
                hdr.cabac_init_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_CABAC_INIT,
            ),
            (
                hdr.collocated_from_l0_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_COLLOCATED_FROM_L0,
            ),
            (
                hdr.use_integer_mv_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_USE_INTEGER_MV,
            ),
            (
                hdr.deblocking_filter_disabled_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_DEBLOCKING_FILTER_DISABLED,
            ),
            (
                hdr.loop_filter_across_slices_enabled_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED,
            ),
            (
                hdr.dependent_slice_segment_flag,
                uapi::V4L2_HEVC_SLICE_PARAMS_FLAG_DEPENDENT_SLICE_SEGMENT,
            ),
        ]);
        slices.push(rec);
    }

    let (poc_st_curr_before, num_poc_st_curr_before) = indices(&plan.rps.st_curr_before)?;
    let (poc_st_curr_after, num_poc_st_curr_after) = indices(&plan.rps.st_curr_after)?;
    let (poc_lt_curr, num_poc_lt_curr) = indices(&plan.rps.lt_curr)?;
    let decode = V4l2CtrlHevcDecodeParams {
        pic_order_cnt_val: pic.pic_order_cnt,
        short_term_ref_pic_set_size: st_bits,
        long_term_ref_pic_set_size: lt_bits,
        num_active_dpb_entries: plan.dpb_refs.len() as u8,
        num_poc_st_curr_before,
        num_poc_st_curr_after,
        num_poc_lt_curr,
        poc_st_curr_before,
        poc_st_curr_after,
        poc_lt_curr,
        num_delta_pocs_of_ref_rps_idx: num_delta_pocs,
        reserved: [0; 3],
        dpb,
        flags: flags(&[
            (pic.is_irap, uapi::V4L2_HEVC_DECODE_PARAM_FLAG_IRAP_PIC),
            (pic.is_idr, uapi::V4L2_HEVC_DECODE_PARAM_FLAG_IDR_PIC),
            (
                first.no_output_of_prior_pics_flag,
                uapi::V4L2_HEVC_DECODE_PARAM_FLAG_NO_OUTPUT_OF_PRIOR,
            ),
        ]),
    };

    let top = usize::from(sps.max_sub_layers_minus1).min(6);
    let sps_ctrl = V4l2CtrlHevcSps {
        video_parameter_set_id: sps.video_parameter_set_id,
        seq_parameter_set_id: sps.seq_parameter_set_id,
        pic_width_in_luma_samples: sps.pic_width_in_luma_samples,
        pic_height_in_luma_samples: sps.pic_height_in_luma_samples,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        sps_max_dec_pic_buffering_minus1: sps.max_dec_pic_buffering_minus1[top],
        sps_max_num_reorder_pics: sps.max_num_reorder_pics[top],
        sps_max_latency_increase_plus1: sps.max_latency_increase_plus1[top],
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_luma_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_luma_transform_block_size: sps
            .log2_diff_max_min_luma_transform_block_size,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        pcm_sample_bit_depth_luma_minus1: sps.pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: sps.pcm_sample_bit_depth_chroma_minus1,
        log2_min_pcm_luma_coding_block_size_minus3: sps.log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: sps
            .log2_diff_max_min_pcm_luma_coding_block_size,
        num_short_term_ref_pic_sets: sps.num_short_term_ref_pic_sets,
        num_long_term_ref_pics_sps: sps.num_long_term_ref_pics_sps,
        chroma_format_idc: sps.chroma_format_idc,
        sps_max_sub_layers_minus1: sps.max_sub_layers_minus1,
        reserved: [0; 6],
        flags: flags(&[
            (
                sps.separate_colour_plane_flag,
                uapi::V4L2_HEVC_SPS_FLAG_SEPARATE_COLOUR_PLANE,
            ),
            (
                sps.scaling_list_enabled_flag,
                uapi::V4L2_HEVC_SPS_FLAG_SCALING_LIST_ENABLED,
            ),
            (sps.amp_enabled_flag, uapi::V4L2_HEVC_SPS_FLAG_AMP_ENABLED),
            (
                sps.sample_adaptive_offset_enabled_flag,
                uapi::V4L2_HEVC_SPS_FLAG_SAMPLE_ADAPTIVE_OFFSET,
            ),
            (sps.pcm_enabled_flag, uapi::V4L2_HEVC_SPS_FLAG_PCM_ENABLED),
            (
                sps.pcm_loop_filter_disabled_flag,
                uapi::V4L2_HEVC_SPS_FLAG_PCM_LOOP_FILTER_DISABLED,
            ),
            (
                sps.long_term_ref_pics_present_flag,
                uapi::V4L2_HEVC_SPS_FLAG_LONG_TERM_REF_PICS_PRESENT,
            ),
            (
                sps.temporal_mvp_enabled_flag,
                uapi::V4L2_HEVC_SPS_FLAG_SPS_TEMPORAL_MVP_ENABLED,
            ),
            (
                sps.strong_intra_smoothing_enabled_flag,
                uapi::V4L2_HEVC_SPS_FLAG_STRONG_INTRA_SMOOTHING_ENABLED,
            ),
        ]),
    };

    let pps_ctrl = V4l2CtrlHevcPps {
        pic_parameter_set_id: pps.pic_parameter_set_id,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        init_qp_minus26: pps.init_qp_minus26,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth,
        pps_cb_qp_offset: pps.cb_qp_offset,
        pps_cr_qp_offset: pps.cr_qp_offset,
        num_tile_columns_minus1: pps.num_tile_columns_minus1,
        num_tile_rows_minus1: pps.num_tile_rows_minus1,
        column_width_minus1: narrow(&pps.column_width_minus1),
        row_height_minus1: narrow(&pps.row_height_minus1),
        pps_beta_offset_div2: pps.beta_offset_div2,
        pps_tc_offset_div2: pps.tc_offset_div2,
        log2_parallel_merge_level_minus2: pps.log2_parallel_merge_level_minus2,
        reserved: 0,
        flags: flags(&[
            (
                pps.dependent_slice_segments_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_DEPENDENT_SLICE_SEGMENT_ENABLED,
            ),
            (
                pps.output_flag_present_flag,
                uapi::V4L2_HEVC_PPS_FLAG_OUTPUT_FLAG_PRESENT,
            ),
            (
                pps.sign_data_hiding_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_SIGN_DATA_HIDING_ENABLED,
            ),
            (
                pps.cabac_init_present_flag,
                uapi::V4L2_HEVC_PPS_FLAG_CABAC_INIT_PRESENT,
            ),
            (
                pps.constrained_intra_pred_flag,
                uapi::V4L2_HEVC_PPS_FLAG_CONSTRAINED_INTRA_PRED,
            ),
            (
                pps.transform_skip_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_TRANSFORM_SKIP_ENABLED,
            ),
            (
                pps.cu_qp_delta_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_CU_QP_DELTA_ENABLED,
            ),
            (
                pps.slice_chroma_qp_offsets_present_flag,
                uapi::V4L2_HEVC_PPS_FLAG_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT,
            ),
            (
                pps.weighted_pred_flag,
                uapi::V4L2_HEVC_PPS_FLAG_WEIGHTED_PRED,
            ),
            (
                pps.weighted_bipred_flag,
                uapi::V4L2_HEVC_PPS_FLAG_WEIGHTED_BIPRED,
            ),
            (
                pps.transquant_bypass_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_TRANSQUANT_BYPASS_ENABLED,
            ),
            (
                pps.tiles_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_TILES_ENABLED,
            ),
            (
                pps.entropy_coding_sync_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_ENTROPY_CODING_SYNC_ENABLED,
            ),
            (
                pps.loop_filter_across_tiles_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_LOOP_FILTER_ACROSS_TILES_ENABLED,
            ),
            (
                pps.loop_filter_across_slices_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED,
            ),
            (
                pps.deblocking_filter_override_enabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_DEBLOCKING_FILTER_OVERRIDE_ENABLED,
            ),
            (
                pps.deblocking_filter_disabled_flag,
                uapi::V4L2_HEVC_PPS_FLAG_PPS_DISABLE_DEBLOCKING_FILTER,
            ),
            (
                pps.lists_modification_present_flag,
                uapi::V4L2_HEVC_PPS_FLAG_LISTS_MODIFICATION_PRESENT,
            ),
            (
                pps.slice_segment_header_extension_present_flag,
                uapi::V4L2_HEVC_PPS_FLAG_SLICE_SEGMENT_HEADER_EXTENSION_PRESENT,
            ),
            (
                pps.deblocking_filter_control_present_flag,
                uapi::V4L2_HEVC_PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT,
            ),
            (
                pps.uniform_spacing_flag,
                uapi::V4L2_HEVC_PPS_FLAG_UNIFORM_SPACING,
            ),
        ]),
    };

    let scaling = plan
        .active_scaling_lists()
        .map_or(V4l2CtrlHevcScalingMatrix::FLAT, scaling_matrix);

    Ok(Request {
        sps: sps_ctrl,
        pps: pps_ctrl,
        decode,
        slices,
        scaling,
        entry_points,
        st_rps: short_term_sets(sps),
        lt_rps: long_term_sets(sps),
        data,
    })
}

fn flags(set: &[(bool, u64)]) -> u64 {
    set.iter()
        .filter(|(on, _)| *on)
        .fold(0, |acc, (_, bit)| acc | bit)
}

/// Parser `u32` tile sizes → the control's `u8`, saturating.
fn narrow<const N: usize>(src: &[u32]) -> [u8; N] {
    std::array::from_fn(|i| u8::try_from(src.get(i).copied().unwrap_or(0)).unwrap_or(u8::MAX))
}

/// The coded chroma offset deltas, clamped into the control's `s8`.
fn narrow_offsets(src: &[[i16; 2]; 15]) -> [[i8; 2]; 16] {
    std::array::from_fn(|i| {
        src.get(i).map_or([0; 2], |pair| {
            pair.map(|v| v.clamp(i16::from(i8::MIN), i16::from(i8::MAX)) as i8)
        })
    })
}

/// Bits of `ue(v)` coding `v`.
fn ue_bits(v: u32) -> u32 {
    2 * (u64::from(v) + 1).ilog2() + 1
}

/// Bits of the long-term reference syntax in the slice header (7.3.6.1),
/// which a header-parsing decoder skips by count.
fn long_term_bits(hdr: &SliceHeader, sps: &Sps) -> u16 {
    if !sps.long_term_ref_pics_present_flag {
        return 0;
    }
    let from_sps = usize::from(hdr.num_long_term_sps);
    let total = (from_sps + usize::from(hdr.num_long_term_pics)).min(hdr.poc_lsb_lt.len());
    let mut bits = ue_bits(u32::from(hdr.num_long_term_pics));
    if sps.num_long_term_ref_pics_sps > 0 {
        bits += ue_bits(u32::from(hdr.num_long_term_sps));
    }
    for i in 0..total {
        if i < from_sps {
            if sps.num_long_term_ref_pics_sps > 1 {
                // `Ceil(Log2(num_long_term_ref_pics_sps))`.
                bits += u32::from(sps.num_long_term_ref_pics_sps - 1).ilog2() + 1;
            }
        } else {
            bits += u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4 + 1;
        }
        bits += 1;
        if hdr.delta_poc_msb_present_flag[i] {
            // The parser keeps the running sum (7-52); the stream coded the step.
            let prior = if i != 0 && i != from_sps {
                hdr.delta_poc_msb_cycle_lt[i - 1]
            } else {
                0
            };
            bits += ue_bits(hdr.delta_poc_msb_cycle_lt[i].wrapping_sub(prior));
        }
    }
    bits.min(u32::from(u16::MAX)) as u16
}

/// The SPS's short-term sets, each written out explicitly. The parser keeps
/// the derived deltas, which describe the same set whether or not the stream
/// predicted it from another.
fn short_term_sets(sps: &Sps) -> Vec<V4l2CtrlHevcExtSpsStRps> {
    sps.short_term_ref_pic_set
        .iter()
        .take(usize::from(sps.num_short_term_ref_pic_sets))
        .map(|set| {
            let negative = usize::from(set.num_negative_pics).min(16);
            let positive = usize::from(set.num_positive_pics).min(16);
            let mut out = V4l2CtrlHevcExtSpsStRps {
                num_negative_pics: negative as u8,
                num_positive_pics: positive as u8,
                ..Default::default()
            };
            let mut prior = 0i32;
            for i in 0..negative {
                out.delta_poc_s0_minus1[i] = (prior - set.delta_poc_s0[i] - 1).max(0) as u16;
                prior = set.delta_poc_s0[i];
                out.used_by_curr_pic |= u32::from(set.used_by_curr_pic_s0[i]) << i;
            }
            prior = 0;
            for i in 0..positive {
                out.delta_poc_s1_minus1[i] = (set.delta_poc_s1[i] - prior - 1).max(0) as u16;
                prior = set.delta_poc_s1[i];
                out.used_by_curr_pic |= u32::from(set.used_by_curr_pic_s1[i]) << (negative + i);
            }
            out
        })
        .collect()
}

fn long_term_sets(sps: &Sps) -> Vec<V4l2CtrlHevcExtSpsLtRps> {
    (0..usize::from(sps.num_long_term_ref_pics_sps).min(sps.lt_ref_pic_poc_lsb_sps.len()))
        .map(|i| V4l2CtrlHevcExtSpsLtRps {
            lt_ref_pic_poc_lsb_sps: sps.lt_ref_pic_poc_lsb_sps[i] as u16,
            flags: if sps.used_by_curr_pic_lt_sps_flag[i] {
                uapi::V4L2_HEVC_EXT_SPS_LT_RPS_FLAG_USED_LT
            } else {
                0
            },
        })
        .collect()
}

/// Raster position of each coefficient in up-right diagonal order (6.5.3).
fn diagonal_scan(size: usize) -> Vec<usize> {
    let mut scan = Vec::with_capacity(size * size);
    let (mut x, mut y) = (0isize, 0isize);
    while scan.len() < size * size {
        while y >= 0 {
            if (x as usize) < size && (y as usize) < size {
                scan.push(y as usize * size + x as usize);
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    scan
}

fn to_raster<const N: usize>(coded: &[u8; N], scan: &[usize]) -> [u8; N] {
    let mut out = [0u8; N];
    for (value, position) in coded.iter().zip(scan) {
        out[*position] = *value;
    }
    out
}

fn scaling_matrix(lists: &ScalingLists) -> V4l2CtrlHevcScalingMatrix {
    let scan4 = diagonal_scan(4);
    let scan8 = diagonal_scan(8);
    let dc = |minus8: i16| (minus8 + 8).clamp(0, 255) as u8;
    V4l2CtrlHevcScalingMatrix {
        scaling_list_4x4: lists.scaling_list_4x4.map(|l| to_raster(&l, &scan4)),
        scaling_list_8x8: lists.scaling_list_8x8.map(|l| to_raster(&l, &scan8)),
        scaling_list_16x16: lists.scaling_list_16x16.map(|l| to_raster(&l, &scan8)),
        // Only matrixIds 0 and 3 exist at 32x32; the parser keeps six slots.
        scaling_list_32x32: [
            to_raster(&lists.scaling_list_32x32[0], &scan8),
            to_raster(&lists.scaling_list_32x32[3], &scan8),
        ],
        scaling_list_dc_coef_16x16: lists.scaling_list_dc_coef_minus8_16x16.map(dc),
        scaling_list_dc_coef_32x32: [
            dc(lists.scaling_list_dc_coef_minus8_32x32[0]),
            dc(lists.scaling_list_dc_coef_minus8_32x32[3]),
        ],
    }
}

#[cfg(test)]
mod tests {
    use pf_bitstream::h265::H265Planner;
    use pf_bitstream::testing::split_h265_aus;
    use pf_bitstream::testing::H265_25FPS;

    use super::*;

    #[test]
    fn the_4x4_scan_is_the_standards() {
        assert_eq!(
            diagonal_scan(4),
            [0, 4, 1, 8, 5, 2, 12, 9, 6, 3, 13, 10, 7, 14, 11, 15]
        );
        let mut scan8 = diagonal_scan(8);
        assert_eq!(&scan8[..6], [0, 8, 1, 16, 9, 2]);
        scan8.sort_unstable();
        assert!(
            scan8.iter().copied().eq(0..64),
            "a permutation of the block"
        );
    }

    #[test]
    fn exp_golomb_lengths() {
        assert_eq!(ue_bits(0), 1);
        assert_eq!(ue_bits(1), 3);
        assert_eq!(ue_bits(2), 3);
        assert_eq!(ue_bits(3), 5);
        assert_eq!(ue_bits(6), 5);
        assert_eq!(ue_bits(7), 7);
    }

    /// Every picture of the vendored stream converts, with its references
    /// resolved and each slice's data offset landing inside that slice.
    #[test]
    fn the_whole_vendored_vector_converts() {
        let aus = split_h265_aus(H265_25FPS);
        let mut planner = H265Planner::new();
        let mut saw_references = false;
        for (index, au) in aus.iter().enumerate() {
            let plan = planner.plan_au(au).expect("the vector plans");
            for annex_b in [false, true] {
                // Any stored picture resolves: the timestamp is its id here.
                let req = fill(&plan, au, |id| Some(id * 1000), annex_b)
                    .unwrap_or_else(|e| panic!("AU {index}: {e}"));
                assert_eq!(req.slices.len(), plan.slices.len());
                let total: u32 = req.slices.iter().map(|s| s.bit_size).sum();
                assert_eq!(total as usize, req.data.len() * 8);
                let mut at = 0usize;
                for (n, (rec, sp)) in req.slices.iter().zip(&plan.slices).enumerate() {
                    let bytes = (rec.bit_size / 8) as usize;
                    let slice = &req.data[at..at + bytes];
                    let nal = if annex_b {
                        assert_eq!(&slice[..3], [0, 0, 1], "AU {index} slice {n}");
                        &slice[3..]
                    } else {
                        slice
                    };
                    assert_eq!(nal, &au[sp.nal.clone()]);
                    assert!((rec.data_byte_offset as usize) < bytes);
                    assert!(rec.data_byte_offset as usize > 2 + usize::from(annex_b) * 3);
                    for idx in rec.ref_idx_l0.iter().chain(&rec.ref_idx_l1) {
                        if *idx != UNUSED {
                            saw_references = true;
                            assert!(*idx < req.decode.num_active_dpb_entries);
                        }
                    }
                    at += bytes;
                }
                assert_eq!(
                    usize::from(req.decode.num_active_dpb_entries),
                    plan.dpb_refs.len()
                );
                for (entry, rp) in req.decode.dpb.iter().zip(&plan.dpb_refs) {
                    assert_eq!(entry.timestamp, rp.id * 1000);
                    assert_eq!(entry.pic_order_cnt_val, rp.pic_order_cnt);
                }
                assert_eq!(req.decode.pic_order_cnt_val, plan.picture.pic_order_cnt);
                assert_eq!(
                    req.st_rps.len(),
                    usize::from(plan.sps.num_short_term_ref_pic_sets)
                );
            }
        }
        assert!(saw_references, "no slice ever named a reference");
    }

    #[test]
    fn a_reference_with_no_buffer_is_refused() {
        let aus = split_h265_aus(H265_25FPS);
        let mut planner = H265Planner::new();
        let inter = aus
            .iter()
            .map(|au| (au, planner.plan_au(au).expect("plans")))
            .find(|(_, plan)| !plan.dpb_refs.is_empty())
            .expect("the vector has inter pictures");
        assert!(matches!(
            fill(&inter.1, inter.0, |_| None, false),
            Err(FillError::UnresolvedReference(_))
        ));
    }

    /// What the other rungs refuse is refused here too: an inline RPS that
    /// predicts from a missing candidate, and an RPS bit count past `u16`.
    #[test]
    fn a_header_the_other_rungs_refuse_is_refused() {
        let aus = split_h265_aus(H265_25FPS);
        let plan = H265Planner::new().plan_au(aus[0]).expect("plans");

        let mut bad_rps = plan.clone();
        let inline = bad_rps.sps.num_short_term_ref_pic_sets;
        let hdr = &mut bad_rps.slices[0].header;
        hdr.short_term_ref_pic_set_sps_flag = false;
        hdr.curr_rps_idx = inline;
        hdr.short_term_ref_pic_set.inter_ref_pic_set_prediction_flag = true;
        hdr.short_term_ref_pic_set.delta_idx_minus1 = inline; // RefRpsIdx = -1
        assert_eq!(
            fill(&bad_rps, aus[0], Some, false).unwrap_err(),
            FillError::RefRpsIdx(RefRpsIdxError::Invalid {
                curr_rps_idx: inline,
                delta_idx_minus1: inline,
            })
        );

        let mut bad_bits = plan;
        bad_bits.picture.short_term_ref_pic_set_size_bits = 1 << 16;
        assert_eq!(
            fill(&bad_bits, aus[0], Some, false).unwrap_err(),
            FillError::StRpsBitsOverflow(1 << 16)
        );
    }
}
