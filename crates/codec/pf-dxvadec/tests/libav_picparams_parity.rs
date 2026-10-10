//! DXVA submission parity for H.264, HEVC, and AV1.
//!
//! Ordinary tests check descriptor set, matrix presence, MB counts, and tiling on the
//! vendored vectors. Ignored tests compare picture parameters, matrices, and descriptors
//! against patched FFmpeg n8.1 D3D11VA captures (`PFPP`/`PFQM`/`PFBD`/`PFCFG` under a
//! zero-based picture index). AV1 indexes 274 decoded frames, not 250 temporal units,
//! and has no matrix or PFCFG check. Software fallback is void.
//!
//! ```text
//! cargo test -p pf-dxvadec --test libav_picparams_parity
//! ffmpeg -hwaccel d3d11va -hwaccel_output_format d3d11 -i test-25fps.h264 -f null - 2> h264.log
//! grep -oE 'PF(PP|QM|BD|CFG) .*' h264.log > libav-h264.capture
//! PF_LIBAV_CAPTURE_H264=libav-h264.capture cargo test -p pf-dxvadec --test libav_picparams_parity -- --ignored
//! ```
//! Repeat for HEVC (`test-25fps.h265`) and AV1 (`test-25fps.ivf.av1`). H.264 POC offset
//! and HEVC tiles-disabled bit 10 are the only allowances; reject `Reserved16Bits == 0`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::mem::offset_of;
use std::mem::size_of;
use std::ops::Range;

use pf_bitstream::testing::split_h264_aus;
use pf_bitstream::testing::split_h265_aus;
use pf_bitstream::testing::split_ivf;
use pf_dxvadec::descriptors::BUFFER_BITSTREAM;
use pf_dxvadec::descriptors::BUFFER_INVERSE_QUANTIZATION_MATRIX;
use pf_dxvadec::descriptors::BUFFER_PICTURE_PARAMETERS;
use pf_dxvadec::descriptors::BUFFER_SLICE_CONTROL;
use pf_dxvadec::dxva::PicParamsH264;
use pf_dxvadec::dxva::PicParamsHevc;
use pf_dxvadec::dxva::QmatrixH264;
use pf_dxvadec::dxva::QmatrixHevc;
use pf_dxvadec::dxva::SliceH264Short;
use pf_dxvadec::dxva::SliceHevcShort;
use pf_dxvadec::dxva::UNUSED_ENTRY;
use pf_dxvadec::dxva_av1::PicEntryAv1;
use pf_dxvadec::dxva_av1::UNUSED_INDEX;
use pf_dxvadec::AuPlan;
use pf_dxvadec::Av1Planner;
use pf_dxvadec::BufferDescriptor;
use pf_dxvadec::Codec;
use pf_dxvadec::H264Planner;
use pf_dxvadec::H265Planner;
use pf_dxvadec::PicParamsAv1;
use pf_dxvadec::SliceRecord;
use pf_dxvadec::SlotMap;
use pf_dxvadec::TileAv1;
use pf_dxvadec::NUM_REF_SLOTS;

const TEST_25FPS_H264: &[u8] = pf_bitstream::testing::H264_25FPS;
const TEST_25FPS_H265: &[u8] = pf_bitstream::testing::H265_25FPS;
/// IVF packet, not AU: one packet may decode several frames, of which at most one shows.
const TEST_25FPS_AV1: &[u8] = pf_bitstream::testing::AV1_25FPS;

/// 250 AUs: pf-bitstream golden and a valid capture's `PFPP` count.
const VENDORED_AUS: usize = 250;

/// 1 MiB stand-in. 320×240 vectors never hit pack's tail-padding clamp.
const MAPPING_BYTES: usize = 1 << 20;

struct OurSubmission {
    pic_params: Vec<u8>,
    qmatrix: Option<Vec<u8>>,
    descriptors: Vec<BufferDescriptor>,
    /// Empty on AV1; those submissions use `tiles`.
    records: Vec<SliceRecord>,
    /// One `DXVA_Tile_AV1` per tile, not per tile group. Empty on H.264/H.265.
    tiles: Vec<TileAv1>,
    /// Packer bytes before 128-byte tail padding.
    unpadded: u32,
    /// H.264: `mb_width * mb_height`. HEVC/AV1: 0.
    mb_count: u32,
}

/// One submission per AU. A skip on this golden vector is a regression; swallowing a plan
/// error would report a clean run while comparing nothing.
fn our_h264_submissions() -> Vec<OurSubmission> {
    let mut planner = H264Planner::new();
    let mut slots: Option<SlotMap> = None;
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let mut out = Vec::new();
    for (i, au) in split_h264_aus(TEST_25FPS_H264).into_iter().enumerate() {
        let plan: AuPlan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {i} of the vendored H.264 vector must plan: {e}"));
        let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
        if map.capacity() != plan.picture.max_dpb_frames + 1 {
            *map = SlotMap::new(plan.picture.max_dpb_frames);
        }
        // `StatusReportFeedbackNumber` is 1-based; libavcodec uses `1 + report_id++`.
        let dxva = pf_dxvadec::plan_to_dxva(&plan, map, out.len() as u32 + 1)
            .unwrap_or_else(|e| panic!("AU {i} must convert: {e}"));
        // Apply `release_after_decode` even though this vector never fills it: skip it
        // and a convert-only loop holds a surface per AU and dries the ledger.
        for &id in &dxva.release_after_decode {
            assert!(
                map.release(id),
                "AU {i}: a deferred release named a picture holding no surface"
            );
        }
        let packed = pf_dxvadec::pack(au, &dxva.slice_ranges, &mut mapping)
            .unwrap_or_else(|e| panic!("AU {i} must pack: {e}"));
        let unpadded = pf_dxvadec::packed_size(au, &dxva.slice_ranges).expect("packed size") as u32;
        out.push(OurSubmission {
            pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
            qmatrix: Some(pf_dxvadec::as_bytes(&dxva.qmatrix).to_vec()),
            descriptors: pf_dxvadec::descriptors_h264(&dxva, &packed),
            records: packed.records,
            tiles: Vec::new(),
            unpadded,
            mb_count: dxva.mb_count,
        });
    }
    assert_eq!(out.len(), VENDORED_AUS);
    out
}

/// Same no-skip as H.264. `RaslSkipped` cannot arise on an IDR-start vector.
fn our_hevc_submissions() -> Vec<OurSubmission> {
    let mut planner = H265Planner::new();
    let mut slots: Option<SlotMap> = None;
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let mut out = Vec::new();
    for (i, au) in split_h265_aus(TEST_25FPS_H265).into_iter().enumerate() {
        let plan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {i} of the vendored HEVC vector must plan: {e}"));
        let map = slots.get_or_insert_with(|| SlotMap::new(plan.picture.max_dpb_frames));
        if map.capacity() != plan.picture.max_dpb_frames + 1 {
            *map = SlotMap::new(plan.picture.max_dpb_frames);
        }
        let dxva = pf_dxvadec::plan_to_dxva_h265(&plan, map, out.len() as u32 + 1)
            .unwrap_or_else(|e| panic!("AU {i} must convert: {e}"));
        let packed = pf_dxvadec::pack(au, &dxva.slice_ranges, &mut mapping)
            .unwrap_or_else(|e| panic!("AU {i} must pack: {e}"));
        let unpadded = pf_dxvadec::packed_size(au, &dxva.slice_ranges).expect("packed size") as u32;
        out.push(OurSubmission {
            pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
            qmatrix: dxva
                .qmatrix
                .as_ref()
                .map(|qm| pf_dxvadec::as_bytes(qm).to_vec()),
            descriptors: pf_dxvadec::descriptors_h265(&dxva, &packed),
            records: packed.records,
            tiles: Vec::new(),
            unpadded,
            mb_count: 0,
        });
    }
    assert_eq!(out.len(), VENDORED_AUS);
    out
}

/// 274 decoded frames (250 displayed). Capture index walks `ff_dxva2_common_end_frame`
/// pictures, not temporal units. This vector has no `show_existing_frame`.
const VENDORED_AV1_FRAMES: usize = 274;

/// One entry per decoded frame. Must apply `release_after_decode`: skip it and 268 of 274
/// frames hold a surface and the nine-slot ledger dries inside ten.
fn our_av1_submissions() -> Vec<OurSubmission> {
    let mut planner = Av1Planner::new();
    let mut slots = SlotMap::new(NUM_REF_SLOTS);
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let mut out = Vec::new();
    for (i, unit) in split_ivf(TEST_25FPS_AV1).into_iter().enumerate() {
        let plans = planner
            .plan_au(unit)
            .unwrap_or_else(|e| panic!("unit {i} of the vendored AV1 vector must plan: {e}"));
        for plan in &plans {
            if plan.dpb.stored.is_none() {
                continue; // `show_existing_frame`: no submission
            }
            let dxva = pf_dxvadec::plan_to_dxva_av1(unit, plan, &mut slots)
                .unwrap_or_else(|e| panic!("unit {i} must convert: {e}"));
            let packed = pf_dxvadec::pack_av1(unit, &dxva.bitstream, &dxva.tiles, &mut mapping)
                .unwrap_or_else(|e| panic!("unit {i} must pack: {e}"));
            let unpadded = pf_dxvadec::packed_size_av1(&dxva.bitstream) as u32;
            out.push(OurSubmission {
                pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
                // AV1 has no matrix buffer (`dxva2_av1_end_frame` passes `NULL, 0`).
                qmatrix: None,
                descriptors: pf_dxvadec::descriptors_av1(&packed),
                // `SliceRecord` checks do not apply to `DXVA_Tile_AV1`.
                records: Vec::new(),
                tiles: packed.tiles.clone(),
                unpadded,
                mb_count: 0,
            });
            for &id in &dxva.release_after_decode {
                assert!(
                    slots.release(id),
                    "unit {i}: a deferred release named a picture holding no surface"
                );
            }
        }
    }
    assert_eq!(out.len(), VENDORED_AV1_FRAMES);
    out
}

/// `(name, offset)` from field identifiers so a copy-paste cannot name the wrong byte.
/// Nested paths (`tiles.cols`) keep AV1's inner blocks from collapsing to one name.
macro_rules! field_table {
    ($ty:ty, $($($field:ident).+),+ $(,)?) => {
        &[$((stringify!($($field).+), offset_of!($ty, $($field).+))),+]
    };
}

/// Lengths from the next field's offset; `dxva.rs` proves no interior padding.
const H264_FIELDS: &[(&str, usize)] = field_table!(
    PicParamsH264,
    wFrameWidthInMbsMinus1,
    wFrameHeightInMbsMinus1,
    CurrPic,
    num_ref_frames,
    wBitFields,
    bit_depth_luma_minus8,
    bit_depth_chroma_minus8,
    Reserved16Bits,
    StatusReportFeedbackNumber,
    RefFrameList,
    CurrFieldOrderCnt,
    FieldOrderCntList,
    pic_init_qs_minus26,
    chroma_qp_index_offset,
    second_chroma_qp_index_offset,
    ContinuationFlag,
    pic_init_qp_minus26,
    num_ref_idx_l0_active_minus1,
    num_ref_idx_l1_active_minus1,
    Reserved8BitsA,
    FrameNumList,
    UsedForReferenceFlags,
    NonExistingFrameFlags,
    frame_num,
    log2_max_frame_num_minus4,
    pic_order_cnt_type,
    log2_max_pic_order_cnt_lsb_minus4,
    delta_pic_order_always_zero_flag,
    direct_8x8_inference_flag,
    entropy_coding_mode_flag,
    pic_order_present_flag,
    num_slice_groups_minus1,
    slice_group_map_type,
    deblocking_filter_control_present_flag,
    redundant_pic_cnt_present_flag,
    Reserved8BitsB,
    slice_group_change_rate_minus1,
    SliceGroupMap,
);

const HEVC_FIELDS: &[(&str, usize)] = field_table!(
    PicParamsHevc,
    PicWidthInMinCbsY,
    PicHeightInMinCbsY,
    wFormatAndSequenceInfoFlags,
    CurrPic,
    sps_max_dec_pic_buffering_minus1,
    log2_min_luma_coding_block_size_minus3,
    log2_diff_max_min_luma_coding_block_size,
    log2_min_transform_block_size_minus2,
    log2_diff_max_min_transform_block_size,
    max_transform_hierarchy_depth_inter,
    max_transform_hierarchy_depth_intra,
    num_short_term_ref_pic_sets,
    num_long_term_ref_pics_sps,
    num_ref_idx_l0_default_active_minus1,
    num_ref_idx_l1_default_active_minus1,
    init_qp_minus26,
    ucNumDeltaPocsOfRefRpsIdx,
    wNumBitsForShortTermRPSInSlice,
    ReservedBits2,
    dwCodingParamToolFlags,
    dwCodingSettingPicturePropertyFlags,
    pps_cb_qp_offset,
    pps_cr_qp_offset,
    num_tile_columns_minus1,
    num_tile_rows_minus1,
    column_width_minus1,
    row_height_minus1,
    diff_cu_qp_delta_depth,
    pps_beta_offset_div2,
    pps_tc_offset_div2,
    log2_parallel_merge_level_minus2,
    CurrPicOrderCntVal,
    RefPicList,
    ReservedBits5,
    PicOrderCntValList,
    RefPicSetStCurrBefore,
    RefPicSetStCurrAfter,
    RefPicSetLtCurr,
    ReservedBits6,
    ReservedBits7,
    StatusReportFeedbackNumber,
);

