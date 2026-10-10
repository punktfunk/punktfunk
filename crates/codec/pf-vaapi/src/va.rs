//! Hand-declared libva H.264 decode-buffer layouts, plus the `VAImage` pair
//! the test-only surface readback needs.
//!
//! No libva binding in this workspace: the crate must compile where the
//! headers do not exist. Same contract as `pf-dxvadec`'s `dxva` module.
//! Pinning is `layout-probe.c` against libva 2.23.0; sizes, offsets, and
//! LSB-first bit-field words are `const` assertions below.
//!
//! `VAPictureH264` is 36 bytes and is embedded 17 + 64 times, so a wrong
//! size shifts every later offset. Deprecated FMO fields still occupy
//! bytes 624..628. `picture_id` is a `VASurfaceID`, not a slot index —
//! `plan_to_va` looks it up; this module never invents one.
//!
//! [`VaImage`] is not a decode buffer. Production exports a dmabuf; only
//! the `#[cfg(test)]` parity harness maps a surface. They live here so
//! that harness does not pull `libva-dev`. The walk is [`pack_two_plane`].

/// Unused `ReferenceFrames` / `RefPicList` entry. Always paired with
/// [`VA_PICTURE_H264_INVALID`]: drivers key on the flag, but a stale
/// surface id in an "invalid" slot decodes on one vendor and not another.
pub const VA_INVALID_SURFACE: u32 = 0xffff_ffff;

/// Slice parameter is **4** and slice data is **5**, not 3 and 4 —
/// `VABitPlaneBufferType` and `VASliceGroupMapBufferType` sit in between.
/// A wrong type is not a decode error; it is a decode of garbage.
pub const VA_PICTURE_PARAMETER_BUFFER_TYPE: u32 = 0;
pub const VA_IQ_MATRIX_BUFFER_TYPE: u32 = 1;
pub const VA_SLICE_PARAMETER_BUFFER_TYPE: u32 = 4;
pub const VA_SLICE_DATA_BUFFER_TYPE: u32 = 5;

pub const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_H264_TOP_FIELD: u32 = 0x0000_0002;
pub const VA_PICTURE_H264_BOTTOM_FIELD: u32 = 0x0000_0004;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x0000_0010;

/// Whole slice in this buffer. The wire delivers complete access units.
pub const VA_SLICE_DATA_FLAG_ALL: u32 = 0x00;

/// `VAPictureH264` — one DPB entry, or the current picture.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureH264 {
    pub picture_id: u32,
    /// `frame_num` (short-term) or `LongTermFrameIdx` (long-term). Same
    /// pair [`pf_bitstream::h264::RefPic`] already carries.
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// `va_reserved[VA_PADDING_LOW]`. Must be zero.
    pub va_reserved: [u32; 4],
}

impl VaPictureH264 {
    /// Invalid flag **and** invalid surface. Drivers key on either.
    pub const fn invalid() -> Self {
        VaPictureH264 {
            picture_id: VA_INVALID_SURFACE,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; 4],
        }
    }
}

/// `VAPictureParameterBufferH264::seq_fields`, unpacked. [`Self::pack`] is
/// the only place the shifts appear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeqFieldsH264 {
    pub chroma_format_idc: u8,
    /// Header spelling is `residual_colour_transform_flag`.
    pub separate_colour_plane_flag: bool,
    pub gaps_in_frame_num_value_allowed_flag: bool,
    pub frame_mbs_only_flag: bool,
    pub mb_adaptive_frame_field_flag: bool,
    pub direct_8x8_inference_flag: bool,
    /// A.3.3.2 — level-derived, not an SPS syntax element.
    pub min_luma_bi_pred_size8x8: bool,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: bool,
}

impl SeqFieldsH264 {
    /// LSB first, declaration order. Measured, not assumed (module docs).
    pub const fn pack(self) -> u32 {
        (self.chroma_format_idc as u32 & 0x3)
            | ((self.separate_colour_plane_flag as u32) << 2)
            | ((self.gaps_in_frame_num_value_allowed_flag as u32) << 3)
            | ((self.frame_mbs_only_flag as u32) << 4)
            | ((self.mb_adaptive_frame_field_flag as u32) << 5)
            | ((self.direct_8x8_inference_flag as u32) << 6)
            | ((self.min_luma_bi_pred_size8x8 as u32) << 7)
            | ((self.log2_max_frame_num_minus4 as u32 & 0xf) << 8)
            | ((self.pic_order_cnt_type as u32 & 0x3) << 12)
            | ((self.log2_max_pic_order_cnt_lsb_minus4 as u32 & 0xf) << 14)
            | ((self.delta_pic_order_always_zero_flag as u32) << 18)
    }
}

