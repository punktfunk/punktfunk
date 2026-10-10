//! H.264 encode parameter buffers, hand-declared.
//!
//! The mirror of [`crate::va`] for the encode direction. Same rules: `#[repr(C)]`
//! against libva's `va_enc_h264.h`, every layout pinned by a `const _` assert, and
//! no libva — the runtime lives in `pf-libva`, so everything here builds and tests
//! on any OS.
//!
//! The asserts are not decoration. A wrong offset here is a driver reading a field
//! from the wrong bytes, which encodes a picture nobody can decode and reports
//! success. They were generated from `offsetof` on the target's own header
//! (libva 2.22, `.50`), not written from memory.
//!
//! Bitfield unions are declared as their `u32` `value` half only. libva defines both
//! arms and the driver reads `value`; naming the bits in Rust would add a second
//! layout to keep true for nothing.

use std::mem::offset_of;
use std::mem::size_of;

/// `VABufferType` discriminants this crate constructs.
pub const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: u32 = 22;
pub const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: u32 = 23;
pub const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: u32 = 24;
pub const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: u32 = 25;
pub const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: u32 = 26;
pub const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: u32 = 27;
/// Where the driver writes the bitstream; read back through `VACodedBufferSegment`.
pub const VA_ENC_CODED_BUFFER_TYPE: u32 = 21;

/// `VAEncMiscParameterType`.
pub const VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE: u32 = 0;
pub const VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL: u32 = 1;
pub const VA_ENC_MISC_PARAMETER_TYPE_HRD: u32 = 5;

/// Packed headers are described by **two different numbering schemes** that share a
/// prefix in libva, agree on the first two values, and diverge on the third.
///
/// `VAConfigAttribEncPackedHeaders` takes a bitmask of `VA_ENC_PACKED_HEADER_*`
/// (SLICE is `0x4`); `VAEncPackedHeaderParameterBuffer::type` takes an ordinal from
/// `VAEncPackedHeaderType` (SLICE is `3`). Using one where the other belongs is
/// silent: `SEQUENCE | PICTURE | SLICE` with the ordinals computes `1 | 2 | 3 == 3`,
/// which is `SEQUENCE | PICTURE` — the driver then ignores every packed header the
/// app supplies and encodes anyway, reporting success.
///
/// Named apart so the two can never be confused again.
pub const VA_ENC_PACKED_HEADER_FLAG_SEQUENCE: u32 = 0x0000_0001;
pub const VA_ENC_PACKED_HEADER_FLAG_PICTURE: u32 = 0x0000_0002;
pub const VA_ENC_PACKED_HEADER_FLAG_SLICE: u32 = 0x0000_0004;
pub const VA_ENC_PACKED_HEADER_FLAG_MISC: u32 = 0x0000_0008;

/// `VAEncPackedHeaderType`, for the descriptor's `type` field.
pub const VA_ENC_PACKED_HEADER_TYPE_SEQUENCE: u32 = 1;
pub const VA_ENC_PACKED_HEADER_TYPE_PICTURE: u32 = 2;
pub const VA_ENC_PACKED_HEADER_TYPE_SLICE: u32 = 3;

const _: () = {
    // The trap, pinned: the two schemes agree on the first two and part on the third.
    assert!(VA_ENC_PACKED_HEADER_FLAG_SEQUENCE == VA_ENC_PACKED_HEADER_TYPE_SEQUENCE);
    assert!(VA_ENC_PACKED_HEADER_FLAG_PICTURE == VA_ENC_PACKED_HEADER_TYPE_PICTURE);
    assert!(VA_ENC_PACKED_HEADER_FLAG_SLICE != VA_ENC_PACKED_HEADER_TYPE_SLICE);
};

/// `VAEntrypointEncSlice` — the slice-level encode entrypoint both drivers expose.
pub const VA_ENTRYPOINT_ENC_SLICE: i32 = 6;
/// `VAEntrypointEncSliceLP` — Intel's VDEnc, the fixed-function path; AMD has none.
/// Measured on the UHD 750: `EncSlice` needs 17 ms for a 1080p HEVC picture, past
/// the 60 fps budget, and this one is the way under it.
pub const VA_ENTRYPOINT_ENC_SLICE_LP: i32 = 8;

