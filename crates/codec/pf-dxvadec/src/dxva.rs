//! Hand-declared `dxva.h` picture-parameter, Q-matrix, and short-slice layouts.
//!
//! `windows-rs` does not emit these structs (pinned rev `acb5a1a`). A wrong
//! offset is not a compile error: the driver reads garbage as a QP or a
//! reference index.
//!
//! Pinning: `size_of` / `offset_of` against the C totals, plus
//! `size == last_field_offset + last_field_size` so tail padding cannot hide.
//! Bitfields are plain integers; `pack` builders encode MSVC's LSB-up order.
//! Construction is `const fn zeroed()`, not `mem::zeroed`. Byte views are
//! `bytemuck` casts: deriving `Pod` rejects padding at compile time.
//!
//! `dxva.h` uses 1-byte packing. Five of six structs match natural alignment;
//! `{UINT, UINT, USHORT}` slice records are **10 bytes packed, 12 naturally**.
//! Record 0 lands either way; from record 1 every field is two bytes off.
//! Slice types therefore use `#[repr(C, packed)]` and `align_of == 1`.
//!
//! Specs: H.264 DXVA §4.2/4.4/4.6, HEVC DXVA §4.1/4.2/4.3, mingw-w64 `dxva.h`.
//! Runtime `sizeof` in `tests/libav_picparams_parity.rs`.

// Spec names, character for character. snake_case would make every conversion
// line a translation against dxva.h / libavcodec `dxva2_*.c`.
#![allow(non_snake_case)]

use bytemuck::{Pod, Zeroable};
use std::mem::align_of;
use std::mem::offset_of;
use std::mem::size_of;

/// Unused `RefFrameList` / `RefPicSet*` slot: `Index7Bits = 0x7F`,
/// `AssociatedFlag = 1`. Both specs write `0xFF`.
pub const UNUSED_ENTRY: u8 = 0xFF;

/// Bitstream `DataSize` must be a multiple of 128; pad with zeros and charge
/// the pad to the last slice's `SliceBytesInBuffer`.
pub const BITSTREAM_ALIGN: usize = 128;

/// `DXVA_PicEntry_H264` / `DXVA_PicEntry_HEVC` — one byte in both specs:
/// `Index7Bits : 7` (D3D11VA `ArraySlice` / [`crate::SlotMap`] DPB slot) then
/// `AssociatedFlag : 1` (bottom field on `CurrPic`, long-term on a ref list).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Pod, Zeroable)]
#[repr(C)]
pub struct PicEntry(pub u8);

impl PicEntry {
    pub const UNUSED: PicEntry = PicEntry(UNUSED_ENTRY);

    /// `index` is masked to seven bits, not checked. Callers pass envelope-gated
    /// DPB slots; a `debug_assert` catches a miss without panicking a release client.
    pub const fn new(index: u8, associated: bool) -> PicEntry {
        debug_assert!(index < 0x80, "a surface index must fit seven bits");
        PicEntry((index & 0x7F) | ((associated as u8) << 7))
    }

    pub const fn index(self) -> u8 {
        self.0 & 0x7F
    }

    pub const fn associated(self) -> bool {
        self.0 & 0x80 != 0
    }
}

/// `DXVA_PicParams_H264` (`D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS`).
/// 1040 bytes; field offsets in the `const` proofs below.
///
/// `CHAR` members are `i8`: MSVC `CHAR` is signed, and QP / chroma offsets
/// are negative in real streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct PicParamsH264 {
    pub wFrameWidthInMbsMinus1: u16,
    pub wFrameHeightInMbsMinus1: u16,
    pub CurrPic: PicEntry,
    pub num_ref_frames: u8,
    pub wBitFields: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub Reserved16Bits: u16,
    pub StatusReportFeedbackNumber: u32,
    pub RefFrameList: [PicEntry; 16],
    pub CurrFieldOrderCnt: [i32; 2],
    pub FieldOrderCntList: [[i32; 2]; 16],
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub ContinuationFlag: u8,
    pub pic_init_qp_minus26: i8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub Reserved8BitsA: u8,
    pub FrameNumList: [u16; 16],
    pub UsedForReferenceFlags: u32,
    pub NonExistingFrameFlags: u16,
    pub frame_num: u16,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: u8,
    pub direct_8x8_inference_flag: u8,
    pub entropy_coding_mode_flag: u8,
    pub pic_order_present_flag: u8,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub deblocking_filter_control_present_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,
    pub Reserved8BitsB: u8,
    pub slice_group_change_rate_minus1: u16,
    /// Always zero: this backend refuses slice groups
    /// ([`crate::pic::PlanToDxvaError::SliceGroups`]) rather than invent a map.
    pub SliceGroupMap: [u8; 810],
}

