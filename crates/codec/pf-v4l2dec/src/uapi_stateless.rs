//! The stateless-decoder half of the kernel interface: extended controls,
//! media requests, the topology query that finds a video node's media device,
//! and the HEVC control structs of `v4l2-controls.h`.
//!
//! Same ABI and the same rules as [`crate::uapi`]: fixed-width fields, sizes
//! and union offsets asserted at compile time. Pointers are `u64`. The HEVC
//! structs are plain `repr(C)`; their explicit `reserved` members are the
//! padding the kernel header spells out, and the kernel zeroes them on entry.

/// `_IOC(dir, type, nr, size)`.
const fn ioc(dir: u32, kind: u8, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((kind as u32) << 8) | nr
}

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// HEVC as parsed slices: the client sends the headers as controls.
pub const V4L2_PIX_FMT_HEVC_SLICE: u32 = fourcc(b'S', b'2', b'6', b'5');
/// 8-bit 4:2:0 in 128-pixel columns, luma then interleaved chroma per column.
pub const V4L2_PIX_FMT_NV12_COL128: u32 = fourcc(b'N', b'C', b'1', b'2');
/// The 10-bit column format: three samples packed into each 32-bit word.
pub const V4L2_PIX_FMT_NV12_10_COL128: u32 = fourcc(b'N', b'C', b'3', b'0');

pub const V4L2_BUF_FLAG_REQUEST_FD: u32 = 0x0080_0000;
pub const V4L2_CTRL_WHICH_REQUEST_VAL: u32 = 0x0f01_0000;

/// `V4L2_CID_CODEC_STATELESS_BASE`.
const STATELESS_BASE: u32 = 0x00a4_0900;
pub const V4L2_CID_STATELESS_HEVC_SPS: u32 = STATELESS_BASE + 400;
pub const V4L2_CID_STATELESS_HEVC_PPS: u32 = STATELESS_BASE + 401;
pub const V4L2_CID_STATELESS_HEVC_SLICE_PARAMS: u32 = STATELESS_BASE + 402;
pub const V4L2_CID_STATELESS_HEVC_SCALING_MATRIX: u32 = STATELESS_BASE + 403;
pub const V4L2_CID_STATELESS_HEVC_DECODE_PARAMS: u32 = STATELESS_BASE + 404;
pub const V4L2_CID_STATELESS_HEVC_DECODE_MODE: u32 = STATELESS_BASE + 405;
pub const V4L2_CID_STATELESS_HEVC_START_CODE: u32 = STATELESS_BASE + 406;
pub const V4L2_CID_STATELESS_HEVC_ENTRY_POINT_OFFSETS: u32 = STATELESS_BASE + 407;
pub const V4L2_CID_STATELESS_HEVC_EXT_SPS_ST_RPS: u32 = STATELESS_BASE + 408;
pub const V4L2_CID_STATELESS_HEVC_EXT_SPS_LT_RPS: u32 = STATELESS_BASE + 409;

pub const V4L2_STATELESS_HEVC_DECODE_MODE_FRAME_BASED: i64 = 1;
pub const V4L2_STATELESS_HEVC_START_CODE_NONE: i64 = 0;
pub const V4L2_STATELESS_HEVC_START_CODE_ANNEX_B: i64 = 1;

pub const V4L2_HEVC_SPS_FLAG_SEPARATE_COLOUR_PLANE: u64 = 1 << 0;
pub const V4L2_HEVC_SPS_FLAG_SCALING_LIST_ENABLED: u64 = 1 << 1;
pub const V4L2_HEVC_SPS_FLAG_AMP_ENABLED: u64 = 1 << 2;
pub const V4L2_HEVC_SPS_FLAG_SAMPLE_ADAPTIVE_OFFSET: u64 = 1 << 3;
pub const V4L2_HEVC_SPS_FLAG_PCM_ENABLED: u64 = 1 << 4;
pub const V4L2_HEVC_SPS_FLAG_PCM_LOOP_FILTER_DISABLED: u64 = 1 << 5;
pub const V4L2_HEVC_SPS_FLAG_LONG_TERM_REF_PICS_PRESENT: u64 = 1 << 6;
pub const V4L2_HEVC_SPS_FLAG_SPS_TEMPORAL_MVP_ENABLED: u64 = 1 << 7;
pub const V4L2_HEVC_SPS_FLAG_STRONG_INTRA_SMOOTHING_ENABLED: u64 = 1 << 8;