const H264_QMATRIX_FIELDS: &[(&str, usize)] =
    field_table!(QmatrixH264, bScalingLists4x4, bScalingLists8x8);

/// Ten-byte packed records; `#[repr(C)] {u32, u32, u16}` would be twelve.
const H264_SLICE_FIELDS: &[(&str, usize)] = field_table!(
    SliceH264Short,
    BSNALunitDataLocation,
    SliceBytesInBuffer,
    wBadSliceChopping,
);
const HEVC_SLICE_FIELDS: &[(&str, usize)] = field_table!(
    SliceHevcShort,
    BSNALunitDataLocation,
    SliceBytesInBuffer,
    wBadSliceChopping,
);

const HEVC_QMATRIX_FIELDS: &[(&str, usize)] = field_table!(
    QmatrixHevc,
    ucScalingLists0,
    ucScalingLists1,
    ucScalingLists2,
    ucScalingLists3,
    ucScalingListDCCoefSizeID2,
    ucScalingListDCCoefSizeID3,
);

/// Nested members: `tiles` is 260 bytes, `segmentation` 140. `frame_refs` stays whole — its
/// `Index` is an AV1 slot, not a surface, so it compares byte for byte. Surface array is
/// [`av1_reference_store`]. `film_grain` is 158 bytes this vector never codes.
const AV1_FIELDS: &[(&str, usize)] = field_table!(
    PicParamsAv1,
    width,
    height,
    max_width,
    max_height,
    curr_pic_texture_index,
    superres_denom,
    bitdepth,
    seq_profile,
    tiles.cols,
    tiles.rows,
    tiles.context_update_id,
    tiles.widths,
    tiles.heights,
    coding,
    format,
    primary_ref_frame,
    order_hint,
    order_hint_bits,
    frame_refs,
    ref_frame_map_texture_index,
    loop_filter.filter_level,
    loop_filter.filter_level_u,
    loop_filter.filter_level_v,
    loop_filter.sharpness_level,
    loop_filter.control_flags,
    loop_filter.ref_deltas,
    loop_filter.mode_deltas,
    loop_filter.delta_lf_res,
    loop_filter.frame_restoration_type,
    loop_filter.log2_restoration_unit_size,
    loop_filter.reserved16,
    quantization.control_flags,
    quantization.base_qindex,
    quantization.y_dc_delta_q,
    quantization.u_dc_delta_q,
    quantization.v_dc_delta_q,
    quantization.u_ac_delta_q,
    quantization.v_ac_delta_q,
    quantization.qm_y,
    quantization.qm_u,
    quantization.qm_v,
    quantization.reserved16,
    cdef.control_flags,
    cdef.y_strengths,
    cdef.uv_strengths,
    interp_filter,
    segmentation.control_flags,
    segmentation.reserved24,
    segmentation.feature_mask,
    segmentation.feature_data,
    film_grain,
    reserved32,
    status_report_feedback_number,
);

/// Size is what the descriptor states; `dxva.h` declares the type.
const AV1_TILE_FIELDS: &[(&str, usize)] = field_table!(
    TileAv1,
    data_offset,
    data_size,
    row,
    column,
    reserved16,
    anchor_frame,
    reserved8,
);

fn field_ranges(
    fields: &[(&'static str, usize)],
    total: usize,
) -> Vec<(&'static str, Range<usize>)> {
    fields
        .iter()
        .enumerate()
        .map(|(i, &(name, offset))| {
            let end = fields.get(i + 1).map_or(total, |&(_, next)| next);
            (name, offset..end)
        })
        .collect()
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn i32_at(bytes: &[u8], offset: usize) -> i32 {
    u32_at(bytes, offset) as i32
}

struct Finding {
    count: usize,
    first_au: usize,
    detail: String,
}

/// `note` fails the run; `document` prints an allowed divergence so it cannot go unread.
#[derive(Default)]
struct Findings {
    by_field: BTreeMap<String, Finding>,
    documented: BTreeMap<String, Finding>,
}

impl Findings {
    fn note(&mut self, field: impl Into<String>, au: usize, detail: impl Into<String>) {
        let entry = self
            .by_field
            .entry(field.into())
            .or_insert_with(|| Finding {
                count: 0,
                first_au: au,
                detail: detail.into(),
            });
        entry.count += 1;
    }

    fn document(&mut self, field: impl Into<String>, au: usize, reason: impl Into<String>) {
        let entry = self
            .documented
            .entry(field.into())
            .or_insert_with(|| Finding {
                count: 0,
                first_au: au,
                detail: reason.into(),
            });
        entry.count += 1;
    }

    fn is_empty(&self) -> bool {
        self.by_field.is_empty()
    }

    fn fields(&self) -> Vec<&str> {
        self.by_field.keys().map(String::as_str).collect()
    }

    fn documented_fields(&self) -> Vec<&str> {
        self.documented.keys().map(String::as_str).collect()
    }

    /// Prints AU count even when empty so a no-op compare cannot look clean.
    fn verdict(&self, what: &str, aus: usize) {
        for (field, documented) in &self.documented {
            println!(
                "{what}: {field} diverges on {} of {aus} AUs (first at AU {}) — DOCUMENTED, not a \
                 defect: {}",
                documented.count, documented.first_au, documented.detail
            );
        }
        if self.is_empty() {
            println!("{what}: {aus} AUs compared, no undocumented divergence");
            return;
        }
        println!(
            "{what}: {aus} AUs compared, {} fields diverge:",
            self.by_field.len()
        );
        for (field, finding) in &self.by_field {
            println!(
                "  {field}: {} AUs, first at AU {} — {}",
                finding.count, finding.first_au, finding.detail
            );
        }
        panic!(
            "{what}: {} fields diverge ({}) — read each against the module docs' list of \
             expected divergences before treating it as a defect",
            self.by_field.len(),
            self.fields().join(", ")
        );
    }
}

/// `(long-term, FrameNum/LongTermFrameIdx, TopPOC, BottomPOC)`. HEVC zeros members 2 and 4.
/// Re-marking long-term changes the key, so the mapping drops the earlier self — a miss,
/// never a false finding.
type PictureKey = (bool, u16, i32, i32);

/// Reference identity without surface index, so order differences do not count.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct RefEntry {
    long_term: bool,
    /// H.264 `FrameNum` or `LongTermFrameIdx`. Always 0 for HEVC.
    frame_num_or_lt_idx: u16,
    /// H.264 `FieldOrderCntList[i]`, or HEVC `PicOrderCntValList[i]` in `top`.
    top: i32,
    bottom: i32,
    used_top: bool,
    used_bottom: bool,
    non_existing: bool,
}

impl RefEntry {
    fn key(self) -> PictureKey {
        (
            self.long_term,
            self.frame_num_or_lt_idx,
            self.top,
            self.bottom,
        )
    }
}

/// libavcodec seeds `prev_poc_msb = 1 << 16` at IDR, so its POCs are spec + 65536. This crate
/// keeps 8.2.1 values; drivers use differences, so a uniform offset cancels. Compare relative:
/// first AU sets the base (only 0 or 65536), later AUs must hold it.
#[derive(Default)]
struct PocBase {
    offset: Option<i64>,
}

impl PocBase {
    fn offset(&self) -> i64 {
        self.offset.unwrap_or(0)
    }

    fn check(&mut self, au: usize, field: &str, ours: i32, theirs: i32, findings: &mut Findings) {
        let delta = i64::from(theirs) - i64::from(ours);
        match self.offset {
            None => {
                if delta != 0 && delta != 65536 {
                    findings.note(
                        format!("{field}[POC base]"),
                        au,
                        format!(
                            "libav's first POC is ours {ours} + {delta}, which is neither 0 nor \
                             FFmpeg's documented 65536 `prev_poc_msb` seed — an unexplained POC \
                             base is a finding, not a quirk to absorb"
                        ),
                    );
                }
                if delta == 65536 {
                    findings.document(
                        "FieldOrderCnt[POC base]",
                        au,
                        "libavcodec seeds `prev_poc_msb = 1 << 16` at every IDR, so its POCs are \
                         the specification's plus 65536; this crate keeps 8.2.1's values and the \
                         harness compares POCs RELATIVE to that constant, which it requires to \
                         hold on every AU",
                    );
                }
                self.offset = Some(delta);
            }
            Some(offset) if delta != offset => findings.note(
                format!("{field}[POC]"),
                au,
                format!(
                    "ours {ours}, libav {theirs}: a difference of {delta} where every earlier POC \
                     of this stream differed by {offset} — the POC base is not constant, so this \
                     is a real POC divergence rather than libav's base offset"
                ),
            ),
            Some(_) => {}
        }
    }
}

/// Subtract libav's POC base so the set compares pictures, not bases. `bottom_too` is false
/// for HEVC: `bottom` is a placeholder 0; shifting it would invent a difference.
fn shift_poc(entries: &mut [(u8, RefEntry)], offset: i64, bottom_too: bool) {
    for (_, entry) in entries.iter_mut() {
        entry.top = (i64::from(entry.top) - offset) as i32;
        if bottom_too {
            entry.bottom = (i64::from(entry.bottom) - offset) as i32;
        }
    }
}

/// Per-difference, not per-field: an allowed field still reports anything outside the reason.
type Allowance = fn(&str, &[u8], &[u8]) -> Option<&'static str>;

fn no_allowance(_: &str, _: &[u8], _: &[u8]) -> Option<&'static str> {
    None
}

/// Bit 10 of `dwCodingSettingPicturePropertyFlags` (`loop_filter_across_tiles_enabled_flag`):
/// 7.4.3.3.1 infers 1 when tiles are off; libavcodec emits 0. Inert with tiles disabled.
/// Only that bit, ours-set/theirs-clear, tiles off on both sides.
fn hevc_allowance(field: &str, ours: &[u8], theirs: &[u8]) -> Option<&'static str> {
    /// `tiles_enabled_flag`.
    const TILES: u32 = 1 << 7;
    /// `loop_filter_across_tiles_enabled_flag`.
    const ACROSS_TILES: u32 = 1 << 10;

    if field != "dwCodingSettingPicturePropertyFlags" {
        return None;
    }
    let (Ok(ours), Ok(theirs)) = (
        <[u8; 4]>::try_from(ours).map(u32::from_le_bytes),
        <[u8; 4]>::try_from(theirs).map(u32::from_le_bytes),
    ) else {
        return None;
    };
    let only_bit_10 = ours ^ theirs == ACROSS_TILES;
    let ours_sets_it = ours & ACROSS_TILES != 0;
    let tiles_off = (ours | theirs) & TILES == 0;
    (only_bit_10 && ours_sets_it && tiles_off).then_some(
        "loop_filter_across_tiles_enabled_flag (bit 10): 7.4.3.3.1 infers 1 when the PPS codes no \
         tiles and the vendored parser reports that; libavcodec emits 0. Inert either way — with \
         tiles_enabled_flag clear there is no tile boundary for a loop filter to cross",
    )
}

fn h264_ref_entries(pp: &[u8]) -> Vec<(u8, RefEntry)> {
    let list = offset_of!(PicParamsH264, RefFrameList);
    let poc = offset_of!(PicParamsH264, FieldOrderCntList);
    let nums = offset_of!(PicParamsH264, FrameNumList);
    let used = u32_at(pp, offset_of!(PicParamsH264, UsedForReferenceFlags));
    let missing = u16_at(pp, offset_of!(PicParamsH264, NonExistingFrameFlags));
    (0..16)
        .filter(|i| pp[list + i] != UNUSED_ENTRY)
        .map(|i| {
            (
                pp[list + i] & 0x7F,
                RefEntry {
                    long_term: pp[list + i] & 0x80 != 0,
                    frame_num_or_lt_idx: u16_at(pp, nums + 2 * i),
                    top: i32_at(pp, poc + 8 * i),
                    bottom: i32_at(pp, poc + 8 * i + 4),
                    used_top: used >> (2 * i) & 1 != 0,
                    used_bottom: used >> (2 * i + 1) & 1 != 0,
                    non_existing: missing >> i & 1 != 0,
                },
            )
        })
        .collect()
}

/// HEVC has no FrameNum/use flags; residency is the statement, so those members stay neutral.
fn hevc_ref_entries(pp: &[u8]) -> Vec<(u8, RefEntry)> {
    let list = offset_of!(PicParamsHevc, RefPicList);
    let poc = offset_of!(PicParamsHevc, PicOrderCntValList);
    (0..15)
        .filter(|i| pp[list + i] != UNUSED_ENTRY)
        .map(|i| {
            (
                pp[list + i] & 0x7F,
                RefEntry {
                    long_term: pp[list + i] & 0x80 != 0,
                    frame_num_or_lt_idx: 0,
                    top: i32_at(pp, poc + 4 * i),
                    bottom: 0,
                    used_top: true,
                    used_bottom: true,
                    non_existing: false,
                },
            )
        })
        .collect()
}

