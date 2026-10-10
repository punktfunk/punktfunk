//! One AV1 [`AuPlan`] into Vulkan decode structures.
//!
//! AV1 puts the frame header in picture info, not session parameters.
//! `StdVideoDecodeAV1PictureInfo` therefore holds eight pointers to per-frame
//! blocks (tile, quantisation, segmentation, loop filter, CDEF, loop
//! restoration, global motion, film grain). This module boxes each block
//! beside the Std struct that points at it — same ownership as
//! [`crate::OwnedStdSps`].
//!
//! `VkVideoDecodeAV1PictureInfoKHR::referenceNameSlotIndices` is indexed by
//! AV1 reference name (`LAST_FRAME`..`ALTREF_FRAME`) and stores the DPB slot
//! that name resolves to, or `-1`. That is not a position in
//! `pReferenceSlots`. The backend lays `pReferenceSlots` out in
//! [`DecodePlanVkAv1::refs`] order independently.
//!
//! `AuPlan::refs` is also indexed by name; a lost reference is a hole, not a
//! compact. Compacting would rename every name after the first loss.

use ash::vk::native as hh;
use pf_bitstream::av1::coded_cdef_sec_strength;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::REFS_PER_FRAME;
use pf_bitstream::av1::{TilePlan, NUM_REF_SLOTS};

use crate::slots::SlotError;
use crate::slots::SlotMap;

const STD_FRAME_TYPE_KEY: hh::StdVideoAV1FrameType = 0;
const STD_FRAME_TYPE_INTER: hh::StdVideoAV1FrameType = 1;
const STD_FRAME_TYPE_INTRA_ONLY: hh::StdVideoAV1FrameType = 2;
const STD_FRAME_TYPE_SWITCH: hh::StdVideoAV1FrameType = 3;

/// Unused `referenceNameSlotIndices` entry: this frame does not use that name.
pub const REFERENCE_NAME_UNUSED: i32 = -1;

/// Spec minimum superres denominator; `coded_denom` is the real one minus this.
const SUPERRES_DENOM_MIN: u32 = 9;

#[derive(Debug, Clone)]
pub struct VkRefAv1 {
    pub slot: u8,
    pub std: hh::StdVideoDecodeAV1ReferenceInfo,
    pub id: PicId,
}

#[derive(Debug)]
pub struct DecodePlanVkAv1 {
    pub pic: OwnedStdAv1PictureInfo,
    /// DPB slot per reference name (`LAST_FRAME`..`ALTREF_FRAME`), or
    /// [`REFERENCE_NAME_UNUSED`]. Not an index into [`Self::refs`].
    pub reference_name_slot_indices: [i32; REFS_PER_FRAME],
    /// Tile-group OBU ranges as planned. The bitstream buffer holds tile
    /// payloads inside those OBUs, not the OBUs; `decoder_av1` walks them
    /// because this conversion never sees the access-unit bytes.
    pub tiles: Vec<TilePlan>,
    pub setup_slot: u8,
    pub setup_ref: hh::StdVideoDecodeAV1ReferenceInfo,
    pub setup_id: PicId,
    /// Unique referenced pictures, first appearance first. The backend lays
    /// `pReferenceSlots` out in this order.
    pub refs: Vec<VkRefAv1>,
    /// Pictures this frame's `refresh_frame_flags` displaces while this frame
    /// still reads them. Release their slots only after the decode op is
    /// recorded — never here.
    ///
    /// AV1 applies refresh after decode (7.20), so reading a slot and
    /// overwriting it is ordinary. Releasing here would hand the vacated slot
    /// to [`Self::setup_slot`]; [`Self::refs`] would then name the picture
    /// being written. Always a subset of the plan's `dpb.removed`.
    pub release_after_decode: Vec<PicId>,
}

/// Std picture info plus the heap its eight pointers target.
///
/// Same contract as [`crate::OwnedStdSps`]: boxed backing, movable wrapper,
/// no mutation, not `Clone`.
#[derive(Debug)]
pub struct OwnedStdAv1PictureInfo {
    std: hh::StdVideoDecodeAV1PictureInfo,
    _tile_info: Box<hh::StdVideoAV1TileInfo>,
    /// `StdVideoAV1TileInfo`'s four arrays, behind its pointers — why this
    /// is a wrapper rather than a plain struct.
    _tile_arrays: TileArrays,
    _quantization: Box<hh::StdVideoAV1Quantization>,
    _segmentation: Box<hh::StdVideoAV1Segmentation>,
    _loop_filter: Box<hh::StdVideoAV1LoopFilter>,
    _cdef: Box<hh::StdVideoAV1CDEF>,
    _loop_restoration: Box<hh::StdVideoAV1LoopRestoration>,
    _global_motion: Box<hh::StdVideoAV1GlobalMotion>,
    /// Present only when the stream codes film grain. A zeroed block behind
    /// a live pointer would ask the decoder to synthesise grain never coded.
    _film_grain: Option<Box<hh::StdVideoAV1FilmGrain>>,
}

impl OwnedStdAv1PictureInfo {
    /// The Std struct, valid for as long as `self` lives.
    pub fn std(&self) -> &hh::StdVideoDecodeAV1PictureInfo {
        &self.std
    }
}