pub const V4L2_HEVC_PPS_FLAG_DEPENDENT_SLICE_SEGMENT_ENABLED: u64 = 1 << 0;
pub const V4L2_HEVC_PPS_FLAG_OUTPUT_FLAG_PRESENT: u64 = 1 << 1;
pub const V4L2_HEVC_PPS_FLAG_SIGN_DATA_HIDING_ENABLED: u64 = 1 << 2;
pub const V4L2_HEVC_PPS_FLAG_CABAC_INIT_PRESENT: u64 = 1 << 3;
pub const V4L2_HEVC_PPS_FLAG_CONSTRAINED_INTRA_PRED: u64 = 1 << 4;
pub const V4L2_HEVC_PPS_FLAG_TRANSFORM_SKIP_ENABLED: u64 = 1 << 5;
pub const V4L2_HEVC_PPS_FLAG_CU_QP_DELTA_ENABLED: u64 = 1 << 6;
pub const V4L2_HEVC_PPS_FLAG_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT: u64 = 1 << 7;
pub const V4L2_HEVC_PPS_FLAG_WEIGHTED_PRED: u64 = 1 << 8;
pub const V4L2_HEVC_PPS_FLAG_WEIGHTED_BIPRED: u64 = 1 << 9;
pub const V4L2_HEVC_PPS_FLAG_TRANSQUANT_BYPASS_ENABLED: u64 = 1 << 10;
pub const V4L2_HEVC_PPS_FLAG_TILES_ENABLED: u64 = 1 << 11;
pub const V4L2_HEVC_PPS_FLAG_ENTROPY_CODING_SYNC_ENABLED: u64 = 1 << 12;
pub const V4L2_HEVC_PPS_FLAG_LOOP_FILTER_ACROSS_TILES_ENABLED: u64 = 1 << 13;
pub const V4L2_HEVC_PPS_FLAG_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED: u64 = 1 << 14;
pub const V4L2_HEVC_PPS_FLAG_DEBLOCKING_FILTER_OVERRIDE_ENABLED: u64 = 1 << 15;
pub const V4L2_HEVC_PPS_FLAG_PPS_DISABLE_DEBLOCKING_FILTER: u64 = 1 << 16;
pub const V4L2_HEVC_PPS_FLAG_LISTS_MODIFICATION_PRESENT: u64 = 1 << 17;
pub const V4L2_HEVC_PPS_FLAG_SLICE_SEGMENT_HEADER_EXTENSION_PRESENT: u64 = 1 << 18;
pub const V4L2_HEVC_PPS_FLAG_DEBLOCKING_FILTER_CONTROL_PRESENT: u64 = 1 << 19;
pub const V4L2_HEVC_PPS_FLAG_UNIFORM_SPACING: u64 = 1 << 20;

pub const V4L2_HEVC_DPB_ENTRY_LONG_TERM_REFERENCE: u8 = 0x01;
pub const V4L2_HEVC_DPB_ENTRIES_NUM_MAX: usize = 16;

pub const V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_SAO_LUMA: u64 = 1 << 0;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_SAO_CHROMA: u64 = 1 << 1;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_TEMPORAL_MVP_ENABLED: u64 = 1 << 2;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_MVD_L1_ZERO: u64 = 1 << 3;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_CABAC_INIT: u64 = 1 << 4;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_COLLOCATED_FROM_L0: u64 = 1 << 5;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_USE_INTEGER_MV: u64 = 1 << 6;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_DEBLOCKING_FILTER_DISABLED: u64 = 1 << 7;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED: u64 = 1 << 8;
pub const V4L2_HEVC_SLICE_PARAMS_FLAG_DEPENDENT_SLICE_SEGMENT: u64 = 1 << 9;