/// `(CurrPicTextureIndex, RefFrameMapTextureIndex[8], frame_refs[name].Index[7])`.
/// `frame_refs[].Index` is an AV1 slot from the frame header (value). The other two are
/// surfaces (shape only: occupancy and decode-target collision).
fn av1_reference_store(pp: &[u8]) -> (u8, [u8; 8], [u8; 7]) {
    let curr = pp[offset_of!(PicParamsAv1, curr_pic_texture_index)];
    let mut store = [UNUSED_INDEX; 8];
    let base = offset_of!(PicParamsAv1, ref_frame_map_texture_index);
    store.copy_from_slice(&pp[base..base + 8]);
    let mut names = [UNUSED_INDEX; 7];
    // Stride and member offset from the type, not the 36/33 `dxva_av1.rs` pins. A second
    // copy would drift: slot vs warp coefficient.
    for (name, slot) in names.iter_mut().enumerate() {
        *slot = pp[offset_of!(PicParamsAv1, frame_refs)
            + name * size_of::<PicEntryAv1>()
            + offset_of!(PicEntryAv1, index)];
    }
    (curr, store, names)
}

/// Per-picture surface pairing. Not a stream-wide index bijection: both sides reuse
/// surfaces after DPB exit, not necessarily together. Drop the pair when either reassigns.
#[derive(Default)]
struct SurfaceMapping {
    live: BTreeMap<PictureKey, (u8, u8)>,
}

impl SurfaceMapping {
    fn observe(
        &mut self,
        au: usize,
        field: &str,
        pairs: &[(PictureKey, (u8, u8))],
        findings: &mut Findings,
    ) {
        let mut ours_seen: BTreeMap<u8, PictureKey> = BTreeMap::new();
        let mut theirs_seen: BTreeMap<u8, PictureKey> = BTreeMap::new();
        for &(key, (ours, theirs)) in pairs {
            if let Some(&(known_ours, known_theirs)) = self.live.get(&key) {
                if (known_ours, known_theirs) != (ours, theirs) {
                    findings.note(
                        format!("{field}[surface mapping]"),
                        au,
                        format!(
                            "picture {key:?} was surface {known_ours} (ours) = {known_theirs} \
                             (libav) and is now {ours} = {theirs}: the mapping is not a \
                             bijection over this picture's lifetime"
                        ),
                    );
                }
            }
            if let Some(other) = ours_seen.insert(ours, key) {
                if other != key {
                    findings.note(
                        format!("{field}[surface aliasing]"),
                        au,
                        format!("our surface {ours} carries both {other:?} and {key:?}"),
                    );
                }
            }
            if let Some(other) = theirs_seen.insert(theirs, key) {
                if other != key {
                    findings.note(
                        format!("{field}[surface aliasing]"),
                        au,
                        format!("libav's surface {theirs} carries both {other:?} and {key:?}"),
                    );
                }
            }
            self.live.insert(key, (ours, theirs));
        }
        // Drop pairs whose surface was reassigned: pool reuse, not a broken mapping.
        self.live.retain(|key, &mut (ours, theirs)| {
            let ours_now = ours_seen.get(&ours);
            let theirs_now = theirs_seen.get(&theirs);
            ours_now.is_none_or(|k| k == key) && theirs_now.is_none_or(|k| k == key)
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapturedDescriptor {
    buffer_type: u32,
    data_size: u32,
    num_mbs_in_buffer: u32,
    data_offset: u32,
}

#[derive(Default)]
struct Capture {
    pic_params: BTreeMap<usize, Vec<u8>>,
    /// Missing key ≠ `None` (`absent`); an AU not in this map was never reported.
    qmatrix: BTreeMap<usize, Option<Vec<u8>>>,
    descriptors: BTreeMap<usize, Vec<CapturedDescriptor>>,
    config_bitstream_raw: BTreeMap<usize, u32>,
    /// Malformed prefix lines. Fail loud; do not compare a shorter capture.
    unreadable: Vec<String>,
}

fn from_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 || hex.is_empty() {
        return None;
    }
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(2 * bytes.len());
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// `codec` is FFmpeg `avcodec_get_name`: `h264`, `hevc`, or `av1`.
fn parse_capture(text: &str, codec: &str) -> Capture {
    /// Trailing space so a bare word cannot match.
    const MARKERS: [&str; 4] = ["PFPP ", "PFQM ", "PFBD ", "PFCFG "];

    let mut out = Capture::default();
    for raw in text.lines() {
        // Marker anywhere in the line: FFmpeg prefixes `[h264 @ 0x…] ` on codec-context logs.
        let Some(start) = MARKERS.iter().filter_map(|m| raw.find(m)).min() else {
            continue;
        };
        let line = raw[start..].trim();
        let Some((prefix, rest)) = line.split_once(' ') else {
            continue;
        };
        if !matches!(prefix, "PFPP" | "PFQM" | "PFBD" | "PFCFG") {
            continue;
        }
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let (Some(line_codec), Some(au)) = (fields.first(), fields.get(1)) else {
            out.unreadable.push(line.to_string());
            continue;
        };
        if *line_codec != codec {
            continue;
        }
        let Ok(au) = au.parse::<usize>() else {
            out.unreadable.push(line.to_string());
            continue;
        };
        let ok = match (prefix, &fields[2..]) {
            ("PFPP", [hex]) => match from_hex(hex) {
                Some(bytes) => out.pic_params.insert(au, bytes).is_none(),
                None => false,
            },
            ("PFQM", ["absent"]) => out.qmatrix.insert(au, None).is_none(),
            ("PFQM", [hex]) => match from_hex(hex) {
                Some(bytes) => out.qmatrix.insert(au, Some(bytes)).is_none(),
                None => false,
            },
            ("PFBD", [kind, size, mbs, offset]) => {
                match (
                    kind.parse::<u32>(),
                    size.parse::<u32>(),
                    mbs.parse::<u32>(),
                    offset.parse::<u32>(),
                ) {
                    (Ok(buffer_type), Ok(data_size), Ok(num_mbs_in_buffer), Ok(data_offset)) => {
                        out.descriptors
                            .entry(au)
                            .or_default()
                            .push(CapturedDescriptor {
                                buffer_type,
                                data_size,
                                num_mbs_in_buffer,
                                data_offset,
                            });
                        true
                    }
                    _ => false,
                }
            }
            ("PFCFG", [raw]) => match raw.parse::<u32>() {
                Ok(raw) => {
                    out.config_bitstream_raw.insert(au, raw);
                    true
                }
                Err(_) => false,
            },
            _ => false,
        };
        if !ok {
            out.unreadable.push(line.to_string());
        }
    }
    out
}

fn capture_from_env(var: &str, codec: &str) -> Option<Capture> {
    let path = std::env::var(var).ok()?;
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{var}={path} could not be read: {e}"));
    Some(parse_capture(&text, codec))
}

fn preflight(capture: &Capture, ours: usize, codec: &str, reserved16: Option<usize>) {
    assert!(
        capture.unreadable.is_empty(),
        "the capture holds {} unreadable line(s) — the first is:\n  {}\nthe recipe in this \
         file's module docs is the format",
        capture.unreadable.len(),
        capture.unreadable[0]
    );
    assert!(
        !capture.pic_params.is_empty(),
        "the capture holds no `PFPP {codec}` lines: either the patch did not apply or the \
         hwaccel never engaged (a software fallback logs nothing)"
    );
    assert_eq!(
        capture.pic_params.len(),
        ours,
        "the capture covers {} AUs and this crate plans {ours} — the two sides must decode the \
         same elementary stream, split the same way",
        capture.pic_params.len()
    );
    let expected: BTreeSet<usize> = (0..ours).collect();
    let seen: BTreeSet<usize> = capture.pic_params.keys().copied().collect();
    assert_eq!(
        seen, expected,
        "the capture's AU indices are not 0..{ours}; pairing by index would compare different \
         pictures"
    );
    if let Some(offset) = reserved16 {
        let zeroed = capture
            .pic_params
            .values()
            .filter(|pp| pp.len() > offset + 1 && u16_at(pp, offset) == 0)
            .count();
        assert_eq!(
            zeroed, 0,
            "{zeroed} of {ours} captured pictures carry Reserved16Bits = 0, which libavcodec \
             writes only under FF_DXVA2_WORKAROUND_INTEL_CLEARVIDEO or \
             FF_DXVA2_WORKAROUND_SCALING_LIST_ZIGZAG — this capture is against a workaround \
             path and is VOID for comparison (module docs)"
        );
    }
    // Wrong `PFCFG` voids slice-control compare (driver read the other struct). AV1 has
    // no short/long pair; a `_ =>` arm would test `DXVA_Tile_AV1` against HEVC's 1.
    let want = match codec {
        "h264" => pf_dxvadec::short_slice_config(Codec::H264),
        "hevc" => pf_dxvadec::short_slice_config(Codec::H265),
        _ => return,
    };
    for (au, &raw) in &capture.config_bitstream_raw {
        assert_eq!(
            raw, want,
            "AU {au}: the capture's ConfigBitstreamRaw is {raw}, and short format for {codec} \
             is {want} — the capture used the other slice-control format, so its slice-control \
             sizes describe a different struct"
        );
    }
}

/// Hex when ≤8 bytes; else count and first differing byte (`SliceGroupMap` is 810 bytes).
fn byte_diff_detail(ours: &[u8], theirs: &[u8]) -> String {
    if ours.len() <= 8 {
        return format!("ours {}, libav {}", to_hex(ours), to_hex(theirs));
    }
    let first = ours
        .iter()
        .zip(theirs)
        .position(|(a, b)| a != b)
        .unwrap_or(0);
    let differing = ours.iter().zip(theirs).filter(|(a, b)| a != b).count();
    format!(
        "{differing} of {} bytes differ, first at byte {first} of the field (ours {:#04x}, libav \
         {:#04x})",
        ours.len(),
        ours[first],
        theirs[first]
    )
}

fn compare_scalars(
    au: usize,
    ours: &[u8],
    theirs: &[u8],
    ranges: &[(&'static str, Range<usize>)],
    structural: &[&str],
    allowance: Allowance,
    findings: &mut Findings,
) {
    let mut classified = vec![false; ours.len()];
    for (name, range) in ranges {
        for byte in range.clone() {
            classified[byte] = true;
        }
        if structural.contains(name) {
            continue;
        }
        if ours[range.clone()] != theirs[range.clone()] {
            match allowance(name, &ours[range.clone()], &theirs[range.clone()]) {
                Some(reason) => findings.document(*name, au, reason),
                None => findings.note(
                    *name,
                    au,
                    byte_diff_detail(&ours[range.clone()], &theirs[range.clone()]),
                ),
            }
        }
    }
    // Untabled bytes still report by raw offset; a gap cannot pass silently.
    for (offset, covered) in classified.iter().enumerate() {
        if !covered && ours[offset] != theirs[offset] {
            findings.note(
                format!("<unclassified byte {offset:#06x}>"),
                au,
                format!("ours {:#04x}, libav {:#04x}", ours[offset], theirs[offset]),
            );
        }
    }
}

fn compare_ref_array(
    au: usize,
    field: &str,
    ours: &[(u8, RefEntry)],
    theirs: &[(u8, RefEntry)],
    mapping: &mut SurfaceMapping,
    findings: &mut Findings,
) {
    let mut ours_sorted: Vec<RefEntry> = ours.iter().map(|&(_, e)| e).collect();
    let mut theirs_sorted: Vec<RefEntry> = theirs.iter().map(|&(_, e)| e).collect();
    ours_sorted.sort_unstable();
    theirs_sorted.sort_unstable();
    if ours_sorted != theirs_sorted {
        let only_ours: Vec<&RefEntry> = ours_sorted
            .iter()
            .filter(|e| !theirs_sorted.contains(e))
            .collect();
        let only_theirs: Vec<&RefEntry> = theirs_sorted
            .iter()
            .filter(|e| !ours_sorted.contains(e))
            .collect();
        findings.note(
            format!("{field}[set]"),
            au,
            format!(
                "{} entries ours vs {} libav; only ours: {only_ours:?}; only libav: {only_theirs:?}",
                ours.len(),
                theirs.len()
            ),
        );
    }

    // Ambiguous keys are skipped and reported; guessing a pair would invent a mapping.
    let mut pairs = Vec::new();
    for &(our_slot, entry) in ours {
        let key = entry.key();
        let ours_same = ours.iter().filter(|(_, e)| e.key() == key).count();
        let matches: Vec<u8> = theirs
            .iter()
            .filter(|(_, e)| e.key() == key)
            .map(|&(slot, _)| slot)
            .collect();
        if ours_same > 1 || matches.len() > 1 {
            findings.note(
                format!("{field}[ambiguous key]"),
                au,
                format!(
                    "{key:?} appears {ours_same} times ours and {} libav",
                    matches.len()
                ),
            );
            continue;
        }
        if let Some(&their_slot) = matches.first() {
            pairs.push((key, (our_slot, their_slot)));
        }
    }
    mapping.observe(au, field, &pairs, findings);
}

fn compare_h264_picparams(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let ranges = field_ranges(H264_FIELDS, size_of::<PicParamsH264>());
    let structural = [
        "CurrPic",
        // Compared relative via `PocBase`, not byte-for-byte.
        "CurrFieldOrderCnt",
        "RefFrameList",
        "FieldOrderCntList",
        "FrameNumList",
        "UsedForReferenceFlags",
        "NonExistingFrameFlags",
    ];
    let mut findings = Findings::default();
    let mut mapping = SurfaceMapping::default();
    let mut poc = PocBase::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.pic_params.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFPP line for this AU",
            );
            continue;
        };
        if theirs.len() != sub.pic_params.len() {
            findings.note(
                "<struct size>",
                au,
                format!(
                    "the capture's picture parameters are {} bytes and ours are {} — the \
                     hand-declared layout and the header disagree, which is a finding on its own",
                    theirs.len(),
                    sub.pic_params.len()
                ),
            );
            continue;
        }
        compare_scalars(
            au,
            &sub.pic_params,
            theirs,
            &ranges,
            &structural,
            no_allowance,
            &mut findings,
        );
        // Base from CurrFieldOrderCnt (same picture on both sides), then required of later POCs.
        let poc_at = offset_of!(PicParamsH264, CurrFieldOrderCnt);
        poc.check(
            au,
            "CurrFieldOrderCnt[0]",
            i32_at(&sub.pic_params, poc_at),
            i32_at(theirs, poc_at),
            &mut findings,
        );
        poc.check(
            au,
            "CurrFieldOrderCnt[1]",
            i32_at(&sub.pic_params, poc_at + 4),
            i32_at(theirs, poc_at + 4),
            &mut findings,
        );
        let mut their_entries = h264_ref_entries(theirs);
        shift_poc(&mut their_entries, poc.offset(), true);
        compare_ref_array(
            au,
            "RefFrameList",
            &h264_ref_entries(&sub.pic_params),
            &their_entries,
            &mut mapping,
            &mut findings,
        );
        // Same AU is the same picture; feed CurrPic into the mapping so later refs check this pair.
        let curr = offset_of!(PicParamsH264, CurrPic);
        let frame_num = offset_of!(PicParamsH264, frame_num);
        if sub.pic_params[curr] & 0x80 != theirs[curr] & 0x80 {
            findings.note(
                "CurrPic[AssociatedFlag]",
                au,
                format!(
                    "ours {:#04x}, libav {:#04x} — the bottom-field flag, which is 0 for every \
                     picture inside this backend's progressive envelope",
                    sub.pic_params[curr], theirs[curr]
                ),
            );
        }
        let key = (
            false,
            u16_at(&sub.pic_params, frame_num),
            i32_at(&sub.pic_params, poc_at),
            i32_at(&sub.pic_params, poc_at + 4),
        );
        mapping.observe(
            au,
            "CurrPic",
            &[(key, (sub.pic_params[curr] & 0x7F, theirs[curr] & 0x7F))],
            &mut findings,
        );
    }
    findings
}

/// Resolve RPS indexes through this side's `RefPicList`; raw indexes are not comparable.
fn hevc_rps_pictures(pp: &[u8], array: usize, entries: &[(u8, RefEntry)]) -> Vec<Option<RefEntry>> {
    let list = offset_of!(PicParamsHevc, RefPicList);
    (0..8)
        .map(|i| {
            let index = pp[array + i];
            // `0xFF` and any index ≥15 name nothing; do not treat them as a `RefPicList` slot.
            if usize::from(index) >= 15 {
                return None;
            }
            let slot = pp[list + usize::from(index)];
            if slot == UNUSED_ENTRY {
                return None;
            }
            entries
                .iter()
                .find(|(s, _)| *s == slot & 0x7F)
                .map(|&(_, entry)| entry)
        })
        .collect()
}

/// `frame_refs[].Index` is an AV1 slot, not a surface: those 36-byte entries compare byte
/// for byte. Surfaces are occupancy/collision only ([`av1_reference_store`]). `order_hint`
/// is coded, so no POC base. No allowance on `width`/`height`: this crate sends
/// UpscaledWidth, libavcodec FrameWidth; equal with superres off (this vector). A superres
/// capture must report, not absorb. See `pic_av1.rs` at `pp.width`.
fn compare_av1_picparams(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let ranges = field_ranges(AV1_FIELDS, size_of::<PicParamsAv1>());
    // Surfaces only; every other byte is from the same frame header.
    let structural = ["curr_pic_texture_index", "ref_frame_map_texture_index"];
    let mut findings = Findings::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.pic_params.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFPP line for this frame",
            );
            continue;
        };
        if theirs.len() != sub.pic_params.len() {
            findings.note(
                "<struct size>",
                au,
                format!(
                    "the capture's picture parameters are {} bytes and ours are {}",
                    theirs.len(),
                    sub.pic_params.len()
                ),
            );
            continue;
        }
        compare_scalars(
            au,
            &sub.pic_params,
            theirs,
            &ranges,
            &structural,
            no_allowance,
            &mut findings,
        );

        // Occupancy must agree; surface numbers come from each side's pool.
        let (our_curr, our_store, _) = av1_reference_store(&sub.pic_params);
        let (their_curr, their_store, _) = av1_reference_store(theirs);
        for slot in 0..8 {
            let ours_occupied = our_store[slot] != UNUSED_INDEX;
            let theirs_occupied = their_store[slot] != UNUSED_INDEX;
            if ours_occupied != theirs_occupied {
                findings.note(
                    format!("ref_frame_map_texture_index[{slot}][occupied]"),
                    au,
                    format!("ours {ours_occupied}, libav {theirs_occupied}"),
                );
            }
        }
        // Decode target must not alias a store surface. Do not require unique surfaces
        // across slots: one picture in several slots is ordinary (this vector's key frame
        // sits in BWDREF and ALTREF2); that check would fire on 273 of 274 correct frames.
        for (label, curr, store) in [
            ("ours", our_curr, our_store),
            ("libav", their_curr, their_store),
        ] {
            if store.contains(&curr) {
                findings.note(
                    "curr_pic_texture_index[aliases the store]",
                    au,
                    format!(
                        "{label}: surface {curr} is both the decode target and a reference \
                         store entry — the frame decodes into a picture it predicts from"
                    ),
                );
            }
        }
    }
    findings
}