/// Tile-info arrays, boxed so `StdVideoAV1TileInfo`'s pointers stay valid.
#[derive(Debug)]
struct TileArrays {
    _mi_col_starts: Box<[u16]>,
    _mi_row_starts: Box<[u16]>,
    _width_in_sbs_minus_1: Box<[u16]>,
    _height_in_sbs_minus_1: Box<[u16]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVkAv1Error {
    /// `show_existing_frame`: nothing to decode; the backend displays the
    /// named picture instead.
    NoDecode,
    UnresolvedReference(PicId),
    TooManyReferences(usize),
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVkAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToVkAv1Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToVkAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVkAv1Error::NoDecode => {
                write!(f, "a show_existing_frame plan has no decode submission")
            }
            PlanToVkAv1Error::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToVkAv1Error::TooManyReferences(n) => {
                write!(f, "{n} distinct references exceed the DPB")
            }
            PlanToVkAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its Std field")
            }
            PlanToVkAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVkAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToVkAv1Error> {
    u8::try_from(value).map_err(|_| PlanToVkAv1Error::FieldOverflow { field, value })
}

/// Parser frame type as `StdVideoAV1FrameType`.
///
/// Matched, not cast: the discriminants coincide between a vendored enum
/// and a Vulkan header, neither of which we own. Picture info and every
/// reference info go through here so they cannot disagree.
fn std_frame_type(frame_type: pf_bitstream::av1::FrameType) -> hh::StdVideoAV1FrameType {
    match frame_type {
        pf_bitstream::av1::FrameType::KeyFrame => STD_FRAME_TYPE_KEY,
        pf_bitstream::av1::FrameType::InterFrame => STD_FRAME_TYPE_INTER,
        pf_bitstream::av1::FrameType::IntraOnlyFrame => STD_FRAME_TYPE_INTRA_ONLY,
        pf_bitstream::av1::FrameType::SwitchFrame => STD_FRAME_TYPE_SWITCH,
    }
}

/// Convert one planned AV1 frame: `refs` and `picture_info` build the
/// sections, then the slot commit.
///
/// `slots` is untouched until every fallible step has passed. A half-applied
/// DPB update is a corrupt reference.
pub fn plan_to_vk_av1(
    plan: &AuPlan,
    slots: &mut SlotMap,
) -> Result<DecodePlanVkAv1, PlanToVkAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToVkAv1Error::NoDecode)?;
    let header = &*plan.header;

    let (refs, reference_name_slot_indices) = refs(plan, slots)?;
    let pic = picture_info(header, &plan.sequence)?;
    // Setup needs the same answers a later frame will read from this slot.
    // Built through `reference_info` so the cached copy matches a reference
    // built from this header. libavcodec zeros `SavedOrderHints` here
    // because it rebuilds every frame and never re-reads the setup entry.
    let setup_ref = reference_info(&pf_bitstream::av1::RefState::of(header))?;

    // A picture this frame reads may be displaced by this same refresh.
    // Hold its slot and hand it to the caller; see
    // `DecodePlanVkAv1::release_after_decode`.
    let release_after_decode: Vec<PicId> = plan
        .dpb
        .removed
        .iter()
        .copied()
        .filter(|id| *id != setup_id && refs.iter().any(|r| r.id == *id))
        .collect();
    for &id in &plan.dpb.removed {
        if id == setup_id || release_after_decode.contains(&id) {
            continue;
        }
        let _ = slots.release(id);
    }
    let setup_slot = match slots.slot_of(setup_id) {
        // Refresh of a slot this picture already occupies: do not re-assign.
        Some(existing) => existing,
        None => slots.assign(setup_id)?,
    };

    Ok(DecodePlanVkAv1 {
        pic,
        reference_name_slot_indices,
        tiles: plan.tiles.clone(),
        setup_slot,
        setup_ref,
        setup_id,
        refs,
        release_after_decode,
    })
}

/// Unique refs, first appearance first, plus the per-name slot table.
/// `plan.refs` is indexed by name; holes are lost refs. Skip them —
/// compacting would rename every name after the first hole. Read-only on
/// `slots`.
fn refs(
    plan: &AuPlan,
    slots: &SlotMap,
) -> Result<(Vec<VkRefAv1>, [i32; REFS_PER_FRAME]), PlanToVkAv1Error> {
    let mut refs: Vec<VkRefAv1> = Vec::new();
    let mut reference_name_slot_indices = [REFERENCE_NAME_UNUSED; REFS_PER_FRAME];
    for (name, r) in plan.refs.iter().enumerate() {
        let Some(r) = r else { continue };
        let slot = slots
            .slot_of(r.id)
            .ok_or(PlanToVkAv1Error::UnresolvedReference(r.id))?;
        reference_name_slot_indices[name] = i32::from(slot);
        if !refs.iter().any(|existing| existing.id == r.id) {
            refs.push(VkRefAv1 {
                slot,
                // The reference's own state, never this frame's.
                std: reference_info(&r.state)?,
                id: r.id,
            });
        }
    }
    if refs.len() > NUM_REF_SLOTS {
        return Err(PlanToVkAv1Error::TooManyReferences(refs.len()));
    }
    Ok((refs, reference_name_slot_indices))
}

/// One picture's `StdVideoDecodeAV1ReferenceInfo` from that picture's header.
///
/// Every field is about the reference. Filling any from the frame currently
/// being decoded is a silent mispredict. Matches libavcodec `vk_av1_fill_pict`;
/// RADV reads `RefFrameSignBias` and `SavedOrderHints`.
fn reference_info(
    state: &pf_bitstream::av1::RefState,
) -> Result<hh::StdVideoDecodeAV1ReferenceInfo, PlanToVkAv1Error> {
    // SAFETY: StdVideoDecodeAV1ReferenceInfo is a plain-C bindgen struct of a
    // bitfield word, three small integers and a byte array; all-zero is valid for
    // every field.
    let mut std: hh::StdVideoDecodeAV1ReferenceInfo = unsafe { std::mem::zeroed() };
    std.flags
        .set_disable_frame_end_update_cdf(state.disable_frame_end_update_cdf.into());
    std.flags
        .set_segmentation_enabled(state.segmentation_enabled.into());
    std.frame_type = narrow("frame_type", std_frame_type(state.frame_type))?;
    std.RefFrameSignBias = state.ref_frame_sign_bias;
    std.OrderHint = narrow("OrderHint", state.order_hint)?;
    for (dst, hint) in std
        .SavedOrderHints
        .iter_mut()
        .zip(state.saved_order_hints.iter())
    {
        // `order_hint_bits` is at most 8, so this truncation is unreachable.
        // Same cast as `OrderHints` on the picture info.
        *dst = *hint as u8;
    }
    Ok(std)
}

/// Frame header plus sequence (film-grain gate) into Std picture info: one
/// helper per boxed block, assembled here.
///
/// Headers rather than [`AuPlan`] so a unit test can convert a hand-built
/// header without inventing a plan. The vendored vector never codes grain.
fn picture_info(
    p: &pf_bitstream::av1::ParsedFrameHeader,
    sequence: &pf_bitstream::av1::ParsedSequenceHeader,
) -> Result<OwnedStdAv1PictureInfo, PlanToVkAv1Error> {
    let (tile_std, tile_arrays) = tiles(p, sequence)?;
    let tile_info = Box::new(tile_std);
    let quantization = Box::new(quantization(p)?);
    let segmentation = Box::new(segmentation(p));
    let loop_filter = Box::new(loop_filter(p));
    let cdef = Box::new(cdef(p)?);
    let loop_restoration = Box::new(loop_restoration(p));
    let global_motion = Box::new(global_motion(p));
    let film_grain = film_grain(p, sequence)?.map(Box::new);

    // SAFETY: StdVideoDecodeAV1PictureInfo is a plain-C bindgen struct of a
    // bitfield word, integers, byte arrays and const pointers; all-zero is valid
    // for every field, and every pointer is assigned below.
    let mut std: hh::StdVideoDecodeAV1PictureInfo = unsafe { std::mem::zeroed() };
    // Matches the `pFilmGrain` gate: grain applied iff a block is attached.
    std.flags = frame_flags(p, film_grain.is_some());
    std.frame_type = std_frame_type(p.frame_type);
    std.current_frame_id = p.current_frame_id;
    std.OrderHint = narrow("OrderHint", p.order_hint)?;
    std.primary_ref_frame = narrow("primary_ref_frame", p.primary_ref_frame)?;
    std.refresh_frame_flags = narrow("refresh_frame_flags", p.refresh_frame_flags)?;
    std.interpolation_filter = p.interpolation_filter as u32;
    std.TxMode = p.tx_mode as u32;
    std.delta_q_res = narrow("delta_q_res", p.quantization_params.delta_q_res)?;
    std.delta_lf_res = p.loop_filter_params.delta_lf_res;
    std.SkipModeFrame = [
        narrow("SkipModeFrame[0]", p.skip_mode_frame[0])?,
        narrow("SkipModeFrame[1]", p.skip_mode_frame[1])?,
    ];
    // Superres denominator as coded: spec stores it `SUPERRES_DENOM_MIN`
    // less than the real one, and only when superres is in use.
    std.coded_denom = if p.use_superres {
        narrow(
            "coded_denom",
            p.superres_denom.saturating_sub(SUPERRES_DENOM_MIN),
        )?
    } else {
        0
    };
    for (i, hint) in p.order_hints.iter().enumerate().take(NUM_REF_SLOTS) {
        std.OrderHints[i] = *hint as u8;
    }
    std.pTileInfo = &*tile_info;
    std.pQuantization = &*quantization;
    std.pSegmentation = &*segmentation;
    std.pLoopFilter = &*loop_filter;
    std.pCDEF = &*cdef;
    std.pLoopRestoration = &*loop_restoration;
    std.pGlobalMotion = &*global_motion;
    std.pFilmGrain = film_grain
        .as_ref()
        .map_or(std::ptr::null(), |g| &**g as *const _);

    Ok(OwnedStdAv1PictureInfo {
        std,
        _tile_info: tile_info,
        _tile_arrays: tile_arrays,
        _quantization: quantization,
        _segmentation: segmentation,
        _loop_filter: loop_filter,
        _cdef: cdef,
        _loop_restoration: loop_restoration,
        _global_motion: global_motion,
        _film_grain: film_grain,
    })
}