/// `VAPictureParameterBufferH264::pic_fields`, unpacked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PicFieldsH264 {
    pub entropy_coding_mode_flag: bool,
    pub weighted_pred_flag: bool,
    pub weighted_bipred_idc: u8,
    pub transform_8x8_mode_flag: bool,
    pub field_pic_flag: bool,
    pub constrained_intra_pred_flag: bool,
    /// Spec spelling: `bottom_field_pic_order_in_frame_present_flag`.
    pub pic_order_present_flag: bool,
    pub deblocking_filter_control_present_flag: bool,
    pub redundant_pic_cnt_present_flag: bool,
    /// `nal_ref_idc != 0` — this picture, not the PPS.
    pub reference_pic_flag: bool,
}

impl PicFieldsH264 {
    pub const fn pack(self) -> u32 {
        (self.entropy_coding_mode_flag as u32)
            | ((self.weighted_pred_flag as u32) << 1)
            | ((self.weighted_bipred_idc as u32 & 0x3) << 2)
            | ((self.transform_8x8_mode_flag as u32) << 4)
            | ((self.field_pic_flag as u32) << 5)
            | ((self.constrained_intra_pred_flag as u32) << 6)
            | ((self.pic_order_present_flag as u32) << 7)
            | ((self.deblocking_filter_control_present_flag as u32) << 8)
            | ((self.redundant_pic_cnt_present_flag as u32) << 9)
            | ((self.reference_pic_flag as u32) << 10)
    }
}

/// `VAPictureParameterBufferH264` — one per picture, before any slice data.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaPictureParameterBufferH264 {
    pub curr_pic: VaPictureH264,
    /// The DPB, not this AU's reference lists. Same as DXVA `RefFrameList`,
    /// opposite of Vulkan `pReferenceSlots`. Filled from `dpb_refs`.
    pub reference_frames: [VaPictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    /// Packed [`SeqFieldsH264`]. One pad byte precedes it (`num_ref_frames`
    /// ends at 619; the union is 4-aligned at 620).
    pub seq_fields: u32,
    /// Deprecated FMO. Still occupy bytes 624..628; always zero.
    /// `plan_to_va` refuses a stream that uses slice groups.
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub frame_num: u16,
    /// `va_reserved[VA_PADDING_MEDIUM]`. Two pad bytes precede it
    /// (`frame_num` ends at 638; the array is 4-aligned at 640).
    pub va_reserved: [u32; 8],
}

/// `VAIQMatrixBufferH264` — both scaling list sets, raster scan order.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaIqMatrixBufferH264 {
    pub scaling_list4x4: [[u8; 16]; 6],
    pub scaling_list8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; 4],
}

/// `VASliceParameterBufferH264` — one per slice NALU.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaSliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    /// Bit offset from the NAL-unit byte to the first bit of `slice_data()`,
    /// counted **after** emulation-prevention bytes are removed even though
    /// the buffer handed to the driver still contains them.
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    /// 8.2.4.2 lists — this AU's, unlike [`VaPictureParameterBufferH264::reference_frames`].
    pub ref_pic_list0: [VaPictureH264; 32],
    pub ref_pic_list1: [VaPictureH264; 32],
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
    pub va_reserved: [u32; 4],
}

impl VaSliceParameterBufferH264 {
    /// All-zero with both reference lists invalidated, so an unwritten
    /// entry is never a stale surface id.
    pub const fn zeroed() -> Self {
        VaSliceParameterBufferH264 {
            slice_data_size: 0,
            slice_data_offset: 0,
            slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
            slice_data_bit_offset: 0,
            first_mb_in_slice: 0,
            slice_type: 0,
            direct_spatial_mv_pred_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            ref_pic_list0: [VaPictureH264::invalid(); 32],
            ref_pic_list1: [VaPictureH264::invalid(); 32],
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
            va_reserved: [0; 4],
        }
    }
}

// Layout proofs — `layout-probe.c` output, pinned. These assert the
// fields and their order match the header, not that `repr(C)` works.

