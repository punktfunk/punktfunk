//! One HEVC [`AuPlanH265`] into libva picture, IQ, and slice buffers. Twin of
//! [`crate::pic`]: validate and resolve against the pre-removal DPB, then mutate.
//!
//! HEVC differs from that H.264 conversion in five ways that mis-wire a driver:
//!
//! * `ReferenceFrames` is 15 entries, not 16.
//! * RPS membership is flags ORed onto each DPB entry
//!   (`VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE` / `_AFTER` / `_LT_CURR`). There are no
//!   `RefPicSet*` arrays: Vulkan wants slot indices there, DXVA wants list
//!   positions. Either convention here submits an empty RPS.
//! * Per-slice lists are indices into `ReferenceFrames` (`0xff` unused). Build the
//!   DPB array first; a named picture missing from it is a refusal.
//! * `slice_data_byte_offset` is bytes. `header_bit_size / 8` is exact — never
//!   round; `slice_data()` is already byte-aligned.
//! * The IQ matrix is submitted only when `scaling_list_enabled_flag` is set. A
//!   table of parser zeros would dequantise every residual to zero.

use std::ops::Range;

use cros_codecs::codec::h265::parser::SliceType as SliceTypeH265;
use pf_bitstream::h265::AuPlan as AuPlanH265;
use pf_bitstream::h265::PicId;

use crate::va::VA_SLICE_DATA_FLAG_ALL;
use crate::va_h265::LongSliceFlagsH265;
use crate::va_h265::PicFieldsH265;
use crate::va_h265::SliceParsingFieldsH265;
use crate::va_h265::VaIqMatrixBufferHEVC;
use crate::va_h265::VaPictureHEVC;
use crate::va_h265::VaPictureParameterBufferHEVC;
use crate::va_h265::VaSliceParameterBufferHEVC;
use crate::va_h265::REFERENCE_FRAMES_LEN_H265;
use crate::va_h265::REF_PIC_LIST_LEN_H265;
use crate::va_h265::VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
use crate::va_h265::VA_PICTURE_HEVC_RPS_LT_CURR;
use crate::va_h265::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER;
use crate::va_h265::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE;
use crate::SlotError;
use crate::SlotMap;
use pf_bitstream::slots::Removals;

/// Everything one HEVC `vaRenderPicture` sequence needs.
#[derive(Debug, Clone)]
pub struct DecodePlanVaH265 {
    pub pic_params: VaPictureParameterBufferHEVC,
    /// `None` when scaling lists are off — then no IQ buffer is submitted.
    pub iq_matrix: Option<VaIqMatrixBufferHEVC>,
    pub slices: Vec<VaSliceParameterBufferHEVC>,
    /// Each slice's data range, start code excluded. Parallel to [`Self::slices`].
    pub slice_data: Vec<Range<usize>>,
    pub setup_slot: u8,
}