/// `StdVideoAV1TileInfo` plus the four arrays its pointers target. The
/// arrays live on the heap, so moving `TileArrays` keeps the pointers valid.
fn tiles(
    p: &pf_bitstream::av1::ParsedFrameHeader,
    sequence: &pf_bitstream::av1::ParsedSequenceHeader,
) -> Result<(hh::StdVideoAV1TileInfo, TileArrays), PlanToVkAv1Error> {
    let tile = &p.tile_info;
    // `pMiColStarts`/`pMiRowStarts` take SUPERBLOCK starts, not the 4x4 units their
    // names read as; libavcodec writes `tile_start_col_sb`/`tile_start_row_sb` there.
    // The parser holds MI, so shift. Only entry 0 is common to both readings, which
    // is why a one-tile frame survives either way and a tiled one does not.
    let sb_shift = if sequence.use_128x128_superblock {
        5
    } else {
        4
    };
    let mi_col_starts: Box<[u16]> = tile
        .mi_col_starts
        .iter()
        .map(|v| (*v >> sb_shift) as u16)
        .collect();
    let mi_row_starts: Box<[u16]> = tile
        .mi_row_starts
        .iter()
        .map(|v| (*v >> sb_shift) as u16)
        .collect();
    let width_in_sbs: Box<[u16]> = tile
        .width_in_sbs_minus_1
        .iter()
        .map(|v| *v as u16)
        .collect();
    let height_in_sbs: Box<[u16]> = tile
        .height_in_sbs_minus_1
        .iter()
        .map(|v| *v as u16)
        .collect();
    // SAFETY: StdVideoAV1TileInfo is a plain-C bindgen struct of a bitfield
    // word, integers and const pointers; all-zero is valid for every field, and
    // every pointer is assigned below.
    let mut tile_std: hh::StdVideoAV1TileInfo = unsafe { std::mem::zeroed() };
    tile_std
        .flags
        .set_uniform_tile_spacing_flag(tile.uniform_tile_spacing_flag.into());
    tile_std.TileCols = narrow("TileCols", tile.tile_cols)?;
    tile_std.TileRows = narrow("TileRows", tile.tile_rows)?;
    tile_std.context_update_tile_id = tile.context_update_tile_id as u16;
    tile_std.tile_size_bytes_minus_1 = narrow(
        "tile_size_bytes_minus_1",
        tile.tile_size_bytes.saturating_sub(1),
    )?;
    tile_std.pMiColStarts = mi_col_starts.as_ptr();
    tile_std.pMiRowStarts = mi_row_starts.as_ptr();
    tile_std.pWidthInSbsMinus1 = width_in_sbs.as_ptr();
    tile_std.pHeightInSbsMinus1 = height_in_sbs.as_ptr();
    let tile_arrays = TileArrays {
        _mi_col_starts: mi_col_starts,
        _mi_row_starts: mi_row_starts,
        _width_in_sbs_minus_1: width_in_sbs,
        _height_in_sbs_minus_1: height_in_sbs,
    };
    Ok((tile_std, tile_arrays))
}

fn quantization(
    p: &pf_bitstream::av1::ParsedFrameHeader,
) -> Result<hh::StdVideoAV1Quantization, PlanToVkAv1Error> {
    let q = &p.quantization_params;
    // SAFETY: StdVideoAV1Quantization is a plain-C bindgen struct of a bitfield
    // word and integers; all-zero is valid for every field.
    let mut q_std: hh::StdVideoAV1Quantization = unsafe { std::mem::zeroed() };
    q_std.flags.set_using_qmatrix(q.using_qmatrix.into());
    q_std.flags.set_diff_uv_delta(q.diff_uv_delta.into());
    q_std.base_q_idx = narrow("base_q_idx", q.base_q_idx)?;
    q_std.DeltaQYDc = q.delta_q_y_dc as i8;
    q_std.DeltaQUDc = q.delta_q_u_dc as i8;
    q_std.DeltaQUAc = q.delta_q_u_ac as i8;
    q_std.DeltaQVDc = q.delta_q_v_dc as i8;
    q_std.DeltaQVAc = q.delta_q_v_ac as i8;
    q_std.qm_y = narrow("qm_y", q.qm_y)?;
    q_std.qm_u = narrow("qm_u", q.qm_u)?;
    q_std.qm_v = narrow("qm_v", q.qm_v)?;
    Ok(q_std)
}

fn segmentation(p: &pf_bitstream::av1::ParsedFrameHeader) -> hh::StdVideoAV1Segmentation {
    let s = &p.segmentation_params;
    // SAFETY: StdVideoAV1Segmentation is a plain-C bindgen struct of integer
    // arrays; all-zero is valid for every field.
    let mut s_std: hh::StdVideoAV1Segmentation = unsafe { std::mem::zeroed() };
    for (seg, enabled) in s.feature_enabled.iter().enumerate() {
        let mut bits = 0u8;
        for (feature, on) in enabled.iter().enumerate() {
            if *on {
                bits |= 1 << feature;
            }
        }
        s_std.FeatureEnabled[seg] = bits;
        s_std.FeatureData[seg] = s.feature_data[seg];
    }
    s_std
}