const _: () = {
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<VaPictureH264>() == 36);
    assert!(offset_of!(VaPictureH264, picture_id) == 0);
    assert!(offset_of!(VaPictureH264, frame_idx) == 4);
    assert!(offset_of!(VaPictureH264, flags) == 8);
    assert!(offset_of!(VaPictureH264, top_field_order_cnt) == 12);
    assert!(offset_of!(VaPictureH264, bottom_field_order_cnt) == 16);
    assert!(offset_of!(VaPictureH264, va_reserved) == 20);

    assert!(size_of::<VaPictureParameterBufferH264>() == 672);
    assert!(offset_of!(VaPictureParameterBufferH264, curr_pic) == 0);
    assert!(offset_of!(VaPictureParameterBufferH264, reference_frames) == 36);
    assert!(offset_of!(VaPictureParameterBufferH264, picture_width_in_mbs_minus1) == 612);
    assert!(offset_of!(VaPictureParameterBufferH264, picture_height_in_mbs_minus1) == 614);
    assert!(offset_of!(VaPictureParameterBufferH264, bit_depth_luma_minus8) == 616);
    assert!(offset_of!(VaPictureParameterBufferH264, bit_depth_chroma_minus8) == 617);
    assert!(offset_of!(VaPictureParameterBufferH264, num_ref_frames) == 618);
    assert!(offset_of!(VaPictureParameterBufferH264, seq_fields) == 620);
    assert!(offset_of!(VaPictureParameterBufferH264, num_slice_groups_minus1) == 624);
    assert!(offset_of!(VaPictureParameterBufferH264, slice_group_map_type) == 625);
    assert!(offset_of!(VaPictureParameterBufferH264, slice_group_change_rate_minus1) == 626);
    assert!(offset_of!(VaPictureParameterBufferH264, pic_init_qp_minus26) == 628);
    assert!(offset_of!(VaPictureParameterBufferH264, pic_init_qs_minus26) == 629);
    assert!(offset_of!(VaPictureParameterBufferH264, chroma_qp_index_offset) == 630);
    assert!(offset_of!(VaPictureParameterBufferH264, second_chroma_qp_index_offset) == 631);
    assert!(offset_of!(VaPictureParameterBufferH264, pic_fields) == 632);
    assert!(offset_of!(VaPictureParameterBufferH264, frame_num) == 636);
    assert!(offset_of!(VaPictureParameterBufferH264, va_reserved) == 640);

    assert!(size_of::<VaIqMatrixBufferH264>() == 240);
    assert!(offset_of!(VaIqMatrixBufferH264, scaling_list4x4) == 0);
    assert!(offset_of!(VaIqMatrixBufferH264, scaling_list8x8) == 96);
    assert!(offset_of!(VaIqMatrixBufferH264, va_reserved) == 224);

    assert!(size_of::<VaSliceParameterBufferH264>() == 3128);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_size) == 0);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_offset) == 4);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_flag) == 8);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_data_bit_offset) == 12);
    assert!(offset_of!(VaSliceParameterBufferH264, first_mb_in_slice) == 14);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_type) == 16);
    assert!(offset_of!(VaSliceParameterBufferH264, direct_spatial_mv_pred_flag) == 17);
    assert!(offset_of!(VaSliceParameterBufferH264, num_ref_idx_l0_active_minus1) == 18);
    assert!(offset_of!(VaSliceParameterBufferH264, num_ref_idx_l1_active_minus1) == 19);
    assert!(offset_of!(VaSliceParameterBufferH264, cabac_init_idc) == 20);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_qp_delta) == 21);
    assert!(offset_of!(VaSliceParameterBufferH264, disable_deblocking_filter_idc) == 22);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_alpha_c0_offset_div2) == 23);
    assert!(offset_of!(VaSliceParameterBufferH264, slice_beta_offset_div2) == 24);
    assert!(offset_of!(VaSliceParameterBufferH264, ref_pic_list0) == 28);
    assert!(offset_of!(VaSliceParameterBufferH264, ref_pic_list1) == 1180);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_log2_weight_denom) == 2332);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_log2_weight_denom) == 2333);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l0_flag) == 2334);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l0) == 2336);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_offset_l0) == 2400);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l0_flag) == 2464);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l0) == 2466);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_offset_l0) == 2594);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l1_flag) == 2722);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_weight_l1) == 2724);
    assert!(offset_of!(VaSliceParameterBufferH264, luma_offset_l1) == 2788);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l1_flag) == 2852);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_weight_l1) == 2854);
    assert!(offset_of!(VaSliceParameterBufferH264, chroma_offset_l1) == 2982);
    assert!(offset_of!(VaSliceParameterBufferH264, va_reserved) == 3112);
};