/// Why an HEVC plan cannot be expressed as VAAPI buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVaH265Error {
    NoSlices,
    NoStoredId,
    SeparateColourPlanes,
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// A slice list or RPS set named a picture the marked DPB does not hold.
    /// HEVC lists are indices into that array: there is no fallback.
    UnresolvedReference(PicId),
    /// More marked references than `ReferenceFrames[15]` can express.
    TooManyReferences(usize),
    RefListTooLong {
        slice: usize,
        len: usize,
    },
    SurfaceOutOfRange {
        slot: u8,
        surfaces: usize,
    },
    SliceRange {
        slice: usize,
    },
    /// `header_bit_size` is not a whole number of bytes. Never round: a rounded
    /// `slice_data_byte_offset` would skip into the payload.
    UnalignedSliceHeader {
        slice: usize,
        bits: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVaH265Error {
    fn from(e: SlotError) -> Self {
        PlanToVaH265Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToVaH265Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVaH265Error::NoSlices => write!(f, "the access unit planned no slices"),
            PlanToVaH265Error::NoStoredId => write!(f, "the plan stored no picture id"),
            PlanToVaH265Error::SeparateColourPlanes => {
                write!(f, "separate colour planes are outside the envelope")
            }
            PlanToVaH265Error::CapacityMismatch { required, capacity } => write!(
                f,
                "the slot map holds {capacity} slots, this stream needs {required}"
            ),
            PlanToVaH265Error::UnresolvedReference(id) => {
                write!(f, "picture {id} is not in the marked DPB array")
            }
            PlanToVaH265Error::TooManyReferences(n) => {
                write!(f, "{n} marked references exceed ReferenceFrames[15]")
            }
            PlanToVaH265Error::RefListTooLong { slice, len } => {
                write!(f, "slice {slice}: reference list of {len} exceeds 15")
            }
            PlanToVaH265Error::SurfaceOutOfRange { slot, surfaces } => {
                write!(f, "DPB slot {slot} has no surface in a table of {surfaces}")
            }
            PlanToVaH265Error::SliceRange { slice } => {
                write!(f, "slice {slice}: byte range lies outside the access unit")
            }
            PlanToVaH265Error::UnalignedSliceHeader { slice, bits } => write!(
                f,
                "slice {slice}: a {bits}-bit header is not byte-aligned, so \
                 slice_data_byte_offset cannot be exact"
            ),
            PlanToVaH265Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVaH265Error {}

/// Convert one planned HEVC access unit. `au`, `surfaces`, and `setup_surface`
/// match [`crate::pic::plan_to_va`]: the caller binds the decode target, because
/// reading it from a slot-indexed table would reuse a surface still on screen.
pub fn plan_to_va_h265(
    plan: &AuPlanH265,
    au: &[u8],
    slots: &mut SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<DecodePlanVaH265, PlanToVaH265Error> {
    if plan.slices.is_empty() {
        return Err(PlanToVaH265Error::NoSlices);
    }
    let setup_id = plan.dpb.stored.ok_or(PlanToVaH265Error::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if sps.separate_colour_plane_flag {
        return Err(PlanToVaH265Error::SeparateColourPlanes);
    }
    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVaH265Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }
    // Pre-check so the caller's post-return bind of `setup_surface` to the
    // returned slot cannot land past `surfaces`.
    if surfaces.len() < slots.capacity() {
        return Err(PlanToVaH265Error::SurfaceOutOfRange {
            slot: (slots.capacity() - 1) as u8,
            surfaces: surfaces.len(),
        });
    }
    if plan.dpb_refs.len() > REFERENCE_FRAMES_LEN_H265 {
        return Err(PlanToVaH265Error::TooManyReferences(plan.dpb_refs.len()));
    }

    // Built first: per-slice lists are indices into this array. `dpb_refs` is
    // the marked DPB, including RefPicSet*Foll pictures later AUs still need.
    let mut reference_frames = [VaPictureHEVC::invalid(); REFERENCE_FRAMES_LEN_H265];
    let mut index_of: Vec<(PicId, u8)> = Vec::with_capacity(plan.dpb_refs.len());
    for (slot_out, rp) in reference_frames.iter_mut().zip(&plan.dpb_refs) {
        let slot = slots
            .slot_of(rp.id)
            .ok_or(PlanToVaH265Error::UnresolvedReference(rp.id))?;
        let surface =
            *surfaces
                .get(usize::from(slot))
                .ok_or(PlanToVaH265Error::SurfaceOutOfRange {
                    slot,
                    surfaces: surfaces.len(),
                })?;
        *slot_out = VaPictureHEVC {
            picture_id: surface,
            pic_order_cnt: rp.pic_order_cnt,
            flags: if rp.is_long_term {
                VA_PICTURE_HEVC_LONG_TERM_REFERENCE
            } else {
                0
            },
            va_reserved: [0; 4],
        };
        index_of.push((rp.id, index_of.len() as u8));
    }

    // VAAPI has no RefPicSet* arrays: OR membership onto the DPB entries the
    // three current sets name.
    for (set, flag) in [
        (&plan.rps.st_curr_before, VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE),
        (&plan.rps.st_curr_after, VA_PICTURE_HEVC_RPS_ST_CURR_AFTER),
        (&plan.rps.lt_curr, VA_PICTURE_HEVC_RPS_LT_CURR),
    ] {
        for rp in set {
            let idx = index_of
                .iter()
                .find(|(id, _)| *id == rp.id)
                .map(|(_, i)| usize::from(*i))
                .ok_or(PlanToVaH265Error::UnresolvedReference(rp.id))?;
            reference_frames[idx].flags |= flag;
        }
    }

    let find_index = |id: PicId| -> Result<u8, PlanToVaH265Error> {
        index_of
            .iter()
            .find(|(other, _)| *other == id)
            .map(|(_, i)| *i)
            .ok_or(PlanToVaH265Error::UnresolvedReference(id))
    };

    let mut slices = Vec::with_capacity(plan.slices.len());
    let mut slice_data = Vec::with_capacity(plan.slices.len());
    for (index, sp) in plan.slices.iter().enumerate() {
        let hdr = &sp.header;
        let mut rec = VaSliceParameterBufferHEVC::zeroed();

        // VAAPI takes the NAL without its start code.
        if au.get(sp.nal.clone()).is_none() {
            return Err(PlanToVaH265Error::SliceRange { slice: index });
        }
        let payload = sp.nal.clone();
        rec.slice_data_size = (payload.end - payload.start) as u32;
        rec.slice_data_offset = 0;
        rec.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
        if hdr.header_bit_size % 8 != 0 {
            return Err(PlanToVaH265Error::UnalignedSliceHeader {
                slice: index,
                bits: hdr.header_bit_size,
            });
        }
        rec.slice_data_byte_offset = hdr.header_bit_size / 8;
        rec.slice_data_num_emu_prevn_bytes = hdr.n_emulation_prevention_bytes as u16;
        slice_data.push(payload);

        rec.slice_segment_address = hdr.segment_address;
        rec.collocated_ref_idx = hdr.collocated_ref_idx;
        rec.num_ref_idx_l0_active_minus1 = hdr.num_ref_idx_l0_active_minus1;
        rec.num_ref_idx_l1_active_minus1 = hdr.num_ref_idx_l1_active_minus1;
        rec.slice_qp_delta = hdr.qp_delta;
        rec.slice_cb_qp_offset = hdr.cb_qp_offset;
        rec.slice_cr_qp_offset = hdr.cr_qp_offset;
        rec.slice_beta_offset_div2 = hdr.beta_offset_div2;
        rec.slice_tc_offset_div2 = hdr.tc_offset_div2;
        rec.five_minus_max_num_merge_cand = hdr.five_minus_max_num_merge_cand;
        rec.num_entry_point_offsets = hdr.num_entry_point_offsets as u16;

        rec.long_slice_flags = LongSliceFlagsH265 {
            // The plan is one picture, so the last record IS the last slice of it.
            last_slice_of_pic: index + 1 == plan.slices.len(),
            dependent_slice_segment_flag: hdr.dependent_slice_segment_flag,
            // H.265's own numbering (B=0, P=1, I=2), which is what libva's two bits
            // take — no remap, unlike H.264's.
            slice_type: hdr.type_ as u8,
            color_plane_id: 0,
            slice_sao_luma_flag: hdr.sao_luma_flag,
            slice_sao_chroma_flag: hdr.sao_chroma_flag,
            mvd_l1_zero_flag: hdr.mvd_l1_zero_flag,
            cabac_init_flag: hdr.cabac_init_flag,
            slice_temporal_mvp_enabled_flag: hdr.temporal_mvp_enabled_flag,
            slice_deblocking_filter_disabled_flag: hdr.deblocking_filter_disabled_flag,
            collocated_from_l0_flag: hdr.collocated_from_l0_flag,
            slice_loop_filter_across_slices_enabled_flag: hdr
                .loop_filter_across_slices_enabled_flag,
        }
        .pack();

        for (list_out, list_in) in [(0usize, &sp.ref_list0), (1usize, &sp.ref_list1)] {
            if list_in.len() > REF_PIC_LIST_LEN_H265 {
                return Err(PlanToVaH265Error::RefListTooLong {
                    slice: index,
                    len: list_in.len(),
                });
            }
            for (n, rp) in list_in.iter().enumerate() {
                rec.ref_pic_list[list_out][n] = find_index(rp.id)?;
            }
        }

        // 7.3.6.1: a weight table is coded only for P+weighted_pred or
        // B+weighted_bipred. Copying it elsewhere hands the driver parser defaults
        // as if the stream had coded them.
        let weighted = (pps.weighted_pred_flag && hdr.type_ == SliceTypeH265::P)
            || (pps.weighted_bipred_flag && hdr.type_ == SliceTypeH265::B);
        if weighted {
            let pwt = &hdr.pred_weight_table;
            rec.luma_log2_weight_denom = pwt.luma_log2_weight_denom;
            rec.delta_chroma_log2_weight_denom = pwt.delta_chroma_log2_weight_denom;
            rec.delta_luma_weight_l0 = pwt.delta_luma_weight_l0;
            rec.luma_offset_l0 = pwt.luma_offset_l0;
            rec.delta_chroma_weight_l0 = pwt.delta_chroma_weight_l0;
            rec.delta_luma_weight_l1 = pwt.delta_luma_weight_l1;
            rec.luma_offset_l1 = pwt.luma_offset_l1;
            rec.delta_chroma_weight_l1 = pwt.delta_chroma_weight_l1;

            // libva wants derived ChromaOffsetLX; the parser stores the coded
            // delta. Clamp the shift so a malformed denominator cannot panic.
            let denom = (i32::from(pwt.luma_log2_weight_denom)
                + i32::from(pwt.delta_chroma_log2_weight_denom))
            .clamp(0, 7);
            let half_range = if sps.range_extension.high_precision_offsets_enabled_flag {
                1i32 << (i32::from(sps.bit_depth_chroma_minus8) + 8 - 1)
            } else {
                128
            };
            rec.chroma_offset_l0 = chroma_offsets(
                &pwt.delta_chroma_weight_l0,
                &pwt.delta_chroma_offset_l0,
                denom,
                half_range,
            );
            rec.chroma_offset_l1 = chroma_offsets(
                &pwt.delta_chroma_weight_l1,
                &pwt.delta_chroma_offset_l1,
                denom,
                half_range,
            );
        }

        slices.push(rec);
    }

    // Slot mutations only after every fallible step has passed.
    let (setup_slot, _) = slots.commit_setup(setup_id, &plan.dpb.removed, Removals::ReleaseNow)?;
    let pic_params = VaPictureParameterBufferHEVC {
        curr_pic: VaPictureHEVC {
            picture_id: setup_surface,
            pic_order_cnt: pic.pic_order_cnt,
            flags: 0,
            va_reserved: [0; 4],
        },
        reference_frames,
        pic_width_in_luma_samples: sps.pic_width_in_luma_samples,
        pic_height_in_luma_samples: sps.pic_height_in_luma_samples,
        pic_fields: PicFieldsH265 {
            chroma_format_idc: sps.chroma_format_idc,
            separate_colour_plane_flag: sps.separate_colour_plane_flag,
            pcm_enabled_flag: sps.pcm_enabled_flag,
            scaling_list_enabled_flag: sps.scaling_list_enabled_flag,
            transform_skip_enabled_flag: pps.transform_skip_enabled_flag,
            amp_enabled_flag: sps.amp_enabled_flag,
            strong_intra_smoothing_enabled_flag: sps.strong_intra_smoothing_enabled_flag,
            sign_data_hiding_enabled_flag: pps.sign_data_hiding_enabled_flag,
            constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
            cu_qp_delta_enabled_flag: pps.cu_qp_delta_enabled_flag,
            weighted_pred_flag: pps.weighted_pred_flag,
            weighted_bipred_flag: pps.weighted_bipred_flag,
            transquant_bypass_enabled_flag: pps.transquant_bypass_enabled_flag,
            tiles_enabled_flag: pps.tiles_enabled_flag,
            entropy_coding_sync_enabled_flag: pps.entropy_coding_sync_enabled_flag,
            pps_loop_filter_across_slices_enabled_flag: pps.loop_filter_across_slices_enabled_flag,
            loop_filter_across_tiles_enabled_flag: pps.loop_filter_across_tiles_enabled_flag,
            pcm_loop_filter_disabled_flag: sps.pcm_loop_filter_disabled_flag,
            // Derived hints. A false "no reordering" is a correctness bug, not a
            // slow path, so leave both 0.
            no_pic_reordering_flag: false,
            no_bi_pred_flag: false,
        }
        .pack(),
        sps_max_dec_pic_buffering_minus1: sps.max_dec_pic_buffering_minus1
            [usize::from(sps.max_sub_layers_minus1)],
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
        pcm_sample_bit_depth_luma_minus1: sps.pcm_sample_bit_depth_luma_minus1,
        pcm_sample_bit_depth_chroma_minus1: sps.pcm_sample_bit_depth_chroma_minus1,
        log2_min_luma_coding_block_size_minus3: sps.log2_min_luma_coding_block_size_minus3,
        log2_diff_max_min_luma_coding_block_size: sps.log2_diff_max_min_luma_coding_block_size,
        log2_min_transform_block_size_minus2: sps.log2_min_luma_transform_block_size_minus2,
        log2_diff_max_min_transform_block_size: sps.log2_diff_max_min_luma_transform_block_size,
        log2_min_pcm_luma_coding_block_size_minus3: sps.log2_min_pcm_luma_coding_block_size_minus3,
        log2_diff_max_min_pcm_luma_coding_block_size: sps
            .log2_diff_max_min_pcm_luma_coding_block_size,
        max_transform_hierarchy_depth_intra: sps.max_transform_hierarchy_depth_intra,
        max_transform_hierarchy_depth_inter: sps.max_transform_hierarchy_depth_inter,
        init_qp_minus26: pps.init_qp_minus26,
        diff_cu_qp_delta_depth: pps.diff_cu_qp_delta_depth,
        pps_cb_qp_offset: pps.cb_qp_offset,
        pps_cr_qp_offset: pps.cr_qp_offset,
        log2_parallel_merge_level_minus2: pps.log2_parallel_merge_level_minus2,
        num_tile_columns_minus1: pps.num_tile_columns_minus1,
        num_tile_rows_minus1: pps.num_tile_rows_minus1,
        column_width_minus1: narrow_19(&pps.column_width_minus1),
        row_height_minus1: narrow_21(&pps.row_height_minus1),
        slice_parsing_fields: SliceParsingFieldsH265 {
            lists_modification_present_flag: pps.lists_modification_present_flag,
            long_term_ref_pics_present_flag: sps.long_term_ref_pics_present_flag,
            sps_temporal_mvp_enabled_flag: sps.temporal_mvp_enabled_flag,
            cabac_init_present_flag: pps.cabac_init_present_flag,
            output_flag_present_flag: pps.output_flag_present_flag,
            dependent_slice_segments_enabled_flag: pps.dependent_slice_segments_enabled_flag,
            pps_slice_chroma_qp_offsets_present_flag: pps.slice_chroma_qp_offsets_present_flag,
            sample_adaptive_offset_enabled_flag: sps.sample_adaptive_offset_enabled_flag,
            deblocking_filter_override_enabled_flag: pps.deblocking_filter_override_enabled_flag,
            pps_disable_deblocking_filter_flag: pps.deblocking_filter_disabled_flag,
            slice_segment_header_extension_present_flag: pps
                .slice_segment_header_extension_present_flag,
            rap_pic_flag: pic.is_irap,
            idr_pic_flag: pic.is_idr,
            // IRAP ⇒ intra; the envelope does not code an intra-only non-IRAP.
            intra_pic_flag: pic.is_irap,
        }
        .pack(),
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        num_short_term_ref_pic_sets: sps.num_short_term_ref_pic_sets,
        num_long_term_ref_pic_sps: sps.num_long_term_ref_pics_sps,
        num_ref_idx_l0_default_active_minus1: pps.num_ref_idx_l0_default_active_minus1,
        num_ref_idx_l1_default_active_minus1: pps.num_ref_idx_l1_default_active_minus1,
        pps_beta_offset_div2: pps.beta_offset_div2,
        pps_tc_offset_div2: pps.tc_offset_div2,
        num_extra_slice_header_bits: pps.num_extra_slice_header_bits,
        st_rps_bits: pic.short_term_ref_pic_set_size_bits,
        va_reserved: [0; 8],
    };

    // No buffer at all when scaling lists are off — a table of parser zeros
    // would dequantise to zero.
    // libva takes the DC VALUE, the parser stores `minus8`. Clamped, not wrapped, as DXVA
    // and V4L2 do: the parser bounds the coded value, so the clamp is unreachable.
    let dc = |minus8: i16| (minus8 + 8).clamp(0, 255) as u8;
    let iq_matrix = plan.active_scaling_lists().map(|sl| VaIqMatrixBufferHEVC {
        scaling_list4x4: sl.scaling_list_4x4,
        scaling_list8x8: sl.scaling_list_8x8,
        scaling_list16x16: sl.scaling_list_16x16,
        // Only matrixIds 0 and 3 exist at 32x32; the parser keeps six slots.
        scaling_list32x32: [sl.scaling_list_32x32[0], sl.scaling_list_32x32[3]],
        scaling_list_dc16x16: sl.scaling_list_dc_coef_minus8_16x16.map(dc),
        scaling_list_dc32x32: [
            dc(sl.scaling_list_dc_coef_minus8_32x32[0]),
            dc(sl.scaling_list_dc_coef_minus8_32x32[3]),
        ],
        va_reserved: [0; 4],
    });

    Ok(DecodePlanVaH265 {
        pic_params,
        iq_matrix,
        slices,
        slice_data,
        setup_slot,
    })
}

/// `ChromaOffsetLX` per equation 7-56.
///
/// libva wants the derived offset; the parser stores coded
/// `delta_chroma_offset_lX`. A straight copy would tint every weighted block.
fn chroma_offsets(
    delta_weight: &[[i8; 2]; 15],
    delta_offset: &[[i16; 2]; 15],
    chroma_log2_weight_denom: i32,
    half_range: i32,
) -> [[i8; 2]; 15] {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            let weight = (1i32 << chroma_log2_weight_denom) + i32::from(delta_weight[i][j]);
            let offset = half_range + i32::from(delta_offset[i][j])
                - ((half_range * weight) >> chroma_log2_weight_denom);
            offset.clamp(-half_range, half_range - 1).clamp(-128, 127) as i8
        })
    })
}

