//! Parameter-set conversion: the vendored parser's [`Sps`]/[`Pps`] into the
//! `StdVideoH264*ParameterSet` structs a Vulkan Video session-parameters object
//! is created from (`vkCreateVideoSessionParametersKHR`).
//!
//! Std structs embed raw pointers (`pOffsetForRefFrame`, `pScalingLists`,
//! `pSequenceParameterSetVui`), so conversion returns owning wrappers — see
//! [`OwnedStdSps`] for the aliasing/lifetime contract. [`crate::session`] stores
//! the wrappers next to the parameters object; this module's move tests pin
//! that boxed backing keeps those addresses valid.
//!
//! VUI is not converted: a decode session consumes none of it (display, not
//! reconstruction), so `vui_parameters_present_flag` stays 0 and
//! `pSequenceParameterSetVui` stays null. Colour rides
//! [`pf_bitstream::h264::PicturePlan`] into the presenter.

use ash::vk::native as hh;
use cros_codecs::codec::h264::parser::Level;
pub use cros_codecs::codec::h264::parser::Pps;
pub use cros_codecs::codec::h264::parser::Sps;

/// A parameter set Vulkan Video cannot express. Reject; do not fill a Std
/// struct that silently drops the unrepresentable field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsError {
    /// No `StdVideoH264ProfileIdc` for this `profile_idc`. Vulkan has Baseline
    /// (66), Main (77), High (100), High 4:4:4 Predictive (244).
    UnmappableProfileIdc(u8),
    /// `chroma_format_idc` past 3 — not legal H.264.
    InvalidChromaFormatIdc(u8),
    /// `pic_order_cnt_type` past 2 — not legal H.264.
    InvalidPocType(u8),
    /// `weighted_bipred_idc` 3 fits the two-bit field but 7.4.2.2 forbids it,
    /// and no `StdVideoH264WeightedBipredIdc` code point exists.
    InvalidWeightedBipredIdc(u8),
    /// FMO (`num_slice_groups_minus1 > 0`): `StdVideoH264PictureParameterSet`
    /// has no slice-group fields — Vulkan Video cannot express it.
    SliceGroups(u32),
}

impl std::fmt::Display for ParamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamsError::UnmappableProfileIdc(idc) => {
                write!(
                    f,
                    "profile_idc {idc} has no StdVideoH264ProfileIdc code point"
                )
            }
            ParamsError::InvalidChromaFormatIdc(idc) => {
                write!(f, "invalid chroma_format_idc {idc}")
            }
            ParamsError::InvalidPocType(t) => write!(f, "invalid pic_order_cnt_type {t}"),
            ParamsError::InvalidWeightedBipredIdc(idc) => {
                write!(f, "invalid weighted_bipred_idc {idc}")
            }
            ParamsError::SliceGroups(n) => {
                write!(
                    f,
                    "FMO ({} slice groups) is not expressible in Vulkan Video",
                    n + 1
                )
            }
        }
    }
}

impl std::error::Error for ParamsError {}

/// Converted SPS plus the heap allocations its embedded pointers target.
///
/// `StdVideoH264SequenceParameterSet` points at data it does not contain
/// (`pOffsetForRefFrame`, `pScalingLists`). This wrapper owns those blocks.
///
/// Backing is boxed so the wrapper may move: a move relocates the `Box`
/// handles, never the heap the Std pointers address. The Std struct itself is
/// boxed for the same reason: `pStdSPSs` is [`Self::std`]'s address, and the
/// session hands it over before moving the wrapper into stored parameters
/// (`crate::session`'s `an_added_set_keeps_the_address_the_update_call_was_given`).
///
/// [`Self::std`] is `Copy`; a copy still points into this wrapper and must not
/// outlive it. The wrapper must outlive the parameters object — a driver may
/// retain an embedded pointer across `vkCmdDecodeVideoKHR` (`crate::session_av1`).
/// No mutation of the backing, so `*const` aliasing holds. Not `Clone`: a
/// derived clone would copy pointer values without the blocks. Re-convert.
#[derive(Debug)]
pub struct OwnedStdSps {
    /// Boxed so [`Self::std`]'s address — what `pStdSPSs` points at — survives
    /// every move of the wrapper.
    std: Box<hh::StdVideoH264SequenceParameterSet>,
    _offset_backing: Option<Box<[i32]>>,
    _scaling_backing: Option<Box<hh::StdVideoH264ScalingLists>>,
}

impl OwnedStdSps {
    /// The Std struct, valid while `self` lives. Do not let a `Copy` of it
    /// outlive the wrapper.
    pub fn std(&self) -> &hh::StdVideoH264SequenceParameterSet {
        &self.std
    }