/// Rate-control modes, as `VAConfigAttribRateControl` values. CBR is the only one
/// both radeonsi and iHD advertise for every profile we open, so it is the default.
pub const VA_RC_CBR: u32 = 0x00000002;
pub const VA_RC_VBR: u32 = 0x00000004;

/// Sequence-level parameters: what becomes the SPS.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    /// `chroma_format_idc:2 | frame_mbs_only:1 | mb_adaptive:1 | seq_scaling:1 |
    /// direct_8x8:1 | log2_max_frame_num_minus4:4 | poc_type:2 |
    /// log2_max_poc_lsb_minus4:4 | delta_poc_always_zero:1`, from the low bit up.
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    /// `aspect_ratio_info:1 | timing_info:1 | bitstream_restriction:1 |
    /// log2_max_mv_len_horizontal:5 | log2_max_mv_len_vertical:5 |
    /// fixed_frame_rate:1 | low_delay_hrd:1 | mvs_over_pic_boundaries:1`.
    ///
    /// Bit 2 is the one WP1.1 wanted and neither AMF nor Media Foundation can set:
    /// with it, the host states its reorder bound and the client stops holding
    /// pictures for a depth the stream never uses.
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

/// Picture-level parameters: the DPB the driver sees, and where the bitstream lands.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaEncPictureParameterBufferH264 {
    pub curr_pic: crate::va::VaPictureH264,
    /// The marked DPB. Unused entries must be `VA_INVALID_ID` with
    /// [`VA_PICTURE_H264_INVALID`](crate::va::VA_PICTURE_H264_INVALID) flags.
    pub reference_frames: [crate::va::VaPictureH264; 16],
    /// The `VAEncCodedBufferType` buffer this picture's bitstream is written into.
    pub coded_buf: u32,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    /// `1` on the final picture of the stream, so the driver can flush.
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// `idr_pic_flag:1 | reference_pic_flag:2 | entropy_coding_mode:1 |
    /// weighted_pred:1 | weighted_bipred_idc:2 | constrained_intra_pred:1 |
    /// transform_8x8_mode:1 | deblocking_filter_control_present:1 |
    /// redundant_pic_cnt_present:1 | pic_order_present:1 | pic_scaling_matrix_present:1`.
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

/// Slice-level parameters. One per slice; `RefPicList0[0]` is the reference the
/// hardware actually uses on AMD (`VAConfigAttribEncMaxRefFrames: l0=1`).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    /// Optional per-MB buffer; `VA_INVALID_ID` when unused.
    pub macroblock_info: u32,
    /// 0 = P, 1 = B, 2 = I (7.4.3; the 5..9 aliases are not used here).
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list_0: [crate::va::VaPictureH264; 32],
    pub ref_pic_list_1: [crate::va::VaPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

/// Header of every misc buffer; the payload follows inline, so these are always
/// allocated as `size_of::<VaEncMiscParameterBuffer>() + size_of::<payload>()`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaEncMiscParameterBuffer {
    pub kind: u32,
    /// Flexible array member in C; the payload is written after this header.
    pub data: [u32; 0],
}