impl PicParamsH264 {
    /// Real zeros, not `mem::zeroed`. A new field without a value here is a
    /// compile error.
    pub const fn zeroed() -> PicParamsH264 {
        PicParamsH264 {
            wFrameWidthInMbsMinus1: 0,
            wFrameHeightInMbsMinus1: 0,
            CurrPic: PicEntry(0),
            num_ref_frames: 0,
            wBitFields: 0,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            Reserved16Bits: 0,
            StatusReportFeedbackNumber: 0,
            RefFrameList: [PicEntry(0); 16],
            CurrFieldOrderCnt: [0; 2],
            FieldOrderCntList: [[0; 2]; 16],
            pic_init_qs_minus26: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            ContinuationFlag: 0,
            pic_init_qp_minus26: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            Reserved8BitsA: 0,
            FrameNumList: [0; 16],
            UsedForReferenceFlags: 0,
            NonExistingFrameFlags: 0,
            frame_num: 0,
            log2_max_frame_num_minus4: 0,
            pic_order_cnt_type: 0,
            log2_max_pic_order_cnt_lsb_minus4: 0,
            delta_pic_order_always_zero_flag: 0,
            direct_8x8_inference_flag: 0,
            entropy_coding_mode_flag: 0,
            pic_order_present_flag: 0,
            num_slice_groups_minus1: 0,
            slice_group_map_type: 0,
            deblocking_filter_control_present_flag: 0,
            redundant_pic_cnt_present_flag: 0,
            Reserved8BitsB: 0,
            slice_group_change_rate_minus1: 0,
            SliceGroupMap: [0; 810],
        }
    }
}

/// Fifteen bits in [`PicParamsH264::wBitFields`]. MSVC fills from LSB:
/// `field_pic_flag` 0, `MbaffFrameFlag` 1, `residual_colour_transform_flag` 2,
/// `sp_for_switch_flag` 3, `chroma_format_idc` 4–5, `RefPicFlag` 6,
/// `constrained_intra_pred_flag` 7, `weighted_pred_flag` 8,
/// `weighted_bipred_idc` 9–10, `MbsConsecutiveFlag` 11, `frame_mbs_only_flag` 12,
/// `transform_8x8_mode_flag` 13, `MinLumaBipredSize8x8Flag` 14, `IntraPicFlag` 15.
///
/// `field_pic_flag`, `MbaffFrameFlag`, and `residual_colour_transform_flag` stay
/// 0: the envelope gate already rejected interlaced / separate-colour-plane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct H264BitFields {
    pub chroma_format_idc: u8,
    pub ref_pic_flag: bool,
    pub constrained_intra_pred_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub frame_mbs_only_flag: bool,
    pub transform_8x8_mode_flag: bool,
    /// `MinLumaBipredSize8x8Flag` — level 3.1 and above (the DXVA spec defines it
    /// as `level_idc >= 31`, and libavcodec's DXVA path writes exactly that).
    pub min_luma_bipred_size_8x8: bool,
    /// Every slice of the AU is I or SI.
    pub intra_pic_flag: bool,
}

impl H264BitFields {
    /// Two-bit members are masked, not checked. Envelope-gated inputs; a mask
    /// cannot panic on a hostile stream.
    pub const fn pack(self) -> u16 {
        // Bit 11 hard-1: raster order within a slice. FMO is refused before this.
        ((self.chroma_format_idc as u16 & 0x3) << 4)
            | ((self.ref_pic_flag as u16) << 6)
            | ((self.constrained_intra_pred_flag as u16) << 7)
            | ((self.weighted_pred_flag as u16) << 8)
            | ((self.weighted_bipred_idc as u16 & 0x3) << 9)
            | (1 << 11)
            | ((self.frame_mbs_only_flag as u16) << 12)
            | ((self.transform_8x8_mode_flag as u16) << 13)
            | ((self.min_luma_bipred_size_8x8 as u16) << 14)
            | ((self.intra_pic_flag as u16) << 15)
    }
}

/// `DXVA_Qmatrix_H264` (`D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX`).
/// 224 bytes. `[i][j]` is bitstream (zig-zag) order, not raster — matching
/// `parse_scaling_list`. Only Intra Y / Inter Y 8×8 lists (indices 0 and 3);
/// 8×8 chroma is 4:4:4-only.
///
/// Old ATI/AMD UVD wanted raster (`FF_DXVA2_WORKAROUND_SCALING_LIST_ZIGZAG`);
/// not implemented — hosts encode flat lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct QmatrixH264 {
    pub bScalingLists4x4: [[u8; 16]; 6],
    pub bScalingLists8x8: [[u8; 64]; 2],
}

impl QmatrixH264 {
    pub const fn zeroed() -> QmatrixH264 {
        QmatrixH264 {
            bScalingLists4x4: [[0; 16]; 6],
            bScalingLists8x8: [[0; 64]; 2],
        }
    }
}

/// `DXVA_Slice_H264_Short` — one slice-control record. **10 bytes packed**,
/// not 12: `{UINT, UINT, USHORT}` under `#[repr(C, packed)]`. Natural
/// alignment inserts 2 bytes of tail padding; record 0 still lands, every
/// later record is two bytes off.
///
/// Short format only. Long format (`DXVA_Slice_H264_Long`) is refused at
/// decoder creation; the ladder then uses FFmpeg.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C, packed)]
pub struct SliceH264Short {
    /// Byte offset of the slice's **start code** within the bitstream buffer.
    pub BSNALunitDataLocation: u32,
    /// Start code + NALU bytes; the last slice of an AU also carries the
    /// buffer's trailing 128-byte alignment padding.
    pub SliceBytesInBuffer: u32,
    /// 0 — a whole slice, in one buffer. The nonzero values describe a slice
    /// split across bitstream buffers, which this backend never does.
    pub wBadSliceChopping: u16,
}