    /// Lower `level_idc` to `max` when the stream declares a higher one. A set
    /// above the device's `maxLevelIdc` is invalid usage; coded extent and DPB
    /// depth already bound the stream. Runs before handover — the no-mutation
    /// contract is about a live object's blocks.
    pub(crate) fn clamp_level(&mut self, max: hh::StdVideoH264LevelIdc) {
        if self.std.level_idc > max {
            self.std.level_idc = max;
        }
    }
}

/// Converted PPS plus the scaling-list allocation its `pScalingLists` targets.
/// Same ownership contract as [`OwnedStdSps`].
#[derive(Debug)]
pub struct OwnedStdPps {
    /// Boxed for [`OwnedStdSps`]'s reason: `pStdPPSs` is this field's address.
    std: Box<hh::StdVideoH264PictureParameterSet>,
    _scaling_backing: Option<Box<hh::StdVideoH264ScalingLists>>,
}

impl OwnedStdPps {
    /// The Std struct, valid while `self` lives (see [`OwnedStdSps`]).
    pub fn std(&self) -> &hh::StdVideoH264PictureParameterSet {
        &self.std
    }
}

/// H.264 `level_idc` (value-coded: 10 ⇒ 1.0) to Vulkan's index-coded
/// `StdVideoH264LevelIdc`. Std code points ascend with the level, so
/// `maxLevelIdc` compares numerically.
pub(crate) const fn level_to_std(level: Level) -> hh::StdVideoH264LevelIdc {
    match level {
        Level::L1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_0,
        // Vulkan has no 1b code point. 1b is level_idc 11 plus constraint_set3_flag;
        // the flag is mapped, so 1.1 is the faithful cap.
        Level::L1B => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        Level::L1_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        Level::L1_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_2,
        Level::L1_3 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_3,
        Level::L2_0 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_0,
        Level::L2_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_1,
        Level::L2_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_2,
        Level::L3 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_0,
        Level::L3_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1,
        Level::L3_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2,
        Level::L4 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0,
        Level::L4_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1,
        Level::L4_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2,
        Level::L5 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0,
        Level::L5_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1,
        Level::L5_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2,
        Level::L6 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0,
        Level::L6_1 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1,
        Level::L6_2 => hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
    }
}

/// Pack resolved scaling lists into the Std layout.
///
/// The parser has already run 7.3.2.1.1.1 and Table 7-2. Declare every
/// resolved list present (`use_default_scaling_matrix_mask` 0) so the driver
/// infers nothing.
///
/// `num_8x8`: 2 for 4:2:0/4:2:2, 6 for 4:4:4, 0 when a PPS has no
/// `transform_8x8_mode_flag` (those 8x8 arrays are zeros — do not declare).
fn scaling_lists_to_std(
    lists_4x4: &[[u8; 16]; 6],
    lists_8x8: &[[u8; 64]; 6],
    num_8x8: u16,
) -> hh::StdVideoH264ScalingLists {
    // SAFETY: StdVideoH264ScalingLists is a plain-C bindgen struct of two u16 masks
    // and two byte arrays; the all-zero bit pattern is a valid value for every field.
    let mut std: hh::StdVideoH264ScalingLists = unsafe { std::mem::zeroed() };
    std.scaling_list_present_mask = 0x3F | (((1u16 << num_8x8) - 1) << 6);
    std.use_default_scaling_matrix_mask = 0;
    std.ScalingList4x4 = *lists_4x4;
    // All six 8x8 arrays are copied even when only two are declared present; the
    // driver ignores entries whose mask bit is clear.
    std.ScalingList8x8 = *lists_8x8;
    std
}

