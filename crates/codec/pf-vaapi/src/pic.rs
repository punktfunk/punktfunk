//! One [`AuPlan`] into the libva buffers a `vaRenderPicture` call carries:
//! picture parameters, inverse-quantization matrices, and one slice-parameter
//! record per slice.
//!
//! Same transaction as `pf-dxvadec`'s `pic`. Envelope and capacity first
//! (read-only); references resolve against the PRE-removal map (this AU's
//! marking can evict a picture its slices still name); `removed` is applied,
//! then the setup slot last. A half-applied DPB update is a corrupt reference.
//!
//! VAAPI extras: `slice_data_bit_offset` from the NAL header byte
//! (`SliceHeader::header_bit_size`); slice data without its 3-or-4-byte start
//! code; per-slice `RefPicList0`/`1` in 8.2.4.2 order.
//!
//! `reference_frames` is the marked DPB (`dpb_refs`), like DXVA's
//! `RefFrameList` and unlike Vulkan's `pReferenceSlots`. Slice lists come
//! from the slice. Evidence: tests in this module.

use std::ops::Range;

use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::RefPic;

use crate::va::PicFieldsH264;
use crate::va::SeqFieldsH264;
use crate::va::VaIqMatrixBufferH264;
use crate::va::VaPictureH264;
use crate::va::VaPictureParameterBufferH264;
use crate::va::VaSliceParameterBufferH264;
use crate::va::VA_PICTURE_H264_LONG_TERM_REFERENCE;
use crate::va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
use crate::va::VA_SLICE_DATA_FLAG_ALL;
use crate::SlotError;
use crate::SlotMap;
use pf_bitstream::slots::Removals;

/// H.264 DPB ceiling and `reference_frames` length. Overflow is a malformed
/// plan, not an expressiveness limit.
pub const REFERENCE_FRAMES_LEN: usize = 16;

pub const REF_PIC_LIST_LEN: usize = 32;

/// One `vaBeginPicture` / `vaRenderPicture` / `vaEndPicture` call.
#[derive(Debug, Clone)]
pub struct DecodePlanVa {
    pub pic_params: VaPictureParameterBufferH264,
    pub iq_matrix: VaIqMatrixBufferH264,
    pub slices: Vec<VaSliceParameterBufferH264>,
    /// Slice payload, start code stripped. What `VASliceDataBuffer` carries.
    pub slice_data: Vec<Range<usize>>,
    /// DPB slot this picture decodes into. Indexes the caller's surface table.
    pub setup_slot: u8,
}