fn compare_hevc_picparams(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let ranges = field_ranges(HEVC_FIELDS, size_of::<PicParamsHevc>());
    let structural = [
        "CurrPic",
        // Relative like H.264. This vector's HEVC POCs have offset 0; derive, do not assume.
        "CurrPicOrderCntVal",
        "RefPicList",
        "PicOrderCntValList",
        "RefPicSetStCurrBefore",
        "RefPicSetStCurrAfter",
        "RefPicSetLtCurr",
    ];
    let mut findings = Findings::default();
    let mut mapping = SurfaceMapping::default();
    let mut poc = PocBase::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.pic_params.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFPP line for this AU",
            );
            continue;
        };
        if theirs.len() != sub.pic_params.len() {
            findings.note(
                "<struct size>",
                au,
                format!(
                    "the capture's picture parameters are {} bytes and ours are {}",
                    theirs.len(),
                    sub.pic_params.len()
                ),
            );
            continue;
        }
        compare_scalars(
            au,
            &sub.pic_params,
            theirs,
            &ranges,
            &structural,
            hevc_allowance,
            &mut findings,
        );
        let poc_at = offset_of!(PicParamsHevc, CurrPicOrderCntVal);
        poc.check(
            au,
            "CurrPicOrderCntVal",
            i32_at(&sub.pic_params, poc_at),
            i32_at(theirs, poc_at),
            &mut findings,
        );
        let our_entries = hevc_ref_entries(&sub.pic_params);
        let mut their_entries = hevc_ref_entries(theirs);
        // `bottom` is unused; do not shift it.
        shift_poc(&mut their_entries, poc.offset(), false);
        compare_ref_array(
            au,
            "RefPicList",
            &our_entries,
            &their_entries,
            &mut mapping,
            &mut findings,
        );
        for (name, offset) in [
            (
                "RefPicSetStCurrBefore",
                offset_of!(PicParamsHevc, RefPicSetStCurrBefore),
            ),
            (
                "RefPicSetStCurrAfter",
                offset_of!(PicParamsHevc, RefPicSetStCurrAfter),
            ),
            (
                "RefPicSetLtCurr",
                offset_of!(PicParamsHevc, RefPicSetLtCurr),
            ),
        ] {
            let ours_named = hevc_rps_pictures(&sub.pic_params, offset, &our_entries);
            let theirs_named = hevc_rps_pictures(theirs, offset, &their_entries);
            for (position, (a, b)) in ours_named.iter().zip(&theirs_named).enumerate() {
                if a != b {
                    findings.note(
                        format!("{name}[{position}]"),
                        au,
                        format!("ours names {a:?}, libav names {b:?}"),
                    );
                }
            }
        }
        let curr = offset_of!(PicParamsHevc, CurrPic);
        let key = (false, 0u16, i32_at(&sub.pic_params, poc_at), 0);
        mapping.observe(
            au,
            "CurrPic",
            &[(key, (sub.pic_params[curr] & 0x7F, theirs[curr] & 0x7F))],
            &mut findings,
        );
    }
    findings
}

/// Presence first, then contents. Missing capture key is a finding, not `absent`.
fn compare_qmatrix(
    ours: &[OurSubmission],
    capture: &Capture,
    fields: &[(&'static str, usize)],
    total: usize,
) -> Findings {
    let ranges = field_ranges(fields, total);
    let mut findings = Findings::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.qmatrix.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture reports the matrix neither present nor `absent` for this AU — the \
                 PFQM patch (recipe step 4) is missing",
            );
            continue;
        };
        match (&sub.qmatrix, theirs) {
            (None, None) => {}
            (Some(_), None) => findings.note(
                "<submitted>",
                au,
                "we submit an inverse-quantization-matrix buffer where libavcodec submits NONE — \
                 for HEVC this is review 13's defect: the picture parameters have told the \
                 driver to ignore the matrix, and a driver that honours it anyway dequantizes \
                 every residual against it",
            ),
            (None, Some(_)) => findings.note(
                "<submitted>",
                au,
                "libavcodec submits an inverse-quantization-matrix buffer and we submit none — \
                 the hardware is left to dequantize against whatever it last held",
            ),
            (Some(mine), Some(theirs)) => {
                if mine.len() != theirs.len() {
                    findings.note(
                        "<struct size>",
                        au,
                        format!("ours {} bytes, libav {}", mine.len(), theirs.len()),
                    );
                    continue;
                }
                for (name, range) in &ranges {
                    if mine[range.clone()] != theirs[range.clone()] {
                        findings.note(
                            *name,
                            au,
                            byte_diff_detail(&mine[range.clone()], &theirs[range.clone()]),
                        );
                    }
                }
            }
        }
    }
    findings
}

fn buffer_name(buffer_type: u32) -> &'static str {
    match buffer_type {
        BUFFER_PICTURE_PARAMETERS => "PICTURE_PARAMETERS",
        BUFFER_INVERSE_QUANTIZATION_MATRIX => "INVERSE_QUANTIZATION_MATRIX",
        BUFFER_SLICE_CONTROL => "SLICE_CONTROL",
        BUFFER_BITSTREAM => "BITSTREAM",
        _ => "<unknown buffer type>",
    }
}

/// All fields exact except bitstream `DataSize`, which has a delimitation class.
fn compare_descriptors(ours: &[OurSubmission], capture: &Capture) -> Findings {
    let mut findings = Findings::default();
    for (au, sub) in ours.iter().enumerate() {
        let Some(theirs) = capture.descriptors.get(&au) else {
            findings.note(
                "<no capture>",
                au,
                "the capture holds no PFBD lines for this AU",
            );
            continue;
        };
        let our_types: Vec<u32> = sub.descriptors.iter().map(|d| d.buffer_type).collect();
        let their_types: Vec<u32> = theirs.iter().map(|d| d.buffer_type).collect();
        if our_types != their_types {
            // Missing BITSTREAM is a missed patch: libavcodec fills that descriptor outside
            // the choke point.
            let detail = if !their_types.contains(&BUFFER_BITSTREAM) {
                "the capture carries no BITSTREAM descriptor at all: recipe step 2's SECOND \
                 patch site (the inline fill in commit_bitstream_and_slice_buffer) was missed"
                    .to_string()
            } else {
                format!(
                    "ours {:?}, libav {:?}",
                    our_types
                        .iter()
                        .map(|&t| buffer_name(t))
                        .collect::<Vec<_>>(),
                    their_types
                        .iter()
                        .map(|&t| buffer_name(t))
                        .collect::<Vec<_>>()
                )
            };
            findings.note("<buffer set>", au, detail);
        }
        for our_desc in &sub.descriptors {
            let name = buffer_name(our_desc.buffer_type);
            let Some(their_desc) = theirs
                .iter()
                .find(|d| d.buffer_type == our_desc.buffer_type)
            else {
                continue; // already reported by the set comparison
            };
            if our_desc.data_offset != their_desc.data_offset {
                findings.note(
                    format!("{name}.DataOffset"),
                    au,
                    format!(
                        "ours {}, libav {}",
                        our_desc.data_offset, their_desc.data_offset
                    ),
                );
            }
            if our_desc.num_mbs_in_buffer != their_desc.num_mbs_in_buffer {
                findings.note(
                    format!("{name}.NumMBsInBuffer"),
                    au,
                    format!(
                        "ours {}, libav {} — this field cannot legitimately differ: \
                         mb_width*mb_height on H.264's bitstream and slice-control buffers, 0 \
                         everywhere else",
                        our_desc.num_mbs_in_buffer, their_desc.num_mbs_in_buffer
                    ),
                );
            }
            if our_desc.data_size == their_desc.data_size {
                continue;
            }
            if our_desc.buffer_type != BUFFER_BITSTREAM {
                findings.note(
                    format!("{name}.DataSize"),
                    au,
                    format!(
                        "ours {}, libav {} — a fixed-size buffer ({} is `sizeof` a structure or \
                         slices * 10), so this cannot legitimately differ",
                        our_desc.data_size, their_desc.data_size, name
                    ),
                );
                continue;
            }
            // Slice count from their slice-control size (10-byte records, both codecs).
            // Different counts void the size compare; a non-multiple is already a DataSize finding.
            let their_slices = theirs
                .iter()
                .find(|d| d.buffer_type == BUFFER_SLICE_CONTROL)
                .map(|d| d.data_size as usize / size_of::<SliceH264Short>());
            let our_slices = sub.records.len();
            match their_slices {
                Some(count) if count != our_slices => findings.note(
                    "BITSTREAM.DataSize[slice count]",
                    au,
                    format!(
                        "ours {} bytes over {our_slices} slices, libav {} over {count} — the two \
                         sides disagree about how many slices this AU has, which voids the size \
                         comparison and is the finding itself",
                        our_desc.data_size, their_desc.data_size
                    ),
                ),
                _ if their_desc.data_size % 128 != 0 => findings.note(
                    "BITSTREAM.DataSize[unpadded]",
                    au,
                    format!(
                        "libav's {} is not a multiple of 128, which means its tail padding was \
                         clamped by a mapping too small for the AU",
                        their_desc.data_size
                    ),
                ),
                _ => {
                    // Classify on unpadded size: 128-byte pad can turn an 8-byte split
                    // difference into 0 or 128. Their pad is 1..=128; legitimate iff that
                    // window is within four bytes per slice of our unpadded size.
                    let delta = i64::from(our_desc.data_size) - i64::from(their_desc.data_size);
                    let tolerance = 4 * our_slices.max(1) as i64;
                    let their_low = i64::from(their_desc.data_size) - 128;
                    let their_high = i64::from(their_desc.data_size) - 1;
                    let ours_unpadded = i64::from(sub.unpadded);
                    let legitimate = their_low <= ours_unpadded + tolerance
                        && ours_unpadded - tolerance <= their_high;
                    findings.note(
                        if legitimate {
                            "BITSTREAM.DataSize[delimitation]"
                        } else {
                            "BITSTREAM.DataSize"
                        },
                        au,
                        format!(
                            "ours {} (unpadded {ours_unpadded}), libav {} (unpadded \
                             {their_low}..={their_high}), delta {delta} over {our_slices} \
                             slices — {}",
                            our_desc.data_size,
                            their_desc.data_size,
                            if legitimate {
                                "within the trailing-zero delimitation class the module docs \
                                 describe, but read it once rather than assume it"
                            } else {
                                "OUTSIDE the legitimate delimitation class: too large to be \
                                 trailing zeros"
                            }
                        ),
                    );
                }
            }
        }
    }
    findings
}