// `VAImage` / `VAImageFormat` — CPU-readable view of a decoded surface.
// Test-only: production never maps one. `pack_two_plane` is the only logic.

/// `VA_LSB_FIRST`. Zero in [`VaImageFormat::byte_order`] is not "unset";
/// it is an invalid enumerator.
pub const VA_LSB_FIRST: u32 = 1;
pub const VA_MSB_FIRST: u32 = 2;

/// `VAImageFormat`. RGB fields are unused for NV12/P010 but occupy bytes
/// 12..32; dropping them would shift `va_reserved`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VaImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    /// RGB only.
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

/// `VAImage` — what `vaDeriveImage` / `vaCreateImage` fills in.
///
/// `width` and `height` are **`unsigned short`**, not `unsigned int`, so
/// `data_size` is at 60 not 64. `pitches`/`offsets` are per plane and are
/// the driver's: chroma starts at `offsets[1]`, never `pitches[0] * height`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaImage {
    pub image_id: u32,
    pub format: VaImageFormat,
    pub buf: u32,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    /// Palette fields. Meaningless for YUV; declared for their bytes.
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

impl VaImage {
    /// All-zero — what a caller hands `vaDeriveImage` to fill.
    ///
    /// Zero, not uninitialised: a failed derive still leaves a descriptor
    /// the caller reads, and that read of uninitialised bytes is UB.
    pub const fn zeroed() -> VaImage {
        VaImage {
            image_id: 0,
            format: VaImageFormat {
                fourcc: 0,
                byte_order: 0,
                bits_per_pixel: 0,
                depth: 0,
                red_mask: 0,
                green_mask: 0,
                blue_mask: 0,
                alpha_mask: 0,
                va_reserved: [0; 4],
            },
            buf: 0,
            width: 0,
            height: 0,
            data_size: 0,
            num_planes: 0,
            pitches: [0; 3],
            offsets: [0; 3],
            num_palette_entries: 0,
            entry_bytes: 0,
            component_order: [0; 4],
            va_reserved: [0; 4],
        }
    }
}

/// Why a mapped image could not be packed. Each arm names what the driver
/// reported; a guessed or truncated buffer would hash as a decode defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageReadError {
    UnsupportedFourcc {
        fourcc: u32,
    },
    /// Driver substituted a different fourcc from the surface pool's.
    Fourcc {
        got: u32,
        want: u32,
    },
    NotTwoPlane {
        planes: u32,
    },
    TooSmall {
        image: (u32, u32),
        display: (u32, u32),
    },
    Pitch {
        plane: usize,
        pitch: u32,
        need: usize,
    },
    OutOfBounds {
        plane: usize,
        row: u32,
        at: usize,
        end: usize,
        mapped: usize,
    },
}

impl std::fmt::Display for ImageReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageReadError::UnsupportedFourcc { fourcc } => {
                write!(f, "no two-plane layout for fourcc {}", fourcc_name(*fourcc))
            }
            ImageReadError::Fourcc { got, want } => write!(
                f,
                "the image is {} but the surface pool is {}",
                fourcc_name(*got),
                fourcc_name(*want)
            ),
            ImageReadError::NotTwoPlane { planes } => {
                write!(f, "the image has {planes} plane(s), not two")
            }
            ImageReadError::TooSmall { image, display } => write!(
                f,
                "the image is {}x{} but the picture is {}x{}",
                image.0, image.1, display.0, display.1
            ),
            ImageReadError::Pitch { plane, pitch, need } => write!(
                f,
                "plane {plane}'s pitch is {pitch} bytes, a row needs {need}"
            ),
            ImageReadError::OutOfBounds {
                plane,
                row,
                at,
                end,
                mapped,
            } => write!(
                f,
                "plane {plane} row {row} spans {at}..{end} of a {mapped}-byte mapping"
            ),
        }
    }
}

impl std::error::Error for ImageReadError {}

fn fourcc_name(fourcc: u32) -> String {
    let bytes = fourcc.to_le_bytes();
    match std::str::from_utf8(&bytes) {
        Ok(s) if bytes.iter().all(|b| b.is_ascii_graphic()) => s.to_string(),
        _ => format!("{fourcc:#010x}"),
    }
}