/// The retarget knob. Re-sent mid-stream, this is what libav cannot do — an ABR
/// step changes the rate without rebuilding the encoder or emitting an IDR.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaEncMiscParameterRateControl {
    pub bits_per_second: u32,
    /// Percent of `bits_per_second` to target; 100 for CBR.
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub icq_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    /// libva pads every misc struct; a buffer short of it is read past its end.
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaEncMiscParameterHrd {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaEncMiscParameterFrameRate {
    /// `num | (den << 16)`; a bare integer means denominator 1.
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

/// `VAEncMiscParameterTypeRIR`: rolling intra refresh, one stripe per picture.
pub const VA_ENC_MISC_PARAMETER_TYPE_RIR: u32 = 7;
/// `rir_flags.bits.enable_rir_row`.
pub const VA_RIR_ROW: u32 = 0x2;

/// `VAEncMiscParameterRIR`: the intra stripe this picture carries, in the driver's row unit
/// (MB rows for H.264; 32-px rows on Intel's HEVC, CTB rows on AMD's). Explicit padding
/// after the byte field so every byte the buffer copies is initialised.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaEncMiscParameterRir {
    pub rir_flags: u32,
    pub intra_insertion_location: u16,
    pub intra_insert_size: u16,
    pub qp_delta_for_inserted_intra: u8,
    pub padding: [u8; 3],
    pub va_reserved: [u32; 4],
}

/// Precedes each packed header's bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaEncPackedHeaderParameterBuffer {
    pub kind: u32,
    pub bit_length: u32,
    /// `1` when the bytes already carry emulation-prevention; `0` asks the driver
    /// to insert it. We write our own, so this is `1`.
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

/// What `vaMapBuffer` hands back for a coded buffer: a linked list of segments.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaCodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut std::ffi::c_void,
    pub next: *mut std::ffi::c_void,
    pub va_reserved: [u32; 4],
}

/// Generated from `offsetof` on libva 2.22's own header, on the box that runs it.
/// A mismatch is a driver reading the wrong bytes and reporting success.
const _: () = {
    assert!(size_of::<VaEncSequenceParameterBufferH264>() == 1132);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, intra_period) == 4);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, bits_per_second) == 16);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, picture_width_in_mbs) == 24);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, seq_fields) == 28);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, offset_for_non_ref_pic) == 36);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, offset_for_ref_frame) == 44);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, frame_cropping_flag) == 1068);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, frame_crop_left_offset) == 1072);
    assert!(
        offset_of!(
            VaEncSequenceParameterBufferH264,
            vui_parameters_present_flag
        ) == 1088
    );
    assert!(offset_of!(VaEncSequenceParameterBufferH264, vui_fields) == 1092);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, sar_width) == 1100);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, num_units_in_tick) == 1108);
    assert!(offset_of!(VaEncSequenceParameterBufferH264, va_reserved) == 1116);

    assert!(size_of::<VaEncPictureParameterBufferH264>() == 648);
    assert!(offset_of!(VaEncPictureParameterBufferH264, reference_frames) == 36);
    assert!(offset_of!(VaEncPictureParameterBufferH264, coded_buf) == 612);
    assert!(offset_of!(VaEncPictureParameterBufferH264, frame_num) == 620);
    assert!(offset_of!(VaEncPictureParameterBufferH264, pic_init_qp) == 622);
    assert!(offset_of!(VaEncPictureParameterBufferH264, pic_fields) == 628);
    assert!(offset_of!(VaEncPictureParameterBufferH264, va_reserved) == 632);

    assert!(size_of::<VaEncSliceParameterBufferH264>() == 3140);
    assert!(offset_of!(VaEncSliceParameterBufferH264, slice_type) == 12);
    assert!(offset_of!(VaEncSliceParameterBufferH264, pic_order_cnt_lsb) == 16);
    assert!(offset_of!(VaEncSliceParameterBufferH264, delta_pic_order_cnt) == 24);
    assert!(offset_of!(VaEncSliceParameterBufferH264, ref_pic_list_0) == 36);
    assert!(offset_of!(VaEncSliceParameterBufferH264, ref_pic_list_1) == 1188);
    assert!(offset_of!(VaEncSliceParameterBufferH264, luma_log2_weight_denom) == 2340);
    assert!(offset_of!(VaEncSliceParameterBufferH264, cabac_init_idc) == 3118);
    assert!(offset_of!(VaEncSliceParameterBufferH264, slice_qp_delta) == 3119);
    assert!(offset_of!(VaEncSliceParameterBufferH264, va_reserved) == 3124);

    assert!(size_of::<VaEncMiscParameterBuffer>() == 4);
    assert!(size_of::<VaEncMiscParameterRateControl>() == 60);
    assert!(offset_of!(VaEncMiscParameterRateControl, rc_flags) == 24);
    assert!(offset_of!(VaEncMiscParameterRateControl, target_frame_size) == 40);
    assert!(offset_of!(VaEncMiscParameterRateControl, va_reserved) == 44);
    assert!(size_of::<VaEncMiscParameterHrd>() == 24);
    assert!(size_of::<VaEncMiscParameterFrameRate>() == 24);
    // layout-probe on the 780M: size 28, location 4, size 6, qp delta 8, reserved 12.
    assert!(size_of::<VaEncMiscParameterRir>() == 28);
    assert!(offset_of!(VaEncMiscParameterRir, intra_insert_size) == 6);
    assert!(offset_of!(VaEncMiscParameterRir, qp_delta_for_inserted_intra) == 8);
    assert!(offset_of!(VaEncMiscParameterRir, va_reserved) == 12);
    assert!(offset_of!(VaEncPackedHeaderParameterBuffer, bit_length) == 4);
    assert!(offset_of!(VaEncPackedHeaderParameterBuffer, has_emulation_bytes) == 8);

    assert!(size_of::<VaCodedBufferSegment>() == 48);
    assert!(offset_of!(VaCodedBufferSegment, buf) == 16);
    assert!(offset_of!(VaCodedBufferSegment, next) == 24);
};