fn loop_filter(p: &pf_bitstream::av1::ParsedFrameHeader) -> hh::StdVideoAV1LoopFilter {
    let lf = &p.loop_filter_params;
    // SAFETY: StdVideoAV1LoopFilter is a plain-C bindgen struct of a bitfield
    // word and integer arrays; all-zero is valid for every field.
    let mut lf_std: hh::StdVideoAV1LoopFilter = unsafe { std::mem::zeroed() };
    lf_std
        .flags
        .set_loop_filter_delta_enabled(lf.loop_filter_delta_enabled.into());
    lf_std
        .flags
        .set_loop_filter_delta_update(lf.loop_filter_delta_update.into());
    lf_std.loop_filter_level = lf.loop_filter_level;
    lf_std.loop_filter_sharpness = lf.loop_filter_sharpness;
    lf_std.loop_filter_ref_deltas = lf.loop_filter_ref_deltas;
    lf_std.loop_filter_mode_deltas = lf.loop_filter_mode_deltas;
    lf_std
}

/// Secondary strengths are the coded two-bit values, not the spec fixup
/// (`== 3` becomes 4). Sending 4 overflows the two bits every driver packs;
/// strongest secondary filter reads back as none. `coded_cdef_sec_strength`
/// is the inverse.
fn cdef(p: &pf_bitstream::av1::ParsedFrameHeader) -> Result<hh::StdVideoAV1CDEF, PlanToVkAv1Error> {
    let c = &p.cdef_params;
    // SAFETY: StdVideoAV1CDEF is a plain-C bindgen struct of integers and
    // integer arrays; all-zero is valid for every field.
    let mut c_std: hh::StdVideoAV1CDEF = unsafe { std::mem::zeroed() };
    c_std.cdef_damping_minus_3 = narrow("cdef_damping_minus_3", c.cdef_damping.saturating_sub(3))?;
    c_std.cdef_bits = narrow("cdef_bits", c.cdef_bits)?;
    for i in 0..8 {
        c_std.cdef_y_pri_strength[i] = c.cdef_y_pri_strength[i] as u8;
        c_std.cdef_y_sec_strength[i] = coded_cdef_sec_strength(c.cdef_y_sec_strength[i]);
        c_std.cdef_uv_pri_strength[i] = c.cdef_uv_pri_strength[i] as u8;
        c_std.cdef_uv_sec_strength[i] = coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]);
    }
    Ok(c_std)
}

/// `LoopRestorationSize` is the coded value, not pixels. RADV stores
/// `log2_restoration_size_minus5`; libavcodec sends `1 + lr_unit_shift`. The
/// parser holds 64/128/256. Sending 64 where the driver expects 1 asks for a
/// 2^69-pixel restoration unit.
fn loop_restoration(p: &pf_bitstream::av1::ParsedFrameHeader) -> hh::StdVideoAV1LoopRestoration {
    let lr = &p.loop_restoration_params;
    // SAFETY: StdVideoAV1LoopRestoration is a plain-C bindgen struct of integer
    // arrays; all-zero is valid for every field.
    let mut lr_std: hh::StdVideoAV1LoopRestoration = unsafe { std::mem::zeroed() };
    let luma_size = 1 + u16::from(lr.lr_unit_shift);
    // `lr_uv_shift` is 0 or 1 and `luma_size` ≥ 1, so this cannot wrap.
    // Saturation is so a malformed parse cannot send 65535 as log2(size)−5.
    let chroma_size = luma_size.saturating_sub(u16::from(lr.lr_uv_shift));
    for i in 0..3 {
        lr_std.FrameRestorationType[i] = lr.frame_restoration_type[i] as u32;
        lr_std.LoopRestorationSize[i] = if i == 0 { luma_size } else { chroma_size };
    }
    lr_std
}

fn global_motion(p: &pf_bitstream::av1::ParsedFrameHeader) -> hh::StdVideoAV1GlobalMotion {
    let gm = &p.global_motion_params;
    // SAFETY: StdVideoAV1GlobalMotion is a plain-C bindgen struct of integer
    // arrays; all-zero is valid for every field.
    let mut gm_std: hh::StdVideoAV1GlobalMotion = unsafe { std::mem::zeroed() };
    for i in 0..NUM_REF_SLOTS {
        gm_std.GmType[i] = gm.gm_type[i] as u8;
        gm_std.gm_params[i] = gm.gm_params[i];
    }
    gm_std
}

/// Grain only when the sequence enables it and this frame applies it. Either
/// gate false: a zeroed block behind a live pointer would ask the decoder to
/// synthesise grain the stream never described.
fn film_grain(
    p: &pf_bitstream::av1::ParsedFrameHeader,
    sequence: &pf_bitstream::av1::ParsedSequenceHeader,
) -> Result<Option<hh::StdVideoAV1FilmGrain>, PlanToVkAv1Error> {
    if !(sequence.film_grain_params_present && p.film_grain_params.apply_grain) {
        return Ok(None);
    }
    let fg = &p.film_grain_params;
    // SAFETY: StdVideoAV1FilmGrain is a plain-C bindgen struct of a bitfield
    // word, integers and integer arrays; all-zero is valid for every field.
    let mut fg_std: hh::StdVideoAV1FilmGrain = unsafe { std::mem::zeroed() };
    fg_std
        .flags
        .set_chroma_scaling_from_luma(fg.chroma_scaling_from_luma.into());
    fg_std.flags.set_overlap_flag(fg.overlap_flag.into());
    fg_std
        .flags
        .set_clip_to_restricted_range(fg.clip_to_restricted_range.into());
    fg_std.flags.set_update_grain(fg.update_grain.into());
    fg_std.grain_scaling_minus_8 = fg.grain_scaling_minus_8;
    fg_std.ar_coeff_lag = narrow("ar_coeff_lag", fg.ar_coeff_lag)?;
    fg_std.ar_coeff_shift_minus_6 = fg.ar_coeff_shift_minus_6;
    fg_std.grain_scale_shift = fg.grain_scale_shift;
    fg_std.grain_seed = fg.grain_seed;
    fg_std.film_grain_params_ref_idx = fg.film_grain_params_ref_idx;
    // Chroma scaling (7.18.3.5 `scaling_lut`). Zeroes are not "less
    // grain": they synthesise different chroma noise. libavcodec and
    // the DXVA conversion set all six.
    fg_std.cb_mult = fg.cb_mult;
    fg_std.cb_luma_mult = fg.cb_luma_mult;
    fg_std.cb_offset = fg.cb_offset;
    fg_std.cr_mult = fg.cr_mult;
    fg_std.cr_luma_mult = fg.cr_luma_mult;
    fg_std.cr_offset = fg.cr_offset;

    // Parser arrays are 16; Std is 14 (luma) and 10 (chroma) — spec
    // maxima. Refuse overflow rather than truncate: fewer scaling
    // points than the stream declared synthesises different grain.
    let points = |name: &'static str, count: u8, cap: usize| -> Result<usize, PlanToVkAv1Error> {
        if usize::from(count) > cap {
            return Err(PlanToVkAv1Error::FieldOverflow {
                field: name,
                value: u32::from(count),
            });
        }
        Ok(usize::from(count))
    };
    let ny = points("num_y_points", fg.num_y_points, fg_std.point_y_value.len())?;
    let ncb = points(
        "num_cb_points",
        fg.num_cb_points,
        fg_std.point_cb_value.len(),
    )?;
    let ncr = points(
        "num_cr_points",
        fg.num_cr_points,
        fg_std.point_cr_value.len(),
    )?;
    fg_std.num_y_points = fg.num_y_points;
    fg_std.num_cb_points = fg.num_cb_points;
    fg_std.num_cr_points = fg.num_cr_points;
    fg_std.point_y_value[..ny].copy_from_slice(&fg.point_y_value[..ny]);
    fg_std.point_y_scaling[..ny].copy_from_slice(&fg.point_y_scaling[..ny]);
    fg_std.point_cb_value[..ncb].copy_from_slice(&fg.point_cb_value[..ncb]);
    fg_std.point_cb_scaling[..ncb].copy_from_slice(&fg.point_cb_scaling[..ncb]);
    fg_std.point_cr_value[..ncr].copy_from_slice(&fg.point_cr_value[..ncr]);
    fg_std.point_cr_scaling[..ncr].copy_from_slice(&fg.point_cr_scaling[..ncr]);
    for (dst, src) in fg_std
        .ar_coeffs_y_plus_128
        .iter_mut()
        .zip(fg.ar_coeffs_y_plus_128.iter())
    {
        *dst = *src as i8;
    }
    for (dst, src) in fg_std
        .ar_coeffs_cb_plus_128
        .iter_mut()
        .zip(fg.ar_coeffs_cb_plus_128.iter())
    {
        *dst = *src as i8;
    }
    for (dst, src) in fg_std
        .ar_coeffs_cr_plus_128
        .iter_mut()
        .zip(fg.ar_coeffs_cr_plus_128.iter())
    {
        *dst = *src as i8;
    }
    Ok(Some(fg_std))
}