/// `DXVA_PicParams_HEVC`. 232 bytes; field offsets in the `const` proofs below.
/// `CHAR` members are `i8` (QP / deblock offsets are signed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct PicParamsHevc {
    pub PicWidthInMinCbsY: u16,
    pub PicHeightInMinCbsY: u16,
    pub wFormatAndSequenceInfoFlags: u16,
    pub CurrPic: PicEntry,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub ucNumDeltaPocsOfRefRpsIdx: u8,
    pub wNumBitsForShortTermRPSInSlice: u16,
    pub ReservedBits2: u16,
    pub dwCodingParamToolFlags: u32,
    pub dwCodingSettingPicturePropertyFlags: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub diff_cu_qp_delta_depth: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub CurrPicOrderCntVal: i32,
    pub RefPicList: [PicEntry; 15],
    pub ReservedBits5: u8,
    pub PicOrderCntValList: [i32; 15],
    pub RefPicSetStCurrBefore: [u8; 8],
    pub RefPicSetStCurrAfter: [u8; 8],
    pub RefPicSetLtCurr: [u8; 8],
    pub ReservedBits6: u16,
    pub ReservedBits7: u16,
    pub StatusReportFeedbackNumber: u32,
}

impl PicParamsHevc {
    pub const fn zeroed() -> PicParamsHevc {
        PicParamsHevc {
            PicWidthInMinCbsY: 0,
            PicHeightInMinCbsY: 0,
            wFormatAndSequenceInfoFlags: 0,
            CurrPic: PicEntry(0),
            sps_max_dec_pic_buffering_minus1: 0,
            log2_min_luma_coding_block_size_minus3: 0,
            log2_diff_max_min_luma_coding_block_size: 0,
            log2_min_transform_block_size_minus2: 0,
            log2_diff_max_min_transform_block_size: 0,
            max_transform_hierarchy_depth_inter: 0,
            max_transform_hierarchy_depth_intra: 0,
            num_short_term_ref_pic_sets: 0,
            num_long_term_ref_pics_sps: 0,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            init_qp_minus26: 0,
            ucNumDeltaPocsOfRefRpsIdx: 0,
            wNumBitsForShortTermRPSInSlice: 0,
            ReservedBits2: 0,
            dwCodingParamToolFlags: 0,
            dwCodingSettingPicturePropertyFlags: 0,
            pps_cb_qp_offset: 0,
            pps_cr_qp_offset: 0,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            column_width_minus1: [0; 19],
            row_height_minus1: [0; 21],
            diff_cu_qp_delta_depth: 0,
            pps_beta_offset_div2: 0,
            pps_tc_offset_div2: 0,
            log2_parallel_merge_level_minus2: 0,
            CurrPicOrderCntVal: 0,
            RefPicList: [PicEntry(0); 15],
            ReservedBits5: 0,
            PicOrderCntValList: [0; 15],
            RefPicSetStCurrBefore: [0; 8],
            RefPicSetStCurrAfter: [0; 8],
            RefPicSetLtCurr: [0; 8],
            ReservedBits6: 0,
            ReservedBits7: 0,
            StatusReportFeedbackNumber: 0,
        }
    }
}

/// [`PicParamsHevc::wFormatAndSequenceInfoFlags`]. LSB-up: `chroma_format_idc`
/// 0–1, `separate_colour_plane_flag` 2, `bit_depth_luma_minus8` 3–5,
/// `bit_depth_chroma_minus8` 6–8, `log2_max_pic_order_cnt_lsb_minus4` 9–12,
/// `NoPicReorderingFlag` 13, `NoBiPredFlag` 14, reserved 15.
///
/// Bits 13–14 stay 0 (same as libavcodec). They are hardware hints, not stream
/// facts; claiming them would let a driver skip reordering this decoder cannot
/// guarantee across a per-AU SPS re-read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HevcFormatFlags {
    pub chroma_format_idc: u8,
    pub separate_colour_plane_flag: bool,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
}

impl HevcFormatFlags {
    /// Widths masked; see [`H264BitFields::pack`].
    pub const fn pack(self) -> u16 {
        (self.chroma_format_idc as u16 & 0x3)
            | ((self.separate_colour_plane_flag as u16) << 2)
            | ((self.bit_depth_luma_minus8 as u16 & 0x7) << 3)
            | ((self.bit_depth_chroma_minus8 as u16 & 0x7) << 6)
            | ((self.log2_max_pic_order_cnt_lsb_minus4 as u16 & 0xF) << 9)
    }
}

/// [`PicParamsHevc::dwCodingParamToolFlags`]. LSB-up: bits 0–3 the enable
/// flags, 4–7 / 8–11 PCM bit depths, 12–13 / 14–15 PCM CB sizes, 16 PCM loop
/// filter off, 17 long-term refs, 18 temporal MVP, 19 strong intra smoothing,
/// 20 dependent slices, 21 output_flag_present, 22–24 extra slice-header bits,
/// 25 sign hiding, 26 cabac_init_present, 27–31 reserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HevcToolFlags {
    pub scaling_list_enabled_flag: bool,
    pub amp_enabled_flag: bool,
    pub sample_adaptive_offset_enabled_flag: bool,
    pub pcm_enabled_flag: bool,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub pcm_loop_filter_disabled_flag: bool,
    pub long_term_ref_pics_present_flag: bool,
    pub sps_temporal_mvp_enabled_flag: bool,
    pub strong_intra_smoothing_enabled_flag: bool,
    pub dependent_slice_segments_enabled_flag: bool,
    pub output_flag_present_flag: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled_flag: bool,
    pub cabac_init_present_flag: bool,
}