/// Bytes in one tightly packed `display`-sized picture of `fourcc` — the
/// layout the goldens hash. `None` if this walk has no 4:2:0 layout for it.
pub fn packed_len(display: (u32, u32), fourcc: u32) -> Option<usize> {
    let bytes_per_sample = bytes_per_sample(fourcc)?;
    let (w, h) = (display.0 as usize, display.1 as usize);
    Some(w * bytes_per_sample * (h + h.div_ceil(2)))
}

fn bytes_per_sample(fourcc: u32) -> Option<usize> {
    match fourcc {
        crate::drm::VA_FOURCC_NV12 => Some(1),
        // P010 is 16 bits per sample, ten meaningful bits in the HIGH end
        // of each LE word. This walk copies bytes and never realigns; a
        // LSB-aligned buffer would have the right size and the wrong content.
        crate::drm::VA_FOURCC_P010 => Some(2),
        _ => None,
    }
}

/// Pack the `display`-sized picture out of a mapped `VAImage` — the layout
/// `pf-vkdecode`'s goldens hash.
///
/// Three traps this walk exists to not get wrong:
///
/// * Chroma starts at `offsets[1]`, never `pitches[0] * height`. A decode
///   surface is padded to the codec granule (240 HEVC lines in 256), so
///   computing the offset from display height reads luma padding as chroma.
/// * Padding columns are dropped per row: only `width * bytes_per_sample`
///   of each `pitches[n]` row belong to the golden.
/// * Every read is bounds-checked against `mapped`; a short mapping is
///   returned, not clamped. `mapped` is the `vaMapBuffer` slice of length
///   [`VaImage::data_size`].
pub fn pack_two_plane(
    image: &VaImage,
    mapped: &[u8],
    display: (u32, u32),
    fourcc: u32,
) -> Result<Vec<u8>, ImageReadError> {
    let bytes_per_sample =
        bytes_per_sample(fourcc).ok_or(ImageReadError::UnsupportedFourcc { fourcc })?;
    if image.format.fourcc != fourcc {
        return Err(ImageReadError::Fourcc {
            got: image.format.fourcc,
            want: fourcc,
        });
    }
    if image.num_planes < 2 {
        return Err(ImageReadError::NotTwoPlane {
            planes: image.num_planes,
        });
    }
    let (width, height) = display;
    if u32::from(image.width) < width || u32::from(image.height) < height {
        return Err(ImageReadError::TooSmall {
            image: (u32::from(image.width), u32::from(image.height)),
            display,
        });
    }
    // 4:2:0 chroma is half the rows but interleaved (U,V) pairs, so a
    // chroma row carries as many bytes as a luma row.
    let row_bytes = width as usize * bytes_per_sample;
    let rows = [height, height.div_ceil(2)];
    let mut out = Vec::with_capacity(row_bytes * (rows[0] + rows[1]) as usize);
    for (plane, plane_rows) in rows.iter().enumerate() {
        let pitch = image.pitches[plane] as usize;
        if pitch < row_bytes {
            return Err(ImageReadError::Pitch {
                plane,
                pitch: image.pitches[plane],
                need: row_bytes,
            });
        }
        let base = image.offsets[plane] as usize;
        for row in 0..*plane_rows {
            let at = base + row as usize * pitch;
            let end = at + row_bytes;
            if end > mapped.len() {
                return Err(ImageReadError::OutOfBounds {
                    plane,
                    row,
                    at,
                    end,
                    mapped: mapped.len(),
                });
            }
            out.extend_from_slice(&mapped[at..end]);
        }
    }
    Ok(out)
}

// Image layout proofs — `layout-probe.c` output, pinned.