pub const V4L2_HEVC_DECODE_PARAM_FLAG_IRAP_PIC: u64 = 0x1;
pub const V4L2_HEVC_DECODE_PARAM_FLAG_IDR_PIC: u64 = 0x2;
pub const V4L2_HEVC_DECODE_PARAM_FLAG_NO_OUTPUT_OF_PRIOR: u64 = 0x4;

pub const V4L2_HEVC_EXT_SPS_LT_RPS_FLAG_USED_LT: u16 = 0x1;

pub const MEDIA_INTF_T_V4L_VIDEO: u32 = 0x0000_0200;

/// `struct v4l2_ext_control`: packed, with a pointer-sized payload union.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2ExtControl {
    pub id: u32,
    /// Payload bytes for a compound or array control; 0 for a plain value.
    pub size: u32,
    pub reserved2: u32,
    /// The payload's address, or the value itself in the low 32 bits.
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2ExtControls {
    pub which: u32,
    pub count: u32,
    pub error_idx: u32,
    pub request_fd: i32,
    pub reserved: u32,
    pub pad: u32,
    /// The address of `count` [`V4l2ExtControl`]s.
    pub controls: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2QueryExtCtrl {
    pub id: u32,
    pub type_: u32,
    pub name: [u8; 32],
    pub minimum: i64,
    pub maximum: i64,
    pub step: u64,
    pub default_value: i64,
    pub flags: u32,
    pub elem_size: u32,
    pub elems: u32,
    pub nr_of_dims: u32,
    pub dims: [u32; 4],
    pub reserved: [u32; 32],
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaV2Topology {
    pub topology_version: u64,
    pub num_entities: u32,
    pub reserved1: u32,
    pub ptr_entities: u64,
    pub num_interfaces: u32,
    pub reserved2: u32,
    /// The address of `num_interfaces` [`MediaV2Interface`]s, or 0 to count.
    pub ptr_interfaces: u64,
    pub num_pads: u32,
    pub reserved3: u32,
    pub ptr_pads: u64,
    pub num_links: u32,
    pub reserved4: u32,
    pub ptr_links: u64,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct MediaV2Interface {
    pub id: u32,
    pub intf_type: u32,
    pub flags: u32,
    pub reserved: [u32; 9],
    /// The device node this interface is, as `major:minor`.
    pub major: u32,
    pub minor: u32,
    pub raw_tail: [u32; 14],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2CtrlHevcSps {
    pub video_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub sps_max_num_reorder_pics: u8,
    pub sps_max_latency_increase_plus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_luma_transform_block_size_minus2: u8,
    pub log2_diff_max_min_luma_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub chroma_format_idc: u8,
    pub sps_max_sub_layers_minus1: u8,
    pub reserved: [u8; 6],
    pub flags: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2CtrlHevcPps {
    pub pic_parameter_set_id: u8,
    pub num_extra_slice_header_bits: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 20],
    pub row_height_minus1: [u8; 22],
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub reserved: u8,
    pub flags: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V4l2HevcDpbEntry {
    /// The CAPTURE buffer holding the reference, by its timestamp in ns.
    pub timestamp: u64,
    pub flags: u8,
    pub field_pic: u8,
    pub reserved: u16,
    pub pic_order_cnt_val: i32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2HevcPredWeightTable {
    pub delta_luma_weight_l0: [i8; 16],
    pub luma_offset_l0: [i8; 16],
    pub delta_chroma_weight_l0: [[i8; 2]; 16],
    pub chroma_offset_l0: [[i8; 2]; 16],
    pub delta_luma_weight_l1: [i8; 16],
    pub luma_offset_l1: [i8; 16],
    pub delta_chroma_weight_l1: [[i8; 2]; 16],
    pub chroma_offset_l1: [[i8; 2]; 16],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2CtrlHevcSliceParams {
    /// Bits of this slice in the buffer, start code included when one is sent.
    pub bit_size: u32,
    /// Bytes from the slice's start in the buffer to its slice data.
    pub data_byte_offset: u32,
    pub num_entry_point_offsets: u32,
    pub nal_unit_type: u8,
    pub nuh_temporal_id_plus1: u8,
    pub slice_type: u8,
    pub colour_plane_id: u8,
    pub slice_pic_order_cnt: i32,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub collocated_ref_idx: u8,
    pub five_minus_max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_act_y_qp_offset: i8,
    pub slice_act_cb_qp_offset: i8,
    pub slice_act_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub pic_struct: u8,
    pub reserved0: [u8; 3],
    pub slice_segment_addr: u32,
    /// Indices into [`V4l2CtrlHevcDecodeParams::dpb`]; `0xff` is unused.
    pub ref_idx_l0: [u8; 16],
    pub ref_idx_l1: [u8; 16],
    pub short_term_ref_pic_set_size: u16,
    pub long_term_ref_pic_set_size: u16,
    pub pred_weight_table: V4l2HevcPredWeightTable,
    pub reserved1: [u8; 2],
    pub flags: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2CtrlHevcDecodeParams {
    pub pic_order_cnt_val: i32,
    pub short_term_ref_pic_set_size: u16,
    pub long_term_ref_pic_set_size: u16,
    pub num_active_dpb_entries: u8,
    pub num_poc_st_curr_before: u8,
    pub num_poc_st_curr_after: u8,
    pub num_poc_lt_curr: u8,
    /// Indices into `dpb`, like the slice lists.
    pub poc_st_curr_before: [u8; 16],
    pub poc_st_curr_after: [u8; 16],
    pub poc_lt_curr: [u8; 16],
    pub num_delta_pocs_of_ref_rps_idx: u8,
    pub reserved: [u8; 3],
    pub dpb: [V4l2HevcDpbEntry; 16],
    pub flags: u64,
}

/// Scaling lists in raster order, the 16x16 and 32x32 ones as 8x8 sets.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct V4l2CtrlHevcScalingMatrix {
    pub scaling_list_4x4: [[u8; 16]; 6],
    pub scaling_list_8x8: [[u8; 64]; 6],
    pub scaling_list_16x16: [[u8; 64]; 6],
    pub scaling_list_32x32: [[u8; 64]; 2],
    pub scaling_list_dc_coef_16x16: [u8; 6],
    pub scaling_list_dc_coef_32x32: [u8; 2],
}

impl V4l2CtrlHevcScalingMatrix {
    /// Every coefficient 16: what a stream without scaling lists decodes with.
    pub const FLAT: V4l2CtrlHevcScalingMatrix = V4l2CtrlHevcScalingMatrix {
        scaling_list_4x4: [[16; 16]; 6],
        scaling_list_8x8: [[16; 64]; 6],
        scaling_list_16x16: [[16; 64]; 6],
        scaling_list_32x32: [[16; 64]; 2],
        scaling_list_dc_coef_16x16: [16; 6],
        scaling_list_dc_coef_32x32: [16; 2],
    };
}

/// One `st_ref_pic_set()` of the SPS, in its explicit (not predicted) form.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2CtrlHevcExtSpsStRps {
    pub delta_idx_minus1: u8,
    pub delta_rps_sign: u8,
    pub num_negative_pics: u8,
    pub num_positive_pics: u8,
    /// Bit `i` for negative entry `i`, then the positive ones from bit
    /// `num_negative_pics` up.
    pub used_by_curr_pic: u32,
    pub use_delta_flag: u32,
    pub abs_delta_rps_minus1: u16,
    pub delta_poc_s0_minus1: [u16; 16],
    pub delta_poc_s1_minus1: [u16; 16],
    pub flags: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct V4l2CtrlHevcExtSpsLtRps {
    pub lt_ref_pic_poc_lsb_sps: u16,
    pub flags: u16,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<V4l2ExtControl>() == 20);
    assert!(size_of::<V4l2ExtControls>() == 32);
    assert!(std::mem::offset_of!(V4l2ExtControls, controls) == 24);
    assert!(size_of::<V4l2QueryExtCtrl>() == 232);
    assert!(size_of::<MediaV2Topology>() == 72);
    assert!(size_of::<MediaV2Interface>() == 112);
    assert!(std::mem::offset_of!(MediaV2Interface, major) == 48);
    assert!(size_of::<V4l2CtrlHevcSps>() == 40);
    assert!(std::mem::offset_of!(V4l2CtrlHevcSps, flags) == 32);
    assert!(size_of::<V4l2CtrlHevcPps>() == 64);
    assert!(std::mem::offset_of!(V4l2CtrlHevcPps, flags) == 56);
    assert!(size_of::<V4l2HevcDpbEntry>() == 16);
    assert!(size_of::<V4l2HevcPredWeightTable>() == 194);
    assert!(size_of::<V4l2CtrlHevcSliceParams>() == 280);
    assert!(std::mem::offset_of!(V4l2CtrlHevcSliceParams, slice_segment_addr) == 36);
    assert!(std::mem::offset_of!(V4l2CtrlHevcSliceParams, pred_weight_table) == 76);
    assert!(std::mem::offset_of!(V4l2CtrlHevcSliceParams, flags) == 272);
    assert!(size_of::<V4l2CtrlHevcDecodeParams>() == 328);
    assert!(std::mem::offset_of!(V4l2CtrlHevcDecodeParams, dpb) == 64);
    assert!(std::mem::offset_of!(V4l2CtrlHevcDecodeParams, flags) == 320);
    assert!(size_of::<V4l2CtrlHevcScalingMatrix>() == 1000);
    assert!(size_of::<V4l2CtrlHevcExtSpsStRps>() == 80);
    assert!(size_of::<V4l2CtrlHevcExtSpsLtRps>() == 4);
};

pub const VIDIOC_S_CTRL: u32 = ioc(IOC_READ | IOC_WRITE, b'V', 28, 8);
pub const VIDIOC_S_EXT_CTRLS: u32 =
    ioc(IOC_READ | IOC_WRITE, b'V', 72, size_of::<V4l2ExtControls>());
pub const VIDIOC_QUERY_EXT_CTRL: u32 = ioc(
    IOC_READ | IOC_WRITE,
    b'V',
    103,
    size_of::<V4l2QueryExtCtrl>(),
);
pub const MEDIA_IOC_G_TOPOLOGY: u32 = ioc(
    IOC_READ | IOC_WRITE,
    b'|',
    0x04,
    size_of::<MediaV2Topology>(),
);
pub const MEDIA_IOC_REQUEST_ALLOC: u32 = ioc(IOC_READ, b'|', 0x05, size_of::<i32>());
pub const MEDIA_REQUEST_IOC_QUEUE: u32 = ioc(IOC_NONE, b'|', 0x80, 0);
pub const MEDIA_REQUEST_IOC_REINIT: u32 = ioc(IOC_NONE, b'|', 0x81, 0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_codes_match_the_kernel() {
        assert_eq!(VIDIOC_S_CTRL, 0xC008_561C);
        assert_eq!(VIDIOC_S_EXT_CTRLS, 0xC020_5648);
        assert_eq!(VIDIOC_QUERY_EXT_CTRL, 0xC0E8_5667);
        assert_eq!(MEDIA_IOC_G_TOPOLOGY, 0xC048_7C04);
        assert_eq!(MEDIA_IOC_REQUEST_ALLOC, 0x8004_7C05);
        assert_eq!(MEDIA_REQUEST_IOC_QUEUE, 0x0000_7C80);
        assert_eq!(MEDIA_REQUEST_IOC_REINIT, 0x0000_7C81);
    }

    #[test]
    fn control_ids_match_the_kernel() {
        assert_eq!(V4L2_CID_STATELESS_HEVC_SPS, 0x00a4_0a90);
        assert_eq!(V4L2_CID_STATELESS_HEVC_EXT_SPS_LT_RPS, 0x00a4_0a99);
    }
}