impl HevcToolFlags {
    pub const fn pack(self) -> u32 {
        (self.scaling_list_enabled_flag as u32)
            | ((self.amp_enabled_flag as u32) << 1)
            | ((self.sample_adaptive_offset_enabled_flag as u32) << 2)
            | ((self.pcm_enabled_flag as u32) << 3)
            | ((self.pcm_sample_bit_depth_luma_minus1 as u32 & 0xF) << 4)
            | ((self.pcm_sample_bit_depth_chroma_minus1 as u32 & 0xF) << 8)
            | ((self.log2_min_pcm_luma_coding_block_size_minus3 as u32 & 0x3) << 12)
            | ((self.log2_diff_max_min_pcm_luma_coding_block_size as u32 & 0x3) << 14)
            | ((self.pcm_loop_filter_disabled_flag as u32) << 16)
            | ((self.long_term_ref_pics_present_flag as u32) << 17)
            | ((self.sps_temporal_mvp_enabled_flag as u32) << 18)
            | ((self.strong_intra_smoothing_enabled_flag as u32) << 19)
            | ((self.dependent_slice_segments_enabled_flag as u32) << 20)
            | ((self.output_flag_present_flag as u32) << 21)
            | ((self.num_extra_slice_header_bits as u32 & 0x7) << 22)
            | ((self.sign_data_hiding_enabled_flag as u32) << 25)
            | ((self.cabac_init_present_flag as u32) << 26)
    }
}

/// [`PicParamsHevc::dwCodingSettingPicturePropertyFlags`]. LSB-up:
/// 0 constrained_intra, 1 transform_skip, 2 cu_qp_delta, 3 chroma QP offsets,
/// 4 weighted_pred, 5 weighted_bipred, 6 transquant_bypass, 7 tiles,
/// 8 entropy_coding_sync, 9 uniform_spacing, 10 loop_filter_across_tiles,
/// 11 loop_filter_across_slices, 12 deblocking override, 13 deblocking off,
/// 14 lists_modification, 15 slice_segment_header_extension, 16 IrapPicFlag,
/// 17 IdrPicFlag, 18 IntraPicFlag, 19–31 reserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HevcPictureFlags {
    pub constrained_intra_pred_flag: bool,
    pub transform_skip_enabled_flag: bool,
    pub cu_qp_delta_enabled_flag: bool,
    pub pps_slice_chroma_qp_offsets_present_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_flag: bool,
    pub transquant_bypass_enabled_flag: bool,
    pub tiles_enabled_flag: bool,
    pub entropy_coding_sync_enabled_flag: bool,
    pub uniform_spacing_flag: bool,
    pub loop_filter_across_tiles_enabled_flag: bool,
    pub pps_loop_filter_across_slices_enabled_flag: bool,
    pub deblocking_filter_override_enabled_flag: bool,
    pub pps_deblocking_filter_disabled_flag: bool,
    pub lists_modification_present_flag: bool,
    pub slice_segment_header_extension_present_flag: bool,
    pub irap_pic_flag: bool,
    pub idr_pic_flag: bool,
    pub intra_pic_flag: bool,
}

impl HevcPictureFlags {
    pub const fn pack(self) -> u32 {
        (self.constrained_intra_pred_flag as u32)
            | ((self.transform_skip_enabled_flag as u32) << 1)
            | ((self.cu_qp_delta_enabled_flag as u32) << 2)
            | ((self.pps_slice_chroma_qp_offsets_present_flag as u32) << 3)
            | ((self.weighted_pred_flag as u32) << 4)
            | ((self.weighted_bipred_flag as u32) << 5)
            | ((self.transquant_bypass_enabled_flag as u32) << 6)
            | ((self.tiles_enabled_flag as u32) << 7)
            | ((self.entropy_coding_sync_enabled_flag as u32) << 8)
            | ((self.uniform_spacing_flag as u32) << 9)
            | ((self.loop_filter_across_tiles_enabled_flag as u32) << 10)
            | ((self.pps_loop_filter_across_slices_enabled_flag as u32) << 11)
            | ((self.deblocking_filter_override_enabled_flag as u32) << 12)
            | ((self.pps_deblocking_filter_disabled_flag as u32) << 13)
            | ((self.lists_modification_present_flag as u32) << 14)
            | ((self.slice_segment_header_extension_present_flag as u32) << 15)
            | ((self.irap_pic_flag as u32) << 16)
            | ((self.idr_pic_flag as u32) << 17)
            | ((self.intra_pic_flag as u32) << 18)
    }
}

/// `DXVA_Qmatrix_HEVC`. 1000 bytes. `ucScalingLists{0,1,2,3}` are sizeIds
/// 0..3 in coded (diagonal) order. sizeId 3 has two matrices (HEVC matrixId
/// 0 and 3), so `[k]` is the parser's `scaling_list_32x32[k * 3]`. DC entries
/// are `scaling_list_dc_coef_minus8 + 8`, the ScalingFactor DC, not the delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[repr(C)]
pub struct QmatrixHevc {
    pub ucScalingLists0: [[u8; 16]; 6],
    pub ucScalingLists1: [[u8; 64]; 6],
    pub ucScalingLists2: [[u8; 64]; 6],
    pub ucScalingLists3: [[u8; 64]; 2],
    pub ucScalingListDCCoefSizeID2: [u8; 6],
    pub ucScalingListDCCoefSizeID3: [u8; 2],
}