/// Parser `u32` → libva `u16`. Saturate: a wrap would invent a tiny last tile;
/// 65535 columns is not a real picture.
///
/// Slice, not `[u32; N]`: libva's 19/21 are frozen ABI (`offset_of!` in `va_h265`);
/// the parser stores one extra remainder at `[num_tile_*_minus1]`. Copy the first
/// 19/21 — libva derives the last tile — so a longer parser array cannot break this.
fn narrow_19(src: &[u32]) -> [u16; 19] {
    std::array::from_fn(|i| u16::try_from(src.get(i).copied().unwrap_or(0)).unwrap_or(u16::MAX))
}

fn narrow_21(src: &[u32]) -> [u16; 21] {
    std::array::from_fn(|i| u16::try_from(src.get(i).copied().unwrap_or(0)).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use pf_bitstream::testing::split_h265_aus;

    use super::*;
    use crate::va_h265::REF_PIC_LIST_UNUSED;
    use crate::va_h265::VA_PICTURE_HEVC_INVALID;

    const SURFACE_BASE: u32 = 0xa000;

    const TEST_25FPS_H265: &[u8] = pf_bitstream::testing::H265_25FPS;
    const TEST_MAIN10_H265: &[u8] = include_bytes!("../../pf-vkdecode/tests/data/test-main10.h265");

    fn walk(stream: &[u8], expect_aus: usize, label: &str) {
        use pf_bitstream::h265::H265Planner;

        let aus = split_h265_aus(stream);
        assert_eq!(aus.len(), expect_aus, "{label}: access-unit count");

        let mut planner = H265Planner::new();
        // One never-reused surface id per picture, bound to its slot after
        // conversion returns — the caller's model, same as the H.264 twin.
        let mut surfaces: Vec<u32> = Vec::new();
        let mut slots: Option<SlotMap> = None;
        let mut saw_rps_flags = false;
        let mut saw_list_entries = false;

        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("{label} AU {index}: must plan, got {e:?}"));
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            surfaces.resize(map.capacity(), crate::va::VA_INVALID_SURFACE);
            let setup_surface = SURFACE_BASE + index as u32;
            let out = plan_to_va_h265(&plan, au, map, &surfaces, setup_surface)
                .unwrap_or_else(|e| panic!("{label} AU {index}: conversion failed: {e}"));
            surfaces[usize::from(out.setup_slot)] = setup_surface;

            assert_eq!(out.slices.len(), plan.slices.len());
            for (n, (rec, range)) in out.slices.iter().zip(&out.slice_data).enumerate() {
                assert!(range.end <= au.len() && range.start < range.end);
                assert_eq!(rec.slice_data_size as usize, range.end - range.start);
                assert_ne!(
                    &au[range.start..range.start + 3.min(range.end - range.start)],
                    &[0x00, 0x00, 0x01][..],
                    "{label} AU {index} slice {n}: start code not trimmed"
                );
                assert!(rec.slice_data_byte_offset > 0);
                assert!((rec.slice_data_byte_offset as usize) < range.end - range.start);
                // Used list entries are indices into ReferenceFrames. A stale 0xff
                // or an out-of-range index is a silent wrong reference, not a refusal.
                for list in &rec.ref_pic_list {
                    for &idx in list.iter().filter(|&&i| i != REF_PIC_LIST_UNUSED) {
                        saw_list_entries = true;
                        let e = out.pic_params.reference_frames[usize::from(idx)];
                        assert_eq!(
                            e.flags & VA_PICTURE_HEVC_INVALID,
                            0,
                            "{label} AU {index}: a list entry indexes an invalid DPB slot"
                        );
                    }
                }
            }

            let valid = out
                .pic_params
                .reference_frames
                .iter()
                .filter(|e| e.flags & VA_PICTURE_HEVC_INVALID == 0)
                .count();
            assert_eq!(valid, plan.dpb_refs.len(), "{label} AU {index}: DPB count");

            let rps_marked = out
                .pic_params
                .reference_frames
                .iter()
                .filter(|e| {
                    e.flags
                        & (VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
                            | VA_PICTURE_HEVC_RPS_ST_CURR_AFTER
                            | VA_PICTURE_HEVC_RPS_LT_CURR)
                        != 0
                })
                .count();
            assert_eq!(
                rps_marked,
                plan.rps.st_curr_before.len()
                    + plan.rps.st_curr_after.len()
                    + plan.rps.lt_curr.len(),
                "{label} AU {index}: exactly the current sets carry RPS flags"
            );
            saw_rps_flags |= rps_marked > 0;
            assert_eq!(
                out.pic_params.curr_pic.pic_order_cnt,
                plan.picture.pic_order_cnt
            );
        }
        assert!(
            saw_rps_flags,
            "{label}: no picture ever carried an RPS flag"
        );
        assert!(saw_list_entries, "{label}: no slice ever named a reference");
    }

    #[test]
    fn the_whole_vendored_vector_converts() {
        walk(TEST_25FPS_H265, 250, "8-bit");
    }

    /// Main 10 as well: pic-params carry bit depth, and an 8-bit-only walk would
    /// not notice a depth field wired to a constant.
    #[test]
    fn the_main10_vector_converts() {
        walk(TEST_MAIN10_H265, 50, "Main 10");
    }
}