/// Writer shares no code with the parser; a format drift fails the self-compare.
fn dump(codec: &str, ours: &[OurSubmission]) -> String {
    let mut text = String::new();
    let raw = pf_dxvadec::short_slice_config(match codec {
        "h264" => Codec::H264,
        _ => Codec::H265,
    });
    let _ = writeln!(text, "PFCFG {codec} 0 {raw}");
    for (au, sub) in ours.iter().enumerate() {
        let _ = writeln!(text, "PFPP {codec} {au} {}", to_hex(&sub.pic_params));
        match &sub.qmatrix {
            Some(qm) => {
                let _ = writeln!(text, "PFQM {codec} {au} {}", to_hex(qm));
            }
            None => {
                let _ = writeln!(text, "PFQM {codec} {au} absent");
            }
        }
        for desc in &sub.descriptors {
            let _ = writeln!(
                text,
                "PFBD {codec} {au} {} {} {} {}",
                desc.buffer_type, desc.data_size, desc.num_mbs_in_buffer, desc.data_offset
            );
        }
    }
    text
}

/// Field table tiles each struct with no gap or tail. That is 1-byte packing (`dxva.h`); a
/// gap would name the wrong field. Slice records are here because `repr(C)` would be 12.
#[test]
fn every_hand_declared_dxva_struct_is_tiled_exactly_by_its_fields() {
    for (what, fields, total) in [
        ("PicParamsH264", H264_FIELDS, size_of::<PicParamsH264>()),
        ("PicParamsHevc", HEVC_FIELDS, size_of::<PicParamsHevc>()),
        ("QmatrixH264", H264_QMATRIX_FIELDS, size_of::<QmatrixH264>()),
        ("QmatrixHevc", HEVC_QMATRIX_FIELDS, size_of::<QmatrixHevc>()),
        (
            "SliceH264Short",
            H264_SLICE_FIELDS,
            size_of::<SliceH264Short>(),
        ),
        (
            "SliceHevcShort",
            HEVC_SLICE_FIELDS,
            size_of::<SliceHevcShort>(),
        ),
        // `PicParamsAv1` offsets are measured (`layout-probe-av1.c`); this asserts the table
        // covers all 912 bytes.
        ("PicParamsAv1", AV1_FIELDS, size_of::<PicParamsAv1>()),
        ("TileAv1", AV1_TILE_FIELDS, size_of::<TileAv1>()),
    ] {
        assert_eq!(fields[0].1, 0, "{what}: the first field must start at 0");
        let ranges = field_ranges(fields, total);
        let mut next = 0usize;
        for (name, range) in &ranges {
            assert_eq!(
                range.start, next,
                "{what}: {name} leaves a gap — the struct has no interior padding, so \
                 consecutive offsets must tile it"
            );
            assert!(range.end > range.start, "{what}: {name} is empty");
            next = range.end;
        }
        assert_eq!(next, total, "{what}: the table stops short of the struct");
    }
}

#[test]
fn every_h264_au_submits_four_buffers_in_libavcodecs_order() {
    for (au, sub) in our_h264_submissions().iter().enumerate() {
        assert_eq!(
            sub.descriptors
                .iter()
                .map(|d| d.buffer_type)
                .collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_INVERSE_QUANTIZATION_MATRIX,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "AU {au}"
        );
    }
}

/// Decode surface must not appear in the reference store; each named ref must resolve to an
/// occupied slot. Releasing the `refresh_frame_flags` picture before assign reused that
/// slot as `CurrPicTextureIndex`. Counts are asserted: zero refs would skip every check.
#[test]
fn no_av1_submission_names_its_decode_surface_in_the_reference_store() {
    let subs = our_av1_submissions();
    let (mut with_store, mut named_refs) = (0usize, 0usize);
    for (frame, sub) in subs.iter().enumerate() {
        let (curr, store, names) = av1_reference_store(&sub.pic_params);
        assert!(
            store.iter().all(|surface| *surface != curr),
            "frame {frame}: surface {curr} is both CurrPicTextureIndex and a \
             RefFrameMapTextureIndex entry"
        );
        if store.iter().any(|s| *s != UNUSED_INDEX) {
            with_store += 1;
        }
        for (name, slot) in names.iter().enumerate() {
            if *slot == UNUSED_INDEX {
                continue;
            }
            named_refs += 1;
            assert!(
                usize::from(*slot) < store.len(),
                "frame {frame}, reference name {name}: slot {slot} is outside the eight-entry \
                 store — `Index` is an AV1 reference SLOT, not a surface"
            );
            assert_ne!(
                store[usize::from(*slot)],
                UNUSED_INDEX,
                "frame {frame}, reference name {name}: slot {slot} holds no surface, so the \
                 driver would follow `Index` into an empty entry"
            );
        }
    }
    assert_eq!(
        with_store, 273,
        "every frame but the opening key frame carries a populated reference store"
    );
    assert!(
        named_refs > 0,
        "no frame named a reference, so every check above was skipped"
    );
}

/// Three buffers, never a matrix. AV1 selects `qm_y`/`qm_u`/`qm_v` from decoder tables;
/// `dxva2_av1_end_frame` passes `NULL, 0`. There is no `DXVA_Qmatrix_AV1`.
#[test]
fn every_av1_frame_submits_three_buffers_and_never_a_quantization_matrix() {
    for (frame, sub) in our_av1_submissions().iter().enumerate() {
        assert_eq!(
            sub.descriptors
                .iter()
                .map(|d| d.buffer_type)
                .collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "frame {frame}"
        );
        assert!(sub.qmatrix.is_none(), "frame {frame}");
    }
}

/// AV1 has no macroblocks; `dxva2_av1.c` never writes `NumMBsInBuffer`.
#[test]
fn no_av1_descriptor_ever_carries_a_macroblock_count() {
    for (frame, sub) in our_av1_submissions().iter().enumerate() {
        assert_eq!(sub.mb_count, 0, "frame {frame}");
        for desc in &sub.descriptors {
            assert_eq!(
                desc.num_mbs_in_buffer,
                0,
                "frame {frame}, {}",
                buffer_name(desc.buffer_type)
            );
        }
    }
}

/// Slice-control is `16 * tile count`; bitstream descriptor is padded size. Tiles address
/// payloads after `tile_size_minus_1`, so records do not abut. Must be strictly increasing,
/// non-overlapping, inside the unpadded window, never an OBU header. Padding is on the
/// descriptor only — unlike H.264/HEVC last-slice charging.
#[test]
fn the_av1_bitstream_descriptor_is_padded_and_the_tile_records_tile_it_without_overlapping() {
    let mut frames_with_padding = 0usize;
    for (frame, sub) in our_av1_submissions().iter().enumerate() {
        let bitstream = sub
            .descriptors
            .iter()
            .find(|d| d.buffer_type == BUFFER_BITSTREAM)
            .unwrap_or_else(|| panic!("frame {frame} submits no bitstream buffer"));
        assert_eq!(
            bitstream.data_size % 128,
            0,
            "frame {frame}: the bitstream descriptor states the PADDED size"
        );
        // `1..=128`, not `0..128`: `BITSTREAM_ALIGN - (cursor % ALIGN)` writes a full
        // block when already on the granule. This vector never lands there.
        let padding = bitstream.data_size - sub.unpadded;
        assert!(
            (1..=128).contains(&padding),
            "frame {frame}: {padding} bytes of padding"
        );
        frames_with_padding += 1;

        let slice_control = sub
            .descriptors
            .iter()
            .find(|d| d.buffer_type == BUFFER_SLICE_CONTROL)
            .unwrap_or_else(|| panic!("frame {frame} submits no slice-control buffer"));
        assert_eq!(
            slice_control.data_size as usize,
            size_of::<TileAv1>() * sub.tiles.len(),
            "frame {frame}: sixteen bytes per TILE"
        );
        assert!(!sub.tiles.is_empty(), "frame {frame}: a frame has tiles");

        let mut previous_end = 0u32;
        for (i, tile) in sub.tiles.iter().enumerate() {
            // `#[repr(packed)]`: copy fields before use.
            let (offset, size) = (tile.data_offset, tile.data_size);
            assert!(size > 0, "frame {frame}, tile {i}: an empty tile payload");
            assert!(
                offset >= previous_end,
                "frame {frame}, tile {i}: starts at {offset}, inside the previous tile which \
                 ends at {previous_end}"
            );
            assert!(
                offset + size <= sub.unpadded,
                "frame {frame}, tile {i}: runs past the bytes the packer wrote"
            );
            previous_end = offset + size;
            assert_eq!(
                tile.anchor_frame, UNUSED_INDEX,
                "frame {frame}, tile {i}: large-scale-tile anchors are libavcodec's 0xFF"
            );
        }
    }
    assert_eq!(
        frames_with_padding, VENDORED_AV1_FRAMES,
        "every frame is padded — the rule is unconditional"
    );
}

#[test]
fn every_h264_bitstream_and_slice_control_descriptor_carries_mb_width_times_mb_height() {
    // 320×240 = 20×15 macroblocks. H.264 bitstream and slice-control carry that product.
    for (au, sub) in our_h264_submissions().iter().enumerate() {
        assert_eq!(sub.mb_count, 20 * 15, "AU {au}");
        for desc in &sub.descriptors {
            let expected = match desc.buffer_type {
                BUFFER_BITSTREAM | BUFFER_SLICE_CONTROL => sub.mb_count,
                _ => 0,
            };
            assert_eq!(
                desc.num_mbs_in_buffer,
                expected,
                "AU {au}, {}",
                buffer_name(desc.buffer_type)
            );
        }
    }
}

#[test]
fn every_h264_au_submits_the_quantization_matrix_buffer() {
    // libavcodec always passes `&ctx_pic->qm`; Table 7-2 fallbacks make the lists meaningful.
    for (au, sub) in our_h264_submissions().iter().enumerate() {
        assert!(sub.qmatrix.is_some(), "AU {au}");
        let desc = sub
            .descriptors
            .iter()
            .find(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX)
            .unwrap_or_else(|| panic!("AU {au} submits no matrix buffer"));
        assert_eq!(desc.data_size, size_of::<QmatrixH264>() as u32);
        assert_eq!(desc.num_mbs_in_buffer, 0);
    }
}

#[test]
fn the_whole_vendored_hevc_vector_omits_the_quantization_matrix_buffer() {
    // `scaling_list_enabled_flag` clear: omit the buffer, do not submit an empty one.
    let ours = our_hevc_submissions();
    for (au, sub) in ours.iter().enumerate() {
        assert!(sub.qmatrix.is_none(), "AU {au}");
        assert_eq!(
            sub.descriptors
                .iter()
                .map(|d| d.buffer_type)
                .collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "AU {au}"
        );
        // libavcodec reads this flag as the submit predicate.
        let flags = u32_at(
            &sub.pic_params,
            offset_of!(PicParamsHevc, dwCodingParamToolFlags),
        );
        assert_eq!(flags & 1, 0, "AU {au}: scaling_list_enabled_flag");
    }
}

