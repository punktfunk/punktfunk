//! HEVC encode parameter buffers, hand-declared: the mirror of [`crate::enc_h264`]
//! for `va_enc_hevc.h`. Layouts and every bitfield position were measured on the
//! target (libva 2.22, `.50`) and are pinned below; the union halves are `u32`s
//! built by the `*_fields` helpers so the driver and the packed headers cannot
//! disagree on a bit.

use std::mem::offset_of;
use std::mem::size_of;

pub use crate::config::VA_PROFILE_HEVC_MAIN;
pub use crate::config::VA_PROFILE_HEVC_MAIN10;
pub use crate::va_h265::VaPictureHEVC;

pub const VA_CONFIG_ATTRIB_ENC_HEVC_FEATURES: u32 = 50;
pub const VA_CONFIG_ATTRIB_ENC_HEVC_BLOCK_SIZES: u32 = 51;
/// `VAConfigAttribPredictionDirection`: `BI_NOT_EMPTY` set means every inter
/// slice must be a B slice with L1 filled — VDEnc refuses a P slice outright.
pub const VA_CONFIG_ATTRIB_PREDICTION_DIRECTION: u32 = 39;
pub const VA_PREDICTION_DIRECTION_BI_NOT_EMPTY: u32 = 0x4;
/// `VA_ATTRIB_NOT_SUPPORTED`: the driver has no opinion, take the defaults.
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

/// `VAPictureHEVC::flags` RPS membership; a kept-but-unused picture carries none.
pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x10;

/// HEVC NAL unit types the session writes.
pub const NAL_TRAIL_R: u8 = 1;
pub const NAL_IDR_W_RADL: u8 = 19;
pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_PREFIX_SEI: u8 = 39;

/// What a driver's `VAConfigAttribEncHEVCFeatures` and `BlockSizes` say, reduced to
/// the SPS/PPS choices they force. Both are two-bit "unsupported / supported /
/// required" fields; a feature is enabled when supported, and must be when required.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HevcFeatures {
    pub amp: bool,
    pub sao: bool,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    pub transform_skip: bool,
    pub cu_qp_delta: bool,
    /// `log2(CTB size) - 3`; 3 is 64×64, the largest both drivers take.
    pub log2_ctb_minus3: u8,
    pub log2_min_cb_minus3: u8,
    pub log2_min_tb_minus2: u8,
    pub log2_max_tb_minus2: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    /// Generalised P/B: a P picture is coded as a B slice whose L1 repeats L0.
    /// Intel says so (`BI_NOT_EMPTY`) on both entrypoints; AMD takes P slices.
    pub gpb: bool,
}

impl HevcFeatures {
    /// Decode the two attribute values. Bit layouts are libva's
    /// `VAConfigAttribValEncHEVCFeatures` and `...BlockSizes`, two bits a field.
    pub fn from_attributes(features: u32, block_sizes: u32) -> Self {
        let f = |shift: u32| (features >> shift) & 0x3 != 0;
        let b = |shift: u32| ((block_sizes >> shift) & 0x3) as u8;
        // Two bits a field, in header order. Features: amp at 4, sao 6, temporal
        // MVP 10, strong intra 12, transform skip 20, CU QP delta 22. Blocks: max
        // CTB 0, min CB 4, MAX TB 6, min TB 8, depth inter 10, intra 14 — a
        // minimum field interleaves after each maximum.
        Self {
            amp: f(4),
            sao: f(6),
            temporal_mvp: f(10),
            strong_intra_smoothing: f(12),
            transform_skip: f(20),
            cu_qp_delta: f(22),
            log2_ctb_minus3: b(0),
            log2_min_cb_minus3: b(4),
            log2_max_tb_minus2: b(6),
            log2_min_tb_minus2: b(8),
            max_transform_hierarchy_depth_inter: b(10),
            max_transform_hierarchy_depth_intra: b(14),
            gpb: false,
        }
    }

    /// ffmpeg's fallback when a driver advertises nothing: 32×32 CTB, 16×16 CB.
    pub fn guessed() -> Self {
        Self {
            amp: false,
            sao: false,
            temporal_mvp: false,
            strong_intra_smoothing: false,
            transform_skip: false,
            cu_qp_delta: true,
            log2_ctb_minus3: 2,
            log2_min_cb_minus3: 1,
            log2_min_tb_minus2: 0,
            log2_max_tb_minus2: 3,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            gpb: false,
        }
    }