impl QmatrixHevc {
    pub const fn zeroed() -> QmatrixHevc {
        QmatrixHevc {
            ucScalingLists0: [[0; 16]; 6],
            ucScalingLists1: [[0; 64]; 6],
            ucScalingLists2: [[0; 64]; 6],
            ucScalingLists3: [[0; 64]; 2],
            ucScalingListDCCoefSizeID2: [0; 6],
            ucScalingListDCCoefSizeID3: [0; 2],
        }
    }
}

/// `DXVA_Slice_HEVC_Short`. Byte-for-byte the H.264 short record; separate
/// type because the specs define them separately. 10 bytes packed, same
/// reason as [`SliceH264Short`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Pod, Zeroable)]
#[repr(C, packed)]
pub struct SliceHevcShort {
    pub BSNALunitDataLocation: u32,
    pub SliceBytesInBuffer: u32,
    pub wBadSliceChopping: u16,
}

// `size_of` and per-field `offset_of` miss tail padding: a wrong C total of 12
// satisfies both. `size == last_offset + last_size` is the check that does not.
const _: () = {
    assert!(size_of::<PicEntry>() == 1);

    assert!(size_of::<PicParamsH264>() == 1040);
    assert!(offset_of!(PicParamsH264, wFrameWidthInMbsMinus1) == 0);
    assert!(offset_of!(PicParamsH264, wFrameHeightInMbsMinus1) == 2);
    assert!(offset_of!(PicParamsH264, CurrPic) == 4);
    assert!(offset_of!(PicParamsH264, num_ref_frames) == 5);
    assert!(offset_of!(PicParamsH264, wBitFields) == 6);
    assert!(offset_of!(PicParamsH264, bit_depth_luma_minus8) == 8);
    assert!(offset_of!(PicParamsH264, bit_depth_chroma_minus8) == 9);
    assert!(offset_of!(PicParamsH264, Reserved16Bits) == 10);
    assert!(offset_of!(PicParamsH264, StatusReportFeedbackNumber) == 12);
    assert!(offset_of!(PicParamsH264, RefFrameList) == 16);
    assert!(offset_of!(PicParamsH264, CurrFieldOrderCnt) == 32);
    assert!(offset_of!(PicParamsH264, FieldOrderCntList) == 40);
    assert!(offset_of!(PicParamsH264, pic_init_qs_minus26) == 168);
    assert!(offset_of!(PicParamsH264, chroma_qp_index_offset) == 169);
    assert!(offset_of!(PicParamsH264, second_chroma_qp_index_offset) == 170);
    assert!(offset_of!(PicParamsH264, ContinuationFlag) == 171);
    assert!(offset_of!(PicParamsH264, pic_init_qp_minus26) == 172);
    assert!(offset_of!(PicParamsH264, num_ref_idx_l0_active_minus1) == 173);
    assert!(offset_of!(PicParamsH264, num_ref_idx_l1_active_minus1) == 174);
    assert!(offset_of!(PicParamsH264, Reserved8BitsA) == 175);
    assert!(offset_of!(PicParamsH264, FrameNumList) == 176);
    assert!(offset_of!(PicParamsH264, UsedForReferenceFlags) == 208);
    assert!(offset_of!(PicParamsH264, NonExistingFrameFlags) == 212);
    assert!(offset_of!(PicParamsH264, frame_num) == 214);
    assert!(offset_of!(PicParamsH264, log2_max_frame_num_minus4) == 216);
    assert!(offset_of!(PicParamsH264, pic_order_cnt_type) == 217);
    assert!(offset_of!(PicParamsH264, log2_max_pic_order_cnt_lsb_minus4) == 218);
    assert!(offset_of!(PicParamsH264, delta_pic_order_always_zero_flag) == 219);
    assert!(offset_of!(PicParamsH264, direct_8x8_inference_flag) == 220);
    assert!(offset_of!(PicParamsH264, entropy_coding_mode_flag) == 221);
    assert!(offset_of!(PicParamsH264, pic_order_present_flag) == 222);
    assert!(offset_of!(PicParamsH264, num_slice_groups_minus1) == 223);
    assert!(offset_of!(PicParamsH264, slice_group_map_type) == 224);
    assert!(offset_of!(PicParamsH264, deblocking_filter_control_present_flag) == 225);
    assert!(offset_of!(PicParamsH264, redundant_pic_cnt_present_flag) == 226);
    assert!(offset_of!(PicParamsH264, Reserved8BitsB) == 227);
    assert!(offset_of!(PicParamsH264, slice_group_change_rate_minus1) == 228);
    assert!(offset_of!(PicParamsH264, SliceGroupMap) == 230);

    assert!(size_of::<QmatrixH264>() == 224);
    assert!(offset_of!(QmatrixH264, bScalingLists4x4) == 0);
    assert!(offset_of!(QmatrixH264, bScalingLists8x8) == 96);

    assert!(size_of::<SliceH264Short>() == 10);
    assert!(align_of::<SliceH264Short>() == 1);
    assert!(offset_of!(SliceH264Short, BSNALunitDataLocation) == 0);
    assert!(offset_of!(SliceH264Short, SliceBytesInBuffer) == 4);
    assert!(offset_of!(SliceH264Short, wBadSliceChopping) == 8);

    assert!(size_of::<PicParamsHevc>() == 232);
    assert!(offset_of!(PicParamsHevc, PicWidthInMinCbsY) == 0);
    assert!(offset_of!(PicParamsHevc, PicHeightInMinCbsY) == 2);
    assert!(offset_of!(PicParamsHevc, wFormatAndSequenceInfoFlags) == 4);
    assert!(offset_of!(PicParamsHevc, CurrPic) == 6);
    assert!(offset_of!(PicParamsHevc, sps_max_dec_pic_buffering_minus1) == 7);
    assert!(offset_of!(PicParamsHevc, log2_min_luma_coding_block_size_minus3) == 8);
    assert!(offset_of!(PicParamsHevc, log2_diff_max_min_luma_coding_block_size) == 9);
    assert!(offset_of!(PicParamsHevc, log2_min_transform_block_size_minus2) == 10);
    assert!(offset_of!(PicParamsHevc, log2_diff_max_min_transform_block_size) == 11);
    assert!(offset_of!(PicParamsHevc, max_transform_hierarchy_depth_inter) == 12);
    assert!(offset_of!(PicParamsHevc, max_transform_hierarchy_depth_intra) == 13);
    assert!(offset_of!(PicParamsHevc, num_short_term_ref_pic_sets) == 14);
    assert!(offset_of!(PicParamsHevc, num_long_term_ref_pics_sps) == 15);
    assert!(offset_of!(PicParamsHevc, num_ref_idx_l0_default_active_minus1) == 16);
    assert!(offset_of!(PicParamsHevc, num_ref_idx_l1_default_active_minus1) == 17);
    assert!(offset_of!(PicParamsHevc, init_qp_minus26) == 18);
    assert!(offset_of!(PicParamsHevc, ucNumDeltaPocsOfRefRpsIdx) == 19);
    assert!(offset_of!(PicParamsHevc, wNumBitsForShortTermRPSInSlice) == 20);
    assert!(offset_of!(PicParamsHevc, ReservedBits2) == 22);
    assert!(offset_of!(PicParamsHevc, dwCodingParamToolFlags) == 24);
    assert!(offset_of!(PicParamsHevc, dwCodingSettingPicturePropertyFlags) == 28);
    assert!(offset_of!(PicParamsHevc, pps_cb_qp_offset) == 32);
    assert!(offset_of!(PicParamsHevc, pps_cr_qp_offset) == 33);
    assert!(offset_of!(PicParamsHevc, num_tile_columns_minus1) == 34);
    assert!(offset_of!(PicParamsHevc, num_tile_rows_minus1) == 35);
    assert!(offset_of!(PicParamsHevc, column_width_minus1) == 36);
    assert!(offset_of!(PicParamsHevc, row_height_minus1) == 74);
    assert!(offset_of!(PicParamsHevc, diff_cu_qp_delta_depth) == 116);
    assert!(offset_of!(PicParamsHevc, pps_beta_offset_div2) == 117);
    assert!(offset_of!(PicParamsHevc, pps_tc_offset_div2) == 118);
    assert!(offset_of!(PicParamsHevc, log2_parallel_merge_level_minus2) == 119);
    assert!(offset_of!(PicParamsHevc, CurrPicOrderCntVal) == 120);
    assert!(offset_of!(PicParamsHevc, RefPicList) == 124);
    assert!(offset_of!(PicParamsHevc, ReservedBits5) == 139);
    assert!(offset_of!(PicParamsHevc, PicOrderCntValList) == 140);
    assert!(offset_of!(PicParamsHevc, RefPicSetStCurrBefore) == 200);
    assert!(offset_of!(PicParamsHevc, RefPicSetStCurrAfter) == 208);
    assert!(offset_of!(PicParamsHevc, RefPicSetLtCurr) == 216);
    assert!(offset_of!(PicParamsHevc, ReservedBits6) == 224);
    assert!(offset_of!(PicParamsHevc, ReservedBits7) == 226);
    assert!(offset_of!(PicParamsHevc, StatusReportFeedbackNumber) == 228);

    assert!(size_of::<QmatrixHevc>() == 1000);
    assert!(offset_of!(QmatrixHevc, ucScalingLists0) == 0);
    assert!(offset_of!(QmatrixHevc, ucScalingLists1) == 96);
    assert!(offset_of!(QmatrixHevc, ucScalingLists2) == 480);
    assert!(offset_of!(QmatrixHevc, ucScalingLists3) == 864);
    assert!(offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID2) == 992);
    assert!(offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID3) == 998);

    assert!(size_of::<SliceHevcShort>() == 10);
    assert!(align_of::<SliceHevcShort>() == 1);
    assert!(offset_of!(SliceHevcShort, BSNALunitDataLocation) == 0);
    assert!(offset_of!(SliceHevcShort, SliceBytesInBuffer) == 4);
    assert!(offset_of!(SliceHevcShort, wBadSliceChopping) == 8);

    assert!(size_of::<PicParamsH264>() == offset_of!(PicParamsH264, SliceGroupMap) + 810);
    assert!(size_of::<QmatrixH264>() == offset_of!(QmatrixH264, bScalingLists8x8) + 2 * 64);
    assert!(size_of::<SliceH264Short>() == offset_of!(SliceH264Short, wBadSliceChopping) + 2);
    assert!(
        size_of::<PicParamsHevc>() == offset_of!(PicParamsHevc, StatusReportFeedbackNumber) + 4
    );
    assert!(size_of::<QmatrixHevc>() == offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID3) + 2);
    assert!(size_of::<SliceHevcShort>() == offset_of!(SliceHevcShort, wBadSliceChopping) + 2);
};