#[test]
fn no_hevc_descriptor_ever_carries_a_macroblock_count() {
    // HEVC writes 0; a CTB count would diverge the other way from H.264.
    for (au, sub) in our_hevc_submissions().iter().enumerate() {
        for desc in &sub.descriptors {
            assert_eq!(
                desc.num_mbs_in_buffer,
                0,
                "AU {au}, {}",
                buffer_name(desc.buffer_type)
            );
        }
    }
}

#[test]
fn every_descriptor_of_both_codecs_starts_at_offset_zero_and_names_a_distinct_buffer() {
    for (codec, subs) in [
        ("h264", our_h264_submissions()),
        ("hevc", our_hevc_submissions()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let mut seen = BTreeSet::new();
            for desc in &sub.descriptors {
                assert_eq!(desc.data_offset, 0, "{codec} AU {au}");
                assert!(
                    seen.insert(desc.buffer_type),
                    "{codec} AU {au}: buffer type {} submitted twice",
                    desc.buffer_type
                );
                assert!(desc.data_size > 0, "{codec} AU {au}: an empty buffer");
            }
        }
    }
}

#[test]
fn the_bitstream_descriptor_is_the_packers_padded_size_and_the_slice_records_tile_it_exactly() {
    // Records tile from 0 to `DataSize` inclusive of padding; see `pack.rs`.
    for (codec, subs) in [
        ("h264", our_h264_submissions()),
        ("hevc", our_hevc_submissions()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let bitstream = sub
                .descriptors
                .iter()
                .find(|d| d.buffer_type == BUFFER_BITSTREAM)
                .unwrap_or_else(|| panic!("{codec} AU {au} submits no bitstream buffer"));
            assert_eq!(bitstream.data_size % 128, 0, "{codec} AU {au}: padded size");
            assert!(
                bitstream.data_size >= sub.unpadded,
                "{codec} AU {au}: {} bytes of slices in a {}-byte buffer",
                sub.unpadded,
                bitstream.data_size
            );
            assert!(
                bitstream.data_size - sub.unpadded <= 128,
                "{codec} AU {au}: {} bytes of padding",
                bitstream.data_size - sub.unpadded
            );
            assert!(!sub.records.is_empty(), "{codec} AU {au}: no slices");
            let mut cursor = 0u32;
            for (i, record) in sub.records.iter().enumerate() {
                assert_eq!(
                    record.location, cursor,
                    "{codec} AU {au}: slice {i} location"
                );
                assert!(
                    record.bytes > 3,
                    "{codec} AU {au}: slice {i} is start code only"
                );
                cursor = record.location + record.bytes;
                assert!(
                    cursor <= bitstream.data_size,
                    "{codec} AU {au}: slice {i} runs past DataSize"
                );
            }
            assert_eq!(
                cursor, bitstream.data_size,
                "{codec} AU {au}: the records must tile the whole buffer, padding included"
            );
        }
    }
}

#[test]
fn the_slice_control_descriptor_is_one_ten_byte_short_format_record_per_slice_for_both_codecs() {
    // Short record is 10 bytes (`#[repr(C)] {u32,u32,u16}` is 12). ConfigBitstreamRaw is
    // 2 for H.264 and 1 for HEVC; mismatch makes the driver read a different struct.
    assert_eq!(pf_dxvadec::short_slice_config(Codec::H264), 2);
    assert_eq!(pf_dxvadec::short_slice_config(Codec::H265), 1);
    assert_eq!(size_of::<SliceH264Short>(), 10);
    assert_eq!(size_of::<SliceHevcShort>(), 10);
    for (codec, subs, slices_per_picture, capture_data_size) in [
        ("h264", our_h264_submissions(), 2usize, 20u32),
        ("hevc", our_hevc_submissions(), 1, 10),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let control = sub
                .descriptors
                .iter()
                .find(|d| d.buffer_type == BUFFER_SLICE_CONTROL)
                .unwrap_or_else(|| panic!("{codec} AU {au} submits no slice-control buffer"));
            assert_eq!(
                control.data_size as usize,
                10 * sub.records.len(),
                "{codec} AU {au}"
            );
            // Slice counts from the capture (2 / 1), not from `DataSize / 10`.
            assert_eq!(
                sub.records.len(),
                slices_per_picture,
                "{codec} AU {au}: the vendored vector is {slices_per_picture} slice(s) per picture"
            );
            assert_eq!(
                control.data_size, capture_data_size,
                "{codec} AU {au}: libavcodec's captured slice-control DataSize"
            );
        }
    }
}

#[test]
fn the_tail_padding_is_charged_to_the_last_slice_record_and_to_no_other() {
    // Padding is charged only to the last `SliceBytesInBuffer`. H.264 has two slices per
    // picture; a single-slice vector cannot distinguish last-vs-only.
    for (codec, subs) in [
        ("h264", our_h264_submissions()),
        ("hevc", our_hevc_submissions()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            let bitstream = sub
                .descriptors
                .iter()
                .find(|d| d.buffer_type == BUFFER_BITSTREAM)
                .unwrap_or_else(|| panic!("{codec} AU {au} submits no bitstream buffer"));
            let padding = bitstream.data_size - sub.unpadded;
            assert!(
                (1..=128).contains(&padding),
                "{codec} AU {au}: {padding} bytes of padding"
            );
            let (last, earlier) = sub
                .records
                .split_last()
                .unwrap_or_else(|| panic!("{codec} AU {au}: no slices"));
            for (i, record) in earlier.iter().enumerate() {
                assert_eq!(
                    record.location + record.bytes,
                    sub.records[i + 1].location,
                    "{codec} AU {au}: record {i} does not end where record {} begins",
                    i + 1
                );
            }
            assert_eq!(
                last.location + last.bytes,
                bitstream.data_size,
                "{codec} AU {au}: the last record must reach DataSize"
            );
            assert_eq!(
                sub.records.iter().map(|r| r.bytes).sum::<u32>() - padding,
                sub.unpadded,
                "{codec} AU {au}: the padding is charged more than once, or not at all"
            );
            assert!(
                last.bytes > padding,
                "{codec} AU {au}: the last record is padding only"
            );
        }
    }
}

#[test]
fn the_picture_parameter_buffer_is_the_whole_hand_declared_struct_for_both_codecs() {
    for (codec, subs, size) in [
        ("h264", our_h264_submissions(), size_of::<PicParamsH264>()),
        ("hevc", our_hevc_submissions(), size_of::<PicParamsHevc>()),
        ("av1", our_av1_submissions(), size_of::<PicParamsAv1>()),
    ] {
        for (au, sub) in subs.iter().enumerate() {
            assert_eq!(sub.pic_params.len(), size, "{codec} AU {au}");
            assert_eq!(
                sub.descriptors[0].buffer_type, BUFFER_PICTURE_PARAMETERS,
                "{codec} AU {au}"
            );
            assert_eq!(
                sub.descriptors[0].data_size as usize, size,
                "{codec} AU {au}"
            );
        }
    }
}

/// First HEVC AU with only scaling-list fields rewritten (7.4.5 cases). Uncoded SPS stays
/// `ScalingLists::default()` (zeros); PPS then holds Table 7-5/7-6 defaults.
fn hevc_case(enabled: bool, sps_coded: Option<u8>, pps_coded: Option<u8>) -> OurSubmission {
    use std::rc::Rc;

    let aus = split_h265_aus(TEST_25FPS_H265);
    let mut planner = H265Planner::new();
    let mut plan = planner.plan_au(aus[0]).expect("plan");

    let mut sps = (*plan.sps).clone();
    sps.scaling_list_enabled_flag = enabled;
    sps.scaling_list_data_present_flag = sps_coded.is_some();
    if let Some(fill) = sps_coded {
        sps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
        sps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
        sps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
        sps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
        sps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
        sps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
    }
    let mut pps = (*plan.pps).clone();
    pps.scaling_list_data_present_flag = pps_coded.is_some();
    if let Some(fill) = pps_coded {
        pps.scaling_list.scaling_list_4x4 = [[fill; 16]; 6];
        pps.scaling_list.scaling_list_8x8 = [[fill; 64]; 6];
        pps.scaling_list.scaling_list_16x16 = [[fill; 64]; 6];
        pps.scaling_list.scaling_list_32x32 = [[fill; 64]; 6];
        pps.scaling_list.scaling_list_dc_coef_minus8_16x16 = [i16::from(fill); 6];
        pps.scaling_list.scaling_list_dc_coef_minus8_32x32 = [i16::from(fill); 6];
    }
    plan.sps = Rc::new(sps);
    plan.pps = Rc::new(pps);

    let mut slots = SlotMap::new(plan.picture.max_dpb_frames);
    let dxva = pf_dxvadec::plan_to_dxva_h265(&plan, &mut slots, 1).expect("convert");
    let mut mapping = vec![0u8; MAPPING_BYTES];
    let packed = pf_dxvadec::pack(aus[0], &dxva.slice_ranges, &mut mapping).expect("pack");
    let unpadded = pf_dxvadec::packed_size(aus[0], &dxva.slice_ranges).expect("size") as u32;
    OurSubmission {
        pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
        qmatrix: dxva
            .qmatrix
            .as_ref()
            .map(|qm| pf_dxvadec::as_bytes(qm).to_vec()),
        descriptors: pf_dxvadec::descriptors_h265(&dxva, &packed),
        records: packed.records,
        tiles: Vec::new(),
        unpadded,
        mb_count: 0,
    }
}

#[test]
fn an_hevc_sequence_that_disables_scaling_lists_submits_no_matrix_however_much_is_coded() {
    // Flag decides, not whether lists are coded.
    let sub = hevc_case(false, Some(7), Some(9));
    assert!(sub.qmatrix.is_none());
    assert!(!sub
        .descriptors
        .iter()
        .any(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX));
}

#[test]
fn an_hevc_sequence_that_enables_scaling_lists_and_codes_them_submits_the_coded_lists() {
    let sub = hevc_case(true, Some(7), Some(9));
    let qm = sub
        .qmatrix
        .as_ref()
        .expect("an enabled sequence submits the matrix");
    assert_eq!(qm.len(), size_of::<QmatrixHevc>());
    let desc = sub
        .descriptors
        .iter()
        .find(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX)
        .expect("the matrix buffer is in the set");
    assert_eq!(desc.data_size as usize, size_of::<QmatrixHevc>());
    assert_eq!(desc.num_mbs_in_buffer, 0);
    // PPS wins over SPS (7.4.5).
    assert!(
        qm.iter().all(|&b| b == 9 || b == 17),
        "the PPS's fill of 9 (DC 9 + 8)"
    );
    assert_eq!(
        sub.descriptors
            .iter()
            .map(|d| d.buffer_type)
            .collect::<Vec<_>>(),
        vec![
            BUFFER_PICTURE_PARAMETERS,
            BUFFER_INVERSE_QUANTIZATION_MATRIX,
            BUFFER_BITSTREAM,
            BUFFER_SLICE_CONTROL,
        ]
    );
}

#[test]
fn an_hevc_sequence_that_enables_scaling_lists_but_codes_none_submits_the_defaults_not_zeros() {
    // Enabled with nothing coded: 7.4.5 defaults, not the uncoded-SPS zeros. Submitting
    // zeros while bit 0 claims authority dequantizes every residual to nothing.
    // `pic_h265.rs` checks table contents; this checks presence and not-all-zero.
    let sub = hevc_case(true, None, None);
    let qm = sub
        .qmatrix
        .as_ref()
        .expect("an enabled sequence submits the matrix even with nothing coded");
    let desc = sub
        .descriptors
        .iter()
        .find(|d| d.buffer_type == BUFFER_INVERSE_QUANTIZATION_MATRIX)
        .expect("the matrix buffer is in the set");
    assert_eq!(desc.data_size as usize, size_of::<QmatrixHevc>());
    assert!(
        !qm.iter().all(|&b| b == 0),
        "an all-zero matrix dequantizes every residual to nothing"
    );
    // Table 7-5 4x4 lists are 16; inferred DC is 8+8.
    let lists0 = offset_of!(QmatrixHevc, ucScalingLists0);
    assert!(qm[lists0..lists0 + 96].iter().all(|&b| b == 16));
    let dc2 = offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID2);
    assert!(qm[dc2..dc2 + 6].iter().all(|&b| b == 16));
    let dc3 = offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID3);
    assert!(qm[dc3..dc3 + 2].iter().all(|&b| b == 16));
    let lists1 = offset_of!(QmatrixHevc, ucScalingLists1);
    let lists3_end = offset_of!(QmatrixHevc, ucScalingListDCCoefSizeID2);
    assert!(qm[lists1..lists3_end].iter().all(|&b| b != 0));
}