const _: () = {
    use std::mem::offset_of;
    use std::mem::size_of;

    assert!(size_of::<VaImageFormat>() == 48);
    assert!(offset_of!(VaImageFormat, fourcc) == 0);
    assert!(offset_of!(VaImageFormat, byte_order) == 4);
    assert!(offset_of!(VaImageFormat, bits_per_pixel) == 8);
    assert!(offset_of!(VaImageFormat, depth) == 12);
    assert!(offset_of!(VaImageFormat, red_mask) == 16);
    assert!(offset_of!(VaImageFormat, green_mask) == 20);
    assert!(offset_of!(VaImageFormat, blue_mask) == 24);
    assert!(offset_of!(VaImageFormat, alpha_mask) == 28);
    assert!(offset_of!(VaImageFormat, va_reserved) == 32);

    // `width`/`height` are 16-bit, so `data_size` is at 60, not 64.
    assert!(size_of::<VaImage>() == 120);
    assert!(offset_of!(VaImage, image_id) == 0);
    assert!(offset_of!(VaImage, format) == 4);
    assert!(offset_of!(VaImage, buf) == 52);
    assert!(offset_of!(VaImage, width) == 56);
    assert!(offset_of!(VaImage, height) == 58);
    assert!(offset_of!(VaImage, data_size) == 60);
    assert!(offset_of!(VaImage, num_planes) == 64);
    assert!(offset_of!(VaImage, pitches) == 68);
    assert!(offset_of!(VaImage, offsets) == 80);
    assert!(offset_of!(VaImage, num_palette_entries) == 92);
    assert!(offset_of!(VaImage, entry_bytes) == 96);
    assert!(offset_of!(VaImage, component_order) == 100);
    assert!(offset_of!(VaImage, va_reserved) == 104);
};

#[cfg(test)]
mod tests {
    use super::*;

