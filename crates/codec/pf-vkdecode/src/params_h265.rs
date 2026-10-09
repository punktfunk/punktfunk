//! H.265 parameter-set conversion: vendored [`Vps`]/[`Sps`]/[`Pps`] into the
//! `StdVideoH265*ParameterSet` structs a Vulkan Video session-parameters object
//! is created from. Std structs embed raw pointers, so conversion returns owning
//! wrappers with the aliasing/lifetime contract of [`crate::OwnedStdSps`].
//!
//! A DECODE session consumes neither VUI nor HRD/timing.
//! `vui_parameters_present_flag` stays 0 and `pSequenceParameterSetVui` stays
//! null; colour rides [`pf_bitstream::h265::PicturePlan::colour`].
//! `vps_timing_info_present_flag` stays 0 and `pHrdParameters` stays null.
//!
//! Short-term RPS candidates are re-encoded in resolved form: the parser has
//! already run 7.4.8 inter-RPS prediction, so each set is declared non-predicted
//! with derived `DeltaPocS0`/`DeltaPocS1` written back as `delta_poc_sX_minus1`.
//! Slice-inline prediction still derives from those arrays
//! (`NumDeltaPocsOfRefRpsIdx` rides picture info; see [`crate::pic_h265`]).

use ash::vk::native as hh;
pub use cros_codecs::codec::h265::parser::Pps;
use cros_codecs::codec::h265::parser::ProfileTierLevel;
use cros_codecs::codec::h265::parser::ScalingLists;
use cros_codecs::codec::h265::parser::ShortTermRefPicSet;
pub use cros_codecs::codec::h265::parser::Sps;
pub use cros_codecs::codec::h265::parser::Vps;
use pf_bitstream::h265::Level;

/// Unrepresentable as a StdVideo struct, or outside the decode envelope
/// (4:2:0 and 4:4:4 at 8 or 10 bits). Stream-integrity failure: reject,
/// never submit a half-truth to a driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H265ParamsError {
    /// `general_profile_idc` has no `StdVideoH265ProfileIdc` code point.
    /// Vulkan names 1-4 (Main, Main 10, Main Still Picture, Format Range
    /// Extensions); High Throughput/SCC/scalable land here.
    UnmappableProfileIdc(u8),
    /// `chroma_format_idc` past 3 — not legal H.265.
    InvalidChromaFormatIdc(u8),
    /// 4:2:2 or monochrome: legal H.265, outside the envelope (no host
    /// emits it; no output-format plumbing).
    UnsupportedChromaFormat(u8),
    /// 4:4:4 with `separate_colour_plane_flag`: ChromaArrayType 0 (three
    /// monochrome-coded planes). No output format; most hardware refuses it.
    SeparateColourPlanes,
    /// Bit depth beyond 8/10, or luma and chroma depths that disagree. No
    /// output format (NV12/P010 and 4:4:4 counterparts) can carry it.
    UnsupportedBitDepth { luma_minus8: u8, chroma_minus8: u8 },
    /// SCC palette predictor initializers: this conversion does not populate
    /// `pPredictorPaletteEntries`. A present-flag over a null pointer is a
    /// half-truth.
    PalettePredictorInitializers,
    /// `num_short_term_ref_pic_sets` past the spec's 64 (7.4.3.2.1).
    TooManyShortTermRpsSets(u8),
    /// The SPS declares more short-term RPS candidates than the parser
    /// resolved. Refuse to pad a corrupt table.
    MissingShortTermRps { index: usize },
    /// More entries on one side than the Std struct's 16-element arrays
    /// (7.4.8 bounds both sides by DPB size, itself <= 16).
    RpsEntryOverflow {
        set: usize,
        negative: u8,
        positive: u8,
    },
    /// Derived `DeltaPocS0`/`DeltaPocS1` is not strictly monotonic, or a
    /// step exceeds 7.4.8's 2^15 bound, so it cannot re-encode as
    /// `delta_poc_sX_minus1`. Unreachable off the vendored parser; guards
    /// constructed inputs.
    NonMonotonicRps { set: usize },
    /// `num_long_term_ref_pics_sps` past the spec's 32 (7.4.3.2.1).
    TooManyLongTermSpsPics(u8),
    /// A syntax value overflows the narrower Std field (e.g. a tile column
    /// width past `u16`). The parser bounds reads; this guards constructed
    /// inputs.
    FieldOverflow { field: &'static str, value: i64 },
}

impl std::fmt::Display for H265ParamsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            H265ParamsError::UnmappableProfileIdc(idc) => {
                write!(
                    f,
                    "general_profile_idc {idc} has no StdVideoH265ProfileIdc code point"
                )
            }
            H265ParamsError::SeparateColourPlanes => {
                write!(
                    f,
                    "4:4:4 with separate_colour_plane_flag (ChromaArrayType 0) is \
                     outside the punktfunk decode envelope"
                )
            }
            H265ParamsError::InvalidChromaFormatIdc(idc) => {
                write!(f, "invalid chroma_format_idc {idc}")
            }
            H265ParamsError::UnsupportedChromaFormat(idc) => {
                write!(
                    f,
                    "chroma_format_idc {idc} is outside the punktfunk decode envelope \
                     (4:2:0 and 4:4:4 only)"
                )
            }
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8,
                chroma_minus8,
            } => {
                write!(
                    f,
                    "bit depth {}/{} is outside the punktfunk decode envelope \
                     (8- and 10-bit, luma == chroma)",
                    luma_minus8 + 8,
                    chroma_minus8 + 8
                )
            }
            H265ParamsError::PalettePredictorInitializers => {
                write!(
                    f,
                    "SCC palette predictor initializers are not expressible by this conversion"
                )
            }
            H265ParamsError::TooManyShortTermRpsSets(n) => {
                write!(f, "{n} short-term RPS candidates exceed the spec's 64")
            }
            H265ParamsError::MissingShortTermRps { index } => {
                write!(f, "short-term RPS candidate {index} was never resolved")
            }
            H265ParamsError::RpsEntryOverflow {
                set,
                negative,
                positive,
            } => {
                write!(
                    f,
                    "short-term RPS candidate {set} holds {negative} negative / {positive} \
                     positive entries; the Std arrays hold 16 per side"
                )
            }
            H265ParamsError::NonMonotonicRps { set } => {
                write!(
                    f,
                    "short-term RPS candidate {set} has a non-monotonic DeltaPoc array"
                )
            }
            H265ParamsError::TooManyLongTermSpsPics(n) => {
                write!(f, "{n} long-term SPS candidates exceed the spec's 32")
            }
            H265ParamsError::FieldOverflow { field, value } => {
                write!(f, "{field} value {value} overflows its Std field")
            }
        }
    }
}

impl std::error::Error for H265ParamsError {}