/// Bytes for `memcpy` into the `GetDecoderBuffer` mapping. `Pod` means no
/// padding, so every byte the driver reads is a field the builder wrote.
pub fn as_bytes<T: Pod>(value: &T) -> &[u8] {
    bytemuck::bytes_of(value)
}

/// Slice-control bytes: `n` packed records with no gap. Relies on
/// [`SliceH264Short`] / [`SliceHevcShort`] being 10-byte packed.
pub fn slice_bytes<T: Pod>(values: &[T]) -> &[u8] {
    bytemuck::cast_slice(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    type PackCase<T> = (fn(&mut T), u32);

    #[test]
    fn a_pic_entry_packs_the_index_into_seven_bits_and_the_flag_into_the_eighth() {
        assert_eq!(PicEntry::new(0, false).0, 0x00);
        assert_eq!(PicEntry::new(5, false).0, 0x05);
        assert_eq!(PicEntry::new(5, true).0, 0x85);
        assert_eq!(PicEntry::new(0x7F, true).0, 0xFF);
        let entry = PicEntry::new(17, true);
        assert_eq!(entry.index(), 17);
        assert!(entry.associated());
        assert_eq!(PicEntry::UNUSED.0, UNUSED_ENTRY);
    }

    #[test]
    fn the_h264_bitfield_word_packs_each_member_at_its_declared_bit() {
        let base = H264BitFields::default();
        // MbsConsecutiveFlag is hard-1, so a default word is bit 11 alone.
        assert_eq!(base.pack(), 1 << 11);

        let mut f = base;
        f.chroma_format_idc = 1;
        assert_eq!(f.pack(), (1 << 11) | (1 << 4));
        f.chroma_format_idc = 3;
        assert_eq!(f.pack(), (1 << 11) | (3 << 4));

        let mut f = base;
        f.ref_pic_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 6));

        let mut f = base;
        f.constrained_intra_pred_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 7));

        let mut f = base;
        f.weighted_pred_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 8));

        let mut f = base;
        f.weighted_bipred_idc = 2;
        assert_eq!(f.pack(), (1 << 11) | (2 << 9));

        let mut f = base;
        f.frame_mbs_only_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 12));

        let mut f = base;
        f.transform_8x8_mode_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 13));

        let mut f = base;
        f.min_luma_bipred_size_8x8 = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 14));

        let mut f = base;
        f.intra_pic_flag = true;
        assert_eq!(f.pack(), (1 << 11) | (1 << 15));
    }

    #[test]
    fn a_typical_h264_bitfield_word_matches_a_hand_computed_value() {
        // chroma=1 @4, ref @6, consecutive @11, frame_mbs_only @12,
        // transform_8x8 @13, level≥3.1 @14 → 0b0111_1000_0101_0000.
        let f = H264BitFields {
            chroma_format_idc: 1,
            ref_pic_flag: true,
            frame_mbs_only_flag: true,
            transform_8x8_mode_flag: true,
            min_luma_bipred_size_8x8: true,
            ..Default::default()
        };
        assert_eq!(f.pack(), 0b0111_1000_0101_0000);
    }

    #[test]
    fn the_hevc_format_word_packs_each_member_at_its_declared_bit() {
        let base = HevcFormatFlags::default();
        assert_eq!(base.pack(), 0);

        let mut f = base;
        f.chroma_format_idc = 1;
        assert_eq!(f.pack(), 1);

        let mut f = base;
        f.separate_colour_plane_flag = true;
        assert_eq!(f.pack(), 1 << 2);

        // Main 10: luma AND chroma at 2, at bits 3 and 6.
        let mut f = base;
        f.bit_depth_luma_minus8 = 2;
        f.bit_depth_chroma_minus8 = 2;
        assert_eq!(f.pack(), (2 << 3) | (2 << 6));

        let mut f = base;
        f.log2_max_pic_order_cnt_lsb_minus4 = 4;
        assert_eq!(f.pack(), 4 << 9);
    }

    #[test]
    fn the_hevc_tool_word_packs_each_member_at_its_declared_bit() {
        let base = HevcToolFlags::default();
        assert_eq!(base.pack(), 0);

        let checks: [PackCase<HevcToolFlags>; 13] = [
            (|f| f.scaling_list_enabled_flag = true, 1 << 0),
            (|f| f.amp_enabled_flag = true, 1 << 1),
            (|f| f.sample_adaptive_offset_enabled_flag = true, 1 << 2),
            (|f| f.pcm_enabled_flag = true, 1 << 3),
            (|f| f.pcm_loop_filter_disabled_flag = true, 1 << 16),
            (|f| f.long_term_ref_pics_present_flag = true, 1 << 17),
            (|f| f.sps_temporal_mvp_enabled_flag = true, 1 << 18),
            (|f| f.strong_intra_smoothing_enabled_flag = true, 1 << 19),
            (|f| f.dependent_slice_segments_enabled_flag = true, 1 << 20),
            (|f| f.output_flag_present_flag = true, 1 << 21),
            (|f| f.sign_data_hiding_enabled_flag = true, 1 << 25),
            (|f| f.cabac_init_present_flag = true, 1 << 26),
            (|f| f.num_extra_slice_header_bits = 5, 5 << 22),
        ];
        for (set, expected) in checks {
            let mut f = base;
            set(&mut f);
            assert_eq!(f.pack(), expected);
        }

        let mut f = base;
        f.pcm_sample_bit_depth_luma_minus1 = 7;
        f.pcm_sample_bit_depth_chroma_minus1 = 7;
        f.log2_min_pcm_luma_coding_block_size_minus3 = 1;
        f.log2_diff_max_min_pcm_luma_coding_block_size = 2;
        assert_eq!(f.pack(), (7 << 4) | (7 << 8) | (1 << 12) | (2 << 14));
    }

    #[test]
    fn the_hevc_picture_word_packs_each_member_at_its_declared_bit() {
        let base = HevcPictureFlags::default();
        assert_eq!(base.pack(), 0);

        let checks: [PackCase<HevcPictureFlags>; 19] = [
            (|f| f.constrained_intra_pred_flag = true, 1 << 0),
            (|f| f.transform_skip_enabled_flag = true, 1 << 1),
            (|f| f.cu_qp_delta_enabled_flag = true, 1 << 2),
            (
                |f| f.pps_slice_chroma_qp_offsets_present_flag = true,
                1 << 3,
            ),
            (|f| f.weighted_pred_flag = true, 1 << 4),
            (|f| f.weighted_bipred_flag = true, 1 << 5),
            (|f| f.transquant_bypass_enabled_flag = true, 1 << 6),
            (|f| f.tiles_enabled_flag = true, 1 << 7),
            (|f| f.entropy_coding_sync_enabled_flag = true, 1 << 8),
            (|f| f.uniform_spacing_flag = true, 1 << 9),
            (|f| f.loop_filter_across_tiles_enabled_flag = true, 1 << 10),
            (
                |f| f.pps_loop_filter_across_slices_enabled_flag = true,
                1 << 11,
            ),
            (
                |f| f.deblocking_filter_override_enabled_flag = true,
                1 << 12,
            ),
            (|f| f.pps_deblocking_filter_disabled_flag = true, 1 << 13),
            (|f| f.lists_modification_present_flag = true, 1 << 14),
            (
                |f| f.slice_segment_header_extension_present_flag = true,
                1 << 15,
            ),
            (|f| f.irap_pic_flag = true, 1 << 16),
            (|f| f.idr_pic_flag = true, 1 << 17),
            (|f| f.intra_pic_flag = true, 1 << 18),
        ];
        for (set, expected) in checks {
            let mut f = base;
            set(&mut f);
            assert_eq!(f.pack(), expected);
        }
    }

    #[test]
    fn every_dxva_buffer_has_the_size_its_spec_declares() {
        assert_eq!(size_of::<PicParamsH264>(), 1040);
        assert_eq!(size_of::<QmatrixH264>(), 224);
        assert_eq!(size_of::<PicParamsHevc>(), 232);
        assert_eq!(size_of::<QmatrixHevc>(), 1000);
        assert_eq!(size_of::<PicEntry>(), 1);
        // 10, not 12. `{u32, u32, u16}` is 12 under `#[repr(C)]`; every
        // record after the first then shifts two bytes.
        assert_eq!(size_of::<SliceH264Short>(), 10);
        assert_eq!(size_of::<SliceHevcShort>(), 10);
        assert_eq!(align_of::<SliceH264Short>(), 1);
        assert_eq!(align_of::<SliceHevcShort>(), 1);
    }

    #[test]
    fn as_bytes_sees_the_struct_at_its_declared_offsets() {
        // Read fields back at declared offsets; size asserts miss endianness.
        let mut pp = PicParamsH264::zeroed();
        pp.wFrameWidthInMbsMinus1 = 0x0102;
        pp.CurrPic = PicEntry::new(3, false);
        pp.StatusReportFeedbackNumber = 0x0A0B_0C0D;
        pp.frame_num = 0x1234;
        let bytes = as_bytes(&pp);
        assert_eq!(bytes.len(), 1040);
        assert_eq!(&bytes[0..2], &0x0102u16.to_le_bytes());
        assert_eq!(bytes[4], 3);
        assert_eq!(&bytes[12..16], &0x0A0B_0C0Du32.to_le_bytes());
        assert_eq!(&bytes[214..216], &0x1234u16.to_le_bytes());
        // Untouched bytes are real zeros (reserved fields).
        assert!(bytes[230..1040].iter().all(|&b| b == 0));
    }

    #[test]
    fn slice_bytes_lays_records_out_back_to_back_with_no_gap() {
        let records = [
            SliceH264Short {
                BSNALunitDataLocation: 0,
                SliceBytesInBuffer: 100,
                wBadSliceChopping: 0,
            },
            SliceH264Short {
                BSNALunitDataLocation: 100,
                SliceBytesInBuffer: 250,
                wBadSliceChopping: 0,
            },
        ];
        let bytes = slice_bytes(&records);
        // Second record at byte 10. Natural alignment would put it at 12.
        assert_eq!(bytes.len(), 20);
        assert_eq!(&bytes[0..4], &0u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &100u32.to_le_bytes());
        assert_eq!(&bytes[8..10], &0u16.to_le_bytes());
        assert_eq!(&bytes[10..14], &100u32.to_le_bytes());
        assert_eq!(&bytes[14..18], &250u32.to_le_bytes());
        assert_eq!(&bytes[18..20], &0u16.to_le_bytes());
    }
}