    // Probe-measured bit patterns. A "tidied" shift fails with the
    // measured value in hand.
    #[test]
    fn seq_fields_pack_where_the_probe_measured() {
        assert_eq!(
            SeqFieldsH264 {
                chroma_format_idc: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_0003
        );
        assert_eq!(
            SeqFieldsH264 {
                log2_max_frame_num_minus4: 0xf,
                ..Default::default()
            }
            .pack(),
            0x0000_0f00
        );
    }

    #[test]
    fn pic_fields_pack_where_the_probe_measured() {
        assert_eq!(
            PicFieldsH264 {
                reference_pic_flag: true,
                ..Default::default()
            }
            .pack(),
            0x0000_0400
        );
        assert_eq!(
            PicFieldsH264 {
                weighted_bipred_idc: 3,
                ..Default::default()
            }
            .pack(),
            0x0000_000c
        );
    }

    #[test]
    fn every_seq_field_owns_a_distinct_bit_range() {
        // Each field alone must light only its own bits; OR of all must
        // equal packing all-at-once. Overlap would still pass the two
        // probe vectors above.
        let each = [
            SeqFieldsH264 {
                chroma_format_idc: 3,
                ..Default::default()
            },
            SeqFieldsH264 {
                separate_colour_plane_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                gaps_in_frame_num_value_allowed_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                frame_mbs_only_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                mb_adaptive_frame_field_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                direct_8x8_inference_flag: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                min_luma_bi_pred_size8x8: true,
                ..Default::default()
            },
            SeqFieldsH264 {
                log2_max_frame_num_minus4: 0xf,
                ..Default::default()
            },
            SeqFieldsH264 {
                pic_order_cnt_type: 3,
                ..Default::default()
            },
            SeqFieldsH264 {
                log2_max_pic_order_cnt_lsb_minus4: 0xf,
                ..Default::default()
            },
            SeqFieldsH264 {
                delta_pic_order_always_zero_flag: true,
                ..Default::default()
            },
        ];
        let mut seen = 0u32;
        for f in each {
            let bits = f.pack();
            assert_ne!(bits, 0, "a field packed to nothing");
            assert_eq!(seen & bits, 0, "two fields share a bit: {bits:#010x}");
            seen |= bits;
        }
        let all = SeqFieldsH264 {
            chroma_format_idc: 3,
            separate_colour_plane_flag: true,
            gaps_in_frame_num_value_allowed_flag: true,
            frame_mbs_only_flag: true,
            mb_adaptive_frame_field_flag: true,
            direct_8x8_inference_flag: true,
            min_luma_bi_pred_size8x8: true,
            log2_max_frame_num_minus4: 0xf,
            pic_order_cnt_type: 3,
            log2_max_pic_order_cnt_lsb_minus4: 0xf,
            delta_pic_order_always_zero_flag: true,
        };
        assert_eq!(all.pack(), seen);
        // Last declared bit is 18.
        assert_eq!(seen & !0x0007_ffff, 0);
    }

    #[test]
    fn every_pic_field_owns_a_distinct_bit_range() {
        let each = [
            PicFieldsH264 {
                entropy_coding_mode_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                weighted_pred_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                weighted_bipred_idc: 3,
                ..Default::default()
            },
            PicFieldsH264 {
                transform_8x8_mode_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                field_pic_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                constrained_intra_pred_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                pic_order_present_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                deblocking_filter_control_present_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                redundant_pic_cnt_present_flag: true,
                ..Default::default()
            },
            PicFieldsH264 {
                reference_pic_flag: true,
                ..Default::default()
            },
        ];
        let mut seen = 0u32;
        for f in each {
            let bits = f.pack();
            assert_ne!(bits, 0);
            assert_eq!(seen & bits, 0, "two fields share a bit: {bits:#010x}");
            seen |= bits;
        }
        assert_eq!(seen & !0x0000_07ff, 0);
    }

    #[test]
    fn an_unused_reference_entry_is_invalid_in_both_ways() {
        let e = VaPictureH264::invalid();
        assert_eq!(e.flags, VA_PICTURE_H264_INVALID);
        assert_eq!(e.picture_id, VA_INVALID_SURFACE);
    }

    #[test]
    fn a_zeroed_slice_record_starts_with_invalidated_lists() {
        let s = VaSliceParameterBufferH264::zeroed();
        assert!(s
            .ref_pic_list0
            .iter()
            .all(|e| e.flags == VA_PICTURE_H264_INVALID));
        assert!(s
            .ref_pic_list1
            .iter()
            .all(|e| e.picture_id == VA_INVALID_SURFACE));
        assert_eq!(s.slice_data_flag, VA_SLICE_DATA_FLAG_ALL);
    }

    // Geometry of a surface readback, runnable without a device.

    /// Surface padded past the picture in both axes, chroma where the
    /// driver puts it rather than at display height.
    // `_picture` is named at every call site so each test reads as the
    // shape it is about. The walk takes picture size from its own argument.
    fn padded_image(
        _picture: (u16, u16),
        surface: (u16, u16),
        pitch: u32,
        fourcc: u32,
    ) -> (VaImage, Vec<u8>) {
        let mut image = VaImage::zeroed();
        image.format.fourcc = fourcc;
        image.format.byte_order = VA_LSB_FIRST;
        image.width = surface.0;
        image.height = surface.1;
        image.num_planes = 2;
        image.pitches = [pitch, pitch, 0];
        // Chroma starts after the whole padded luma plane.
        image.offsets = [0, pitch * u32::from(surface.1), 0];
        let total = pitch as usize * (surface.1 as usize + surface.1.div_ceil(2) as usize);
        image.data_size = total as u32;
        // Luma rows count 0.., chroma 128..; padding columns are 0xff so
        // a walk that read them would be obvious.
        let mut mapped = vec![0xffu8; total];
        for y in 0..surface.1 as usize {
            for x in 0..pitch as usize {
                mapped[y * pitch as usize + x] = if x < surface.0 as usize {
                    (y % 100) as u8
                } else {
                    0xff
                };
            }
        }
        let chroma = image.offsets[1] as usize;
        for y in 0..surface.1.div_ceil(2) as usize {
            for x in 0..pitch as usize {
                mapped[chroma + y * pitch as usize + x] = if x < surface.0 as usize {
                    128 + (y % 100) as u8
                } else {
                    0xff
                };
            }
        }
        (image, mapped)
    }

    #[test]
    fn the_walk_crops_to_the_picture_and_takes_chroma_from_the_drivers_offset() {
        // 320×240 in a 320×256 surface at 384-byte pitch — HEVC 128-line
        // granule and a stride that is not the width.
        let (image, mapped) = padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        let out = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12)
            .expect("the walk must read a padded NV12 surface");
        assert_eq!(out.len(), 320 * 240 + 320 * 120);
        assert_eq!(
            out.len(),
            packed_len((320, 240), crate::drm::VA_FOURCC_NV12).unwrap()
        );
        // 0xff is only ever a padding column.
        assert!(
            !out.contains(&0xff),
            "a padding column leaked into the packed picture"
        );
        // Luma row 3 is all 3s; chroma row 3 is all 131 — only true if
        // chroma came from `offsets[1]`, not `pitch * 240`.
        assert!(out[3 * 320..4 * 320].iter().all(|&b| b == 3));
        let chroma = 320 * 240;
        assert!(out[chroma + 3 * 320..chroma + 4 * 320]
            .iter()
            .all(|&b| b == 131));
    }