/// Convert one SPS into an owning Std wrapper. VUI is skipped (module docs).
pub fn sps_to_std(sps: &Sps) -> Result<OwnedStdSps, ParamsError> {
    // StdVideoH264ProfileIdc code points equal the profile_idc values they name.
    let profile_idc = match u32::from(sps.profile_idc) {
        p @ (66 | 77 | 100 | 244) => p,
        _ => return Err(ParamsError::UnmappableProfileIdc(sps.profile_idc)),
    };
    if sps.chroma_format_idc > 3 {
        return Err(ParamsError::InvalidChromaFormatIdc(sps.chroma_format_idc));
    }
    if sps.pic_order_cnt_type > 2 {
        return Err(ParamsError::InvalidPocType(sps.pic_order_cnt_type));
    }

    // SAFETY: StdVideoH264SequenceParameterSet is a plain-C bindgen struct of
    // integers, a bitfield word and const pointers; all-zero is valid for every
    // field (null for the pointers) and is the absent baseline the writes fill.
    let mut std: hh::StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };

    std.flags
        .set_constraint_set0_flag(u32::from(sps.constraint_set0_flag));
    std.flags
        .set_constraint_set1_flag(u32::from(sps.constraint_set1_flag));
    std.flags
        .set_constraint_set2_flag(u32::from(sps.constraint_set2_flag));
    std.flags
        .set_constraint_set3_flag(u32::from(sps.constraint_set3_flag));
    std.flags
        .set_constraint_set4_flag(u32::from(sps.constraint_set4_flag));
    std.flags
        .set_constraint_set5_flag(u32::from(sps.constraint_set5_flag));
    std.flags
        .set_direct_8x8_inference_flag(u32::from(sps.direct_8x8_inference_flag));
    std.flags
        .set_mb_adaptive_frame_field_flag(u32::from(sps.mb_adaptive_frame_field_flag));
    std.flags
        .set_frame_mbs_only_flag(u32::from(sps.frame_mbs_only_flag));
    std.flags
        .set_delta_pic_order_always_zero_flag(u32::from(sps.delta_pic_order_always_zero_flag));
    std.flags
        .set_separate_colour_plane_flag(u32::from(sps.separate_colour_plane_flag));
    std.flags
        .set_gaps_in_frame_num_value_allowed_flag(u32::from(
            sps.gaps_in_frame_num_value_allowed_flag,
        ));
    std.flags
        .set_qpprime_y_zero_transform_bypass_flag(u32::from(
            sps.qpprime_y_zero_transform_bypass_flag,
        ));
    std.flags
        .set_frame_cropping_flag(u32::from(sps.frame_cropping_flag));
    std.flags
        .set_seq_scaling_matrix_present_flag(u32::from(sps.seq_scaling_matrix_present_flag));
    // vui_parameters_present_flag stays 0: decode sessions consume no VUI (module docs).

    std.profile_idc = profile_idc;
    std.level_idc = level_to_std(sps.level_idc);
    std.chroma_format_idc = u32::from(sps.chroma_format_idc);
    std.seq_parameter_set_id = sps.seq_parameter_set_id;
    std.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
    std.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
    std.log2_max_frame_num_minus4 = sps.log2_max_frame_num_minus4;
    std.pic_order_cnt_type = u32::from(sps.pic_order_cnt_type);
    std.offset_for_non_ref_pic = sps.offset_for_non_ref_pic;
    std.offset_for_top_to_bottom_field = sps.offset_for_top_to_bottom_field;
    std.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
    std.max_num_ref_frames = sps.max_num_ref_frames;
    std.pic_width_in_mbs_minus1 = u32::from(sps.pic_width_in_mbs_minus1);
    std.pic_height_in_map_units_minus1 = u32::from(sps.pic_height_in_map_units_minus1);
    std.frame_crop_left_offset = sps.frame_crop_left_offset;
    std.frame_crop_right_offset = sps.frame_crop_right_offset;
    std.frame_crop_top_offset = sps.frame_crop_top_offset;
    std.frame_crop_bottom_offset = sps.frame_crop_bottom_offset;

    // POC type 1 only: box the cycle so the pointer survives moves. Count and
    // pointer come from this one condition — a stale cycle on type 0/2 must not
    // become a nonzero count over a null array (both stay zeroed).
    let offset_backing =
        (sps.pic_order_cnt_type == 1 && sps.num_ref_frames_in_pic_order_cnt_cycle > 0).then(|| {
            let cycle = usize::from(sps.num_ref_frames_in_pic_order_cnt_cycle);
            Box::<[i32]>::from(&sps.offset_for_ref_frame[..cycle])
        });
    if let Some(backing) = &offset_backing {
        std.num_ref_frames_in_pic_order_cnt_cycle = sps.num_ref_frames_in_pic_order_cnt_cycle;
        std.pOffsetForRefFrame = backing.as_ptr();
    }

    let scaling_backing = sps.seq_scaling_matrix_present_flag.then(|| {
        let num_8x8 = if sps.chroma_format_idc == 3 { 6 } else { 2 };
        Box::new(scaling_lists_to_std(
            &sps.scaling_lists_4x4,
            &sps.scaling_lists_8x8,
            num_8x8,
        ))
    });
    if let Some(backing) = &scaling_backing {
        std.pScalingLists = &**backing;
    }

    Ok(OwnedStdSps {
        std: Box::new(std),
        _offset_backing: offset_backing,
        _scaling_backing: scaling_backing,
    })
}