impl Default for VaEncPictureParameterBufferH264 {
    fn default() -> Self {
        Self {
            curr_pic: crate::va::VaPictureH264::invalid(),
            reference_frames: [crate::va::VaPictureH264::invalid(); 16],
            coded_buf: crate::va::VA_INVALID_SURFACE,
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            last_picture: 0,
            frame_num: 0,
            pic_init_qp: 26,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            pic_fields: 0,
            va_reserved: [0; 4],
        }
    }
}

impl Default for VaEncSliceParameterBufferH264 {
    fn default() -> Self {
        Self {
            macroblock_address: 0,
            num_macroblocks: 0,
            macroblock_info: crate::va::VA_INVALID_SURFACE,
            slice_type: 2,
            pic_parameter_set_id: 0,
            idr_pic_id: 0,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            direct_spatial_mv_pred_flag: 0,
            num_ref_idx_active_override_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            ref_pic_list_0: [crate::va::VaPictureH264::invalid(); 32],
            ref_pic_list_1: [crate::va::VaPictureH264::invalid(); 32],
            luma_log2_weight_denom: 0,
            chroma_log2_weight_denom: 0,
            luma_weight_l0_flag: 0,
            luma_weight_l0: [0; 32],
            luma_offset_l0: [0; 32],
            chroma_weight_l0_flag: 0,
            chroma_weight_l0: [[0; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_weight_l1_flag: 0,
            luma_weight_l1: [0; 32],
            luma_offset_l1: [0; 32],
            chroma_weight_l1_flag: 0,
            chroma_weight_l1: [[0; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            va_reserved: [0; 4],
        }
    }
}

impl Default for VaEncSequenceParameterBufferH264 {
    /// Zeroed but for `offset_for_ref_frame`, which is 256 entries and so has no
    /// derived `Default`. Every field a session needs is set explicitly at open.
    fn default() -> Self {
        Self {
            seq_parameter_set_id: 0,
            level_idc: 0,
            intra_period: 0,
            intra_idr_period: 0,
            ip_period: 1,
            bits_per_second: 0,
            max_num_ref_frames: 1,
            picture_width_in_mbs: 0,
            picture_height_in_mbs: 0,
            seq_fields: 0,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            num_ref_frames_in_pic_order_cnt_cycle: 0,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: [0; 256],
            frame_cropping_flag: 0,
            frame_crop_left_offset: 0,
            frame_crop_right_offset: 0,
            frame_crop_top_offset: 0,
            frame_crop_bottom_offset: 0,
            vui_parameters_present_flag: 0,
            vui_fields: 0,
            aspect_ratio_idc: 0,
            sar_width: 0,
            sar_height: 0,
            num_units_in_tick: 0,
            time_scale: 0,
            va_reserved: [0; 4],
        }
    }
}