/// Features this backend does not submit, or a caller/slot-map mismatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVaError {
    NoSlices,
    NoStoredId,
    /// FMO. libva still has the fields; no driver implements them.
    SliceGroups {
        count: u32,
    },
    SeparateColourPlanes,
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    UnresolvedReference(PicId),
    TooManyReferences(usize),
    RefListTooLong {
        slice: usize,
        len: usize,
    },
    SurfaceOutOfRange {
        slot: u8,
        surfaces: usize,
    },
    DimensionOverflow {
        width_mbs: u32,
        height_mbs: u32,
    },
    /// NAL range outside the AU: a plan paired with the wrong buffer.
    SliceRange {
        slice: usize,
    },
    /// `header_bit_size` does not fit `slice_data_bit_offset`'s 16 bits. Refuse,
    /// do not truncate: a wrong bit offset decodes garbage.
    SliceBitOffsetOverflow {
        slice: usize,
        bits: usize,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVaError {
    fn from(e: SlotError) -> Self {
        PlanToVaError::Slot(e)
    }
}

impl std::fmt::Display for PlanToVaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVaError::NoSlices => write!(f, "the access unit planned no slices"),
            PlanToVaError::NoStoredId => write!(f, "the plan stored no picture id"),
            PlanToVaError::SliceGroups { count } => {
                write!(f, "slice groups (FMO) are outside the envelope: {count}")
            }
            PlanToVaError::SeparateColourPlanes => {
                write!(f, "separate colour planes are outside the envelope")
            }
            PlanToVaError::CapacityMismatch { required, capacity } => write!(
                f,
                "the slot map holds {capacity} slots, this stream needs {required}"
            ),
            PlanToVaError::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToVaError::TooManyReferences(n) => {
                write!(f, "{n} marked references exceed reference_frames[16]")
            }
            PlanToVaError::RefListTooLong { slice, len } => {
                write!(f, "slice {slice}: reference list of {len} exceeds 32")
            }
            PlanToVaError::SurfaceOutOfRange { slot, surfaces } => {
                write!(f, "DPB slot {slot} has no surface in a table of {surfaces}")
            }
            PlanToVaError::DimensionOverflow {
                width_mbs,
                height_mbs,
            } => write!(
                f,
                "picture of {width_mbs}x{height_mbs} macroblocks is too large"
            ),
            PlanToVaError::SliceRange { slice } => {
                write!(f, "slice {slice}: byte range lies outside the access unit")
            }
            PlanToVaError::SliceBitOffsetOverflow { slice, bits } => {
                write!(
                    f,
                    "slice {slice}: header of {bits} bits exceeds 16-bit offset"
                )
            }
            PlanToVaError::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVaError {}

fn va_ref(rp: &RefPic, surface: u32) -> VaPictureH264 {
    VaPictureH264 {
        picture_id: surface,
        frame_idx: u32::from(rp.frame_num_or_lt_idx),
        flags: if rp.is_long_term {
            VA_PICTURE_H264_LONG_TERM_REFERENCE
        } else {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        },
        top_field_order_cnt: rp.top_field_order_cnt,
        bottom_field_order_cnt: rp.bottom_field_order_cnt,
        va_reserved: [0; 4],
    }
}

fn surface_of(slots: &SlotMap, surfaces: &[u32], id: PicId) -> Result<(u8, u32), PlanToVaError> {
    let slot = slots
        .slot_of(id)
        .ok_or(PlanToVaError::UnresolvedReference(id))?;
    let surface = *surfaces
        .get(usize::from(slot))
        .ok_or(PlanToVaError::SurfaceOutOfRange {
            slot,
            surfaces: surfaces.len(),
        })?;
    Ok((slot, surface))
}

/// Convert one planned access unit.
///
/// `au` is the bitstream the plan was built from. Slice data starts at the
/// NAL header after a three- or four-byte start code. `surfaces` maps DPB
/// slot to `VASurfaceID` for pictures the DPB already holds; this crate
/// never allocates one.
///
/// The decode target is a parameter, not `surfaces[setup_slot]`.
/// [`SlotMap::assign`] takes the lowest free slot, so a slot this AU just
/// freed is free when setup takes it. Reading the target from a slot-indexed
/// table would decode into the surface the consumer may still be sampling.
/// After a successful call the caller binds `setup_surface` to
/// [`DecodePlanVa::setup_slot`].
///
/// Nothing mutates `slots` until every fallible step has passed.
pub fn plan_to_va(
    plan: &AuPlan,
    au: &[u8],
    slots: &mut SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<DecodePlanVa, PlanToVaError> {
    if plan.slices.is_empty() {
        return Err(PlanToVaError::NoSlices);
    }
    let setup_id = plan.dpb.stored.ok_or(PlanToVaError::NoStoredId)?;
    let sps = &plan.sps;
    let pps = &plan.pps;
    let pic = &plan.picture;

    if pps.num_slice_groups_minus1 != 0 {
        return Err(PlanToVaError::SliceGroups {
            count: pps.num_slice_groups_minus1 + 1,
        });
    }
    if sps.separate_colour_plane_flag {
        return Err(PlanToVaError::SeparateColourPlanes);
    }

    let required = pic.max_dpb_frames + 1;
    if slots.capacity() != required {
        return Err(PlanToVaError::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }
    // Capacity, not the chosen slot: the caller binds `setup_surface` after
    // the call, so this check runs before the ledger mutates.
    if surfaces.len() < slots.capacity() {
        return Err(PlanToVaError::SurfaceOutOfRange {
            slot: (slots.capacity() - 1) as u8,
            surfaces: surfaces.len(),
        });
    }

    // Height is FRAME macroblocks. Map-units doubles when `frame_mbs_only_flag`
    // is 0; the progressive envelope never takes that branch.
    let width_mbs = u32::from(sps.pic_width_in_mbs_minus1) + 1;
    let height_mbs = (u32::from(sps.pic_height_in_map_units_minus1) + 1)
        * (2 - u32::from(sps.frame_mbs_only_flag));
    let (Ok(width_minus1), Ok(height_minus1)) = (
        u16::try_from(width_mbs.saturating_sub(1)),
        u16::try_from(height_mbs.saturating_sub(1)),
    ) else {
        return Err(PlanToVaError::DimensionOverflow {
            width_mbs,
            height_mbs,
        });
    };

    // `reference_frames` is the marked DPB, not this AU.
    if plan.dpb_refs.len() > REFERENCE_FRAMES_LEN {
        return Err(PlanToVaError::TooManyReferences(plan.dpb_refs.len()));
    }
    let mut reference_frames = [VaPictureH264::invalid(); REFERENCE_FRAMES_LEN];
    for (slot_out, rp) in reference_frames.iter_mut().zip(&plan.dpb_refs) {
        let (_, surface) = surface_of(slots, surfaces, rp.id)?;
        *slot_out = va_ref(rp, surface);
    }

    let mut slices = Vec::with_capacity(plan.slices.len());
    let mut slice_data = Vec::with_capacity(plan.slices.len());
    for (index, sp) in plan.slices.iter().enumerate() {
        let hdr = &sp.header;
        let mut rec = VaSliceParameterBufferH264::zeroed();

        // VAAPI takes the NAL without its start code.
        if au.get(sp.nal.clone()).is_none() {
            return Err(PlanToVaError::SliceRange { slice: index });
        }
        let payload = sp.nal.clone();
        rec.slice_data_size = (payload.end - payload.start) as u32;
        rec.slice_data_offset = 0;
        rec.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
        rec.slice_data_bit_offset = u16::try_from(hdr.header_bit_size).map_err(|_| {
            PlanToVaError::SliceBitOffsetOverflow {
                slice: index,
                bits: hdr.header_bit_size,
            }
        })?;
        slice_data.push(payload);

        rec.first_mb_in_slice = hdr.first_mb_in_slice as u16;
        rec.slice_type = hdr.slice_type as u8;
        rec.direct_spatial_mv_pred_flag = u8::from(hdr.direct_spatial_mv_pred_flag);
        rec.num_ref_idx_l0_active_minus1 = hdr.num_ref_idx_l0_active_minus1;
        rec.num_ref_idx_l1_active_minus1 = hdr.num_ref_idx_l1_active_minus1;
        rec.cabac_init_idc = hdr.cabac_init_idc;
        rec.slice_qp_delta = hdr.slice_qp_delta;
        rec.disable_deblocking_filter_idc = hdr.disable_deblocking_filter_idc;
        rec.slice_alpha_c0_offset_div2 = hdr.slice_alpha_c0_offset_div2;
        rec.slice_beta_offset_div2 = hdr.slice_beta_offset_div2;

        for (list_out, list_in) in [
            (&mut rec.ref_pic_list0, &sp.ref_list0),
            (&mut rec.ref_pic_list1, &sp.ref_list1),
        ] {
            if list_in.len() > REF_PIC_LIST_LEN {
                return Err(PlanToVaError::RefListTooLong {
                    slice: index,
                    len: list_in.len(),
                });
            }
            for (entry, rp) in list_out.iter_mut().zip(list_in) {
                // Snapshot is the authority for marking and pair-key. Concealment
                // can relabel a substitute short-term; the list entry is the fallback.
                let marked = plan.dpb_refs.iter().find(|d| d.id == rp.id);
                let (_, surface) = surface_of(slots, surfaces, rp.id)?;
                *entry = va_ref(marked.unwrap_or(rp), surface);
            }
        }

        // 7.3.3: L0 on a weighted P/SP slice; both lists when
        // `weighted_bipred_idc == 1` on a B slice. Else the driver sees parser defaults.
        let pwt = &hdr.pred_weight_table;
        let explicit_l0 = (pps.weighted_pred_flag
            && (hdr.slice_type.is_p() || hdr.slice_type.is_sp()))
            || (pps.weighted_bipred_idc == 1 && hdr.slice_type.is_b());
        let explicit_l1 = pps.weighted_bipred_idc == 1 && hdr.slice_type.is_b();
        if explicit_l0 || explicit_l1 {
            rec.luma_log2_weight_denom = pwt.luma_log2_weight_denom;
            rec.chroma_log2_weight_denom = pwt.chroma_log2_weight_denom;
        }
        if explicit_l0 {
            rec.luma_weight_l0_flag = 1;
            rec.chroma_weight_l0_flag = 1;
            rec.luma_weight_l0 = pwt.luma_weight_l0;
            // Parser stores L0 offsets as i8 and L1 as i16. libva wants i16 for both.
            for (out, v) in rec.luma_offset_l0.iter_mut().zip(pwt.luma_offset_l0) {
                *out = i16::from(v);
            }
            rec.chroma_weight_l0 = pwt.chroma_weight_l0;
            for (out, v) in rec.chroma_offset_l0.iter_mut().zip(pwt.chroma_offset_l0) {
                *out = [i16::from(v[0]), i16::from(v[1])];
            }
        }
        if explicit_l1 {
            rec.luma_weight_l1_flag = 1;
            rec.chroma_weight_l1_flag = 1;
            rec.luma_weight_l1 = pwt.luma_weight_l1;
            rec.luma_offset_l1 = pwt.luma_offset_l1;
            rec.chroma_weight_l1 = pwt.chroma_weight_l1;
            for (out, v) in rec.chroma_offset_l1.iter_mut().zip(pwt.chroma_offset_l1) {
                *out = [i16::from(v[0]), i16::from(v[1])];
            }
        }

        slices.push(rec);
    }

    let (setup_slot, _) = slots.commit_setup(setup_id, &plan.dpb.removed, Removals::ReleaseNow)?;

    let curr_pic = VaPictureH264 {
        picture_id: setup_surface,
        // Current picture: `frame_num`, not a long-term index.
        frame_idx: u32::from(pic.frame_num),
        flags: if pic.is_reference {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        } else {
            0
        },
        top_field_order_cnt: pic.top_field_order_cnt,
        bottom_field_order_cnt: pic.bottom_field_order_cnt,
        va_reserved: [0; 4],
    };

    let seq_fields = SeqFieldsH264 {
        chroma_format_idc: sps.chroma_format_idc,
        separate_colour_plane_flag: sps.separate_colour_plane_flag,
        gaps_in_frame_num_value_allowed_flag: sps.gaps_in_frame_num_value_allowed_flag,
        frame_mbs_only_flag: sps.frame_mbs_only_flag,
        mb_adaptive_frame_field_flag: sps.mb_adaptive_frame_field_flag,
        direct_8x8_inference_flag: sps.direct_8x8_inference_flag,
        // A.3.3.2 is a level constraint, not a picture flag. Drivers are
        // validated against libavcodec's VAAPI path, which leaves this 0.
        min_luma_bi_pred_size8x8: false,
        log2_max_frame_num_minus4: sps.log2_max_frame_num_minus4,
        pic_order_cnt_type: sps.pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb_minus4: sps.log2_max_pic_order_cnt_lsb_minus4,
        delta_pic_order_always_zero_flag: sps.delta_pic_order_always_zero_flag,
    };
    let pic_fields = PicFieldsH264 {
        entropy_coding_mode_flag: pps.entropy_coding_mode_flag,
        weighted_pred_flag: pps.weighted_pred_flag,
        weighted_bipred_idc: pps.weighted_bipred_idc,
        transform_8x8_mode_flag: pps.transform_8x8_mode_flag,
        // Progressive envelope: pf-bitstream rejects field coding before a plan.
        field_pic_flag: false,
        constrained_intra_pred_flag: pps.constrained_intra_pred_flag,
        pic_order_present_flag: pps.bottom_field_pic_order_in_frame_present_flag,
        deblocking_filter_control_present_flag: pps.deblocking_filter_control_present_flag,
        redundant_pic_cnt_present_flag: pps.redundant_pic_cnt_present_flag,
        reference_pic_flag: pic.is_reference,
    };

    let pic_params = VaPictureParameterBufferH264 {
        curr_pic,
        reference_frames,
        picture_width_in_mbs_minus1: width_minus1,
        picture_height_in_mbs_minus1: height_minus1,
        bit_depth_luma_minus8: pic.bit_depth_luma_minus8,
        bit_depth_chroma_minus8: pic.bit_depth_chroma_minus8,
        num_ref_frames: sps.max_num_ref_frames,
        seq_fields: seq_fields.pack(),
        num_slice_groups_minus1: 0,
        slice_group_map_type: 0,
        slice_group_change_rate_minus1: 0,
        pic_init_qp_minus26: pps.pic_init_qp_minus26,
        pic_init_qs_minus26: pps.pic_init_qs_minus26,
        chroma_qp_index_offset: pps.chroma_qp_index_offset,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset,
        pic_fields: pic_fields.pack(),
        frame_num: pic.frame_num,
        va_reserved: [0; 8],
    };

    // Effective lists: the parser already applied Table 7-2. No SPS/PPS merge here.
    let iq_matrix = VaIqMatrixBufferH264 {
        scaling_list4x4: pps.scaling_lists_4x4,
        // libva carries the two 8x8 lists a 4:2:0 stream uses. The parser keeps six.
        scaling_list8x8: [pps.scaling_lists_8x8[0], pps.scaling_lists_8x8[1]],
        va_reserved: [0; 4],
    };

    Ok(DecodePlanVa {
        pic_params,
        iq_matrix,
        slices,
        slice_data,
        setup_slot,
    })
}

#[cfg(test)]
mod tests {
    use pf_bitstream::testing::split_h264_aus;

    use super::*;
    use crate::va::VA_INVALID_SURFACE;

    /// `SURFACE_BASE + AU index`. Unique, never reused, far from slot indices.
    /// A mix-up is a wild value, not an off-by-one.
    const SURFACE_BASE: u32 = 0x9000;
    use crate::va::VA_PICTURE_H264_INVALID;

    /// Vendored 250-AU vector: two slices per picture, four IDRs, real
    /// reordering. Same stream the other rungs decode.
    const TEST_25FPS_H264: &[u8] = pf_bitstream::testing::H264_25FPS;

    /// Host output: 120 pictures, `max_num_ref_frames = max_dec_frame_buffering = 3`
    /// and `max_num_reorder_frames = 0`. DPB depth equals the reference count, so
    /// 8.2.5 unmark and C.4.5.3 bump land on the same picture in one AU.
    const LOWDELAY_640X480: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h264");

    /// Every AU of a real stream converts, and the fields a driver reads agree.
    /// A synthetic single-picture case never hits a mid-stream slot exhaust.
    #[test]
    fn the_whole_vendored_vector_converts() {
        use pf_bitstream::h264::H264Planner;

        let aus = split_h264_aus(TEST_25FPS_H264);
        assert_eq!(aus.len(), 250, "the vendored vector is 250 access units");

        let mut planner = H264Planner::new();
        let mut surfaces: Vec<u32> = Vec::new();
        let mut slots: Option<SlotMap> = None;
        let mut converted = 0usize;
        let mut saw_multi_slice = false;
        let mut saw_references = false;

        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            surfaces.resize(map.capacity(), VA_INVALID_SURFACE);
            let setup_surface = SURFACE_BASE + index as u32;
            let out = plan_to_va(&plan, au, map, &surfaces, setup_surface)
                .unwrap_or_else(|e| panic!("AU {index}: conversion failed: {e}"));
            surfaces[usize::from(out.setup_slot)] = setup_surface;

            assert_eq!(
                out.slices.len(),
                plan.slices.len(),
                "AU {index}: one record per slice"
            );
            assert_eq!(out.slice_data.len(), out.slices.len());
            saw_multi_slice |= out.slices.len() > 1;

            for (n, (rec, range)) in out.slices.iter().zip(&out.slice_data).enumerate() {
                assert!(
                    range.end <= au.len() && range.start < range.end,
                    "AU {index} slice {n}: range {range:?} is not inside a {}-byte AU",
                    au.len()
                );
                assert_eq!(
                    rec.slice_data_size as usize,
                    range.end - range.start,
                    "AU {index} slice {n}: declared size must match the range"
                );
                assert_ne!(
                    &au[range.start..range.start + 3.min(range.end - range.start)],
                    &[0x00, 0x00, 0x01][..],
                    "AU {index} slice {n}: the start code was not trimmed"
                );
                assert!(
                    rec.slice_data_bit_offset > 0,
                    "AU {index} slice {n}: a slice header cannot be zero bits"
                );
                assert!(
                    usize::from(rec.slice_data_bit_offset) < (range.end - range.start) * 8,
                    "AU {index} slice {n}: the header cannot outrun the slice"
                );
            }

            let valid = out
                .pic_params
                .reference_frames
                .iter()
                .filter(|e| e.flags & VA_PICTURE_H264_INVALID == 0)
                .count();
            assert_eq!(
                valid,
                plan.dpb_refs.len(),
                "AU {index}: reference_frames must carry the marked DPB and nothing else"
            );
            saw_references |= valid > 0;
            for e in out.pic_params.reference_frames.iter().take(valid) {
                assert!(
                    surfaces.contains(&e.picture_id),
                    "AU {index}: a reference names a surface outside the table"
                );
            }

            assert_eq!(out.pic_params.frame_num, plan.picture.frame_num);
            assert!(usize::from(out.setup_slot) < surfaces.len());
            converted += 1;
        }

        assert_eq!(converted, 250);
        assert!(
            saw_multi_slice,
            "this vector is two slices per picture — a run that never saw one is \
             splitting access units wrong"
        );
        assert!(
            saw_references,
            "a 250-frame vector must reference something"
        );
    }

    /// Counts from one walk of [`plan_to_va`]. Each field is access units,
    /// bounded by [`Self::converted`].
    #[derive(Debug, Default)]
    struct AliasWalk {
        converted: usize,
        /// Setup inherited a slot this AU just freed. [`SlotMap::assign`] takes
        /// the lowest free slot, so a slot-indexed target would be the surface
        /// just displayed.
        inherited_a_just_freed_slot: usize,
        /// `removed` and `dpb_refs` name the same picture: 8.2.5 unmark and
        /// C.4.5.3 bump in one AU. Aliasing precondition.
        removed_and_referenced: usize,
        /// Setup took the slot of a picture this AU still reads. The surface
        /// does not follow the slot; [`Self::aliased`] is the actual collision.
        setup_took_a_read_pictures_slot: usize,
        /// Decode target appears in `reference_frames` or a slice list. Must be zero.
        aliased: usize,
    }

    /// Walk `stream` through [`plan_to_va`] as the Linux rung binds: the decode
    /// target is not in the table handed to conversion, and enters it only after
    /// the call returns. `video_vaapi_native` walks the same path on a `Session`.
    fn walk_for_aliasing(stream: &[u8]) -> AliasWalk {
        use pf_bitstream::h264::H264Planner;

        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut table: Vec<u32> = Vec::new();
        let mut out = AliasWalk::default();

        for (index, au) in split_h264_aus(stream).into_iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: this stream must plan, got {e:?}"));
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            assert_eq!(
                map.capacity(),
                plan.picture.max_dpb_frames + 1,
                "AU {index}: neither stream renegotiates its DPB depth mid-walk"
            );
            table.resize(map.capacity(), VA_INVALID_SURFACE);

            if plan
                .dpb
                .removed
                .iter()
                .any(|id| plan.dpb_refs.iter().any(|r| r.id == *id))
            {
                out.removed_and_referenced += 1;
            }
            // Slots this AU's removals will free. Read before conversion applies them.
            let freed: Vec<u8> = plan
                .dpb
                .removed
                .iter()
                .filter_map(|id| map.slot_of(*id))
                .collect();

            // Unique id not already in the table. A collision would not test
            // whether conversion names a surface it was not handed.
            let setup_surface = SURFACE_BASE + index as u32;
            assert!(
                !table.contains(&setup_surface),
                "AU {index}: the model handed out a surface the table already names"
            );
            let displaced = table.clone();
            let converted = plan_to_va(&plan, au, map, &table, setup_surface)
                .unwrap_or_else(|e| panic!("AU {index}: conversion failed: {e}"));
            table[usize::from(converted.setup_slot)] = setup_surface;

            // `dpb_refs` is after this AU's marking. Per-slice lists are before it.
            let named: Vec<u32> = converted
                .pic_params
                .reference_frames
                .iter()
                .chain(
                    converted
                        .slices
                        .iter()
                        .flat_map(|s| s.ref_pic_list0.iter().chain(s.ref_pic_list1.iter())),
                )
                .filter(|e| e.flags & VA_PICTURE_H264_INVALID == 0)
                .map(|e| e.picture_id)
                .collect();

            if freed.contains(&converted.setup_slot) {
                out.inherited_a_just_freed_slot += 1;
            }
            let evicted_surface = displaced[usize::from(converted.setup_slot)];
            if evicted_surface != VA_INVALID_SURFACE && named.contains(&evicted_surface) {
                out.setup_took_a_read_pictures_slot += 1;
            }
            assert_eq!(
                converted.pic_params.curr_pic.picture_id, setup_surface,
                "AU {index}: the current picture must be the surface the caller bound"
            );
            if named.contains(&setup_surface) {
                out.aliased += 1;
            }
            out.converted += 1;
        }
        out
    }

    /// Setup routinely inherits a slot this AU just freed. That is why the
    /// decode target is a parameter, not `surfaces[setup_slot]`.
    #[test]
    fn the_setup_picture_routinely_inherits_a_just_freed_slot() {
        let walk = walk_for_aliasing(TEST_25FPS_H264);
        assert_eq!(walk.converted, 250);
        // Floor, not an exact count: a planner shift of one frame must not fail.
        assert!(
            walk.inherited_a_just_freed_slot > 200,
            "the setup picture inherited a just-freed slot on only {} of 250 access \
             units — the reason `setup_surface` is a parameter no longer holds, and the \
             documentation that cites it needs re-measuring",
            walk.inherited_a_just_freed_slot
        );
    }

    /// One AU must remove a picture still in `dpb_refs`. Low-delay H.264 with
    /// `max_num_ref_frames = max_dec_frame_buffering` and no reorder is what
    /// makes them coincide. The conformance vector never hits this shape.
    #[test]
    fn the_low_delay_stream_reassigns_slots_whose_pictures_it_still_reads() {
        let vector = walk_for_aliasing(TEST_25FPS_H264);
        assert_eq!(vector.converted, 250);
        assert!(
            vector.inherited_a_just_freed_slot > 0,
            "no access unit of the vendored vector reused a freed slot, so the zeroes \
             below would be empty for a reason that has nothing to do with the hazard"
        );
        assert_eq!(
            vector.removed_and_referenced, 0,
            "the vendored vector is supposed to be BLIND to this shape"
        );
        assert_eq!(
            vector.setup_took_a_read_pictures_slot, 0,
            "and therefore never to hand the setup picture a slot it still reads"
        );

        let lowdelay = walk_for_aliasing(LOWDELAY_640X480);
        assert_eq!(lowdelay.converted, 120);
        assert_eq!(
            lowdelay.removed_and_referenced, 117,
            "the low-delay stream must still exercise the aliasing precondition on \
             nearly every access unit — if this drops to zero the exemption below is no \
             longer being TESTED by anything, whatever else still passes"
        );
        assert_eq!(
            lowdelay.setup_took_a_read_pictures_slot, 117,
            "and the slot really is handed straight back to the decode target: this is \
             the D3D11VA/Vulkan defect, present here, and harmless only because the \
             SURFACE does not follow the slot"
        );
    }

    /// No submission names its decode target as a reference, on either stream.
    ///
    /// Conversion still releases `removed` inline. Safe because `plan_to_va`
    /// never invents a surface: every reference is read from the table it was
    /// handed, so a target not in that table cannot be named. The Linux rung
    /// picking the target from outside the table lives in `video_vaapi_native`.
    #[test]
    fn no_submission_names_its_decode_target_as_one_of_its_own_references() {
        for (name, walk) in [
            ("the vendored vector", walk_for_aliasing(TEST_25FPS_H264)),
            ("the low-delay stream", walk_for_aliasing(LOWDELAY_640X480)),
        ] {
            assert_eq!(
                walk.aliased, 0,
                "{name}: {} of {} access units decode into a surface they predict from",
                walk.aliased, walk.converted
            );
        }
    }

    /// Counterfactual: a target taken from the slot table aliases. Without this,
    /// `aliased == 0` could mean the conversion could never alias.
    #[test]
    fn taking_the_decode_target_from_the_slot_table_aliases_on_the_low_delay_stream() {
        use pf_bitstream::h264::H264Planner;

        let mut planner = H264Planner::new();
        let mut slots: Option<SlotMap> = None;
        let mut table: Vec<u32> = Vec::new();
        let (mut converted, mut aliased) = (0usize, 0usize);

        for (index, au) in split_h264_aus(LOWDELAY_640X480).into_iter().enumerate() {
            let plan = planner.plan_au(au).expect("the low-delay stream plans");
            let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
            table.resize(map.capacity(), VA_INVALID_SURFACE);
            // Peek the slot, then re-run with the target read from the table.
            // `SlotMap::assign` never consults the target to pick the slot.
            let mut probe = map.clone();
            let peek = plan_to_va(&plan, au, &mut probe, &table, SURFACE_BASE)
                .expect("the low-delay stream converts");
            let target = table[usize::from(peek.setup_slot)];
            let target = if target == VA_INVALID_SURFACE {
                SURFACE_BASE + index as u32
            } else {
                target
            };
            let out = plan_to_va(&plan, au, map, &table, target).expect("the same conversion");
            table[usize::from(out.setup_slot)] = target;

            let names = |e: &VaPictureH264| {
                e.flags & VA_PICTURE_H264_INVALID == 0 && e.picture_id == target
            };
            if out.pic_params.reference_frames.iter().any(names)
                || out
                    .slices
                    .iter()
                    .any(|s| s.ref_pic_list0.iter().any(names) || s.ref_pic_list1.iter().any(names))
            {
                aliased += 1;
            }
            converted += 1;
        }

        assert_eq!(converted, 120);
        assert_eq!(
            aliased, 117,
            "binding the decode target BY SLOT is supposed to reproduce the defect on \
             this stream; if it no longer does, the exemption test above is passing for \
             a reason nobody has checked"
        );
    }

    #[test]
    fn a_long_term_reference_is_flagged_long_term() {
        let rp = RefPic {
            id: 7,
            top_field_order_cnt: 4,
            bottom_field_order_cnt: 4,
            is_long_term: true,
            frame_num_or_lt_idx: 2,
        };
        let e = va_ref(&rp, 0x1234);
        assert_eq!(e.flags, VA_PICTURE_H264_LONG_TERM_REFERENCE);
        assert_eq!(e.picture_id, 0x1234);
        // Long-term: this field is `LongTermFrameIdx`, not `frame_num`.
        assert_eq!(e.frame_idx, 2);
    }

    #[test]
    fn a_short_term_reference_carries_its_frame_num() {
        let rp = RefPic {
            id: 3,
            top_field_order_cnt: -2,
            bottom_field_order_cnt: -2,
            is_long_term: false,
            frame_num_or_lt_idx: 9,
        };
        let e = va_ref(&rp, 5);
        assert_eq!(e.flags, VA_PICTURE_H264_SHORT_TERM_REFERENCE);
        assert_eq!(e.frame_idx, 9);
        assert_eq!(e.top_field_order_cnt, -2);
        assert_ne!(e.flags & VA_PICTURE_H264_INVALID, VA_PICTURE_H264_INVALID);
    }
}