/// Narrow into a Std field. The parser bounds reads, so overflow here is
/// hostile or constructed input — fail closed; a truncated tile width
/// would decode as garbage.
fn narrow<S, D>(field: &'static str, value: S) -> Result<D, H265ParamsError>
where
    D: TryFrom<S>,
    S: Copy + Into<i64>,
{
    D::try_from(value).map_err(|_| H265ParamsError::FieldOverflow {
        field,
        value: value.into(),
    })
}

/// Converted VPS plus the heap allocations its embedded pointers target.
/// Same ownership as [`crate::OwnedStdSps`]: boxed backing, movable wrapper,
/// no mutation, not `Clone` (re-convert instead).
#[derive(Debug)]
pub struct OwnedStdH265Vps {
    /// Boxed so [`Self::std`]'s address — what `pStdVPSs` points at — survives
    /// every move of the wrapper.
    std: Box<hh::StdVideoH265VideoParameterSet>,
    _ptl_backing: Box<hh::StdVideoH265ProfileTierLevel>,
    _dpb_backing: Box<hh::StdVideoH265DecPicBufMgr>,
    _hrd_backing: Box<hh::StdVideoH265HrdParameters>,
    _hrd_sub_backing: Box<[hh::StdVideoH265SubLayerHrdParameters]>,
}

impl OwnedStdH265Vps {
    /// The Std struct, valid for as long as `self` lives. Do not let a `Copy`
    /// of it outlive the wrapper — see [`crate::OwnedStdSps`].
    pub fn std(&self) -> &hh::StdVideoH265VideoParameterSet {
        &self.std
    }

    /// Lower `general_level_idc` to `max` when the stream over-claims. A
    /// level above the driver's `maxLevelIdc` is invalid usage; real demand
    /// is the session's coded extent and DPB depth. Runs before handover —
    /// the no-mutation contract covers blocks a live parameters object
    /// already points at.
    pub(crate) fn clamp_level(&mut self, max: hh::StdVideoH265LevelIdc) {
        if self._ptl_backing.general_level_idc > max {
            self._ptl_backing.general_level_idc = max;
        }
    }
}

/// Converted SPS plus the heap allocations its embedded pointers target.
/// Same ownership as [`crate::OwnedStdSps`]. Every pointer targets a block:
/// filled when the stream carries the data, zeroed with its flag clear when
/// not. A driver that reads through a pointer whatever the flag says (Intel,
/// Windows, session-parameters create) then finds a struct instead of null.
#[derive(Debug)]
pub struct OwnedStdH265Sps {
    /// Boxed so `pStdSPSs` is this field's address across wrapper moves.
    std: Box<hh::StdVideoH265SequenceParameterSet>,
    _ptl_backing: Box<hh::StdVideoH265ProfileTierLevel>,
    _dpb_backing: Box<hh::StdVideoH265DecPicBufMgr>,
    _scaling_backing: Box<hh::StdVideoH265ScalingLists>,
    /// `pShortTermRefPicSet`'s target: `num_short_term_ref_pic_sets` entries,
    /// one zeroed entry when the SPS carries none.
    _st_rps_backing: Box<[hh::StdVideoH265ShortTermRefPicSet]>,
    _lt_backing: Box<hh::StdVideoH265LongTermRefPicsSps>,
    _vui_backing: Box<hh::StdVideoH265SequenceParameterSetVui>,
    _vui_hrd_backing: Box<hh::StdVideoH265HrdParameters>,
    _vui_hrd_sub_backing: Box<[hh::StdVideoH265SubLayerHrdParameters]>,
    _palette_backing: Box<hh::StdVideoH265PredictorPaletteEntries>,
}

/// A zeroed video-header Std struct: the all-clear baseline every block starts from.
fn zeroed_std<T: Copy>() -> T {
    // SAFETY: the callers pass the `hh` Std structs, plain C data of integers, arrays
    // and raw pointers, for which the all-zero bit pattern is a valid value.
    unsafe { std::mem::zeroed() }
}

/// A zeroed HRD block with one zeroed sub-layer entry for each of its two
/// arrays, so every pointer under it is a struct too.
fn zeroed_hrd() -> (
    Box<hh::StdVideoH265HrdParameters>,
    Box<[hh::StdVideoH265SubLayerHrdParameters]>,
) {
    let sub: Box<[hh::StdVideoH265SubLayerHrdParameters]> = Box::new([zeroed_std()]);
    let mut hrd: hh::StdVideoH265HrdParameters = zeroed_std();
    hrd.pSubLayerHrdParametersNal = sub.as_ptr();
    hrd.pSubLayerHrdParametersVcl = sub.as_ptr();
    (Box::new(hrd), sub)
}

impl OwnedStdH265Sps {
    /// The Std struct, valid for as long as `self` lives (see [`crate::OwnedStdSps`]).
    pub fn std(&self) -> &hh::StdVideoH265SequenceParameterSet {
        &self.std
    }

    /// Same clamp as [`OwnedStdH265Vps::clamp_level`].
    pub(crate) fn clamp_level(&mut self, max: hh::StdVideoH265LevelIdc) {
        if self._ptl_backing.general_level_idc > max {
            self._ptl_backing.general_level_idc = max;
        }
    }
}

/// Converted PPS plus the blocks its pointers target: scaling lists (filled or
/// zeroed) and a zeroed palette. Same ownership as [`crate::OwnedStdSps`].
#[derive(Debug)]
pub struct OwnedStdH265Pps {
    /// Boxed so `pStdPPSs` is this field's address across wrapper moves.
    std: Box<hh::StdVideoH265PictureParameterSet>,
    _scaling_backing: Box<hh::StdVideoH265ScalingLists>,
    _palette_backing: Box<hh::StdVideoH265PredictorPaletteEntries>,
}

impl OwnedStdH265Pps {
    /// The Std struct, valid for as long as `self` lives (see [`crate::OwnedStdSps`]).
    pub fn std(&self) -> &hh::StdVideoH265PictureParameterSet {
        &self.std
    }
}

/// H.265 `general_level_idc` (value-coded: 30 x the level number, Table A.8)
/// to Vulkan's index-coded `StdVideoH265LevelIdc`. Code points ascend with
/// the level, so a driver's `maxLevelIdc` gate compares them numerically.
pub(crate) const fn level_to_std(level: Level) -> hh::StdVideoH265LevelIdc {
    match level {
        Level::L1 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_1_0,
        Level::L2 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_0,
        Level::L2_1 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_1,
        Level::L3 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_0,
        Level::L3_1 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_1,
        Level::L4 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_0,
        Level::L4_1 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1,
        Level::L5 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_0,
        Level::L5_1 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_1,
        Level::L5_2 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_2,
        Level::L6 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0,
        Level::L6_1 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_1,
        Level::L6_2 => hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2,
    }
}

/// `general_profile_idc` to `StdVideoH265ProfileIdc`. Recognised code points
/// equal the `profile_idc` values they name, so they pass through. Shared
/// with [`crate::caps_h265::H265ProfileKey`]: the caps query builds the
/// profile key from the picture's idc before conversion, and both must agree.
pub(crate) fn profile_to_std(idc: u8) -> Result<hh::StdVideoH265ProfileIdc, H265ParamsError> {
    match u32::from(idc) {
        p @ 1..=4 => Ok(p),
        _ => Err(H265ParamsError::UnmappableProfileIdc(idc)),
    }
}

/// `profile_tier_level()` to the Std block (always pointer-backed in VPS and SPS).
fn ptl_to_std(ptl: &ProfileTierLevel) -> Result<hh::StdVideoH265ProfileTierLevel, H265ParamsError> {
    // SAFETY: StdVideoH265ProfileTierLevel is a plain-C bindgen struct of a
    // bitfield word and two enum ints; all-zero is a valid value for every field.
    let mut std: hh::StdVideoH265ProfileTierLevel = unsafe { std::mem::zeroed() };
    std.flags
        .set_general_tier_flag(u32::from(ptl.general_tier_flag));
    std.flags
        .set_general_progressive_source_flag(u32::from(ptl.general_progressive_source_flag));
    std.flags
        .set_general_interlaced_source_flag(u32::from(ptl.general_interlaced_source_flag));
    std.flags
        .set_general_non_packed_constraint_flag(u32::from(ptl.general_non_packed_constraint_flag));
    std.flags
        .set_general_frame_only_constraint_flag(u32::from(ptl.general_frame_only_constraint_flag));
    std.general_profile_idc = profile_to_std(ptl.general_profile_idc)?;
    std.general_level_idc = level_to_std(ptl.general_level_idc);
    Ok(std)
}

/// Pack the parser's scaling lists into the Std layout. The parser has
/// already run 7.4.5 (explicit coefficients, matrix-id prediction, Table
/// 7-5/7-6 defaults), so the arrays are fully resolved.
///
/// 32x32 (sizeId 3) has two lists, at parser matrix ids 0 (intra) and 3
/// (inter) — 7.4.5 steps matrixId by 3 there. The Std array holds them at
/// indices 0 and 1. Std DC fields carry the value
/// (`scaling_list_dc_coef_minus8 + 8`, 1..255), not the minus8 syntax.
fn scaling_lists_to_std(
    lists: &ScalingLists,
) -> Result<hh::StdVideoH265ScalingLists, H265ParamsError> {
    // SAFETY: StdVideoH265ScalingLists is a plain-C bindgen struct of byte
    // arrays; all-zero is a valid value for every field.
    let mut std: hh::StdVideoH265ScalingLists = unsafe { std::mem::zeroed() };
    std.ScalingList4x4 = lists.scaling_list_4x4;
    std.ScalingList8x8 = lists.scaling_list_8x8;
    std.ScalingList16x16 = lists.scaling_list_16x16;
    std.ScalingList32x32 = [lists.scaling_list_32x32[0], lists.scaling_list_32x32[3]];
    for i in 0..6 {
        std.ScalingListDCCoef16x16[i] = narrow(
            "scaling_list_dc_coef_minus8_16x16 + 8",
            i32::from(lists.scaling_list_dc_coef_minus8_16x16[i]) + 8,
        )?;
    }
    for (dst, src) in [0usize, 3].into_iter().enumerate() {
        std.ScalingListDCCoef32x32[dst] = narrow(
            "scaling_list_dc_coef_minus8_32x32 + 8",
            i32::from(lists.scaling_list_dc_coef_minus8_32x32[src]) + 8,
        )?;
    }
    Ok(std)
}

/// One resolved SPS short-term RPS candidate as a non-predicted Std set.
/// The parser flattened 7.4.8 prediction, so flags stay 0 and `DeltaPocSX`
/// re-encodes as `delta_poc_sX_minus1` (`DeltaPocS0` strictly decreasing
/// negative, `DeltaPocS1` strictly increasing positive).
fn st_rps_to_std(
    index: usize,
    set: &ShortTermRefPicSet,
) -> Result<hh::StdVideoH265ShortTermRefPicSet, H265ParamsError> {
    let negative = usize::from(set.num_negative_pics);
    let positive = usize::from(set.num_positive_pics);
    if negative > 16 || positive > 16 {
        return Err(H265ParamsError::RpsEntryOverflow {
            set: index,
            negative: set.num_negative_pics,
            positive: set.num_positive_pics,
        });
    }

    // SAFETY: StdVideoH265ShortTermRefPicSet is a plain-C bindgen struct of a
    // bitfield word, integers and integer arrays; all-zero is valid for every
    // field and is exactly the non-predicted baseline (prediction flags 0).
    let mut std: hh::StdVideoH265ShortTermRefPicSet = unsafe { std::mem::zeroed() };
    std.num_negative_pics = set.num_negative_pics;
    std.num_positive_pics = set.num_positive_pics;

    // 7.4.8: ue(v) steps are bounded at 2^15 - 1, so a legal step is 1..=32768.
    let mut prev: i64 = 0;
    for i in 0..negative {
        let cur = i64::from(set.delta_poc_s0[i]);
        let step = prev - cur;
        if !(1..=32768).contains(&step) {
            return Err(H265ParamsError::NonMonotonicRps { set: index });
        }
        std.delta_poc_s0_minus1[i] = (step - 1) as u16;
        if set.used_by_curr_pic_s0[i] {
            std.used_by_curr_pic_s0_flag |= 1 << i;
        }
        prev = cur;
    }
    let mut prev: i64 = 0;
    for i in 0..positive {
        let cur = i64::from(set.delta_poc_s1[i]);
        let step = cur - prev;
        if !(1..=32768).contains(&step) {
            return Err(H265ParamsError::NonMonotonicRps { set: index });
        }
        std.delta_poc_s1_minus1[i] = (step - 1) as u16;
        if set.used_by_curr_pic_s1[i] {
            std.used_by_curr_pic_s1_flag |= 1 << i;
        }
        prev = cur;
    }
    Ok(std)
}

/// Shared envelope gate: profile, chroma format, bit depth. Runs before
/// any allocation so a rejection is cheap and total.
fn check_envelope(sps: &Sps) -> Result<(), H265ParamsError> {
    profile_to_std(sps.profile_tier_level.general_profile_idc)?;
    if sps.chroma_format_idc > 3 {
        return Err(H265ParamsError::InvalidChromaFormatIdc(
            sps.chroma_format_idc,
        ));
    }
    if sps.chroma_format_idc == 0 || sps.chroma_format_idc == 2 {
        return Err(H265ParamsError::UnsupportedChromaFormat(
            sps.chroma_format_idc,
        ));
    }
    // ChromaArrayType 0: three monochrome-coded planes. Letting it through
    // would fail at the driver or map planes wrongly.
    if sps.chroma_format_idc == 3 && sps.separate_colour_plane_flag {
        return Err(H265ParamsError::SeparateColourPlanes);
    }
    if sps.bit_depth_luma_minus8 != sps.bit_depth_chroma_minus8
        || !matches!(sps.bit_depth_luma_minus8, 0 | 2)
    {
        return Err(H265ParamsError::UnsupportedBitDepth {
            luma_minus8: sps.bit_depth_luma_minus8,
            chroma_minus8: sps.bit_depth_chroma_minus8,
        });
    }
    if sps
        .scc_extension
        .palette_predictor_initializers_present_flag
    {
        return Err(H265ParamsError::PalettePredictorInitializers);
    }
    Ok(())
}

/// Convert one VPS. HRD/timing is skipped (module docs); DPB manager and
/// profile/tier/level ride behind owned pointers.
pub fn vps_to_std_h265(vps: &Vps) -> Result<OwnedStdH265Vps, H265ParamsError> {
    let ptl_backing = Box::new(ptl_to_std(&vps.profile_tier_level)?);

    // SAFETY: StdVideoH265DecPicBufMgr is a plain-C bindgen struct of integer
    // arrays; all-zero is a valid value for every field.
    let mut dpb: hh::StdVideoH265DecPicBufMgr = unsafe { std::mem::zeroed() };
    dpb.max_latency_increase_plus1 = vps.max_latency_increase_plus1;
    for i in 0..7 {
        // Parser arrays are u32; spec bounds both inside u8 (MaxDpbSize <= 16).
        dpb.max_dec_pic_buffering_minus1[i] = narrow(
            "vps_max_dec_pic_buffering_minus1",
            vps.max_dec_pic_buffering_minus1[i],
        )?;
        dpb.max_num_reorder_pics[i] =
            narrow("vps_max_num_reorder_pics", vps.max_num_reorder_pics[i])?;
    }
    let dpb_backing = Box::new(dpb);

    // SAFETY: StdVideoH265VideoParameterSet is a plain-C bindgen struct of a
    // bitfield word, integers and const pointers; all-zero is valid for every
    // field (null pointers) and is the baseline the writes below fill.
    let mut std: hh::StdVideoH265VideoParameterSet = unsafe { std::mem::zeroed() };
    std.flags
        .set_vps_temporal_id_nesting_flag(u32::from(vps.temporal_id_nesting_flag));
    std.flags
        .set_vps_sub_layer_ordering_info_present_flag(u32::from(
            vps.sub_layer_ordering_info_present_flag,
        ));
    // Timing flags stay 0 with their fields: a present-flag over a null
    // `pHrdParameters` would be a half-truth.
    std.vps_video_parameter_set_id = vps.video_parameter_set_id;
    std.vps_max_sub_layers_minus1 = vps.max_sub_layers_minus1;
    std.pDecPicBufMgr = &*dpb_backing;
    std.pProfileTierLevel = &*ptl_backing;
    // Zeroed HRD with its sub-layer arrays: the flags say absent, but a driver
    // that reads through these pointers regardless (Intel, Windows) must find a
    // struct, not null. Same shape the FFmpeg decoder hands every driver.
    let (hrd_backing, hrd_sub_backing) = zeroed_hrd();
    std.pHrdParameters = &*hrd_backing;

    Ok(OwnedStdH265Vps {
        std: Box::new(std),
        _ptl_backing: ptl_backing,
        _dpb_backing: dpb_backing,
        _hrd_backing: hrd_backing,
        _hrd_sub_backing: hrd_sub_backing,
    })
}

/// Minimal Std VPS from the SPS that names it, for streams whose VPS NALU
/// was lost (`sps.vps` is `None` unless the parser saw one). Every stream
/// must carry a VPS (7.4.2.1) and Vulkan requires the parameters object to
/// hold the VPS the SPS names. The fallback restates the facts the SPS
/// already carries (ids, sub-layer count, PTL, DPB sizing) — the VPS's own
/// additions (timing, layer sets) are in the skip category.
pub fn fallback_vps_from_sps(sps: &Sps) -> Result<OwnedStdH265Vps, H265ParamsError> {
    let ptl_backing = Box::new(ptl_to_std(&sps.profile_tier_level)?);
    let dpb_backing = Box::new(sps_dec_pic_buf_mgr(sps));

    // SAFETY: as in vps_to_std_h265 — all-zero is a valid baseline.
    let mut std: hh::StdVideoH265VideoParameterSet = unsafe { std::mem::zeroed() };
    std.vps_video_parameter_set_id = sps.video_parameter_set_id;
    std.vps_max_sub_layers_minus1 = sps.max_sub_layers_minus1;
    std.flags
        .set_vps_temporal_id_nesting_flag(u32::from(sps.temporal_id_nesting_flag));
    std.flags
        .set_vps_sub_layer_ordering_info_present_flag(u32::from(
            sps.sub_layer_ordering_info_present_flag,
        ));
    std.pDecPicBufMgr = &*dpb_backing;
    std.pProfileTierLevel = &*ptl_backing;

    // Zeroed HRD with its sub-layer arrays, as in the parsed-VPS path above.
    let (hrd_backing, hrd_sub_backing) = zeroed_hrd();
    std.pHrdParameters = &*hrd_backing;

    Ok(OwnedStdH265Vps {
        std: Box::new(std),
        _ptl_backing: ptl_backing,
        _dpb_backing: dpb_backing,
        _hrd_backing: hrd_backing,
        _hrd_sub_backing: hrd_sub_backing,
    })
}

/// SPS sub-layer ordering as the Std DPB-manager block. Infallible: the
/// parser's SPS arrays are already `u8`.
fn sps_dec_pic_buf_mgr(sps: &Sps) -> hh::StdVideoH265DecPicBufMgr {
    // SAFETY: plain-C bindgen struct of integer arrays; all-zero is valid.
    let mut dpb: hh::StdVideoH265DecPicBufMgr = unsafe { std::mem::zeroed() };
    for i in 0..7 {
        dpb.max_latency_increase_plus1[i] = u32::from(sps.max_latency_increase_plus1[i]);
    }
    dpb.max_dec_pic_buffering_minus1 = sps.max_dec_pic_buffering_minus1;
    dpb.max_num_reorder_pics = sps.max_num_reorder_pics;
    dpb
}

/// Convert one SPS. VUI is skipped; SCC palette data is rejected, never
/// dropped (module docs).
pub fn sps_to_std_h265(sps: &Sps) -> Result<OwnedStdH265Sps, H265ParamsError> {
    check_envelope(sps)?;
    if sps.num_short_term_ref_pic_sets > 64 {
        return Err(H265ParamsError::TooManyShortTermRpsSets(
            sps.num_short_term_ref_pic_sets,
        ));
    }
    if sps.num_long_term_ref_pics_sps > 32 {
        return Err(H265ParamsError::TooManyLongTermSpsPics(
            sps.num_long_term_ref_pics_sps,
        ));
    }

    let ptl_backing = Box::new(ptl_to_std(&sps.profile_tier_level)?);
    let dpb_backing = Box::new(sps_dec_pic_buf_mgr(sps));

    let scaling_backing = sps
        .scaling_list_data_present_flag
        .then(|| scaling_lists_to_std(&sps.scaling_list).map(Box::new))
        .transpose()?;

    // Count and pointer share one condition: a declared candidate count only
    // ever rides a real array of exactly that many converted sets.
    let st_rps_backing = (sps.num_short_term_ref_pic_sets > 0)
        .then(
            || -> Result<Box<[hh::StdVideoH265ShortTermRefPicSet]>, H265ParamsError> {
                let count = usize::from(sps.num_short_term_ref_pic_sets);
                let mut sets = Vec::with_capacity(count);
                for index in 0..count {
                    let set = sps
                        .short_term_ref_pic_set
                        .get(index)
                        .ok_or(H265ParamsError::MissingShortTermRps { index })?;
                    sets.push(st_rps_to_std(index, set)?);
                }
                Ok(sets.into_boxed_slice())
            },
        )
        .transpose()?;

    // Backed whenever the flag is set, even at num=0: the header requires a
    // valid `pLongTermRefPicsSps` if `long_term_ref_pics_present_flag` is set.
    // `flag=1, num=0` is the LTR/RFI-recovery shape (slice-signalled LTR, no
    // SPS candidates); the all-zero struct is the correct content for num=0.
    let lt_backing = sps.long_term_ref_pics_present_flag.then(|| {
        // SAFETY: plain-C bindgen struct of a mask and an integer array;
        // all-zero is valid.
        let mut lt: hh::StdVideoH265LongTermRefPicsSps = unsafe { std::mem::zeroed() };
        for i in 0..usize::from(sps.num_long_term_ref_pics_sps) {
            if sps.used_by_curr_pic_lt_sps_flag[i] {
                lt.used_by_curr_pic_lt_sps_flag |= 1 << i;
            }
        }
        lt.lt_ref_pic_poc_lsb_sps = sps.lt_ref_pic_poc_lsb_sps;
        Box::new(lt)
    });

    // SAFETY: StdVideoH265SequenceParameterSet is a plain-C bindgen struct of a
    // bitfield word, integers and const pointers; all-zero is valid for every
    // field (null pointers) and is the "everything absent" baseline the field
    // writes below build on.
    let mut std: hh::StdVideoH265SequenceParameterSet = unsafe { std::mem::zeroed() };

    std.flags
        .set_sps_temporal_id_nesting_flag(u32::from(sps.temporal_id_nesting_flag));
    std.flags
        .set_separate_colour_plane_flag(u32::from(sps.separate_colour_plane_flag));
    std.flags
        .set_conformance_window_flag(u32::from(sps.conformance_window_flag));
    std.flags
        .set_sps_sub_layer_ordering_info_present_flag(u32::from(
            sps.sub_layer_ordering_info_present_flag,
        ));
    std.flags
        .set_scaling_list_enabled_flag(u32::from(sps.scaling_list_enabled_flag));
    // Enabled-but-absent: the driver applies Table 7-5/7-6 itself (7.4.5).
    // Declaring data we did not convert would pair a flag with a null pointer.
    std.flags
        .set_sps_scaling_list_data_present_flag(u32::from(sps.scaling_list_data_present_flag));
    std.flags
        .set_amp_enabled_flag(u32::from(sps.amp_enabled_flag));
    std.flags.set_sample_adaptive_offset_enabled_flag(u32::from(
        sps.sample_adaptive_offset_enabled_flag,
    ));
    std.flags
        .set_pcm_enabled_flag(u32::from(sps.pcm_enabled_flag));
    std.flags
        .set_pcm_loop_filter_disabled_flag(u32::from(sps.pcm_loop_filter_disabled_flag));
    std.flags
        .set_long_term_ref_pics_present_flag(u32::from(sps.long_term_ref_pics_present_flag));
    std.flags
        .set_sps_temporal_mvp_enabled_flag(u32::from(sps.temporal_mvp_enabled_flag));
    std.flags.set_strong_intra_smoothing_enabled_flag(u32::from(
        sps.strong_intra_smoothing_enabled_flag,
    ));
    // `vui_parameters_present_flag` stays 0; colour rides the PicturePlan.
    std.flags
        .set_sps_extension_present_flag(u32::from(sps.extension_present_flag));
    std.flags
        .set_sps_range_extension_flag(u32::from(sps.range_extension_flag));
    let rext = &sps.range_extension;
    std.flags
        .set_transform_skip_rotation_enabled_flag(u32::from(
            rext.transform_skip_rotation_enabled_flag,
        ));
    std.flags.set_transform_skip_context_enabled_flag(u32::from(
        rext.transform_skip_context_enabled_flag,
    ));
    std.flags
        .set_implicit_rdpcm_enabled_flag(u32::from(rext.implicit_rdpcm_enabled_flag));
    std.flags
        .set_explicit_rdpcm_enabled_flag(u32::from(rext.explicit_rdpcm_enabled_flag));
    std.flags
        .set_extended_precision_processing_flag(u32::from(rext.extended_precision_processing_flag));
    std.flags
        .set_intra_smoothing_disabled_flag(u32::from(rext.intra_smoothing_disabled_flag));
    std.flags.set_high_precision_offsets_enabled_flag(u32::from(
        rext.high_precision_offsets_enabled_flag,
    ));
    std.flags
        .set_persistent_rice_adaptation_enabled_flag(u32::from(
            rext.persistent_rice_adaptation_enabled_flag,
        ));
    std.flags.set_cabac_bypass_alignment_enabled_flag(u32::from(
        rext.cabac_bypass_alignment_enabled_flag,
    ));
    let scc = &sps.scc_extension;
    std.flags
        .set_sps_scc_extension_flag(u32::from(sps.scc_extension_flag));
    // Copy the flag even though the planner rejects SCC self-reference:
    // conversion is not envelope-coupled beyond representability.
    std.flags
        .set_sps_curr_pic_ref_enabled_flag(u32::from(scc.curr_pic_ref_enabled_flag));
    std.flags
        .set_palette_mode_enabled_flag(u32::from(scc.palette_mode_enabled_flag));
    // Flag stays 0: `check_envelope` rejected any SPS that set it, so the flag
    // and the null `pPredictorPaletteEntries` cannot disagree.
    std.flags
        .set_intra_boundary_filtering_disabled_flag(u32::from(
            scc.intra_boundary_filtering_disabled_flag,
        ));

    // Chroma format code points equal the `chroma_format_idc` values (0..3).
    std.chroma_format_idc = u32::from(sps.chroma_format_idc);
    std.pic_width_in_luma_samples = u32::from(sps.pic_width_in_luma_samples);
    std.pic_height_in_luma_samples = u32::from(sps.pic_height_in_luma_samples);
    std.sps_video_parameter_set_id = sps.video_parameter_set_id;
    std.sps_max_sub_layers_minus1 = sps.max_sub_layers_minus1;
    std.sps_seq_parameter_set_id = sps.seq_parameter_set_id;
    std.bit_depth_luma_minus8 = sps.bit_depth_luma_minus8;
    std.bit_depth_chroma_minus8 = sps.bit_depth_chroma_minus8;
    std.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_pic_order_cnt_lsb_minus4;
    std.log2_min_luma_coding_block_size_minus3 = sps.log2_min_luma_coding_block_size_minus3;
    std.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_luma_coding_block_size;
    std.log2_min_luma_transform_block_size_minus2 = sps.log2_min_luma_transform_block_size_minus2;
    std.log2_diff_max_min_luma_transform_block_size =
        sps.log2_diff_max_min_luma_transform_block_size;
    std.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
    std.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
    std.num_short_term_ref_pic_sets = sps.num_short_term_ref_pic_sets;
    std.num_long_term_ref_pics_sps = sps.num_long_term_ref_pics_sps;
    std.pcm_sample_bit_depth_luma_minus1 = sps.pcm_sample_bit_depth_luma_minus1;
    std.pcm_sample_bit_depth_chroma_minus1 = sps.pcm_sample_bit_depth_chroma_minus1;
    std.log2_min_pcm_luma_coding_block_size_minus3 = sps.log2_min_pcm_luma_coding_block_size_minus3;
    std.log2_diff_max_min_pcm_luma_coding_block_size =
        sps.log2_diff_max_min_pcm_luma_coding_block_size;
    std.palette_max_size = scc.palette_max_size;
    std.delta_palette_max_predictor_size = scc.delta_palette_max_predictor_size;
    std.motion_vector_resolution_control_idc = scc.motion_vector_resolution_control_idc;
    std.sps_num_palette_predictor_initializers_minus1 =
        scc.num_palette_predictor_initializer_minus1;
    std.conf_win_left_offset = sps.conf_win_left_offset;
    std.conf_win_right_offset = sps.conf_win_right_offset;
    std.conf_win_top_offset = sps.conf_win_top_offset;
    std.conf_win_bottom_offset = sps.conf_win_bottom_offset;

    std.pProfileTierLevel = &*ptl_backing;
    std.pDecPicBufMgr = &*dpb_backing;
    // Absent data keeps its flag clear and gets a zeroed block: see the struct docs.
    let scaling_backing = scaling_backing.unwrap_or_else(|| Box::new(zeroed_std()));
    let st_rps_backing: Box<[hh::StdVideoH265ShortTermRefPicSet]> =
        st_rps_backing.unwrap_or_else(|| Box::new([zeroed_std()]));
    let lt_backing = lt_backing.unwrap_or_else(|| Box::new(zeroed_std()));
    let (vui_hrd_backing, vui_hrd_sub_backing) = zeroed_hrd();
    let mut vui: hh::StdVideoH265SequenceParameterSetVui = zeroed_std();
    vui.pHrdParameters = &*vui_hrd_backing;
    let vui_backing = Box::new(vui);
    let palette_backing: Box<hh::StdVideoH265PredictorPaletteEntries> = Box::new(zeroed_std());
    std.pScalingLists = &*scaling_backing;
    std.pShortTermRefPicSet = st_rps_backing.as_ptr();
    std.pLongTermRefPicsSps = &*lt_backing;
    std.pSequenceParameterSetVui = &*vui_backing;
    std.pPredictorPaletteEntries = &*palette_backing;

    Ok(OwnedStdH265Sps {
        std: Box::new(std),
        _ptl_backing: ptl_backing,
        _dpb_backing: dpb_backing,
        _scaling_backing: scaling_backing,
        _st_rps_backing: st_rps_backing,
        _lt_backing: lt_backing,
        _vui_backing: vui_backing,
        _vui_hrd_backing: vui_hrd_backing,
        _vui_hrd_sub_backing: vui_hrd_sub_backing,
        _palette_backing: palette_backing,
    })
}

/// Convert one PPS. SCC palette data is rejected, never dropped; every
/// narrower Std field is checked, never truncated.
pub fn pps_to_std_h265(pps: &Pps) -> Result<OwnedStdH265Pps, H265ParamsError> {
    if pps
        .scc_extension
        .palette_predictor_initializers_present_flag
    {
        return Err(H265ParamsError::PalettePredictorInitializers);
    }

    let scaling_backing = pps
        .scaling_list_data_present_flag
        .then(|| scaling_lists_to_std(&pps.scaling_list).map(Box::new))
        .transpose()?;

    // SAFETY: StdVideoH265PictureParameterSet is a plain-C bindgen struct of a
    // bitfield word, integers, integer arrays and const pointers; all-zero is
    // valid for every field (null pointers) and is the baseline the writes fill.
    let mut std: hh::StdVideoH265PictureParameterSet = unsafe { std::mem::zeroed() };

    std.flags
        .set_dependent_slice_segments_enabled_flag(u32::from(
            pps.dependent_slice_segments_enabled_flag,
        ));
    std.flags
        .set_output_flag_present_flag(u32::from(pps.output_flag_present_flag));
    std.flags
        .set_sign_data_hiding_enabled_flag(u32::from(pps.sign_data_hiding_enabled_flag));
    std.flags
        .set_cabac_init_present_flag(u32::from(pps.cabac_init_present_flag));
    std.flags
        .set_constrained_intra_pred_flag(u32::from(pps.constrained_intra_pred_flag));
    std.flags
        .set_transform_skip_enabled_flag(u32::from(pps.transform_skip_enabled_flag));
    std.flags
        .set_cu_qp_delta_enabled_flag(u32::from(pps.cu_qp_delta_enabled_flag));
    std.flags
        .set_pps_slice_chroma_qp_offsets_present_flag(u32::from(
            pps.slice_chroma_qp_offsets_present_flag,
        ));
    std.flags
        .set_weighted_pred_flag(u32::from(pps.weighted_pred_flag));
    std.flags
        .set_weighted_bipred_flag(u32::from(pps.weighted_bipred_flag));
    std.flags
        .set_transquant_bypass_enabled_flag(u32::from(pps.transquant_bypass_enabled_flag));
    std.flags
        .set_tiles_enabled_flag(u32::from(pps.tiles_enabled_flag));
    std.flags
        .set_entropy_coding_sync_enabled_flag(u32::from(pps.entropy_coding_sync_enabled_flag));
    std.flags
        .set_uniform_spacing_flag(u32::from(pps.uniform_spacing_flag));
    std.flags
        .set_loop_filter_across_tiles_enabled_flag(u32::from(
            pps.loop_filter_across_tiles_enabled_flag,
        ));
    std.flags
        .set_pps_loop_filter_across_slices_enabled_flag(u32::from(
            pps.loop_filter_across_slices_enabled_flag,
        ));
    std.flags
        .set_deblocking_filter_control_present_flag(u32::from(
            pps.deblocking_filter_control_present_flag,
        ));
    std.flags
        .set_deblocking_filter_override_enabled_flag(u32::from(
            pps.deblocking_filter_override_enabled_flag,
        ));
    std.flags
        .set_pps_deblocking_filter_disabled_flag(u32::from(pps.deblocking_filter_disabled_flag));
    std.flags
        .set_pps_scaling_list_data_present_flag(u32::from(pps.scaling_list_data_present_flag));
    std.flags
        .set_lists_modification_present_flag(u32::from(pps.lists_modification_present_flag));
    std.flags
        .set_slice_segment_header_extension_present_flag(u32::from(
            pps.slice_segment_header_extension_present_flag,
        ));
    std.flags
        .set_pps_extension_present_flag(u32::from(pps.extension_present_flag));
    let rext = &pps.range_extension;
    std.flags
        .set_cross_component_prediction_enabled_flag(u32::from(
            rext.cross_component_prediction_enabled_flag,
        ));
    std.flags
        .set_chroma_qp_offset_list_enabled_flag(u32::from(rext.chroma_qp_offset_list_enabled_flag));
    let scc = &pps.scc_extension;
    std.flags
        .set_pps_curr_pic_ref_enabled_flag(u32::from(scc.curr_pic_ref_enabled_flag));
    std.flags
        .set_residual_adaptive_colour_transform_enabled_flag(u32::from(
            scc.residual_adaptive_colour_transform_enabled_flag,
        ));
    std.flags
        .set_pps_slice_act_qp_offsets_present_flag(u32::from(
            scc.slice_act_qp_offsets_present_flag,
        ));
    // `pps_palette_predictor_initializers_present_flag` stays 0 (rejected above).
    std.flags
        .set_monochrome_palette_flag(u32::from(scc.monochrome_palette_flag));
    std.flags
        .set_pps_range_extension_flag(u32::from(pps.range_extension_flag));

    std.pps_pic_parameter_set_id = pps.pic_parameter_set_id;
    std.pps_seq_parameter_set_id = pps.seq_parameter_set_id;
    // The VPS id the Std PPS names is the one its own SPS references — the
    // parser resolved that chain at parse time.
    std.sps_video_parameter_set_id = pps.sps.video_parameter_set_id;
    std.num_extra_slice_header_bits = pps.num_extra_slice_header_bits;
    std.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    std.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    std.init_qp_minus26 = pps.init_qp_minus26;
    std.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
    std.pps_cb_qp_offset = pps.cb_qp_offset;
    std.pps_cr_qp_offset = pps.cr_qp_offset;
    std.pps_beta_offset_div2 = pps.beta_offset_div2;
    std.pps_tc_offset_div2 = pps.tc_offset_div2;
    std.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level_minus2;
    std.log2_max_transform_skip_block_size_minus2 = narrow(
        "log2_max_transform_skip_block_size_minus2",
        rext.log2_max_transform_skip_block_size_minus2,
    )?;
    std.diff_cu_chroma_qp_offset_depth = narrow(
        "diff_cu_chroma_qp_offset_depth",
        rext.diff_cu_chroma_qp_offset_depth,
    )?;
    std.chroma_qp_offset_list_len_minus1 = narrow(
        "chroma_qp_offset_list_len_minus1",
        rext.chroma_qp_offset_list_len_minus1,
    )?;
    for i in 0..6 {
        std.cb_qp_offset_list[i] = narrow("cb_qp_offset_list", rext.cb_qp_offset_list[i])?;
        std.cr_qp_offset_list[i] = narrow("cr_qp_offset_list", rext.cr_qp_offset_list[i])?;
    }
    std.log2_sao_offset_scale_luma = narrow(
        "log2_sao_offset_scale_luma",
        rext.log2_sao_offset_scale_luma,
    )?;
    std.log2_sao_offset_scale_chroma = narrow(
        "log2_sao_offset_scale_chroma",
        rext.log2_sao_offset_scale_chroma,
    )?;
    std.pps_act_y_qp_offset_plus5 = scc.act_y_qp_offset_plus5;
    std.pps_act_cb_qp_offset_plus5 = scc.act_cb_qp_offset_plus5;
    std.pps_act_cr_qp_offset_plus3 = scc.act_cr_qp_offset_plus3;
    std.pps_num_palette_predictor_initializers = scc.num_palette_predictor_initializers;
    std.luma_bit_depth_entry_minus8 = scc.luma_bit_depth_entry_minus8;
    std.chroma_bit_depth_entry_minus8 = scc.chroma_bit_depth_entry_minus8;
    std.num_tile_columns_minus1 = pps.num_tile_columns_minus1;
    std.num_tile_rows_minus1 = pps.num_tile_rows_minus1;
    for i in 0..19 {
        std.column_width_minus1[i] = narrow("column_width_minus1", pps.column_width_minus1[i])?;
    }
    for i in 0..21 {
        std.row_height_minus1[i] = narrow("row_height_minus1", pps.row_height_minus1[i])?;
    }

    // Absent data keeps its flag clear and gets a zeroed block (struct docs).
    let scaling_backing = scaling_backing.unwrap_or_else(|| Box::new(zeroed_std()));
    let palette_backing: Box<hh::StdVideoH265PredictorPaletteEntries> = Box::new(zeroed_std());
    std.pScalingLists = &*scaling_backing;
    std.pPredictorPaletteEntries = &*palette_backing;

    Ok(OwnedStdH265Pps {
        std: Box::new(std),
        _scaling_backing: scaling_backing,
        _palette_backing: palette_backing,
    })
}

#[cfg(test)]
mod tests {
    use cros_codecs::codec::h265::parser::PpsRangeExtension;
    use cros_codecs::codec::h265::parser::PpsSccExtension;
    use cros_codecs::codec::h265::parser::ProfileTierLevel;
    use cros_codecs::codec::h265::parser::SpsRangeExtension;
    use std::rc::Rc;

    use super::*;

    /// `vui_parameters_present_flag` is true at the source because the
    /// conversion must not copy it.
    fn full_sps() -> Sps {
        Sps {
            video_parameter_set_id: 2,
            max_sub_layers_minus1: 1,
            temporal_id_nesting_flag: true,
            profile_tier_level: ProfileTierLevel {
                general_profile_idc: 2, // Main 10
                general_tier_flag: true,
                general_progressive_source_flag: true,
                general_interlaced_source_flag: false,
                general_non_packed_constraint_flag: true,
                general_frame_only_constraint_flag: false,
                general_level_idc: Level::L4_1,
                ..Default::default()
            },
            seq_parameter_set_id: 5,
            chroma_format_idc: 1,
            pic_width_in_luma_samples: 1920,
            pic_height_in_luma_samples: 1080,
            conformance_window_flag: true,
            conf_win_left_offset: 1,
            conf_win_right_offset: 2,
            conf_win_top_offset: 3,
            conf_win_bottom_offset: 4,
            bit_depth_luma_minus8: 2,
            bit_depth_chroma_minus8: 2,
            log2_max_pic_order_cnt_lsb_minus4: 6,
            sub_layer_ordering_info_present_flag: true,
            max_dec_pic_buffering_minus1: [5, 6, 0, 0, 0, 0, 0],
            max_num_reorder_pics: [1, 2, 0, 0, 0, 0, 0],
            max_latency_increase_plus1: [7, 8, 0, 0, 0, 0, 0],
            log2_min_luma_coding_block_size_minus3: 1,
            log2_diff_max_min_luma_coding_block_size: 2,
            log2_min_luma_transform_block_size_minus2: 1,
            log2_diff_max_min_luma_transform_block_size: 3,
            max_transform_hierarchy_depth_inter: 2,
            max_transform_hierarchy_depth_intra: 3,
            scaling_list_enabled_flag: true,
            scaling_list_data_present_flag: false,
            amp_enabled_flag: true,
            sample_adaptive_offset_enabled_flag: false,
            pcm_enabled_flag: true,
            pcm_sample_bit_depth_luma_minus1: 7,
            pcm_sample_bit_depth_chroma_minus1: 9,
            log2_min_pcm_luma_coding_block_size_minus3: 1,
            log2_diff_max_min_pcm_luma_coding_block_size: 2,
            pcm_loop_filter_disabled_flag: true,
            num_short_term_ref_pic_sets: 0,
            long_term_ref_pics_present_flag: false,
            temporal_mvp_enabled_flag: true,
            strong_intra_smoothing_enabled_flag: false,
            vui_parameters_present_flag: true,
            extension_present_flag: true,
            range_extension_flag: true,
            range_extension: SpsRangeExtension {
                transform_skip_rotation_enabled_flag: true,
                transform_skip_context_enabled_flag: false,
                implicit_rdpcm_enabled_flag: true,
                explicit_rdpcm_enabled_flag: false,
                extended_precision_processing_flag: true,
                intra_smoothing_disabled_flag: false,
                high_precision_offsets_enabled_flag: true,
                persistent_rice_adaptation_enabled_flag: false,
                cabac_bypass_alignment_enabled_flag: true,
            },
            ..Default::default()
        }
    }

    /// PPS fixture over `sps`. `Pps` has no `Default`, so the literal is
    /// spelled out once.
    fn full_pps(sps: Sps) -> Pps {
        Pps {
            pic_parameter_set_id: 3,
            seq_parameter_set_id: 5,
            dependent_slice_segments_enabled_flag: true,
            output_flag_present_flag: false,
            num_extra_slice_header_bits: 2,
            sign_data_hiding_enabled_flag: true,
            cabac_init_present_flag: false,
            num_ref_idx_l0_default_active_minus1: 2,
            num_ref_idx_l1_default_active_minus1: 1,
            init_qp_minus26: -3,
            constrained_intra_pred_flag: true,
            transform_skip_enabled_flag: false,
            cu_qp_delta_enabled_flag: true,
            diff_cu_qp_delta_depth: 2,
            cb_qp_offset: -4,
            cr_qp_offset: 5,
            slice_chroma_qp_offsets_present_flag: false,
            weighted_pred_flag: true,
            weighted_bipred_flag: false,
            transquant_bypass_enabled_flag: true,
            tiles_enabled_flag: true,
            entropy_coding_sync_enabled_flag: false,
            num_tile_columns_minus1: 1,
            num_tile_rows_minus1: 2,
            uniform_spacing_flag: false,
            column_width_minus1: {
                let mut w = [0u32; 20];
                w[0] = 17;
                w[1] = 12;
                w
            },
            row_height_minus1: {
                let mut h = [0u32; 22];
                h[0] = 9;
                h[1] = 8;
                h[2] = 16;
                h
            },
            loop_filter_across_tiles_enabled_flag: true,
            loop_filter_across_slices_enabled_flag: false,
            deblocking_filter_control_present_flag: true,
            deblocking_filter_override_enabled_flag: false,
            deblocking_filter_disabled_flag: true,
            beta_offset_div2: -2,
            tc_offset_div2: 3,
            scaling_list_data_present_flag: false,
            scaling_list: Default::default(),
            lists_modification_present_flag: true,
            log2_parallel_merge_level_minus2: 1,
            slice_segment_header_extension_present_flag: false,
            extension_present_flag: true,
            range_extension_flag: true,
            range_extension: PpsRangeExtension {
                log2_max_transform_skip_block_size_minus2: 2,
                cross_component_prediction_enabled_flag: true,
                chroma_qp_offset_list_enabled_flag: true,
                diff_cu_chroma_qp_offset_depth: 1,
                chroma_qp_offset_list_len_minus1: 1,
                cb_qp_offset_list: [1, -2, 0, 0, 0, 0],
                cr_qp_offset_list: [-3, 4, 0, 0, 0, 0],
                log2_sao_offset_scale_luma: 1,
                log2_sao_offset_scale_chroma: 2,
            },
            scc_extension_flag: false,
            scc_extension: PpsSccExtension::default(),
            qp_bd_offset_y: 0,
            sps: Rc::new(sps),
        }
    }

    #[test]
    fn every_mapped_sps_field_and_flag_round_trips_exactly() {
        let sps = full_sps();
        let owned = sps_to_std_h265(&sps).unwrap();
        let std = owned.std();

        assert_eq!(std.flags.sps_temporal_id_nesting_flag(), 1);
        assert_eq!(std.flags.separate_colour_plane_flag(), 0);
        assert_eq!(std.flags.conformance_window_flag(), 1);
        assert_eq!(std.flags.sps_sub_layer_ordering_info_present_flag(), 1);
        assert_eq!(std.flags.scaling_list_enabled_flag(), 1);
        assert_eq!(std.flags.sps_scaling_list_data_present_flag(), 0);
        assert_eq!(std.flags.amp_enabled_flag(), 1);
        assert_eq!(std.flags.sample_adaptive_offset_enabled_flag(), 0);
        assert_eq!(std.flags.pcm_enabled_flag(), 1);
        assert_eq!(std.flags.pcm_loop_filter_disabled_flag(), 1);
        assert_eq!(std.flags.long_term_ref_pics_present_flag(), 0);
        assert_eq!(std.flags.sps_temporal_mvp_enabled_flag(), 1);
        assert_eq!(std.flags.strong_intra_smoothing_enabled_flag(), 0);
        assert_eq!(
            std.flags.vui_parameters_present_flag(),
            0,
            "true at the source, skipped by design"
        );
        assert_eq!(std.flags.sps_extension_present_flag(), 1);
        assert_eq!(std.flags.sps_range_extension_flag(), 1);
        assert_eq!(std.flags.transform_skip_rotation_enabled_flag(), 1);
        assert_eq!(std.flags.transform_skip_context_enabled_flag(), 0);
        assert_eq!(std.flags.implicit_rdpcm_enabled_flag(), 1);
        assert_eq!(std.flags.explicit_rdpcm_enabled_flag(), 0);
        assert_eq!(std.flags.extended_precision_processing_flag(), 1);
        assert_eq!(std.flags.intra_smoothing_disabled_flag(), 0);
        assert_eq!(std.flags.high_precision_offsets_enabled_flag(), 1);
        assert_eq!(std.flags.persistent_rice_adaptation_enabled_flag(), 0);
        assert_eq!(std.flags.cabac_bypass_alignment_enabled_flag(), 1);
        assert_eq!(std.flags.sps_scc_extension_flag(), 0);
        assert_eq!(std.flags.sps_curr_pic_ref_enabled_flag(), 0);
        assert_eq!(std.flags.palette_mode_enabled_flag(), 0);
        assert_eq!(
            std.flags.sps_palette_predictor_initializers_present_flag(),
            0
        );
        assert_eq!(std.flags.intra_boundary_filtering_disabled_flag(), 0);

        assert_eq!(
            std.chroma_format_idc,
            hh::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420
        );
        assert_eq!(std.pic_width_in_luma_samples, 1920);
        assert_eq!(std.pic_height_in_luma_samples, 1080);
        assert_eq!(std.sps_video_parameter_set_id, 2);
        assert_eq!(std.sps_max_sub_layers_minus1, 1);
        assert_eq!(std.sps_seq_parameter_set_id, 5);
        assert_eq!(std.bit_depth_luma_minus8, 2);
        assert_eq!(std.bit_depth_chroma_minus8, 2);
        assert_eq!(std.log2_max_pic_order_cnt_lsb_minus4, 6);
        assert_eq!(std.log2_min_luma_coding_block_size_minus3, 1);
        assert_eq!(std.log2_diff_max_min_luma_coding_block_size, 2);
        assert_eq!(std.log2_min_luma_transform_block_size_minus2, 1);
        assert_eq!(std.log2_diff_max_min_luma_transform_block_size, 3);
        assert_eq!(std.max_transform_hierarchy_depth_inter, 2);
        assert_eq!(std.max_transform_hierarchy_depth_intra, 3);
        assert_eq!(std.num_short_term_ref_pic_sets, 0);
        assert_eq!(std.num_long_term_ref_pics_sps, 0);
        assert_eq!(std.pcm_sample_bit_depth_luma_minus1, 7);
        assert_eq!(
            std.pcm_sample_bit_depth_chroma_minus1, 9,
            "distinct from luma"
        );
        assert_eq!(std.log2_min_pcm_luma_coding_block_size_minus3, 1);
        assert_eq!(std.log2_diff_max_min_pcm_luma_coding_block_size, 2);
        assert_eq!(std.conf_win_left_offset, 1);
        assert_eq!(std.conf_win_right_offset, 2);
        assert_eq!(std.conf_win_top_offset, 3);
        assert_eq!(std.conf_win_bottom_offset, 4);

        assert!(!std.pProfileTierLevel.is_null());
        // SAFETY: pProfileTierLevel targets `owned`'s boxed backing, alive here.
        let ptl = unsafe { &*std.pProfileTierLevel };
        assert_eq!(ptl.flags.general_tier_flag(), 1);
        assert_eq!(ptl.flags.general_progressive_source_flag(), 1);
        assert_eq!(ptl.flags.general_interlaced_source_flag(), 0);
        assert_eq!(ptl.flags.general_non_packed_constraint_flag(), 1);
        assert_eq!(ptl.flags.general_frame_only_constraint_flag(), 0);
        assert_eq!(
            ptl.general_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );
        assert_eq!(
            ptl.general_level_idc,
            hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1
        );
        assert!(!std.pDecPicBufMgr.is_null());
        // SAFETY: pDecPicBufMgr targets `owned`'s boxed backing, alive here.
        let dpb = unsafe { &*std.pDecPicBufMgr };
        assert_eq!(&dpb.max_dec_pic_buffering_minus1[..2], &[5, 6]);
        assert_eq!(&dpb.max_num_reorder_pics[..2], &[1, 2]);
        assert_eq!(&dpb.max_latency_increase_plus1[..2], &[7, 8]);

        // Data-absent blocks are attached zeroed with their flags clear.
        assert!(!std.pScalingLists.is_null());
        assert!(!std.pShortTermRefPicSet.is_null());
        assert!(!std.pLongTermRefPicsSps.is_null());
        assert!(!std.pSequenceParameterSetVui.is_null());
        assert!(!std.pPredictorPaletteEntries.is_null());
        // SAFETY: the pointers target `owned`'s boxed backings, alive here.
        let vui = unsafe { &*std.pSequenceParameterSetVui };
        assert!(!vui.pHrdParameters.is_null());
        assert_eq!(std.num_long_term_ref_pics_sps, 0);
    }

    #[test]
    fn profile_and_level_code_points_map_or_reject() {
        for (idc, expect) in [
            (
                1u8,
                hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN,
            ),
            (
                2,
                hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10,
            ),
            (
                3,
                hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_STILL_PICTURE,
            ),
            (
                4,
                hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_FORMAT_RANGE_EXTENSIONS,
            ),
        ] {
            assert_eq!(profile_to_std(idc).unwrap(), expect);
        }
        // 5 (High Throughput) and 9 (SCC) exist on the wire but not in Vulkan.
        for idc in [0u8, 5, 9, 11] {
            assert_eq!(
                profile_to_std(idc).unwrap_err(),
                H265ParamsError::UnmappableProfileIdc(idc)
            );
        }

        let pairs = [
            (
                Level::L1,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_1_0,
            ),
            (
                Level::L2,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_0,
            ),
            (
                Level::L2_1,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_2_1,
            ),
            (
                Level::L3,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_0,
            ),
            (
                Level::L3_1,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_1,
            ),
            (
                Level::L4,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_0,
            ),
            (
                Level::L4_1,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1,
            ),
            (
                Level::L5,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_0,
            ),
            (
                Level::L5_1,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_1,
            ),
            (
                Level::L5_2,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_5_2,
            ),
            (
                Level::L6,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_0,
            ),
            (
                Level::L6_1,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_1,
            ),
            (
                Level::L6_2,
                hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2,
            ),
        ];
        let mut prev = None;
        for (level, expect) in pairs {
            assert_eq!(level_to_std(level), expect);
            if let Some(prev) = prev {
                assert!(expect > prev, "code points must ascend for the caps gate");
            }
            prev = Some(expect);
        }
    }

    #[test]
    fn the_owned_backings_survive_moving_the_wrapper() {
        // Box after conversion: relocating the wrapper must not invalidate its
        // pointers — the backing is heap-pinned.
        let owned = Box::new(sps_to_std_h265(&full_sps()).unwrap());
        let std = owned.std();
        // SAFETY: both pointers target `owned`'s boxed backings, alive in scope.
        let (ptl, dpb) = unsafe { (&*std.pProfileTierLevel, &*std.pDecPicBufMgr) };
        assert_eq!(
            ptl.general_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );
        assert_eq!(dpb.max_dec_pic_buffering_minus1[0], 5);
    }

    /// Scribble over the stack the conversion's frames just used.
    ///
    /// Move tests must READ a block back: the Std struct copies pointer
    /// values, so a stale one still compares equal. An inlined backing shows
    /// up only as wrong content, and only after the dead slot is reused.
    /// After this runs, a pointer into a dead local reads `0xA5`.
    #[inline(never)]
    fn clobber_the_dead_stack() {
        let mut scratch = [0xA5u8; 16 * 1024];
        std::hint::black_box(&mut scratch);
    }

    /// All three wrappers may be moved — into stored parameters, out of a
    /// `Result`, into a reallocating `Vec` — without disturbing addresses a
    /// driver has already been given. Every backing is `Box`ed; inlining any
    /// one as a field would keep other tests green and hand the driver a
    /// pointer into a moved-from stack slot. This exercises every pointer a
    /// stream can populate.
    #[test]
    fn moving_the_wrapper_leaves_the_driver_s_pointers_put() {
        // SPS with every embeddable pointer populated.
        let mut sps = full_sps();
        sps.scaling_list_data_present_flag = true;
        sps.scaling_list.scaling_list_4x4 = std::array::from_fn(|i| [10 + i as u8; 16]);
        sps.num_short_term_ref_pic_sets = 1;
        let mut st = ShortTermRefPicSet {
            num_negative_pics: 1,
            ..Default::default()
        };
        st.delta_poc_s0[0] = -1;
        st.used_by_curr_pic_s0[0] = true;
        sps.short_term_ref_pic_set = vec![st];
        sps.long_term_ref_pics_present_flag = true;
        sps.num_long_term_ref_pics_sps = 1;
        sps.lt_ref_pic_poc_lsb_sps[0] = 11;
        sps.used_by_curr_pic_lt_sps_flag[0] = true;
        let mut pps = full_pps(sps.clone());
        pps.scaling_list_data_present_flag = true;
        pps.scaling_list.scaling_list_4x4 = std::array::from_fn(|i| [60 + i as u8; 16]);
        let vps = Vps {
            video_parameter_set_id: 2,
            max_sub_layers_minus1: 1,
            profile_tier_level: full_sps().profile_tier_level,
            // Distinct from the SPS's [5, 6, …] so a read-back names which block.
            max_dec_pic_buffering_minus1: [3, 4, 0, 0, 0, 0, 0],
            ..Default::default()
        };

        let owned_vps = vps_to_std_h265(&vps).expect("the VPS converts");
        let owned_sps = sps_to_std_h265(&sps).expect("a Main 10 SPS converts");
        let owned_pps = pps_to_std_h265(&pps).expect("its PPS converts");
        let vps_ptrs = (
            owned_vps.std().pProfileTierLevel,
            owned_vps.std().pDecPicBufMgr,
        );
        let sps_ptrs = (
            owned_sps.std().pProfileTierLevel,
            owned_sps.std().pDecPicBufMgr,
            owned_sps.std().pScalingLists,
            owned_sps.std().pShortTermRefPicSet,
            owned_sps.std().pLongTermRefPicsSps,
        );
        let pps_lists = owned_pps.std().pScalingLists;
        for (what, ptr) in [
            ("vps pProfileTierLevel", vps_ptrs.0.cast::<()>()),
            ("vps pDecPicBufMgr", vps_ptrs.1.cast()),
            ("sps pProfileTierLevel", sps_ptrs.0.cast()),
            ("sps pDecPicBufMgr", sps_ptrs.1.cast()),
            ("sps pScalingLists", sps_ptrs.2.cast()),
            ("sps pShortTermRefPicSet", sps_ptrs.3.cast()),
            ("sps pLongTermRefPicsSps", sps_ptrs.4.cast()),
            ("pps pScalingLists", pps_lists.cast()),
        ] {
            assert!(!ptr.is_null(), "{what} is attached by this fixture");
        }

        // Session stored-parameters moves: into a `Vec`, through a reallocation
        // as later Adds push more sets, then as a whole value.
        let stored_vps = vec![owned_vps];
        let stored_sps = vec![owned_sps];
        let mut stored_pps = vec![owned_pps];
        for id in 1..crate::session_h265::MAX_STD_PPS as u8 {
            let mut more = full_pps(sps.clone());
            more.pic_parameter_set_id = id;
            stored_pps.push(pps_to_std_h265(&more).expect("converts"));
        }
        assert!(
            stored_pps.capacity() > 1,
            "the pushes reallocated, which is the case being pinned"
        );
        let stored = (stored_vps, stored_sps, stored_pps, 0u8);
        let (stored_vps, stored_sps, stored_pps, _) = stored;

        let moved_vps = stored_vps[0].std();
        assert_eq!(
            (moved_vps.pProfileTierLevel, moved_vps.pDecPicBufMgr),
            vps_ptrs
        );
        let moved_sps = stored_sps[0].std();
        assert_eq!(
            (
                moved_sps.pProfileTierLevel,
                moved_sps.pDecPicBufMgr,
                moved_sps.pScalingLists,
                moved_sps.pShortTermRefPicSet,
                moved_sps.pLongTermRefPicsSps,
            ),
            sps_ptrs
        );
        assert_eq!(stored_pps[0].std().pScalingLists, pps_lists);

        clobber_the_dead_stack();
        // Pointer equality cannot fail (Std copies the value). Read every
        // backing so an inlined field addresses 0xA5, not fixture data.
        // SAFETY: `stored_*` are alive here and own every one of these blocks.
        let (vps_ptl, vps_dpb) = unsafe { (&*vps_ptrs.0, &*vps_ptrs.1) };
        // SAFETY: as above.
        let (sps_ptl, sps_dpb, sps_scaling, sps_st, sps_lt) = unsafe {
            (
                &*sps_ptrs.0,
                &*sps_ptrs.1,
                &*sps_ptrs.2,
                &*sps_ptrs.3,
                &*sps_ptrs.4,
            )
        };
        // SAFETY: as above.
        let pps_scaling = unsafe { &*pps_lists };
        assert_eq!(
            vps_ptl.general_level_idc,
            hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1,
            "vps pProfileTierLevel"
        );
        assert_eq!(
            &vps_dpb.max_dec_pic_buffering_minus1[..2],
            &[3, 4],
            "vps pDecPicBufMgr"
        );
        assert_eq!(
            sps_ptl.general_level_idc,
            hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1,
            "sps pProfileTierLevel"
        );
        assert_eq!(
            &sps_dpb.max_dec_pic_buffering_minus1[..2],
            &[5, 6],
            "sps pDecPicBufMgr"
        );
        assert_eq!(sps_scaling.ScalingList4x4[5], [15; 16], "sps pScalingLists");
        assert_eq!(sps_st.num_negative_pics, 1, "sps pShortTermRefPicSet");
        assert_eq!(
            sps_lt.lt_ref_pic_poc_lsb_sps[0], 11,
            "sps pLongTermRefPicsSps"
        );
        assert_eq!(pps_scaling.ScalingList4x4[5], [65; 16], "pps pScalingLists");
    }

    #[test]
    fn sps_scaling_lists_convert_verbatim_including_the_32x32_pair_and_dc_values() {
        let mut sps = full_sps();
        sps.scaling_list_data_present_flag = true;
        // Distinct fill bytes per list. 32x32 lists live at parser matrix ids 0
        // and 3 (7.4.5 steps matrixId by 3 at sizeId 3) and must land at Std 0, 1.
        sps.scaling_list.scaling_list_4x4 = std::array::from_fn(|i| [10 + i as u8; 16]);
        sps.scaling_list.scaling_list_8x8 = std::array::from_fn(|i| [20 + i as u8; 64]);
        sps.scaling_list.scaling_list_16x16 = std::array::from_fn(|i| [30 + i as u8; 64]);
        sps.scaling_list.scaling_list_32x32 = std::array::from_fn(|i| [40 + i as u8; 64]);
        sps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [-7, 0, 8, 100, 200, 247];
        sps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [42, 0, 0, 99, 0, 0];

        let owned = sps_to_std_h265(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.flags.sps_scaling_list_data_present_flag(), 1);
        assert!(!std.pScalingLists.is_null());
        // SAFETY: pScalingLists targets `owned`'s boxed backing, alive here.
        let lists = unsafe { &*std.pScalingLists };
        for i in 0..6 {
            assert_eq!(lists.ScalingList4x4[i], [10 + i as u8; 16], "4x4 list {i}");
            assert_eq!(lists.ScalingList8x8[i], [20 + i as u8; 64], "8x8 list {i}");
            assert_eq!(
                lists.ScalingList16x16[i],
                [30 + i as u8; 64],
                "16x16 list {i}"
            );
        }
        assert_eq!(
            lists.ScalingList32x32[0], [40; 64],
            "intra = parser index 0"
        );
        assert_eq!(
            lists.ScalingList32x32[1], [43; 64],
            "inter = parser index 3"
        );
        // Std DC fields carry the +8 value, not the minus8 syntax element.
        assert_eq!(lists.ScalingListDCCoef16x16, [1, 8, 16, 108, 208, 255]);
        assert_eq!(lists.ScalingListDCCoef32x32, [50, 107]);
    }

    #[test]
    fn short_term_rps_candidates_reencode_to_the_std_syntax_exactly() {
        let mut sps = full_sps();
        sps.num_short_term_ref_pic_sets = 2;
        // Candidate 0: DeltaPocS0 = [-1, -3] (steps 1, 2), DeltaPocS1 = [2] (step 2).
        let mut set0 = ShortTermRefPicSet {
            num_negative_pics: 2,
            num_positive_pics: 1,
            ..Default::default()
        };
        set0.delta_poc_s0[0] = -1;
        set0.delta_poc_s0[1] = -3;
        set0.used_by_curr_pic_s0[0] = true;
        set0.used_by_curr_pic_s0[1] = false;
        set0.delta_poc_s1[0] = 2;
        set0.used_by_curr_pic_s1[0] = true;
        // Candidate 1 as the parser leaves a predicted set: resolved arrays with
        // prediction syntax still recorded. Conversion must emit non-predicted form.
        let mut set1 = ShortTermRefPicSet {
            inter_ref_pic_set_prediction_flag: true,
            delta_idx_minus1: 0,
            abs_delta_rps_minus1: 0,
            num_negative_pics: 1,
            ..Default::default()
        };
        set1.delta_poc_s0[0] = -2;
        set1.used_by_curr_pic_s0[0] = true;
        sps.short_term_ref_pic_set = vec![set0, set1];

        let owned = sps_to_std_h265(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.num_short_term_ref_pic_sets, 2);
        assert!(!std.pShortTermRefPicSet.is_null());
        // SAFETY: pShortTermRefPicSet targets `owned`'s boxed slice of exactly
        // num_short_term_ref_pic_sets entries, alive for this whole scope.
        let sets = unsafe { std::slice::from_raw_parts(std.pShortTermRefPicSet, 2) };

        assert_eq!(sets[0].num_negative_pics, 2);
        assert_eq!(sets[0].num_positive_pics, 1);
        assert_eq!(
            &sets[0].delta_poc_s0_minus1[..2],
            &[0, 1],
            "steps 1 and 2, minus 1"
        );
        assert_eq!(sets[0].delta_poc_s1_minus1[0], 1, "step 2, minus 1");
        assert_eq!(sets[0].used_by_curr_pic_s0_flag, 0b01);
        assert_eq!(sets[0].used_by_curr_pic_s1_flag, 0b1);

        assert_eq!(
            sets[1].flags.inter_ref_pic_set_prediction_flag(),
            0,
            "resolved form: the prediction is flattened, never re-declared"
        );
        assert_eq!(sets[1].delta_idx_minus1, 0);
        assert_eq!(sets[1].num_negative_pics, 1);
        assert_eq!(sets[1].delta_poc_s0_minus1[0], 1);
    }

    #[test]
    fn long_term_sps_candidates_ride_with_mask_and_poc_lsbs() {
        let mut sps = full_sps();
        sps.long_term_ref_pics_present_flag = true;
        sps.num_long_term_ref_pics_sps = 3;
        sps.lt_ref_pic_poc_lsb_sps[0] = 11;
        sps.lt_ref_pic_poc_lsb_sps[1] = 22;
        sps.lt_ref_pic_poc_lsb_sps[2] = 33;
        sps.used_by_curr_pic_lt_sps_flag[0] = true;
        sps.used_by_curr_pic_lt_sps_flag[2] = true;

        let owned = sps_to_std_h265(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.flags.long_term_ref_pics_present_flag(), 1);
        assert_eq!(std.num_long_term_ref_pics_sps, 3);
        assert!(!std.pLongTermRefPicsSps.is_null());
        // SAFETY: pLongTermRefPicsSps targets `owned`'s boxed backing, alive here.
        let lt = unsafe { &*std.pLongTermRefPicsSps };
        assert_eq!(lt.used_by_curr_pic_lt_sps_flag, 0b101);
        assert_eq!(&lt.lt_ref_pic_poc_lsb_sps[..3], &[11, 22, 33]);
    }

    /// `flag=1, num=0` is the LTR/RFI-recovery shape (slice-signalled
    /// long-term pics, zero SPS candidates). The header requires a valid
    /// `pLongTermRefPicsSps` whenever the flag is set; a set flag over a
    /// null pointer is untested in every driver.
    #[test]
    fn long_term_flag_without_sps_candidates_still_backs_the_pointer() {
        let mut sps = full_sps();
        sps.long_term_ref_pics_present_flag = true;
        sps.num_long_term_ref_pics_sps = 0;

        let owned = sps_to_std_h265(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.flags.long_term_ref_pics_present_flag(), 1);
        assert_eq!(std.num_long_term_ref_pics_sps, 0);
        assert!(!std.pLongTermRefPicsSps.is_null());
        // SAFETY: pLongTermRefPicsSps targets `owned`'s boxed backing, alive here.
        let lt = unsafe { &*std.pLongTermRefPicsSps };
        assert_eq!(lt.used_by_curr_pic_lt_sps_flag, 0);
        assert!(lt.lt_ref_pic_poc_lsb_sps.iter().all(|&lsb| lsb == 0));
    }

    #[test]
    fn envelope_rejections_fail_closed_instead_of_approximating() {
        let mut sps = full_sps();
        sps.chroma_format_idc = 2;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(2)
        );
        sps.chroma_format_idc = 0;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(0)
        );
        // ChromaArrayType 0: three monochrome-coded planes.
        sps.chroma_format_idc = 3;
        sps.separate_colour_plane_flag = true;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::SeparateColourPlanes
        );
        sps.separate_colour_plane_flag = false;
        sps.chroma_format_idc = 4;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::InvalidChromaFormatIdc(4)
        );

        let mut sps = full_sps();
        sps.bit_depth_luma_minus8 = 4;
        sps.bit_depth_chroma_minus8 = 4;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 4,
                chroma_minus8: 4
            }
        );
        let mut sps = full_sps();
        sps.bit_depth_chroma_minus8 = 0;
        assert!(matches!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth { .. }
        ));

        // 4:4:4 at 8 and 10 bits is in the envelope (RExt profile).
        let mut sps = full_sps();
        sps.profile_tier_level.general_profile_idc = 4;
        sps.chroma_format_idc = 3;
        sps.bit_depth_luma_minus8 = 0;
        sps.bit_depth_chroma_minus8 = 0;
        let owned = sps_to_std_h265(&sps).unwrap();
        assert_eq!(
            owned.std().chroma_format_idc,
            hh::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_444
        );

        let mut sps = full_sps();
        sps.profile_tier_level.general_profile_idc = 9; // SCC
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::UnmappableProfileIdc(9)
        );

        // Palette predictor initializers: the one pointer this conversion refuses to fake.
        let mut sps = full_sps();
        sps.scc_extension
            .palette_predictor_initializers_present_flag = true;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::PalettePredictorInitializers
        );
        let mut pps = full_pps(full_sps());
        pps.scc_extension
            .palette_predictor_initializers_present_flag = true;
        assert_eq!(
            pps_to_std_h265(&pps).unwrap_err(),
            H265ParamsError::PalettePredictorInitializers
        );
    }

    #[test]
    fn oversized_or_corrupt_rps_tables_are_rejected_not_padded() {
        let mut sps = full_sps();
        sps.num_short_term_ref_pic_sets = 65;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::TooManyShortTermRpsSets(65)
        );

        let mut sps = full_sps();
        sps.num_short_term_ref_pic_sets = 1;
        sps.short_term_ref_pic_set = Vec::new();
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::MissingShortTermRps { index: 0 }
        );

        let mut sps = full_sps();
        sps.num_short_term_ref_pic_sets = 1;
        sps.short_term_ref_pic_set = vec![ShortTermRefPicSet {
            num_negative_pics: 17,
            ..Default::default()
        }];
        assert!(matches!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::RpsEntryOverflow { set: 0, .. }
        ));

        let mut sps = full_sps();
        sps.num_short_term_ref_pic_sets = 1;
        let mut set = ShortTermRefPicSet {
            num_negative_pics: 2,
            ..Default::default()
        };
        set.delta_poc_s0[0] = -3;
        set.delta_poc_s0[1] = -1; // must be strictly decreasing
        sps.short_term_ref_pic_set = vec![set];
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::NonMonotonicRps { set: 0 }
        );

        let mut sps = full_sps();
        sps.long_term_ref_pics_present_flag = true;
        sps.num_long_term_ref_pics_sps = 33;
        assert_eq!(
            sps_to_std_h265(&sps).unwrap_err(),
            H265ParamsError::TooManyLongTermSpsPics(33)
        );
    }

    #[test]
    fn every_mapped_pps_field_and_flag_round_trips_exactly() {
        let pps = full_pps(full_sps());
        let owned = pps_to_std_h265(&pps).unwrap();
        let std = owned.std();

        assert_eq!(std.flags.dependent_slice_segments_enabled_flag(), 1);
        assert_eq!(std.flags.output_flag_present_flag(), 0);
        assert_eq!(std.flags.sign_data_hiding_enabled_flag(), 1);
        assert_eq!(std.flags.cabac_init_present_flag(), 0);
        assert_eq!(std.flags.constrained_intra_pred_flag(), 1);
        assert_eq!(std.flags.transform_skip_enabled_flag(), 0);
        assert_eq!(std.flags.cu_qp_delta_enabled_flag(), 1);
        assert_eq!(std.flags.pps_slice_chroma_qp_offsets_present_flag(), 0);
        assert_eq!(std.flags.weighted_pred_flag(), 1);
        assert_eq!(std.flags.weighted_bipred_flag(), 0);
        assert_eq!(std.flags.transquant_bypass_enabled_flag(), 1);
        assert_eq!(std.flags.tiles_enabled_flag(), 1);
        assert_eq!(std.flags.entropy_coding_sync_enabled_flag(), 0);
        assert_eq!(std.flags.uniform_spacing_flag(), 0);
        assert_eq!(std.flags.loop_filter_across_tiles_enabled_flag(), 1);
        assert_eq!(std.flags.pps_loop_filter_across_slices_enabled_flag(), 0);
        assert_eq!(std.flags.deblocking_filter_control_present_flag(), 1);
        assert_eq!(std.flags.deblocking_filter_override_enabled_flag(), 0);
        assert_eq!(std.flags.pps_deblocking_filter_disabled_flag(), 1);
        assert_eq!(std.flags.pps_scaling_list_data_present_flag(), 0);
        assert_eq!(std.flags.lists_modification_present_flag(), 1);
        assert_eq!(std.flags.slice_segment_header_extension_present_flag(), 0);
        assert_eq!(std.flags.pps_extension_present_flag(), 1);
        assert_eq!(std.flags.cross_component_prediction_enabled_flag(), 1);
        assert_eq!(std.flags.chroma_qp_offset_list_enabled_flag(), 1);
        assert_eq!(std.flags.pps_curr_pic_ref_enabled_flag(), 0);
        assert_eq!(
            std.flags.residual_adaptive_colour_transform_enabled_flag(),
            0
        );
        assert_eq!(std.flags.pps_slice_act_qp_offsets_present_flag(), 0);
        assert_eq!(
            std.flags.pps_palette_predictor_initializers_present_flag(),
            0
        );
        assert_eq!(std.flags.monochrome_palette_flag(), 0);
        assert_eq!(std.flags.pps_range_extension_flag(), 1);

        assert_eq!(std.pps_pic_parameter_set_id, 3);
        assert_eq!(std.pps_seq_parameter_set_id, 5);
        assert_eq!(
            std.sps_video_parameter_set_id, 2,
            "resolved through the PPS's own SPS"
        );
        assert_eq!(std.num_extra_slice_header_bits, 2);
        assert_eq!(std.num_ref_idx_l0_default_active_minus1, 2);
        assert_eq!(std.num_ref_idx_l1_default_active_minus1, 1);
        assert_eq!(std.init_qp_minus26, -3);
        assert_eq!(std.diff_cu_qp_delta_depth, 2);
        assert_eq!(std.pps_cb_qp_offset, -4);
        assert_eq!(std.pps_cr_qp_offset, 5);
        assert_eq!(std.pps_beta_offset_div2, -2);
        assert_eq!(std.pps_tc_offset_div2, 3);
        assert_eq!(std.log2_parallel_merge_level_minus2, 1);
        assert_eq!(std.log2_max_transform_skip_block_size_minus2, 2);
        assert_eq!(std.diff_cu_chroma_qp_offset_depth, 1);
        assert_eq!(std.chroma_qp_offset_list_len_minus1, 1);
        assert_eq!(&std.cb_qp_offset_list[..2], &[1, -2]);
        assert_eq!(&std.cr_qp_offset_list[..2], &[-3, 4]);
        assert_eq!(std.log2_sao_offset_scale_luma, 1);
        assert_eq!(std.log2_sao_offset_scale_chroma, 2);
        assert_eq!(std.num_tile_columns_minus1, 1);
        assert_eq!(std.num_tile_rows_minus1, 2);
        assert_eq!(&std.column_width_minus1[..2], &[17, 12]);
        assert_eq!(&std.row_height_minus1[..3], &[9, 8, 16]);
        assert!(!std.pScalingLists.is_null());
        assert!(!std.pPredictorPaletteEntries.is_null());
    }

    #[test]
    fn a_pps_field_past_its_std_width_is_an_error_not_a_truncation() {
        let mut pps = full_pps(full_sps());
        pps.column_width_minus1[0] = 70_000; // past u16
        assert!(matches!(
            pps_to_std_h265(&pps).unwrap_err(),
            H265ParamsError::FieldOverflow {
                field: "column_width_minus1",
                value: 70_000
            }
        ));

        let mut pps = full_pps(full_sps());
        pps.range_extension.log2_sao_offset_scale_luma = 300; // past u8
        assert!(matches!(
            pps_to_std_h265(&pps).unwrap_err(),
            H265ParamsError::FieldOverflow { .. }
        ));
    }

    #[test]
    fn pps_scaling_lists_ride_behind_the_owned_pointer() {
        let mut pps = full_pps(full_sps());
        pps.scaling_list_data_present_flag = true;
        pps.scaling_list.scaling_list_4x4 = std::array::from_fn(|i| [60 + i as u8; 16]);
        let owned = Box::new(pps_to_std_h265(&pps).unwrap());
        assert_eq!(owned.std().flags.pps_scaling_list_data_present_flag(), 1);
        // SAFETY: pScalingLists targets `owned`'s boxed backing, alive here.
        let lists = unsafe { &*owned.std().pScalingLists };
        assert_eq!(lists.ScalingList4x4[5], [65; 16]);
    }

    #[test]
    fn a_vps_converts_with_hrd_and_timing_skipped_by_design() {
        let vps = Vps {
            video_parameter_set_id: 2,
            max_sub_layers_minus1: 1,
            temporal_id_nesting_flag: true,
            sub_layer_ordering_info_present_flag: true,
            profile_tier_level: full_sps().profile_tier_level,
            max_dec_pic_buffering_minus1: [5, 6, 0, 0, 0, 0, 0],
            max_num_reorder_pics: [1, 2, 0, 0, 0, 0, 0],
            max_latency_increase_plus1: [7, 8, 0, 0, 0, 0, 0],
            // Timing present at the source: the skip must not copy it.
            timing_info_present_flag: true,
            num_units_in_tick: 1000,
            time_scale: 60_000,
            ..Default::default()
        };
        let owned = vps_to_std_h265(&vps).unwrap();
        let std = owned.std();
        assert_eq!(std.vps_video_parameter_set_id, 2);
        assert_eq!(std.vps_max_sub_layers_minus1, 1);
        assert_eq!(std.flags.vps_temporal_id_nesting_flag(), 1);
        assert_eq!(std.flags.vps_sub_layer_ordering_info_present_flag(), 1);
        assert_eq!(
            std.flags.vps_timing_info_present_flag(),
            0,
            "true at the source, skipped by design (HRD/timing)"
        );
        assert_eq!(std.vps_num_units_in_tick, 0);
        assert_eq!(std.vps_time_scale, 0);
        assert!(!std.pHrdParameters.is_null());
        // SAFETY: both pointers target `owned`'s boxed backings, alive here.
        let (ptl, dpb) = unsafe { (&*std.pProfileTierLevel, &*std.pDecPicBufMgr) };
        assert_eq!(
            ptl.general_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );
        assert_eq!(&dpb.max_dec_pic_buffering_minus1[..2], &[5, 6]);
        assert_eq!(&dpb.max_latency_increase_plus1[..2], &[7, 8]);

        // Parser field is u32; Std is u8. 300 overflows.
        let mut hostile = vps;
        hostile.max_dec_pic_buffering_minus1[0] = 300;
        assert!(matches!(
            vps_to_std_h265(&hostile).unwrap_err(),
            H265ParamsError::FieldOverflow { .. }
        ));
    }

    #[test]
    fn the_fallback_vps_restates_exactly_what_the_sps_knows() {
        let sps = full_sps();
        let owned = fallback_vps_from_sps(&sps).unwrap();
        let std = owned.std();
        assert_eq!(std.vps_video_parameter_set_id, sps.video_parameter_set_id);
        assert_eq!(std.vps_max_sub_layers_minus1, sps.max_sub_layers_minus1);
        // SAFETY: both pointers target `owned`'s boxed backings, alive here.
        let (ptl, dpb) = unsafe { (&*std.pProfileTierLevel, &*std.pDecPicBufMgr) };
        assert_eq!(
            ptl.general_level_idc,
            hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_4_1
        );
        assert_eq!(
            dpb.max_dec_pic_buffering_minus1,
            sps.max_dec_pic_buffering_minus1
        );
        assert_eq!(dpb.max_num_reorder_pics, sps.max_num_reorder_pics);
    }

    #[test]
    fn the_25fps_vectors_own_parameter_sets_convert_cleanly() {
        use std::io::Cursor;

        use cros_codecs::codec::h265::parser::Nalu;
        use cros_codecs::codec::h265::parser::NaluType;
        use cros_codecs::codec::h265::parser::Parser;

        const TEST_25FPS: &[u8] = pf_bitstream::testing::H265_25FPS;

        let mut cursor = Cursor::new(TEST_25FPS);
        let mut parser = Parser::default();
        let (mut vps_seen, mut sps_seen, mut pps_seen) = (false, false, false);
        while let Ok(nalu) = Nalu::next(&mut cursor) {
            match nalu.header.type_ {
                NaluType::VpsNut if !vps_seen => {
                    let vps = parser.parse_vps(&nalu).expect("the vector's VPS parses");
                    vps_to_std_h265(vps).expect("the vector's VPS converts");
                    vps_seen = true;
                }
                NaluType::SpsNut if !sps_seen => {
                    let sps = parser.parse_sps(&nalu).expect("the vector's SPS parses");
                    let owned = sps_to_std_h265(sps).expect("the vector's SPS converts");
                    let std = owned.std();
                    assert_eq!(std.pic_width_in_luma_samples, 320, "the vector is 320x240");
                    assert_eq!(std.pic_height_in_luma_samples, 240);
                    assert_eq!(std.bit_depth_luma_minus8, 0);
                    assert_eq!(
                        std.chroma_format_idc,
                        hh::StdVideoH265ChromaFormatIdc_STD_VIDEO_H265_CHROMA_FORMAT_IDC_420
                    );
                    // SAFETY: pProfileTierLevel targets `owned`'s boxed backing.
                    let ptl = unsafe { &*std.pProfileTierLevel };
                    assert_eq!(
                        ptl.general_profile_idc,
                        hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
                    );
                    sps_seen = true;
                }
                NaluType::PpsNut if !pps_seen => {
                    let pps = parser.parse_pps(&nalu).expect("the vector's PPS parses");
                    let owned = pps_to_std_h265(pps).expect("the vector's PPS converts");
                    assert_eq!(owned.std().pps_pic_parameter_set_id, 0);
                    pps_seen = true;
                }
                _ => {}
            }
            if vps_seen && sps_seen && pps_seen {
                break;
            }
        }
        assert!(
            vps_seen && sps_seen && pps_seen,
            "the vector opens with VPS + SPS + PPS"
        );
    }

    /// Lowering writes the ceiling into the PTL backing the driver will read.
    /// A ceiling at or above the declared level changes nothing.
    #[test]
    fn clamp_level_lowers_the_ptl_and_only_lowers() {
        let sps = full_sps();
        let declared = level_to_std(sps.profile_tier_level.general_level_idc);

        let mut owned = sps_to_std_h265(&sps).unwrap();
        // SAFETY: pProfileTierLevel targets `owned`'s boxed backing.
        let level = unsafe { (*owned.std().pProfileTierLevel).general_level_idc };
        assert_eq!(level, declared);
        owned.clamp_level(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2);
        // SAFETY: as above.
        let level = unsafe { (*owned.std().pProfileTierLevel).general_level_idc };
        assert_eq!(level, declared);
        // Ceiling below: written through. The pointer still targets the
        // wrapper's own backing — the clamp mutates in place, never re-points.
        let ceiling = hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_3_1;
        assert!(ceiling < declared, "fixture declares above 3.1");
        owned.clamp_level(ceiling);
        // SAFETY: as above.
        let level = unsafe { (*owned.std().pProfileTierLevel).general_level_idc };
        assert_eq!(level, ceiling);

        let mut owned_vps = fallback_vps_from_sps(&sps).unwrap();
        owned_vps.clamp_level(ceiling);
        // SAFETY: as above, the VPS wrapper's own backing.
        let vps_level = unsafe { (*owned_vps.std().pProfileTierLevel).general_level_idc };
        assert!(vps_level <= ceiling);
    }
}