/// Picture-level flags, `apply_grain` from whether a grain block is attached.
fn frame_flags(
    p: &pf_bitstream::av1::ParsedFrameHeader,
    apply_grain: bool,
) -> hh::StdVideoDecodeAV1PictureInfoFlags {
    // SAFETY: StdVideoDecodeAV1PictureInfoFlags is a plain-C bindgen bitfield
    // word; all-zero is valid.
    let mut flags: hh::StdVideoDecodeAV1PictureInfoFlags = unsafe { std::mem::zeroed() };
    flags.set_error_resilient_mode(p.error_resilient_mode.into());
    flags.set_disable_cdf_update(p.disable_cdf_update.into());
    flags.set_use_superres(p.use_superres.into());
    // `allow_intrabc` is only codeable when `allow_screen_content_tools`
    // is on; the two disagreeing is a contradiction a driver may resolve
    // either way.
    flags.set_allow_screen_content_tools(u32::from(p.allow_screen_content_tools != 0));
    flags.set_allow_warped_motion(p.allow_warped_motion.into());
    flags.set_is_filter_switchable(p.is_filter_switchable.into());
    flags.set_force_integer_mv(u32::from(p.force_integer_mv != 0));
    flags.set_render_and_frame_size_different(p.render_and_frame_size_different.into());
    flags.set_frame_size_override_flag(p.frame_size_override_flag.into());
    flags.set_buffer_removal_time_present_flag(p.buffer_removal_time_present_flag.into());
    flags.set_frame_refs_short_signaling(p.frame_refs_short_signaling.into());
    flags.set_allow_intrabc(p.allow_intrabc.into());
    flags.set_allow_high_precision_mv(p.allow_high_precision_mv.into());
    flags.set_is_motion_mode_switchable(p.is_motion_mode_switchable.into());
    flags.set_use_ref_frame_mvs(p.use_ref_frame_mvs.into());
    flags.set_disable_frame_end_update_cdf(p.disable_frame_end_update_cdf.into());
    flags.set_reduced_tx_set(p.reduced_tx_set.into());
    flags.set_reference_select(p.reference_select.into());
    flags.set_skip_mode_present(p.skip_mode_present.into());
    flags.set_delta_q_present(p.quantization_params.delta_q_present.into());
    flags.set_delta_lf_present(p.loop_filter_params.delta_lf_present.into());
    flags.set_delta_lf_multi(p.loop_filter_params.delta_lf_multi.into());
    let s = &p.segmentation_params;
    flags.set_segmentation_enabled(s.segmentation_enabled.into());
    flags.set_segmentation_update_map(s.segmentation_update_map.into());
    flags.set_segmentation_temporal_update(s.segmentation_temporal_update.into());
    flags.set_segmentation_update_data(s.segmentation_update_data.into());
    // Derived, not coded: any plane's restoration type other than NONE.
    flags.set_UsesLr(u32::from(
        p.loop_restoration_params
            .frame_restoration_type
            .iter()
            .any(|t| *t as u32 != 0),
    ));
    // `usesChromaLr` stays 0 — `zeroed()` above, not an omission.
    // libavcodec never sets it; drivers were validated against that.
    // A truthful 1 would be the first implementation to send one.
    flags.set_apply_grain(u32::from(apply_grain));
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;

    const AV1_25FPS: &[u8] = pf_bitstream::testing::AV1_25FPS;

    /// Convert the way a backend must: conversion, then the deferred
    /// [`DecodePlanVkAv1::release_after_decode`]. Skipping the second half
    /// leaks a slot on most frames of this vector and drains the ledger.
    fn convert(plan: &AuPlan, slots: &mut SlotMap) -> DecodePlanVkAv1 {
        let vk = plan_to_vk_av1(plan, slots).expect("the clean vector converts");
        for &id in &vk.release_after_decode {
            assert!(
                slots.release(id),
                "a deferred release named picture {id}, which holds no slot"
            );
        }
        vk
    }

    /// Convert every frame of the vendored vector.
    ///
    /// `referenceNameSlotIndices` holds DPB slots, not positions in `refs`.
    /// The two coincide while refs land in slots `0..refs.len()` in `refs`
    /// order — a fresh key. Fail if they never disagree: then the test
    /// cannot tell the conventions apart.
    #[test]
    fn every_frame_converts_and_slot_indices_are_not_positions() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_refs) = (0u32, 0u32);
        let mut disagreements = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue; // show_existing_frame: no submission
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;

                for name in vk.reference_name_slot_indices {
                    if name == REFERENCE_NAME_UNUSED {
                        continue;
                    }
                    let slot = u8::try_from(name).expect("a slot index is small and positive");
                    assert!(
                        vk.refs.iter().any(|r| r.slot == slot),
                        "reference name resolves to slot {slot}, which this frame's \
                         reference list does not bind"
                    );
                }
                if !vk.refs.is_empty() {
                    with_refs += 1;
                    // Would treating these as positions in `refs` match?
                    for (name, entry) in vk.reference_name_slot_indices.iter().enumerate() {
                        if *entry == REFERENCE_NAME_UNUSED {
                            continue;
                        }
                        let as_position = vk.refs.get(name).map(|r| i32::from(r.slot));
                        if as_position != Some(*entry) {
                            disagreements += 1;
                        }
                    }
                }
                // A refresh that displaces a picture this frame reads must
                // not recycle that slot into `setup_slot` — `refs` would
                // then name the picture being written.
                for r in &vk.refs {
                    assert_ne!(
                        r.slot, vk.setup_slot,
                        "frame {frames}: reference (picture {}) aliases the setup slot",
                        r.id
                    );
                }
                assert_eq!(slots.slot_of(vk.setup_id), Some(vk.setup_slot));
            }
        }

        assert_eq!(frames, 274, "every frame of the vector must convert");
        assert!(with_refs > 0, "a 274-frame vector must reference something");
        assert!(
            disagreements > 0,
            "slot indices and reference-list positions never disagreed on this \
             vector, so this test cannot tell the two conventions apart — the same \
             blind spot that let the HEVC RPS defect ship"
        );
        eprintln!(
            "frames {frames} · with refs {with_refs} · slot-vs-position disagreements \
             {disagreements}"
        );
    }

    /// A frame that reads a slot its own refresh overwrites keeps that slot
    /// until the decode op is recorded.
    ///
    /// AV1 applies `refresh_frame_flags` after decode (7.20), so
    /// `ref_frame_idx` resolves against the pre-decode store. The count of
    /// deferring frames is pinned so an empty list cannot pass this test.
    #[test]
    fn a_reference_this_frame_displaces_keeps_its_slot_until_after_the_decode() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut deferring, mut deferred_ids) = (0u32, 0u32, 0u32);
        let mut peak_active = 0usize;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = plan_to_vk_av1(&plan, &mut slots).expect("converts");
                frames += 1;
                peak_active = peak_active.max(slots.active());

                for &id in &vk.release_after_decode {
                    assert!(
                        plan.dpb.removed.contains(&id),
                        "frame {frames}: deferred picture {id} is not in this plan's \
                         removed list"
                    );
                    assert!(
                        vk.refs.iter().any(|r| r.id == id),
                        "frame {frames}: picture {id} is deferred without being read — \
                         only a reference of THIS frame earns the reprieve"
                    );
                    assert!(
                        slots.slot_of(id).is_some(),
                        "frame {frames}: a deferred picture must still hold its slot"
                    );
                }
                for r in &vk.refs {
                    assert_ne!(r.slot, vk.setup_slot);
                }
                if !vk.release_after_decode.is_empty() {
                    deferring += 1;
                    deferred_ids += vk.release_after_decode.len() as u32;
                }
                for &id in &vk.release_after_decode {
                    assert!(slots.release(id));
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            deferring, 268,
            "268 of 274 frames of this vector displace a picture they are reading; \
             at zero this test compares an empty list against itself and the \
             deferral could be deleted without a single assertion noticing"
        );
        assert_eq!(deferred_ids, 268, "one displaced reference per frame here");
        assert!(
            peak_active <= slots.capacity(),
            "deferring a release must not overrun the ledger"
        );
        // Capacity is `NUM_REF_SLOTS + 1`; the spare holds a displaced
        // reference one frame longer. Hitting capacity is a sizing bug.
        eprintln!("frames {frames} · deferring {deferring} · peak slots held {peak_active}");
    }

    /// Every picture-info flag this conversion writes, checked against the
    /// header. Incidence of the reconstruction flags is pinned so a bit
    /// that silently stopped being written fails here.
    #[test]
    fn every_picture_info_flag_matches_the_header_and_the_incidence_is_pinned() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;
        let (mut screen, mut warped, mut switchable, mut integer_mv) = (0u32, 0u32, 0u32, 0u32);
        let (mut informational, mut intrabc) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                let f = &vk.pic.std().flags;
                let p = &*plan.header;
                frames += 1;

                let bit = |b: bool| u32::from(b);
                assert_eq!(
                    f.allow_screen_content_tools(),
                    bit(p.allow_screen_content_tools != 0)
                );
                assert_eq!(f.allow_warped_motion(), bit(p.allow_warped_motion));
                assert_eq!(f.is_filter_switchable(), bit(p.is_filter_switchable));
                assert_eq!(f.force_integer_mv(), bit(p.force_integer_mv != 0));
                // Intra block copy is only codeable with screen-content
                // tools on; the two disagreeing is a contradiction a
                // driver may resolve either way.
                if f.allow_intrabc() == 1 {
                    assert_eq!(
                        f.allow_screen_content_tools(),
                        1,
                        "allow_intrabc without allow_screen_content_tools"
                    );
                    intrabc += 1;
                }
                assert_eq!(
                    f.render_and_frame_size_different(),
                    bit(p.render_and_frame_size_different)
                );
                assert_eq!(
                    f.frame_size_override_flag(),
                    bit(p.frame_size_override_flag)
                );
                assert_eq!(
                    f.buffer_removal_time_present_flag(),
                    bit(p.buffer_removal_time_present_flag)
                );
                assert_eq!(
                    f.frame_refs_short_signaling(),
                    bit(p.frame_refs_short_signaling)
                );
                informational += f.render_and_frame_size_different()
                    + f.frame_size_override_flag()
                    + f.buffer_removal_time_present_flag()
                    + f.frame_refs_short_signaling();
                assert_eq!(f.error_resilient_mode(), bit(p.error_resilient_mode));
                assert_eq!(f.disable_cdf_update(), bit(p.disable_cdf_update));
                assert_eq!(f.use_superres(), bit(p.use_superres));
                assert_eq!(f.allow_high_precision_mv(), bit(p.allow_high_precision_mv));
                assert_eq!(
                    f.is_motion_mode_switchable(),
                    bit(p.is_motion_mode_switchable)
                );
                assert_eq!(f.use_ref_frame_mvs(), bit(p.use_ref_frame_mvs));
                assert_eq!(
                    f.disable_frame_end_update_cdf(),
                    bit(p.disable_frame_end_update_cdf)
                );
                assert_eq!(f.reduced_tx_set(), bit(p.reduced_tx_set));
                assert_eq!(f.reference_select(), bit(p.reference_select));
                assert_eq!(f.skip_mode_present(), bit(p.skip_mode_present));
                assert_eq!(
                    f.segmentation_enabled(),
                    bit(p.segmentation_params.segmentation_enabled)
                );
                // Deliberately zero even where the spec would set it — see
                // `picture_info`. Asserted so "fixing" it trips here.
                assert_eq!(
                    f.usesChromaLr(),
                    0,
                    "usesChromaLr is deliberately left at libavcodec's zero"
                );

                screen += f.allow_screen_content_tools();
                warped += f.allow_warped_motion();
                switchable += f.is_filter_switchable();
                integer_mv += f.force_integer_mv();
            }
        }

        assert_eq!(frames, 274);
        // Zero here would mean the four reconstruction flags were never set.
        assert_eq!(screen, 274, "allow_screen_content_tools: 274/274");
        assert_eq!(warped, 273, "allow_warped_motion: 273/274");
        assert_eq!(switchable, 172, "is_filter_switchable: 172/274");
        assert_eq!(integer_mv, 1, "force_integer_mv: the key frame only");
        assert!(intrabc <= frames);
        // This vector codes none of the four informational flags, so those
        // asserts compare 0 against 0. Coverage is the libavcodec cross-read.
        assert_eq!(
            informational, 0,
            "if this ever fires, the informational flags ARE exercised — say so \
             here rather than deleting the count"
        );
    }

    /// `StdVideoAV1LoopFilter` carries four levels; the last two are chroma.
    ///
    /// `[0]`/`[1]` are luma passes, `[2]`/`[3]` are U and V. DXVA and VA-API
    /// spell chroma as named fields, so copying "the levels" as a pair is
    /// the natural mistake and nothing else in this file would notice.
    /// Frame 0 of this vector is `[1, 7, 8, 12]` — the four bytes
    /// libavcodec's Vulkan hwaccel sends for that frame.
    #[test]
    fn the_chroma_deblocking_levels_are_the_last_two_of_four() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_chroma_lf) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let lf = &plan.header.loop_filter_params;
                // SAFETY: `pLoopFilter` points at the boxed block `vk.pic` owns,
                // alive for as long as `vk` is.
                let sent = unsafe { *vk.pic.std().pLoopFilter };
                assert_eq!(
                    sent.loop_filter_level, lf.loop_filter_level,
                    "frame {frames}: all four levels, in order — [0] and [1] the \
                     luma passes, [2] U, [3] V"
                );
                assert_eq!(sent.loop_filter_sharpness, lf.loop_filter_sharpness);
                assert_eq!(sent.loop_filter_ref_deltas, lf.loop_filter_ref_deltas);
                assert_eq!(sent.loop_filter_mode_deltas, lf.loop_filter_mode_deltas);
                if lf.loop_filter_level[2] != 0 || lf.loop_filter_level[3] != 0 {
                    with_chroma_lf += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        sent.loop_filter_level,
                        [1, 7, 8, 12],
                        "frame 0's levels, and the four bytes libavcodec's Vulkan \
                         hwaccel was captured sending for this same frame"
                    );
                    assert_eq!(
                        sent.loop_filter_ref_deltas,
                        [1, 0, 0, 0, -1, 0, -1, -1],
                        "a PRIMARY_REF_NONE frame gets the spec's defaults from \
                         setup_past_independence, and they bump every level by one — \
                         luma included, which is how the parity leg's bit-exact luma \
                         proves the driver read the deltas and the first two levels"
                    );
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            with_chroma_lf, 123,
            "123 of 274 frames of this vector deblock chroma; at zero every level \
             compared above would be zero on both sides and a conversion that sent \
             only the luma pair would pass"
        );
    }

    /// Secondary CDEF strengths are the coded two-bit value, not the spec
    /// fixup (`== 3` becomes 4).
    ///
    /// The parser stores the post-fixup value; every decode API wants the
    /// two-bit read. 4 overflows that field into 0, so the strongest
    /// secondary filter becomes none.
    #[test]
    fn cdef_secondary_strengths_are_the_coded_value_not_the_spec_fixup() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut corrected_frames) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let raw = &plan.header.cdef_params;
                // SAFETY: `pCDEF` points at the boxed block `vk.pic` owns, alive
                // for as long as `vk` is.
                let sent = unsafe { *vk.pic.std().pCDEF };
                let mut corrected = false;
                for i in 0..8 {
                    // Two bits is all any hardware API gives this field.
                    assert!(
                        sent.cdef_y_sec_strength[i] <= 3 && sent.cdef_uv_sec_strength[i] <= 3,
                        "frame {frames}: a secondary strength above 3 overflows the \
                         two bits VA-API, NVDEC and DXVA pack it into"
                    );
                    // Primaries are not fixed up; a correction on the wrong
                    // array of the four would be just as silent.
                    assert_eq!(
                        u32::from(sent.cdef_y_pri_strength[i]),
                        raw.cdef_y_pri_strength[i]
                    );
                    assert_eq!(
                        u32::from(sent.cdef_uv_pri_strength[i]),
                        raw.cdef_uv_pri_strength[i]
                    );
                    if raw.cdef_y_sec_strength[i] == 4 || raw.cdef_uv_sec_strength[i] == 4 {
                        corrected = true;
                    }
                }
                if corrected {
                    corrected_frames += 1;
                }
                if frames == 1 {
                    let coded = 1usize << raw.cdef_bits;
                    assert_eq!(coded, 4, "frame 0 codes cdef_bits = 2");
                    assert_eq!(
                        (
                            &sent.cdef_y_sec_strength[..coded],
                            &sent.cdef_uv_sec_strength[..coded]
                        ),
                        (&[1u8, 2, 0, 3][..], &[3u8, 0, 0, 0][..]),
                        "frame 0's secondary strengths as libavcodec sends them — \
                         the parser holds 4 where these read 3"
                    );
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            corrected_frames, 68,
            "68 of 274 frames of this vector need the correction; at zero this test \
             compares an untouched conversion against itself"
        );
    }

    /// `LoopRestorationSize` is the coded value, not the pixel size.
    ///
    /// `lr_unit_shift = 1` is a 128-pixel unit whose coded value is 2.
    /// Sending 128 to a field read as `log2_restoration_size_minus5` asks
    /// for a 2^133-pixel unit.
    #[test]
    fn loop_restoration_size_is_the_coded_value_not_the_pixel_size() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_lr) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let lr = &plan.header.loop_restoration_params;
                // SAFETY: `pLoopRestoration` points at the boxed block `vk.pic`
                // owns, alive for as long as `vk` is.
                let sizes = unsafe { (*vk.pic.std().pLoopRestoration).LoopRestorationSize };
                assert_eq!(
                    sizes[0],
                    1 + u16::from(lr.lr_unit_shift),
                    "luma: libavcodec sends 1 + lr_unit_shift"
                );
                let chroma = 1 + u16::from(lr.lr_unit_shift) - u16::from(lr.lr_uv_shift);
                assert_eq!(sizes[1], chroma);
                assert_eq!(sizes[2], chroma);
                if lr.uses_lr {
                    with_lr += 1;
                    assert_eq!(lr.loop_restoration_size, [128, 128, 128]);
                    assert_eq!(sizes, [2, 2, 2]);
                    assert_ne!(
                        sizes[0], lr.loop_restoration_size[0],
                        "the coded value and the pixel size must differ here, or \
                         this test cannot tell them apart"
                    );
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            with_lr, 3,
            "three frames of this vector use loop restoration; at zero the \
             assertions above only ever saw the off state"
        );
    }

    /// A reference's Std info describes the reference, not the frame reading
    /// it. `RefFrameSignBias` must carry the future references this vector
    /// is full of.
    #[test]
    fn reference_info_describes_the_reference_and_not_the_current_frame() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;
        let (mut mixed_types, mut biased, mut with_saved_hints) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let vk = convert(&plan, &mut slots);
                frames += 1;
                let current_type = plan.header.frame_type as u8;

                for r in &vk.refs {
                    let by_id = plan
                        .refs
                        .iter()
                        .flatten()
                        .find(|p| p.id == r.id)
                        .expect("every vk ref came from a named plan reference");
                    assert_eq!(r.std.frame_type, by_id.state.frame_type as u8);
                    assert_eq!(r.std.RefFrameSignBias, by_id.state.ref_frame_sign_bias);
                    assert_eq!(
                        r.std.flags.disable_frame_end_update_cdf(),
                        u32::from(by_id.state.disable_frame_end_update_cdf)
                    );
                    assert_eq!(
                        r.std.flags.segmentation_enabled(),
                        u32::from(by_id.state.segmentation_enabled)
                    );
                    assert_eq!(r.std.OrderHint, by_id.state.order_hint as u8);
                    for (sent, want) in r
                        .std
                        .SavedOrderHints
                        .iter()
                        .zip(by_id.state.saved_order_hints.iter())
                    {
                        assert_eq!(u32::from(*sent), *want);
                    }

                    if r.std.frame_type != current_type {
                        mixed_types += 1;
                    }
                    if r.std.RefFrameSignBias != 0 {
                        biased += 1;
                    }
                    if r.std.SavedOrderHints.iter().any(|h| *h != 0) {
                        with_saved_hints += 1;
                    }
                }
                // Setup activates a slot and is cached as that slot's
                // reference info, so it must carry this frame's own state
                // through the same path.
                let own = pf_bitstream::av1::RefState::of(&plan.header);
                assert_eq!(vk.setup_ref.frame_type, own.frame_type as u8);
                assert_eq!(vk.setup_ref.RefFrameSignBias, own.ref_frame_sign_bias);
                assert_eq!(vk.setup_ref.OrderHint, own.order_hint as u8);
            }
        }

        assert_eq!(frames, 274);
        assert!(
            mixed_types > 0,
            "no reference ever had a different frame type from the frame reading \
             it, so handing every reference the CURRENT type would have passed"
        );
        assert!(
            biased > 0,
            "no reference carried a sign bias: this is the hidden-ALTREF vector, \
             so a zero here means the mask never reached the Std struct and every \
             future reference reads as past"
        );
        assert!(with_saved_hints > 0, "SavedOrderHints never carried a hint");
        eprintln!(
            "refs with a foreign frame type {mixed_types} · with a sign bias \
             {biased} · with saved order hints {with_saved_hints}"
        );
    }

    /// Film grain's six chroma-scaling coefficients reach the Std block.
    ///
    /// The vendored vector codes no grain, so this uses a hand-built header.
    /// Zeroes on those six fields are not "less grain".
    #[test]
    fn film_grain_carries_the_chroma_scaling_coefficients() {
        let mut sequence = pf_bitstream::av1::ParsedSequenceHeader {
            film_grain_params_present: true,
            ..Default::default()
        };

        let mut header = pf_bitstream::av1::ParsedFrameHeader::default();
        let fg = &mut header.film_grain_params;
        fg.apply_grain = true;
        fg.grain_seed = 0x1234;
        fg.num_y_points = 2;
        fg.num_cb_points = 1;
        fg.num_cr_points = 1;
        fg.cb_mult = 128;
        fg.cb_luma_mult = 192;
        fg.cb_offset = 256;
        fg.cr_mult = 129;
        fg.cr_luma_mult = 193;
        fg.cr_offset = 257;

        let pic = picture_info(&header, &sequence).expect("a grain header converts");
        assert_eq!(pic.std().flags.apply_grain(), 1);
        assert!(!pic.std().pFilmGrain.is_null());
        // SAFETY: `pFilmGrain` points at the boxed block `pic` owns, alive here.
        let grain = unsafe { *pic.std().pFilmGrain };
        assert_eq!(grain.grain_seed, 0x1234);
        assert_eq!(
            (
                grain.cb_mult,
                grain.cb_luma_mult,
                grain.cb_offset,
                grain.cr_mult,
                grain.cr_luma_mult,
                grain.cr_offset
            ),
            (128, 192, 256, 129, 193, 257),
            "the six chroma-scaling coefficients: nothing else describes how luma \
             feeds chroma grain, and zeroes are not 'less grain', they are \
             different grain"
        );

        sequence.film_grain_params_present = false;
        let pic = picture_info(&header, &sequence).expect("converts");
        assert!(pic.std().pFilmGrain.is_null());
        assert_eq!(pic.std().flags.apply_grain(), 0);
    }

    /// The tile starts reach the driver in superblocks.
    ///
    /// Geometry of the 4K two-tile host stream: the parser codes MI units
    /// (`[0, 272]`) and the driver must read `[0, 17]`. Entry 0 is the same
    /// number either way, so only a tiled frame — on a driver that reads the
    /// array rather than recomputing it — can tell the two apart.
    #[test]
    fn the_tile_starts_reach_the_driver_in_superblocks_not_mi_units() {
        let mut sequence = pf_bitstream::av1::ParsedSequenceHeader::default();
        let mut header = pf_bitstream::av1::ParsedFrameHeader::default();
        let tile = &mut header.tile_info;
        tile.tile_cols = 1;
        tile.tile_rows = 2;
        tile.mi_col_starts[1] = 960;
        tile.mi_row_starts[1] = 272;
        tile.mi_row_starts[2] = 540;

        let starts = |pic: &OwnedStdAv1PictureInfo| {
            // SAFETY: both point at boxed arrays `pic` owns, alive for this call;
            // the row array is `MAX_TILE_ROWS + 1` long, the column one one shorter.
            unsafe {
                let info = &*pic.std().pTileInfo;
                (
                    std::slice::from_raw_parts(info.pMiRowStarts, 3).to_vec(),
                    std::slice::from_raw_parts(info.pMiColStarts, 2).to_vec(),
                )
            }
        };

        let pic = picture_info(&header, &sequence).expect("converts");
        assert_eq!(
            starts(&pic),
            (vec![0, 17, 33], vec![0, 60]),
            "64-px superblocks: 272 MI is superblock row 17, and 960 MI is column 60"
        );

        sequence.use_128x128_superblock = true;
        let pic = picture_info(&header, &sequence).expect("converts");
        assert_eq!(
            starts(&pic),
            (vec![0, 8, 16], vec![0, 30]),
            "the unit is the sequence's superblock, so 128-px halves every start"
        );
    }
}