    #[test]
    fn reading_chroma_at_the_display_height_would_have_been_caught() {
        // Counterfactual: chroma at `pitch * display_height` must differ
        // from the driver's offset, or the walk's claim is untestable
        // (they coincide on an unpadded surface).
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        let right = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12)
            .expect("the driver's own offset reads");
        image.offsets[1] = 384 * 240;
        let wrong = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12)
            .expect("the wrong offset also reads — that is the point");
        assert_ne!(
            right, wrong,
            "chroma at the display height must differ from chroma at the driver's \
             offset, or this walk's central claim is untestable"
        );
    }

    #[test]
    fn ten_bit_rows_are_twice_as_wide() {
        // P010 samples are 16 bits: a 320-sample row is 640 bytes. A walk
        // that assumed one byte per sample could still pass a length check
        // at some other resolution.
        let (image, mapped) = padded_image((320, 240), (320, 256), 768, crate::drm::VA_FOURCC_P010);
        let out = pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_P010)
            .expect("the walk must read a padded P010 surface");
        assert_eq!(out.len(), 320 * 2 * 240 + 320 * 2 * 120);
        assert_eq!(
            out.len(),
            packed_len((320, 240), crate::drm::VA_FOURCC_P010).unwrap()
        );
        assert_eq!(
            out.len(),
            2 * packed_len((320, 240), crate::drm::VA_FOURCC_NV12).unwrap()
        );
    }

    #[test]
    fn an_odd_height_keeps_its_half_chroma_row() {
        let (image, mapped) = padded_image((16, 9), (16, 16), 32, crate::drm::VA_FOURCC_NV12);
        let out = pack_two_plane(&image, &mapped, (16, 9), crate::drm::VA_FOURCC_NV12)
            .expect("an odd height still reads");
        assert_eq!(out.len(), 16 * 9 + 16 * 5, "9 luma rows, 5 chroma rows");
    }

    #[test]
    fn a_substituted_format_is_refused_rather_than_reinterpreted() {
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        image.format.fourcc = crate::drm::VA_FOURCC_P010;
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::Fourcc {
                got: crate::drm::VA_FOURCC_P010,
                want: crate::drm::VA_FOURCC_NV12
            })
        );
    }

    #[test]
    fn a_packed_or_opaque_image_is_refused() {
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        image.num_planes = 1;
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::NotTwoPlane { planes: 1 })
        );
    }

    #[test]
    fn an_image_smaller_than_the_picture_is_refused() {
        let (image, mapped) = padded_image((320, 240), (320, 240), 384, crate::drm::VA_FOURCC_NV12);
        assert_eq!(
            pack_two_plane(&image, &mapped, (321, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::TooSmall {
                image: (320, 240),
                display: (321, 240)
            })
        );
    }

    #[test]
    fn a_pitch_narrower_than_a_row_is_refused() {
        let (mut image, mapped) =
            padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        image.pitches[1] = 16;
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), crate::drm::VA_FOURCC_NV12),
            Err(ImageReadError::Pitch {
                plane: 1,
                pitch: 16,
                need: 320
            })
        );
    }

    #[test]
    fn a_mapping_shorter_than_the_descriptor_claims_is_refused_not_truncated() {
        // A short read must not silently produce a shorter picture: that
        // hash would be of bytes the decoder never wrote.
        let (image, mapped) = padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        // One byte short of the last chroma row the picture needs. Slack
        // after that row is why a `data_size` check alone would miss an
        // offset that points outside the buffer.
        let last_row_end = image.offsets[1] as usize + 119 * image.pitches[1] as usize + 320;
        assert!(
            last_row_end < mapped.len(),
            "the padded surface must have slack after the picture's last chroma row"
        );
        let err = pack_two_plane(
            &image,
            &mapped[..last_row_end - 1],
            (320, 240),
            crate::drm::VA_FOURCC_NV12,
        )
        .expect_err("a short mapping must be refused");
        assert!(
            matches!(
                err,
                ImageReadError::OutOfBounds {
                    plane: 1,
                    row: 119,
                    ..
                }
            ),
            "expected the last chroma row to be refused, got {err}"
        );
    }

    #[test]
    fn an_unknown_fourcc_has_no_packed_length_and_no_walk() {
        assert_eq!(
            packed_len((320, 240), 0x3132_3449),
            None,
            "I421 is not ours"
        );
        let (image, mapped) = padded_image((320, 240), (320, 256), 384, crate::drm::VA_FOURCC_NV12);
        assert_eq!(
            pack_two_plane(&image, &mapped, (320, 240), 0x3132_3449),
            Err(ImageReadError::UnsupportedFourcc {
                fourcc: 0x3132_3449
            })
        );
    }
}