#[test]
fn the_dump_and_the_parser_agree_and_the_comparison_finds_nothing_against_ourselves() {
    // Writer and parser share no code; a format drift fails here.
    let ours = our_h264_submissions();
    let capture = parse_capture(&dump("h264", &ours), "h264");
    preflight(
        &capture,
        ours.len(),
        "h264",
        Some(offset_of!(PicParamsH264, Reserved16Bits)),
    );
    assert_eq!(capture.pic_params.len(), VENDORED_AUS);
    assert_eq!(capture.qmatrix.len(), VENDORED_AUS);
    assert_eq!(capture.descriptors.len(), VENDORED_AUS);
    for findings in [
        compare_h264_picparams(&ours, &capture),
        compare_descriptors(&ours, &capture),
        compare_qmatrix(
            &ours,
            &capture,
            H264_QMATRIX_FIELDS,
            size_of::<QmatrixH264>(),
        ),
    ] {
        assert!(
            findings.is_empty(),
            "comparing our own bytes against themselves must find nothing, got {:?}",
            findings.fields()
        );
        // An allowance that fires on identical bytes would hide a real difference.
        assert!(
            findings.documented_fields().is_empty(),
            "identical bytes documented a divergence: {:?}",
            findings.documented_fields()
        );
    }

    let ours = our_hevc_submissions();
    let capture = parse_capture(&dump("hevc", &ours), "hevc");
    // ClearVideo workaround is H.264-only.
    preflight(&capture, ours.len(), "hevc", None);
    for findings in [
        compare_hevc_picparams(&ours, &capture),
        compare_descriptors(&ours, &capture),
        compare_qmatrix(
            &ours,
            &capture,
            HEVC_QMATRIX_FIELDS,
            size_of::<QmatrixHevc>(),
        ),
    ] {
        assert!(
            findings.is_empty(),
            "comparing our own HEVC bytes against themselves must find nothing, got {:?}",
            findings.fields()
        );
        assert!(
            findings.documented_fields().is_empty(),
            "identical bytes documented a divergence: {:?}",
            findings.documented_fields()
        );
    }
    // `absent` must survive parse; dropping it is the HEVC omit-matrix defect.
    assert!(capture.qmatrix.values().all(Option::is_none));

    // Self-compare is the only AV1 capture stand-in: 912-byte table coverage and store offsets.
    let ours = our_av1_submissions();
    let capture = parse_capture(&dump("av1", &ours), "av1");
    preflight(&capture, ours.len(), "av1", None);
    assert_eq!(capture.pic_params.len(), VENDORED_AV1_FRAMES);
    for findings in [
        compare_av1_picparams(&ours, &capture),
        compare_descriptors(&ours, &capture),
    ] {
        assert!(
            findings.is_empty(),
            "comparing our own AV1 bytes against themselves must find nothing, got {:?}",
            findings.fields()
        );
        assert!(
            findings.documented_fields().is_empty(),
            "identical bytes documented a divergence: {:?}",
            findings.documented_fields()
        );
    }
    // AV1 `absent` is codec-wide, not a per-sequence HEVC decision.
    assert!(capture.qmatrix.values().all(Option::is_none));
}

fn descriptor_only_submission(unpadded: u32, padded: u32, slices: usize) -> OurSubmission {
    let each = unpadded / slices as u32;
    let mut records: Vec<SliceRecord> = (0..slices)
        .map(|i| SliceRecord {
            location: i as u32 * each,
            bytes: each,
        })
        .collect();
    // Last record carries remainder and padding, as the packer does.
    let last = records.last_mut().expect("at least one slice");
    last.bytes = padded - last.location;
    OurSubmission {
        pic_params: vec![0u8; size_of::<PicParamsH264>()],
        qmatrix: None,
        tiles: Vec::new(),
        descriptors: vec![
            BufferDescriptor {
                buffer_type: BUFFER_BITSTREAM,
                data_offset: 0,
                data_size: padded,
                num_mbs_in_buffer: 300,
            },
            BufferDescriptor {
                buffer_type: BUFFER_SLICE_CONTROL,
                data_offset: 0,
                data_size: 10 * slices as u32,
                num_mbs_in_buffer: 300,
            },
        ],
        records,
        unpadded,
        mb_count: 300,
    }
}

/// Rewrite each PFPP so an absorbed divergence is reproducible, or the allowance is untested.
fn map_picparams(text: &str, codec: &str, mut f: impl FnMut(usize, &mut Vec<u8>)) -> String {
    let prefix = format!("PFPP {codec} ");
    let mut out = String::new();
    for line in text.lines() {
        match line.strip_prefix(&prefix) {
            Some(rest) => {
                let (au, hex) = rest.split_once(' ').expect("our own dump is well formed");
                let au: usize = au.parse().expect("a decimal AU index");
                let mut bytes = from_hex(hex).expect("our own dump is hex");
                f(au, &mut bytes);
                let _ = writeln!(out, "{prefix}{au} {}", to_hex(&bytes));
            }
            None => {
                let _ = writeln!(out, "{line}");
            }
        }
    }
    out
}

/// Shift in-use POCs only; unused entries stay 0. Synthesises libavcodec's `prev_poc_msb`.
fn shift_h264_capture_poc(pp: &mut [u8], delta: i32) {
    let curr = offset_of!(PicParamsH264, CurrFieldOrderCnt);
    let list = offset_of!(PicParamsH264, RefFrameList);
    let focl = offset_of!(PicParamsH264, FieldOrderCntList);
    for field in [curr, curr + 4] {
        let shifted = i32_at(pp, field).wrapping_add(delta);
        pp[field..field + 4].copy_from_slice(&shifted.to_le_bytes());
    }
    for i in 0..16 {
        if pp[list + i] == UNUSED_ENTRY {
            continue;
        }
        for field in [focl + 8 * i, focl + 8 * i + 4] {
            let shifted = i32_at(pp, field).wrapping_add(delta);
            pp[field..field + 4].copy_from_slice(&shifted.to_le_bytes());
        }
    }
}

fn descriptor_capture(descs: &[(u32, u32, u32)]) -> String {
    let mut text = String::new();
    for (buffer_type, data_size, mbs) in descs {
        let _ = writeln!(text, "PFBD h264 0 {buffer_type} {data_size} {mbs} 0");
    }
    text
}

#[test]
fn libavcodecs_constant_poc_base_is_documented_and_anything_else_about_a_poc_is_a_finding() {
    // Do not exclude POC fields: an excluded field cannot report a wrong POC.
    let ours = our_h264_submissions();
    let base = dump("h264", &ours);

    // Constant 65536 base: documented, not a finding.
    let shifted = map_picparams(&base, "h264", |_, pp| shift_h264_capture_poc(pp, 65536));
    let findings = compare_h264_picparams(&ours, &parse_capture(&shifted, "h264"));
    assert!(
        findings.is_empty(),
        "a constant POC base is not a finding, got {:?}",
        findings.fields()
    );
    assert_eq!(
        findings.documented_fields(),
        vec!["FieldOrderCnt[POC base]"]
    );

    // Any other constant is a finding.
    let odd = map_picparams(&base, "h264", |_, pp| shift_h264_capture_poc(pp, 7));
    let findings = compare_h264_picparams(&ours, &parse_capture(&odd, "h264"));
    assert_eq!(findings.fields(), vec!["CurrFieldOrderCnt[0][POC base]"]);
    assert!(findings.documented_fields().is_empty());

    // A base that drifts on one AU is a real POC finding.
    let drifting = map_picparams(&base, "h264", |au, pp| {
        shift_h264_capture_poc(pp, if au == 10 { 65536 - 4 } else { 65536 })
    });
    let findings = compare_h264_picparams(&ours, &parse_capture(&drifting, "h264"));
    assert_eq!(
        findings.fields(),
        vec![
            "CurrFieldOrderCnt[0][POC]",
            "CurrFieldOrderCnt[1][POC]",
            "RefFrameList[set]",
        ]
    );
    assert_eq!(findings.by_field["CurrFieldOrderCnt[0][POC]"].first_au, 10);
}

#[test]
fn the_hevc_tiles_flag_allowance_is_exactly_bit_ten_with_tiles_disabled_and_nothing_else() {
    // Per-difference: the same word has eighteen other flags; bit 10 with tiles on is live.
    let ours = our_hevc_submissions();
    let base = dump("hevc", &ours);
    let at = offset_of!(PicParamsHevc, dwCodingSettingPicturePropertyFlags);
    let rewrite = |text: &str, mask_off: u32, mask_on: u32| {
        map_picparams(text, "hevc", |_, pp| {
            let flags = (u32_at(pp, at) & !mask_off) | mask_on;
            pp[at..at + 4].copy_from_slice(&flags.to_le_bytes());
        })
    };

    let capture = parse_capture(&rewrite(&base, 1 << 10, 0), "hevc");
    let findings = compare_hevc_picparams(&ours, &capture);
    assert!(
        findings.is_empty(),
        "the documented tiles-flag divergence is not a finding, got {:?}",
        findings.fields()
    );
    assert_eq!(
        findings.documented_fields(),
        vec!["dwCodingSettingPicturePropertyFlags"]
    );

    // Bit 11 (`pps_loop_filter_across_slices_enabled_flag`) is not inert.
    let capture = parse_capture(&rewrite(&base, 1 << 11, 0), "hevc");
    let findings = compare_hevc_picparams(&ours, &capture);
    assert_eq!(
        findings.fields(),
        vec!["dwCodingSettingPicturePropertyFlags"]
    );
    assert!(findings.documented_fields().is_empty());

    // Bit 10 with tiles enabled governs a real boundary.
    let ours_with_tiles: Vec<OurSubmission> = ours
        .iter()
        .map(|sub| {
            let mut pp = sub.pic_params.clone();
            let flags = u32_at(&pp, at) | (1 << 7) | (1 << 10);
            pp[at..at + 4].copy_from_slice(&flags.to_le_bytes());
            OurSubmission {
                pic_params: pp,
                qmatrix: sub.qmatrix.clone(),
                descriptors: sub.descriptors.clone(),
                records: sub.records.clone(),
                tiles: sub.tiles.clone(),
                unpadded: sub.unpadded,
                mb_count: sub.mb_count,
            }
        })
        .collect();
    let capture = parse_capture(
        &rewrite(&dump("hevc", &ours_with_tiles), 1 << 10, 1 << 7),
        "hevc",
    );
    let findings = compare_hevc_picparams(&ours_with_tiles, &capture);
    assert_eq!(
        findings.fields(),
        vec!["dwCodingSettingPicturePropertyFlags"]
    );
    assert!(findings.documented_fields().is_empty());
}

#[test]
fn a_bitstream_size_difference_is_classified_by_the_unpadded_window_it_implies() {
    // Ours 1026 unpadded / 1152 padded, two slices. Captured 1024 ⇒ unpadded 896..=1023,
    // within 4 bytes/slice of 1026. Captured 512 cannot land in that window.
    let ours = vec![descriptor_only_submission(1026, 1152, 2)];

    let legitimate = descriptor_capture(&[
        (BUFFER_BITSTREAM, 1024, 300),
        (BUFFER_SLICE_CONTROL, 20, 300),
    ]);
    let findings = compare_descriptors(&ours, &parse_capture(&legitimate, "h264"));
    assert_eq!(findings.fields(), vec!["BITSTREAM.DataSize[delimitation]"]);

    let defect = descriptor_capture(&[
        (BUFFER_BITSTREAM, 512, 300),
        (BUFFER_SLICE_CONTROL, 20, 300),
    ]);
    let findings = compare_descriptors(&ours, &parse_capture(&defect, "h264"));
    assert_eq!(findings.fields(), vec!["BITSTREAM.DataSize"]);

    // Slice-count mismatch voids the size compare (three 10-byte records vs two).
    let split = descriptor_capture(&[
        (BUFFER_BITSTREAM, 1024, 300),
        (BUFFER_SLICE_CONTROL, 30, 300),
    ]);
    let findings = compare_descriptors(&ours, &parse_capture(&split, "h264"));
    assert_eq!(
        findings.fields(),
        vec!["BITSTREAM.DataSize[slice count]", "SLICE_CONTROL.DataSize"]
    );

    let zeroed = descriptor_capture(&[(BUFFER_BITSTREAM, 1152, 0), (BUFFER_SLICE_CONTROL, 20, 0)]);
    let findings = compare_descriptors(&ours, &parse_capture(&zeroed, "h264"));
    assert_eq!(
        findings.fields(),
        vec!["BITSTREAM.NumMBsInBuffer", "SLICE_CONTROL.NumMBsInBuffer"]
    );
}

#[test]
fn a_changed_scalar_field_is_reported_by_its_name() {
    let ours = our_h264_submissions();
    let mut text = dump("h264", &ours);
    let offset = offset_of!(PicParamsH264, pic_init_qp_minus26);
    text = mutate_capture_byte(&text, 7, offset, 0x5A);
    let capture = parse_capture(&text, "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert_eq!(findings.fields(), vec!["pic_init_qp_minus26"]);
    assert_eq!(findings.by_field["pic_init_qp_minus26"].first_au, 7);

    // Truncated table: an untabled byte must still report by raw offset.
    let mut findings = Findings::default();
    let mut theirs = ours[0].pic_params.clone();
    theirs[offset] = 0x5A;
    compare_scalars(
        0,
        &ours[0].pic_params,
        &theirs,
        &field_ranges(&H264_FIELDS[..2], 4),
        &[],
        no_allowance,
        &mut findings,
    );
    let expected = format!("<unclassified byte {offset:#06x}>");
    assert_eq!(findings.fields(), vec![expected.as_str()]);
}

#[test]
fn a_reordered_reference_list_is_no_finding_but_a_changed_one_is() {
    let ours = our_h264_submissions();
    let (au, entries) = ours
        .iter()
        .enumerate()
        .map(|(au, sub)| (au, h264_ref_entries(&sub.pic_params)))
        .find(|(_, entries)| entries.len() >= 2)
        .expect("the vector must reach two references");

    let reordered = reverse_h264_reference_list(&ours[au].pic_params);
    assert_ne!(
        reordered, ours[au].pic_params,
        "the reversal must change bytes"
    );
    let capture = parse_capture(
        &with_picparams(&dump("h264", &ours), au, &reordered),
        "h264",
    );
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings.is_empty(),
        "a reordered reference list is not a finding, got {:?}",
        findings.fields()
    );

    let mut dropped = ours[au].pic_params.clone();
    let list = offset_of!(PicParamsH264, RefFrameList);
    let last = entries.len() - 1;
    dropped[list + last] = UNUSED_ENTRY;
    let used = offset_of!(PicParamsH264, UsedForReferenceFlags);
    let cleared = u32_at(&dropped, used) & !(0b11 << (2 * last));
    dropped[used..used + 4].copy_from_slice(&cleared.to_le_bytes());
    let capture = parse_capture(&with_picparams(&dump("h264", &ours), au, &dropped), "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings.fields().contains(&"RefFrameList[set]"),
        "a dropped reference must be reported, got {:?}",
        findings.fields()
    );
}