/// Convert one PPS into an owning Std wrapper.
///
/// `num_slice_groups_minus1` has no Std field; FMO is rejected rather than
/// converted into a struct that claims there is none.
pub fn pps_to_std(pps: &Pps) -> Result<OwnedStdPps, ParamsError> {
    if pps.num_slice_groups_minus1 != 0 {
        return Err(ParamsError::SliceGroups(pps.num_slice_groups_minus1));
    }
    if pps.weighted_bipred_idc > 2 {
        return Err(ParamsError::InvalidWeightedBipredIdc(
            pps.weighted_bipred_idc,
        ));
    }

    // SAFETY: StdVideoH264PictureParameterSet is a plain-C bindgen struct of
    // integers, a bitfield word and one const pointer; all-zero is valid for
    // every field (null for the pointer) and is the baseline the writes fill.
    let mut std: hh::StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };

    std.flags
        .set_transform_8x8_mode_flag(u32::from(pps.transform_8x8_mode_flag));
    std.flags
        .set_redundant_pic_cnt_present_flag(u32::from(pps.redundant_pic_cnt_present_flag));
    std.flags
        .set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred_flag));
    std.flags
        .set_deblocking_filter_control_present_flag(u32::from(
            pps.deblocking_filter_control_present_flag,
        ));
    std.flags
        .set_weighted_pred_flag(u32::from(pps.weighted_pred_flag));
    std.flags
        .set_bottom_field_pic_order_in_frame_present_flag(u32::from(
            pps.bottom_field_pic_order_in_frame_present_flag,
        ));
    std.flags
        .set_entropy_coding_mode_flag(u32::from(pps.entropy_coding_mode_flag));
    std.flags
        .set_pic_scaling_matrix_present_flag(u32::from(pps.pic_scaling_matrix_present_flag));

    std.seq_parameter_set_id = pps.seq_parameter_set_id;
    std.pic_parameter_set_id = pps.pic_parameter_set_id;
    std.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    std.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    std.weighted_bipred_idc = u32::from(pps.weighted_bipred_idc);
    std.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
    std.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
    std.chroma_qp_index_offset = pps.chroma_qp_index_offset;
    std.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;

    let scaling_backing = pps.pic_scaling_matrix_present_flag.then(|| {
        // Parser resolves PPS 8x8 lists only under `transform_8x8_mode_flag`
        // (7.3.2.2); without it the arrays are zeros and must not be declared.
        let num_8x8 = match (pps.transform_8x8_mode_flag, pps.sps.chroma_format_idc == 3) {
            (false, _) => 0,
            (true, false) => 2,
            (true, true) => 6,
        };
        Box::new(scaling_lists_to_std(
            &pps.scaling_lists_4x4,
            &pps.scaling_lists_8x8,
            num_8x8,
        ))
    });
    if let Some(backing) = &scaling_backing {
        std.pScalingLists = &**backing;
    }

    Ok(OwnedStdPps {
        std: Box::new(std),
        _scaling_backing: scaling_backing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS with distinct values on every mapped field. Flags follow Std bit
    /// order and strictly alternate false/true so a swap of two adjacent
    /// mappings fails. `vui_parameters_present_flag` is true at the source
    /// because conversion must not copy it.
    fn full_sps() -> Sps {
        Sps {
            seq_parameter_set_id: 3,
            profile_idc: 100,
            // Flags, Std bit order 0..15: F T F T F T F T F T F T F T F (T).
            constraint_set1_flag: true,
            constraint_set3_flag: true,
            constraint_set5_flag: true,
            mb_adaptive_frame_field_flag: true,
            delta_pic_order_always_zero_flag: true,
            gaps_in_frame_num_value_allowed_flag: true,
            frame_cropping_flag: true,
            vui_parameters_present_flag: true,
            // Remaining flags (constraint_set0/2/4, direct_8x8_inference,
            // frame_mbs_only, separate_colour_plane,
            // qpprime_y_zero_transform_bypass, seq_scaling_matrix_present)
            // stay false via ..Default.
            level_idc: Level::L4_1,
            chroma_format_idc: 1,
            bit_depth_luma_minus8: 2,
            bit_depth_chroma_minus8: 3,
            log2_max_frame_num_minus4: 5,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 6,
            offset_for_non_ref_pic: -7,
            offset_for_top_to_bottom_field: 3,
            max_num_ref_frames: 4,
            pic_width_in_mbs_minus1: 119,
            pic_height_in_map_units_minus1: 67,
            frame_crop_left_offset: 1,
            frame_crop_right_offset: 2,
            frame_crop_top_offset: 3,
            frame_crop_bottom_offset: 4,
            ..Default::default()
        }
    }

    /// PPS over `sps` with distinct values on every mapped field. Flags follow
    /// Std bit order and strictly alternate true/false so a swap fails.
    fn full_pps(sps: Sps) -> Pps {
        Pps {
            pic_parameter_set_id: 5,
            seq_parameter_set_id: 3,
            // Flags, Std bit order 0..7: T F T F T F T F.
            transform_8x8_mode_flag: true,
            redundant_pic_cnt_present_flag: false,
            constrained_intra_pred_flag: true,
            deblocking_filter_control_present_flag: false,
            weighted_pred_flag: true,
            bottom_field_pic_order_in_frame_present_flag: false,
            entropy_coding_mode_flag: true,
            pic_scaling_matrix_present_flag: false,
            num_slice_groups_minus1: 0,
            num_ref_idx_l0_default_active_minus1: 2,
            num_ref_idx_l1_default_active_minus1: 1,
            weighted_bipred_idc: 2,
            pic_init_qp_minus26: -3,
            pic_init_qs_minus26: 4,
            chroma_qp_index_offset: -2,
            scaling_lists_4x4: [[0; 16]; 6],
            scaling_lists_8x8: [[0; 64]; 6],
            second_chroma_qp_index_offset: 6,
            sps: std::rc::Rc::new(sps),
        }
    }

    #[test]
    fn every_mapped_sps_field_and_flag_round_trips_exactly() {
        let sps = full_sps();
        let owned = sps_to_std(&sps).unwrap();
        let std = owned.std();

        assert_eq!(std.flags.constraint_set0_flag(), 0);
        assert_eq!(std.flags.constraint_set1_flag(), 1);
        assert_eq!(std.flags.constraint_set2_flag(), 0);
        assert_eq!(std.flags.constraint_set3_flag(), 1);
        assert_eq!(std.flags.constraint_set4_flag(), 0);
        assert_eq!(std.flags.constraint_set5_flag(), 1);
        assert_eq!(std.flags.direct_8x8_inference_flag(), 0);
        assert_eq!(std.flags.mb_adaptive_frame_field_flag(), 1);
        assert_eq!(std.flags.frame_mbs_only_flag(), 0);
        assert_eq!(std.flags.delta_pic_order_always_zero_flag(), 1);
        assert_eq!(std.flags.separate_colour_plane_flag(), 0);
        assert_eq!(std.flags.gaps_in_frame_num_value_allowed_flag(), 1);
        assert_eq!(std.flags.qpprime_y_zero_transform_bypass_flag(), 0);
        assert_eq!(std.flags.frame_cropping_flag(), 1);
        assert_eq!(std.flags.seq_scaling_matrix_present_flag(), 0);
        assert_eq!(
            std.flags.vui_parameters_present_flag(),
            0,
            "true at the source, skipped by design"
        );

        assert_eq!(
            std.profile_idc,
            hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH
        );
        assert_eq!(
            std.level_idc,
            hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1
        );
        assert_eq!(
            std.chroma_format_idc,
            hh::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420
        );
        assert_eq!(std.seq_parameter_set_id, 3);
        assert_eq!(std.bit_depth_luma_minus8, 2);
        assert_eq!(std.bit_depth_chroma_minus8, 3, "distinct from luma");
        assert_eq!(std.log2_max_frame_num_minus4, 5);
        assert_eq!(
            std.pic_order_cnt_type,
            hh::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_0
        );
        assert_eq!(std.offset_for_non_ref_pic, -7);
        assert_eq!(std.offset_for_top_to_bottom_field, 3);
        assert_eq!(std.log2_max_pic_order_cnt_lsb_minus4, 6);
        assert_eq!(std.num_ref_frames_in_pic_order_cnt_cycle, 0);
        assert_eq!(std.max_num_ref_frames, 4);
        assert_eq!(std.pic_width_in_mbs_minus1, 119);
        assert_eq!(std.pic_height_in_map_units_minus1, 67);
        assert_eq!(std.frame_crop_left_offset, 1);
        assert_eq!(std.frame_crop_right_offset, 2);
        assert_eq!(std.frame_crop_top_offset, 3);
        assert_eq!(std.frame_crop_bottom_offset, 4);

        assert!(
            std.pOffsetForRefFrame.is_null(),
            "POC type 0 carries no offset array"
        );
        assert!(std.pScalingLists.is_null());
        assert!(std.pSequenceParameterSetVui.is_null());
    }

    #[test]
    fn poc_type_1_offsets_are_owned_and_survive_moving_the_wrapper() {
        let mut sps = full_sps();
        sps.pic_order_cnt_type = 1;
        sps.num_ref_frames_in_pic_order_cnt_cycle = 3;
        sps.offset_for_ref_frame[0] = 2;
        sps.offset_for_ref_frame[1] = -1;
        sps.offset_for_ref_frame[2] = 4;

        // Box after conversion: relocating the wrapper must not invalidate the
        // pointer — the backing is heap-pinned.
        let owned = Box::new(sps_to_std(&sps).unwrap());
        let std = owned.std();
        assert_eq!(
            std.pic_order_cnt_type,
            hh::StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_1
        );
        assert_eq!(std.num_ref_frames_in_pic_order_cnt_cycle, 3);
        assert!(!std.pOffsetForRefFrame.is_null());
        // SAFETY: pOffsetForRefFrame points into `owned`'s boxed backing of exactly
        // num_ref_frames_in_pic_order_cnt_cycle i32s, alive for this whole scope.
        let offsets = unsafe { std::slice::from_raw_parts(std.pOffsetForRefFrame, 3) };
        assert_eq!(offsets, [2, -1, 4]);
    }

    /// Overwrite the stack the conversion frames just used.
    ///
    /// Move tests discriminate by reading the block back. Pointer equality cannot:
    /// the Std struct copies pointer values, so a stale one still compares equal.
    /// After this runs, a pointer into a dead local reads `0xA5` rather than its
    /// old contents.
    #[inline(never)]
    fn clobber_the_dead_stack() {
        let mut scratch = [0xA5u8; 16 * 1024];
        std::hint::black_box(&mut scratch);
    }

    /// Wrappers may move — into stored parameters, out of a `Result`, through a
    /// `Vec` realloc — without changing the addresses a driver already holds.
    ///
    /// Every backing is boxed. Inlining one as a field would keep the rest of
    /// this crate green and hand the driver a pointer into a moved-from stack
    /// slot. `params_av1` and `params_h265` pin the same contract.
    #[test]
    fn moving_the_wrapper_leaves_the_driver_s_pointers_put() {
        // SPS carrying both embedded pointers. `Sps` is not `Clone`, so this is a
        // builder rather than a value.
        let pointer_bearing_sps = || {
            let mut sps = full_sps();
            sps.pic_order_cnt_type = 1;
            sps.num_ref_frames_in_pic_order_cnt_cycle = 3;
            sps.offset_for_ref_frame[0] = 2;
            sps.offset_for_ref_frame[1] = -1;
            sps.offset_for_ref_frame[2] = 4;
            sps.seq_scaling_matrix_present_flag = true;
            sps.scaling_lists_4x4 = std::array::from_fn(|i| [10 + i as u8; 16]);
            sps
        };
        let mut pps = full_pps(pointer_bearing_sps());
        pps.pic_scaling_matrix_present_flag = true;
        pps.scaling_lists_4x4 = std::array::from_fn(|i| [60 + i as u8; 16]);

        let owned_sps = sps_to_std(&pointer_bearing_sps()).expect("a High-profile SPS converts");
        let owned_pps = pps_to_std(&pps).expect("its PPS converts");
        let (offsets, sps_lists) = (
            owned_sps.std().pOffsetForRefFrame,
            owned_sps.std().pScalingLists,
        );
        let pps_lists = owned_pps.std().pScalingLists;
        assert!(!offsets.is_null(), "POC type 1 attaches the offset array");
        assert!(!sps_lists.is_null(), "the SPS declares scaling lists");
        assert!(!pps_lists.is_null(), "so does the PPS");

        // Moves stored parameters go through: into a `Vec`, through realloc as
        // later Adds push, and with the whole `StoredParams` via `mem::replace`.
        let stored_sps = vec![owned_sps];
        let mut stored_pps = vec![owned_pps];
        for id in 1..crate::session::MAX_STD_PPS as u8 {
            let mut more = full_pps(pointer_bearing_sps());
            more.pic_parameter_set_id = id;
            stored_pps.push(pps_to_std(&more).expect("converts"));
        }
        assert!(
            stored_pps.capacity() > 1,
            "the pushes reallocated, which is the case being pinned"
        );
        let stored = (stored_sps, stored_pps, 0u8);
        let (stored_sps, stored_pps, _) = stored;

        assert_eq!(stored_sps[0].std().pOffsetForRefFrame, offsets);
        assert_eq!(stored_sps[0].std().pScalingLists, sps_lists);
        assert_eq!(stored_pps[0].std().pScalingLists, pps_lists);

        // Pointer equality above cannot fail — the Std struct copies the value.
        // An inlined backing would still address dead locals this just overwrote.
        clobber_the_dead_stack();
        // SAFETY: `stored_sps`/`stored_pps` are alive here and own every block.
        let (read_offsets, read_sps_lists, read_pps_lists) = unsafe {
            (
                std::slice::from_raw_parts(offsets, 3),
                &*sps_lists,
                &*pps_lists,
            )
        };
        assert_eq!(read_offsets, [2, -1, 4]);
        assert_eq!(read_sps_lists.ScalingList4x4[5], [15; 16]);
        assert_eq!(read_pps_lists.ScalingList4x4[5], [65; 16]);
    }

    #[test]
    fn sps_scaling_lists_convert_when_present_and_stay_absent_when_not() {
        let mut sps = full_sps();
        assert!(sps_to_std(&sps).unwrap().std().pScalingLists.is_null());

        sps.seq_scaling_matrix_present_flag = true;
        // Distinct fill byte per list: a permutation or intra/inter reinterleave fails.
        sps.scaling_lists_4x4 = std::array::from_fn(|i| [10 + i as u8; 16]);
        sps.scaling_lists_8x8 = std::array::from_fn(|i| [20 + i as u8; 64]);
        let owned = sps_to_std(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.flags.seq_scaling_matrix_present_flag(), 1);
        assert!(!std.pScalingLists.is_null());
        // SAFETY: pScalingLists points at `owned`'s boxed StdVideoH264ScalingLists,
        // alive for this whole scope.
        let lists = unsafe { &*std.pScalingLists };
        // 4:2:0: bits 0–5 (six 4x4) + bits 6–7 (two resolved 8x8). Parser already
        // resolved them, so no driver-side defaults.
        assert_eq!(lists.scaling_list_present_mask, 0xFF);
        assert_eq!(lists.use_default_scaling_matrix_mask, 0);
        for i in 0..6 {
            assert_eq!(lists.ScalingList4x4[i], [10 + i as u8; 16], "4x4 list {i}");
            assert_eq!(lists.ScalingList8x8[i], [20 + i as u8; 64], "8x8 list {i}");
        }

        // 4:4:4 resolves all six 8x8 lists (mask 0xFFF).
        sps.chroma_format_idc = 3;
        let owned = sps_to_std(&sps).unwrap();
        // SAFETY: as above — the pointer targets `owned`'s boxed backing.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.scaling_list_present_mask, 0xFFF);
    }

    #[test]
    fn every_mapped_pps_field_and_flag_round_trips_exactly() {
        let pps = full_pps(full_sps());
        let owned = pps_to_std(&pps).unwrap();
        let std = owned.std();

        assert_eq!(std.flags.transform_8x8_mode_flag(), 1);
        assert_eq!(std.flags.redundant_pic_cnt_present_flag(), 0);
        assert_eq!(std.flags.constrained_intra_pred_flag(), 1);
        assert_eq!(std.flags.deblocking_filter_control_present_flag(), 0);
        assert_eq!(std.flags.weighted_pred_flag(), 1);
        assert_eq!(std.flags.bottom_field_pic_order_in_frame_present_flag(), 0);
        assert_eq!(std.flags.entropy_coding_mode_flag(), 1);
        assert_eq!(std.flags.pic_scaling_matrix_present_flag(), 0);

        assert_eq!(std.seq_parameter_set_id, 3);
        assert_eq!(std.pic_parameter_set_id, 5);
        assert_eq!(std.num_ref_idx_l0_default_active_minus1, 2);
        assert_eq!(std.num_ref_idx_l1_default_active_minus1, 1);
        assert_eq!(
            std.weighted_bipred_idc,
            hh::StdVideoH264WeightedBipredIdc_STD_VIDEO_H264_WEIGHTED_BIPRED_IDC_IMPLICIT
        );
        assert_eq!(std.pic_init_qp_minus26, -3);
        assert_eq!(std.pic_init_qs_minus26, 4);
        assert_eq!(std.chroma_qp_index_offset, -2);
        assert_eq!(std.second_chroma_qp_index_offset, 6);
        assert!(std.pScalingLists.is_null());
    }

    #[test]
    fn pps_scaling_lists_declare_8x8_present_only_under_transform_8x8_mode() {
        let mut pps = full_pps(full_sps());
        pps.pic_scaling_matrix_present_flag = true;
        pps.scaling_lists_4x4 = std::array::from_fn(|i| [30 + i as u8; 16]);
        pps.scaling_lists_8x8 = std::array::from_fn(|i| [40 + i as u8; 64]);

        let owned = pps_to_std(&pps).unwrap();
        // SAFETY: pScalingLists points at `owned`'s boxed backing, alive here.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.scaling_list_present_mask, 0xFF);
        assert_eq!(lists.use_default_scaling_matrix_mask, 0);
        for i in 0..6 {
            assert_eq!(lists.ScalingList4x4[i], [30 + i as u8; 16], "4x4 list {i}");
            assert_eq!(lists.ScalingList8x8[i], [40 + i as u8; 64], "8x8 list {i}");
        }

        // Without transform_8x8_mode the parser never resolved the 8x8 arrays: only
        // the six 4x4 lists may be declared present.
        pps.transform_8x8_mode_flag = false;
        let owned = pps_to_std(&pps).unwrap();
        // SAFETY: as above — the pointer targets `owned`'s boxed backing.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.scaling_list_present_mask, 0x3F);
        assert_eq!(lists.use_default_scaling_matrix_mask, 0);
    }

    #[test]
    fn a_stale_cycle_count_on_a_type_0_sps_converts_to_zero_offsets() {
        let mut sps = full_sps();
        sps.pic_order_cnt_type = 0;
        // Stale cycle count with no POC-type-1 semantics: must not claim a cycle
        // over a null array.
        sps.num_ref_frames_in_pic_order_cnt_cycle = 5;
        let owned = sps_to_std(&sps).unwrap();
        assert_eq!(
            owned.std().num_ref_frames_in_pic_order_cnt_cycle,
            0,
            "count and pointer derive from one condition"
        );
        assert!(owned.std().pOffsetForRefFrame.is_null());
    }

    #[test]
    fn the_25fps_vectors_own_parameter_sets_convert_cleanly() {
        use std::io::Cursor;

        use cros_codecs::codec::h264::parser::Nalu;
        use cros_codecs::codec::h264::parser::NaluType;
        use cros_codecs::codec::h264::parser::Parser;

        const TEST_25FPS: &[u8] = pf_bitstream::testing::H264_25FPS;

        let mut cursor = Cursor::new(TEST_25FPS);
        let mut parser = Parser::default();
        let (mut sps_seen, mut pps_seen) = (false, false);
        while let Ok(nalu) = Nalu::next(&mut cursor) {
            match nalu.header.type_ {
                NaluType::Sps if !sps_seen => {
                    let sps = parser.parse_sps(&nalu).expect("the vector's SPS parses");
                    let owned = sps_to_std(sps).expect("the vector's SPS converts");
                    let std = owned.std();
                    assert_eq!(
                        std.profile_idc,
                        hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN
                    );
                    assert_eq!((std.pic_width_in_mbs_minus1 + 1) * 16, 320);
                    assert_eq!((std.pic_height_in_map_units_minus1 + 1) * 16, 240);
                    assert_eq!(std.flags.frame_mbs_only_flag(), 1);
                    assert_eq!(std.flags.vui_parameters_present_flag(), 0);
                    sps_seen = true;
                }
                NaluType::Pps if !pps_seen => {
                    let pps = parser.parse_pps(&nalu).expect("the vector's PPS parses");
                    let owned = pps_to_std(pps).expect("the vector's PPS converts");
                    assert_eq!(owned.std().pic_parameter_set_id, 0);
                    pps_seen = true;
                }
                _ => {}
            }
            if sps_seen && pps_seen {
                break;
            }
        }
        assert!(sps_seen && pps_seen, "the vector opens with SPS + PPS");
    }

    #[test]
    fn unrepresentable_parameter_sets_are_rejected_not_approximated() {
        let mut sps = full_sps();
        sps.profile_idc = 110; // High10: no StdVideoH264ProfileIdc code point.
        assert_eq!(
            sps_to_std(&sps).unwrap_err(),
            ParamsError::UnmappableProfileIdc(110)
        );

        let mut pps = full_pps(full_sps());
        pps.num_slice_groups_minus1 = 1;
        assert_eq!(pps_to_std(&pps).unwrap_err(), ParamsError::SliceGroups(1));

        let mut pps = full_pps(full_sps());
        pps.weighted_bipred_idc = 3;
        assert_eq!(
            pps_to_std(&pps).unwrap_err(),
            ParamsError::InvalidWeightedBipredIdc(3)
        );
    }

    #[test]
    fn clamp_level_lowers_and_only_lowers() {
        let sps = full_sps();
        let declared = level_to_std(sps.level_idc);

        let mut owned = sps_to_std(&sps).unwrap();
        assert_eq!(owned.std().level_idc, declared);
        owned.clamp_level(hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2);
        assert_eq!(owned.std().level_idc, declared);
        let ceiling = hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1;
        assert!(ceiling < declared, "fixture declares above 3.1");
        owned.clamp_level(ceiling);
        assert_eq!(owned.std().level_idc, ceiling);
    }
}