    pub fn ctb_size(&self) -> u32 {
        1 << (self.log2_ctb_minus3 + 3)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaEncSequenceParameterBufferHEVC {
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub general_tier_flag: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    /// [`seq_fields`].
    pub seq_fields: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u32,
    pub pcm_sample_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_luma_coding_block_size_minus3: u32,
    pub log2_max_pcm_luma_coding_block_size_minus3: u32,
    pub vui_parameters_present_flag: u8,
    /// [`vui_fields`].
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub vui_num_units_in_tick: u32,
    pub vui_time_scale: u32,
    pub min_spatial_segmentation_idc: u16,
    pub max_bytes_per_pic_denom: u8,
    pub max_bits_per_min_cu_denom: u8,
    pub scc_fields: u32,
    pub va_reserved: [u32; 7],
}

/// `seq_fields`, measured: chroma_format_idc:2 | separate_colour_plane:1 |
/// bit_depth_luma_minus8:3 | bit_depth_chroma_minus8:3 | scaling_list:1 |
/// strong_intra_smoothing:1 | amp:1 | sao:1 | pcm:1 | pcm_loop_filter_disabled:1 |
/// sps_temporal_mvp:1.
pub fn seq_fields(bit_depth_minus8: u8, f: &HevcFeatures) -> u32 {
    1 | (u32::from(bit_depth_minus8) & 0x7) << 3
        | (u32::from(bit_depth_minus8) & 0x7) << 6
        | u32::from(f.strong_intra_smoothing) << 10
        | u32::from(f.amp) << 11
        | u32::from(f.sao) << 12
        | u32::from(f.temporal_mvp) << 15
}

/// `vui_fields`, measured: timing at bit 3, bitstream_restriction at 4, motion
/// vectors over picture boundaries at 6, the two `log2_max_mv_length`s at 8 and 13.
pub fn vui_fields() -> u32 {
    1 << 3 | 1 << 4 | 1 << 6 | 15 << 8 | 15 << 13
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaEncPictureParameterBufferHEVC {
    pub decoded_curr_pic: VaPictureHEVC,
    pub reference_frames: [VaPictureHEVC; 15],
    pub coded_buf: u32,
    pub collocated_ref_pic_index: u8,
    pub last_picture: u8,
    pub pic_init_qp: u8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 19],
    pub row_height_minus1: [u8; 21],
    pub log2_parallel_merge_level_minus2: u8,
    pub ctu_max_bitsize_allowed: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub slice_pic_parameter_set_id: u8,
    pub nal_unit_type: u8,
    /// [`pic_fields`].
    pub pic_fields: u32,
    pub hierarchical_level_plus1: u8,
    pub va_byte_reserved: u8,
    pub scc_fields: u16,
    pub va_reserved: [u32; 15],
}

impl Default for VaEncPictureParameterBufferHEVC {
    fn default() -> Self {
        Self {
            decoded_curr_pic: VaPictureHEVC::invalid(),
            reference_frames: [VaPictureHEVC::invalid(); 15],
            coded_buf: crate::va::VA_INVALID_SURFACE,
            collocated_ref_pic_index: 0xff,
            last_picture: 0,
            pic_init_qp: 26,
            diff_cu_qp_delta_depth: 0,
            pps_cb_qp_offset: 0,
            pps_cr_qp_offset: 0,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            column_width_minus1: [0; 19],
            row_height_minus1: [0; 21],
            log2_parallel_merge_level_minus2: 0,
            ctu_max_bitsize_allowed: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            slice_pic_parameter_set_id: 0,
            nal_unit_type: NAL_TRAIL_R,
            pic_fields: 0,
            hierarchical_level_plus1: 0,
            va_byte_reserved: 0,
            scc_fields: 0,
            va_reserved: [0; 15],
        }
    }
}

/// `pic_fields`, measured: idr:1 | coding_type:3 | reference_pic:1 at bit 4 |
/// transform_skip at 8 | cu_qp_delta at 9 | pps_loop_filter_across_slices at 16.
/// Every picture is a reference; `coding_type` is 1 for I and 2 for P.
pub fn pic_fields(is_idr: bool, f: &HevcFeatures) -> u32 {
    u32::from(is_idr)
        | (if is_idr { 1 } else { 2 }) << 1
        | 1 << 4
        | u32::from(f.transform_skip) << 8
        | u32::from(f.cu_qp_delta) << 9
        | 1 << 16
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaEncSliceParameterBufferHEVC {
    pub slice_segment_address: u32,
    pub num_ctu_in_slice: u32,
    /// 0 = B, 1 = P, 2 = I — HEVC's own numbering, the reverse of H.264's.
    pub slice_type: u8,
    pub slice_pic_parameter_set_id: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VaPictureHEVC; 15],
    pub ref_pic_list1: [VaPictureHEVC; 15],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    /// [`slice_fields`].
    pub slice_fields: u32,
    pub pred_weight_table_bit_offset: u32,
    pub pred_weight_table_bit_length: u32,
    pub va_reserved: [u32; 6],
}

impl Default for VaEncSliceParameterBufferHEVC {
    fn default() -> Self {
        Self {
            slice_segment_address: 0,
            num_ctu_in_slice: 0,
            slice_type: 2,
            slice_pic_parameter_set_id: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_pic_list0: [VaPictureHEVC::invalid(); 15],
            ref_pic_list1: [VaPictureHEVC::invalid(); 15],
            luma_log2_weight_denom: 0,
            delta_chroma_log2_weight_denom: 0,
            delta_luma_weight_l0: [0; 15],
            luma_offset_l0: [0; 15],
            delta_chroma_weight_l0: [[0; 2]; 15],
            chroma_offset_l0: [[0; 2]; 15],
            delta_luma_weight_l1: [0; 15],
            luma_offset_l1: [0; 15],
            delta_chroma_weight_l1: [[0; 2]; 15],
            chroma_offset_l1: [[0; 2]; 15],
            max_num_merge_cand: 5,
            slice_qp_delta: 0,
            slice_cb_qp_offset: 0,
            slice_cr_qp_offset: 0,
            slice_beta_offset_div2: 0,
            slice_tc_offset_div2: 0,
            slice_fields: 0,
            pred_weight_table_bit_offset: 0,
            pred_weight_table_bit_length: 0,
            va_reserved: [0; 6],
        }
    }
}

/// `slice_fields`, measured: last_slice_of_pic at bit 0, slice_temporal_mvp at 4,
/// sao luma/chroma at 5/6, num_ref_idx_active_override at 7,
/// loop_filter_across_slices at **12** and collocated_from_l0 at 13 — one past
/// where the header's field order suggests.
pub fn slice_fields(is_idr: bool, f: &HevcFeatures) -> u32 {
    1 | u32::from(f.temporal_mvp && !is_idr) << 4
        | u32::from(f.sao) << 5
        | u32::from(f.sao) << 6
        | u32::from(!is_idr) << 7
        | 1 << 12
        | 1 << 13
}

/// Measured by `layout-probe.c` against libva 2.22 on `.50`.
const _: () = {
    assert!(size_of::<VaEncSequenceParameterBufferHEVC>() == 116);
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, intra_period) == 4);
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, pic_width_in_luma_samples) == 20);
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, seq_fields) == 24);
    assert!(
        offset_of!(
            VaEncSequenceParameterBufferHEVC,
            log2_min_luma_coding_block_size_minus3
        ) == 28
    );
    assert!(
        offset_of!(
            VaEncSequenceParameterBufferHEVC,
            pcm_sample_bit_depth_luma_minus1
        ) == 36
    );
    assert!(
        offset_of!(
            VaEncSequenceParameterBufferHEVC,
            vui_parameters_present_flag
        ) == 52
    );
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, vui_fields) == 56);
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, sar_width) == 64);
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, vui_num_units_in_tick) == 72);
    assert!(
        offset_of!(
            VaEncSequenceParameterBufferHEVC,
            min_spatial_segmentation_idc
        ) == 80
    );
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, scc_fields) == 84);
    assert!(offset_of!(VaEncSequenceParameterBufferHEVC, va_reserved) == 88);

    assert!(size_of::<VaEncPictureParameterBufferHEVC>() == 576);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, reference_frames) == 28);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, coded_buf) == 448);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, column_width_minus1) == 460);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, row_height_minus1) == 479);
    assert!(
        offset_of!(
            VaEncPictureParameterBufferHEVC,
            log2_parallel_merge_level_minus2
        ) == 500
    );
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, nal_unit_type) == 505);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, pic_fields) == 508);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, scc_fields) == 514);
    assert!(offset_of!(VaEncPictureParameterBufferHEVC, va_reserved) == 516);

    assert!(size_of::<VaEncSliceParameterBufferHEVC>() == 1076);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, ref_pic_list0) == 12);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, ref_pic_list1) == 432);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, luma_log2_weight_denom) == 852);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, delta_chroma_weight_l0) == 884);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, max_num_merge_cand) == 1034);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, slice_fields) == 1040);
    assert!(offset_of!(VaEncSliceParameterBufferHEVC, va_reserved) == 1052);
    assert!(size_of::<VaPictureHEVC>() == 28);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The two boxes' attribute words, as probed: Intel requires AMP and CU QP delta
    /// and offers depth 2; AMD has no temporal MVP and allows depth 0 only.
    #[test]
    fn the_probed_attribute_words_decode_to_what_the_drivers_said() {
        let intel = HevcFeatures::from_attributes(0x0190_0464, 0x88c7);
        assert!(intel.amp && intel.sao && intel.temporal_mvp && intel.cu_qp_delta);
        assert!(!intel.strong_intra_smoothing);
        assert_eq!(intel.ctb_size(), 64);
        assert_eq!(intel.log2_min_cb_minus3, 0);
        assert_eq!(intel.log2_max_tb_minus2, 3);
        assert_eq!(intel.max_transform_hierarchy_depth_inter, 2);

        let amd = HevcFeatures::from_attributes(0x1054_1050, 0xcf);
        assert!(amd.amp && amd.sao && amd.strong_intra_smoothing && amd.cu_qp_delta);
        assert!(!amd.temporal_mvp);
        assert_eq!(amd.ctb_size(), 64);
        assert_eq!(amd.max_transform_hierarchy_depth_inter, 0);
        assert_eq!(amd.max_transform_hierarchy_depth_intra, 0);
    }

    /// The measured bit positions, restated as the values the probe printed.
    #[test]
    fn the_bitfield_helpers_match_the_probe() {
        let f = HevcFeatures {
            amp: true,
            sao: true,
            temporal_mvp: true,
            strong_intra_smoothing: true,
            ..HevcFeatures::guessed()
        };
        let seq = seq_fields(2, &f);
        assert_eq!(seq & 0x3, 1);
        assert_eq!(seq & 0x10, 0x10, "bit_depth_luma_minus8 = 2");
        assert_eq!(seq & 0x80, 0x80, "bit_depth_chroma_minus8 = 2");
        assert_eq!(seq & 0x400, 0x400, "strong_intra");
        assert_eq!(seq & 0x800, 0x800, "amp");
        assert_eq!(seq & 0x1000, 0x1000, "sao");
        assert_eq!(seq & 0x8000, 0x8000, "tmvp");
        assert_eq!(vui_fields(), 0x8 | 0x10 | 0x40 | 0xf00 | 0x1e000);
        let p = pic_fields(false, &f);
        assert_eq!(p & 0xe, 0x4, "coding_type 2 = P");
        assert_eq!(p & 0x10, 0x10, "reference");
        assert_eq!(p & 0x1_0000, 0x1_0000, "loop filter across slices");
        assert_eq!(pic_fields(true, &f) & 0xf, 0x3, "IDR, coding_type 1");
        let s = slice_fields(false, &f);
        assert_eq!(s & 0x1, 1);
        assert_eq!(s & 0x10, 0x10, "tmvp");
        assert_eq!(s & 0x60, 0x60, "sao");
        assert_eq!(s & 0x80, 0x80, "override");
        assert_eq!(s & 0x3000, 0x3000, "lf across, collocated_from_l0");
        assert_eq!(
            slice_fields(true, &f) & 0x90,
            0,
            "an IDR overrides and predicts nothing"
        );
    }
}