#[test]
fn a_wholly_renumbered_surface_set_is_no_finding_and_an_inconsistent_one_is() {
    // Whole-stream renumber is a pool bijection; one AU alone breaks live mappings.
    let ours = our_h264_submissions();
    let base = dump("h264", &ours);

    let renumbered: Vec<Vec<u8>> = ours
        .iter()
        .map(|sub| renumber_h264_surfaces(&sub.pic_params, |slot| slot + 8))
        .collect();
    let mut text = base.clone();
    for (au, pp) in renumbered.iter().enumerate() {
        text = with_picparams(&text, au, pp);
    }
    let capture = parse_capture(&text, "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings.is_empty(),
        "a consistently renumbered surface set is a bijection, not a finding, got {:?}",
        findings.fields()
    );

    let au = ours
        .iter()
        .position(|sub| h264_ref_entries(&sub.pic_params).len() >= 2)
        .expect("two references");
    let mut text = base;
    for (i, pp) in renumbered.iter().enumerate() {
        if i != au {
            text = with_picparams(&text, i, pp);
        }
    }
    let capture = parse_capture(&text, "h264");
    let findings = compare_h264_picparams(&ours, &capture);
    assert!(
        findings
            .fields()
            .iter()
            .any(|f| f.contains("surface mapping")),
        "an inconsistent surface numbering must be reported, got {:?}",
        findings.fields()
    );
}

#[test]
fn an_omitted_hevc_matrix_buffer_is_reported_as_a_presence_difference() {
    let ours = our_hevc_submissions();
    let mut subs = ours;
    subs[3].qmatrix = Some(vec![0u8; size_of::<QmatrixHevc>()]);
    subs[3].descriptors = vec![
        BufferDescriptor {
            buffer_type: BUFFER_PICTURE_PARAMETERS,
            data_offset: 0,
            data_size: size_of::<PicParamsHevc>() as u32,
            num_mbs_in_buffer: 0,
        },
        BufferDescriptor {
            buffer_type: BUFFER_INVERSE_QUANTIZATION_MATRIX,
            data_offset: 0,
            data_size: size_of::<QmatrixHevc>() as u32,
            num_mbs_in_buffer: 0,
        },
        subs[3].descriptors[1],
        subs[3].descriptors[2],
    ];
    let honest = our_hevc_submissions();
    let capture = parse_capture(&dump("hevc", &honest), "hevc");

    let findings = compare_qmatrix(
        &subs,
        &capture,
        HEVC_QMATRIX_FIELDS,
        size_of::<QmatrixHevc>(),
    );
    assert_eq!(findings.fields(), vec!["<submitted>"]);
    assert_eq!(findings.by_field["<submitted>"].first_au, 3);
    let findings = compare_descriptors(&subs, &capture);
    assert_eq!(findings.fields(), vec!["<buffer set>"]);
}

#[test]
fn a_missing_bitstream_descriptor_is_reported_as_a_missed_patch_site_not_a_defect() {
    let ours = our_h264_submissions();
    let text: String = dump("h264", &ours)
        .lines()
        .filter(|line| {
            // BITSTREAM type is token 3; matching ` 6 ` anywhere would drop AU 6's whole set.
            let fields: Vec<&str> = line.split_whitespace().collect();
            !(fields.first() == Some(&"PFBD") && fields.get(3) == Some(&"6"))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let capture = parse_capture(&text, "h264");
    let findings = compare_descriptors(&ours, &capture);
    assert_eq!(findings.fields(), vec!["<buffer set>"]);
    assert!(
        findings.by_field["<buffer set>"]
            .detail
            .contains("commit_bitstream_and_slice_buffer"),
        "the report must name the patch site, got {:?}",
        findings.by_field["<buffer set>"].detail
    );
}

#[test]
fn an_unreadable_or_short_capture_is_refused_rather_than_partly_compared() {
    let ours = our_h264_submissions();
    let good = dump("h264", &ours);

    let broken = good.replace("PFPP h264 5 ", "PFPP h264 5 zz");
    let capture = parse_capture(&broken, "h264");
    assert_eq!(capture.unreadable.len(), 1);

    let short: String = good
        .lines()
        .filter(|line| !line.starts_with("PFPP h264 24 "))
        .map(|line| format!("{line}\n"))
        .collect();
    let capture = parse_capture(&short, "h264");
    assert_eq!(capture.pic_params.len(), VENDORED_AUS - 1);
    assert!(!capture.pic_params.contains_key(&24));

    assert!(parse_capture(&good, "hevc").pic_params.is_empty());

    let prefixed: String = good
        .lines()
        .map(|line| format!("[h264 @ 0x7ff1c380a200] {line}\n"))
        .collect();
    let capture = parse_capture(&prefixed, "h264");
    assert!(capture.unreadable.is_empty());
    assert_eq!(capture.pic_params.len(), VENDORED_AUS);
    assert_eq!(capture.descriptors.len(), VENDORED_AUS);
}

fn with_picparams(text: &str, au: usize, pp: &[u8]) -> String {
    let prefix = format!("PFPP h264 {au} ");
    let hevc = format!("PFPP hevc {au} ");
    text.lines()
        .map(|line| {
            if line.starts_with(&prefix) {
                format!("{prefix}{}\n", to_hex(pp))
            } else if line.starts_with(&hevc) {
                format!("{hevc}{}\n", to_hex(pp))
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

fn mutate_capture_byte(text: &str, au: usize, offset: usize, value: u8) -> String {
    let prefix = format!("PFPP h264 {au} ");
    let mut out = String::new();
    for line in text.lines() {
        if let Some(hex) = line.strip_prefix(&prefix) {
            let mut bytes = from_hex(hex).expect("our own dump is hex");
            bytes[offset] = value;
            let _ = writeln!(out, "{prefix}{}", to_hex(&bytes));
        } else {
            let _ = writeln!(out, "{line}");
        }
    }
    out
}

/// Reverse in-use entries with keys and flags, as libavcodec's `short_ref` then `long_ref` walk.
fn reverse_h264_reference_list(pp: &[u8]) -> Vec<u8> {
    let mut out = pp.to_vec();
    let list = offset_of!(PicParamsH264, RefFrameList);
    let poc = offset_of!(PicParamsH264, FieldOrderCntList);
    let nums = offset_of!(PicParamsH264, FrameNumList);
    let used_at = offset_of!(PicParamsH264, UsedForReferenceFlags);
    let missing_at = offset_of!(PicParamsH264, NonExistingFrameFlags);
    let entries = h264_ref_entries(pp);
    let slots: Vec<u8> = (0..entries.len()).map(|i| pp[list + i]).collect();
    let used = u32_at(pp, used_at);
    let missing = u16_at(pp, missing_at);
    let mut new_used = used & !((1u32 << (2 * entries.len())) - 1);
    let mut new_missing = missing & !((1u16 << entries.len()) - 1);
    for (i, source) in (0..entries.len()).rev().enumerate() {
        out[list + i] = slots[source];
        out[nums + 2 * i..nums + 2 * i + 2]
            .copy_from_slice(&pp[nums + 2 * source..nums + 2 * source + 2]);
        out[poc + 8 * i..poc + 8 * i + 8]
            .copy_from_slice(&pp[poc + 8 * source..poc + 8 * source + 8]);
        new_used |= (used >> (2 * source) & 0b11) << (2 * i);
        new_missing |= (missing >> source & 1) << i;
    }
    out[used_at..used_at + 4].copy_from_slice(&new_used.to_le_bytes());
    out[missing_at..missing_at + 2].copy_from_slice(&new_missing.to_le_bytes());
    out
}

fn renumber_h264_surfaces(pp: &[u8], f: impl Fn(u8) -> u8) -> Vec<u8> {
    let mut out = pp.to_vec();
    let curr = offset_of!(PicParamsH264, CurrPic);
    out[curr] = (out[curr] & 0x80) | (f(out[curr] & 0x7F) & 0x7F);
    let list = offset_of!(PicParamsH264, RefFrameList);
    for i in 0..16 {
        if out[list + i] != UNUSED_ENTRY {
            out[list + i] = (out[list + i] & 0x80) | (f(out[list + i] & 0x7F) & 0x7F);
        }
    }
    out
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_AV1=<file> (see the module docs)"]
fn our_av1_picture_parameters_match_libavcodecs() {
    let capture = capture_from_env("PF_LIBAV_CAPTURE_AV1", "av1")
        .expect("PF_LIBAV_CAPTURE_AV1=<file> names a capture (see the module docs)");
    let ours = our_av1_submissions();
    // ClearVideo workarounds are H.264-only; AV1 has no `Reserved16Bits`.
    preflight(&capture, ours.len(), "av1", None);
    compare_av1_picparams(&ours, &capture).verdict("AV1 picture parameters", ours.len());
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_H264=<file> (see the module docs)"]
fn our_h264_picture_parameters_match_libavcodecs() {
    let capture = capture_from_env("PF_LIBAV_CAPTURE_H264", "h264")
        .expect("PF_LIBAV_CAPTURE_H264=<file> names a capture (see the module docs)");
    let ours = our_h264_submissions();
    preflight(
        &capture,
        ours.len(),
        "h264",
        Some(offset_of!(PicParamsH264, Reserved16Bits)),
    );
    compare_h264_picparams(&ours, &capture).verdict("H.264 picture parameters", ours.len());
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_HEVC=<file> (see the module docs)"]
fn our_hevc_picture_parameters_match_libavcodecs() {
    let capture = capture_from_env("PF_LIBAV_CAPTURE_HEVC", "hevc")
        .expect("PF_LIBAV_CAPTURE_HEVC=<file> names a capture (see the module docs)");
    let ours = our_hevc_submissions();
    preflight(&capture, ours.len(), "hevc", None);
    compare_hevc_picparams(&ours, &capture).verdict("HEVC picture parameters", ours.len());
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_H264/PF_LIBAV_CAPTURE_HEVC (module docs)"]
fn our_buffer_descriptors_match_libavcodecs() {
    let h264 = capture_from_env("PF_LIBAV_CAPTURE_H264", "h264");
    let hevc = capture_from_env("PF_LIBAV_CAPTURE_HEVC", "hevc");
    assert!(
        h264.is_some() || hevc.is_some(),
        "PF_LIBAV_CAPTURE_H264=<file> and/or PF_LIBAV_CAPTURE_HEVC=<file> name a capture"
    );
    if let Some(capture) = h264 {
        let ours = our_h264_submissions();
        preflight(
            &capture,
            ours.len(),
            "h264",
            Some(offset_of!(PicParamsH264, Reserved16Bits)),
        );
        compare_descriptors(&ours, &capture).verdict("H.264 buffer descriptors", ours.len());
    }
    if let Some(capture) = hevc {
        let ours = our_hevc_submissions();
        preflight(&capture, ours.len(), "hevc", None);
        compare_descriptors(&ours, &capture).verdict("HEVC buffer descriptors", ours.len());
    }
}

#[test]
#[ignore = "needs a libavcodec capture: PF_LIBAV_CAPTURE_H264/PF_LIBAV_CAPTURE_HEVC (module docs)"]
fn our_quantization_matrices_match_libavcodecs() {
    let h264 = capture_from_env("PF_LIBAV_CAPTURE_H264", "h264");
    let hevc = capture_from_env("PF_LIBAV_CAPTURE_HEVC", "hevc");
    assert!(
        h264.is_some() || hevc.is_some(),
        "PF_LIBAV_CAPTURE_H264=<file> and/or PF_LIBAV_CAPTURE_HEVC=<file> name a capture"
    );
    if let Some(capture) = h264 {
        let ours = our_h264_submissions();
        preflight(
            &capture,
            ours.len(),
            "h264",
            Some(offset_of!(PicParamsH264, Reserved16Bits)),
        );
        compare_qmatrix(
            &ours,
            &capture,
            H264_QMATRIX_FIELDS,
            size_of::<QmatrixH264>(),
        )
        .verdict("H.264 quantization matrices", ours.len());
    }
    if let Some(capture) = hevc {
        let ours = our_hevc_submissions();
        preflight(&capture, ours.len(), "hevc", None);
        compare_qmatrix(
            &ours,
            &capture,
            HEVC_QMATRIX_FIELDS,
            size_of::<QmatrixHevc>(),
        )
        .verdict("HEVC quantization matrices", ours.len());
    }
}
