//! One AV1 [`AuPlanAv1`] into libva picture and tile buffers.
//!
//! `ref_frame_map` is by AV1 slot and holds `VASurfaceID`s. `ref_frame_idx` and
//! global motion are by reference name; `ref_frame_idx` holds slots. libva has
//! no per-reference dimensions: drivers recover them from each surface.
//! Rules: `va_dec_av1.h`, AV1 §7.11.3.3, libavcodec `vaapi_av1.c`.
//!
//! `apply_grain` is refused: synthesis needs a second surface this rung does
//! not allocate. The gate is per frame, not `film_grain_params_present`.
//! A published store replaces a missing reference with a live surface and
//! reports it in [`DecodePlanVaAv1::substituted_refs`]. Shown key frames keep
//! libavcodec's all-invalid map.
//!
//! Removals and assignment run before the tile walk and the grain gate so the
//! ledger stays aligned with [`Av1Planner::plan_au`](pf_bitstream::av1::Av1Planner::plan_au).
//! On refusal the caller binds nothing to the assigned slot.
//!
//! One tile-group OBU is one parameter buffer plus one data buffer of that
//! group's `tile_data`; `slice_data_offset` is relative to that data buffer.
//! Pin: `the_whole_vendored_vector_converts_and_the_indexings_hold`.

use std::ops::Range;

use pf_bitstream::av1::coded_cdef_sec_strength;
use pf_bitstream::av1::AuPlan as AuPlanAv1;
use pf_bitstream::av1::FrameType;
use pf_bitstream::av1::ParsedFrameHeader as ParsedFrameHeaderAv1;
use pf_bitstream::av1::ParsedSequenceHeader as ParsedSequenceHeaderAv1;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::av1::REFS_PER_FRAME;

use crate::va::VA_INVALID_SURFACE;
use crate::va::VA_SLICE_DATA_FLAG_ALL;
use crate::va_av1::FilmGrainInfoFieldsAV1;
use crate::va_av1::LoopFilterInfoFieldsAV1;
use crate::va_av1::LoopRestorationFieldsAV1;
use crate::va_av1::ModeControlFieldsAV1;
use crate::va_av1::PicInfoFieldsAV1;
use crate::va_av1::QmatrixFieldsAV1;
use crate::va_av1::SegmentInfoFieldsAV1;
use crate::va_av1::SeqInfoFieldsAV1;
use crate::va_av1::VaDecPictureParameterBufferAV1;
use crate::va_av1::VaSegmentationStructAV1;
use crate::va_av1::VaSliceParameterBufferAV1;
use crate::va_av1::VaWarpedMotionParamsAV1;
use crate::va_av1::ANCHOR_FRAME_UNUSED;
use crate::va_av1::CDEF_MAX;
use crate::va_av1::LAST_FRAME;
use crate::va_av1::SUPERRES_NUM;
use crate::va_av1::TILE_SBS_LEN;
use crate::SlotError;
use crate::SlotMap;
use pf_bitstream::av1::tiles::plan_bitstream;
use pf_bitstream::av1::tiles::Av1Bitstream;
use pf_bitstream::av1::tiles::Av1TileError;

/// libavcodec's ceiling: 256. Spec `MAX_TILE_COLS` × `MAX_TILE_ROWS` is 4096,
/// which no AV1 level defines.
pub const MAX_TILES: usize = 256;

/// AV1 `MAX_TILE_COLS` / `MAX_TILE_ROWS`, and the parser's `TileInfo` array bound.
pub const MAX_TILE_DIM: usize = 64;

/// One tile-group OBU: per-tile records plus the AU-relative `tile_data` range
/// they address. `slice_data_offset` is relative to the data buffer handed with
/// the parameter buffer, so they travel as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileGroupVa {
    pub tiles: Vec<VaSliceParameterBufferAV1>,
    pub data: Range<usize>,
}

#[derive(Debug, Clone)]
pub struct DecodePlanVaAv1 {
    pub pic_params: VaDecPictureParameterBufferAV1,
    pub tile_groups: Vec<TileGroupVa>,
    /// Ledger slot this picture took, or `None` when `refresh_frame_flags == 0`
    /// and the conversion released it immediately ([`plan_to_va_av1`]).
    pub setup_slot: Option<u8>,
    pub setup_id: PicId,
    /// Empty `ref_frame_map` entries replaced with a live surface — bit `i` for
    /// AV1 slot `i`. Non-zero means concealment; a clean stream must stay 0.
    pub substituted_refs: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToVaAv1Error {
    /// `show_existing_frame`: no submission. The caller displays a surface it
    /// already holds.
    NoDecode,
    NoTiles,
    Tiles(Av1TileError),
    /// Header grid disagrees with tiles the AU carried. A dropped tile group
    /// raises no other warning.
    TileCountMismatch {
        records: usize,
        walked: usize,
        grid: usize,
    },
    TooManyTiles {
        cols: u32,
        rows: u32,
    },
    TileOutsideGroup {
        tile: usize,
    },
    /// `apply_grain` needs a second surface this rung does not allocate (module docs).
    FilmGrain,
    CapacityMismatch {
        required: usize,
        capacity: usize,
    },
    /// Marked-store picture has no ledger slot, so `ref_frame_map` cannot name a surface.
    UnresolvedReference(PicId),
    SurfaceOutOfRange {
        slot: u8,
        surfaces: usize,
    },
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToVaAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToVaAv1Error::Slot(e)
    }
}

impl PlanToVaAv1Error {
    /// These five are a lost tile group. With an integrity warning they are
    /// damage (conceal); on a clean plan they are a defect. Other refusals stay
    /// refusals: concealing them would bury this rung's state.
    pub fn lost_tiles(&self) -> bool {
        matches!(
            self,
            PlanToVaAv1Error::NoTiles
                | PlanToVaAv1Error::Tiles(_)
                | PlanToVaAv1Error::TileCountMismatch { .. }
                | PlanToVaAv1Error::TooManyTiles { .. }
                | PlanToVaAv1Error::TileOutsideGroup { .. }
        )
    }
}

impl std::fmt::Display for PlanToVaAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToVaAv1Error::NoDecode => {
                write!(f, "a show_existing_frame access unit decodes nothing")
            }
            PlanToVaAv1Error::NoTiles => write!(f, "the access unit planned no tiles"),
            PlanToVaAv1Error::Tiles(e) => write!(f, "tile walk: {e}"),
            PlanToVaAv1Error::TileCountMismatch {
                records,
                walked,
                grid,
            } => write!(
                f,
                "{records} tile records and {walked} walked tiles for a {grid}-tile \
                 grid — a tile group was lost"
            ),
            PlanToVaAv1Error::TooManyTiles { cols, rows } => {
                write!(f, "a {cols}x{rows} tile grid is outside AV1's limits")
            }
            PlanToVaAv1Error::TileOutsideGroup { tile } => write!(
                f,
                "tile {tile}'s payload is not inside its tile group's data region"
            ),
            PlanToVaAv1Error::FilmGrain => write!(
                f,
                "this frame applies film grain, which needs a separate display \
                 surface this rung does not allocate"
            ),
            PlanToVaAv1Error::CapacityMismatch { required, capacity } => write!(
                f,
                "the slot map holds {capacity} slots, AV1 needs {required}"
            ),
            PlanToVaAv1Error::UnresolvedReference(id) => {
                write!(f, "picture {id} holds a reference slot but no surface")
            }
            PlanToVaAv1Error::SurfaceOutOfRange { slot, surfaces } => {
                write!(
                    f,
                    "ledger slot {slot} has no surface in a table of {surfaces}"
                )
            }
            PlanToVaAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its libva field")
            }
            PlanToVaAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToVaAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToVaAv1Error> {
    u8::try_from(value).map_err(|_| PlanToVaAv1Error::FieldOverflow { field, value })
}

fn narrow16(field: &'static str, value: u32) -> Result<u16, PlanToVaAv1Error> {
    u16::try_from(value).map_err(|_| PlanToVaAv1Error::FieldOverflow { field, value })
}

fn narrow32(field: &'static str, value: usize) -> Result<u32, PlanToVaAv1Error> {
    u32::try_from(value).map_err(|_| PlanToVaAv1Error::FieldOverflow {
        field,
        value: u32::MAX,
    })
}

/// Convert one planned AV1 frame: one helper per picture-parameter section,
/// assembled here.
///
/// `au` is the access unit `plan` was planned from: tile records need per-tile
/// byte ranges from each group's `tile_size_minus_1` walk, which the plan does
/// not store. Shared [`plan_bitstream`] with the Vulkan and DXVA rungs.
/// `surfaces` is ledger-slot → `VASurfaceID`; `setup_surface` is the decode
/// target, bound after activation so a slot this AU freed is free again.
///
/// `refresh_frame_flags == 0` never enters the planner's store. A slot is still
/// assigned and released here: a VAAPI slot is not a surface.
/// [`DecodePlanVaAv1::setup_slot`] is then `None` — nothing binds the surface;
/// only the pending-output claim holds it.
///
/// `slots` mutates before the tile walk and the grain gate, matching the
/// planner's already-stored picture. On refusal bind nothing to the assigned
/// slot: the previous binding is the wrong picture. Pre-mutation refusals
/// (`NoDecode`, capacity, surface range, unresolved ref, `RefPic::slot`) leave
/// `slots` untouched; bind-nothing is still correct.
pub fn plan_to_va_av1(
    plan: &AuPlanAv1,
    au: &[u8],
    slots: &mut SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<DecodePlanVaAv1, PlanToVaAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToVaAv1Error::NoDecode)?;
    let h = &*plan.header;
    let seq = &*plan.sequence;

    let required = NUM_REF_SLOTS + 1;
    if slots.capacity() != required {
        return Err(PlanToVaAv1Error::CapacityMismatch {
            required,
            capacity: slots.capacity(),
        });
    }
    // Caller's post-call bind of `setup_surface` is always in range.
    if surfaces.len() < slots.capacity() {
        return Err(PlanToVaAv1Error::SurfaceOutOfRange {
            slot: (slots.capacity() - 1) as u8,
            surfaces: surfaces.len(),
        });
    }

    let (ref_frame_map, substituted_refs) = refs(plan, slots, surfaces, setup_surface)?;

    // After reference resolve, before the tile walk: a later refusal must
    // leave the ledger in step with the planner. Resolve used the pre-removal
    // store; releasing first would drop a still-named picture.
    for &id in &plan.dpb.removed {
        if id == setup_id {
            continue;
        }
        let _ = slots.release(id);
    }
    let assigned = slots.assign(setup_id)?;
    // Never enters the store, so nothing asks the ledger for it again.
    let setup_slot = if h.refresh_frame_flags == 0 {
        slots.release(setup_id);
        None
    } else {
        Some(assigned)
    };

    // Refused, not approximated (module docs). After mutations so the GOP
    // stays in step.
    if seq.film_grain_params_present && h.film_grain_params.apply_grain {
        return Err(PlanToVaAv1Error::FilmGrain);
    }

    let tile_groups = tiles(au, plan)?;
    let seg_info = segmentation(h);
    let (cdef_y_strengths, cdef_uv_strengths) = cdef_strengths(h)?;
    let (width_in_sbs_minus_1, height_in_sbs_minus_1) = tile_sizes(h)?;
    let wm = global_motion(h);

    let t = &h.tile_info;
    let color = &seq.color_config;
    let mut pic_params = VaDecPictureParameterBufferAV1::zeroed();
    pic_params.profile = seq.seq_profile as u8;
    // Parser leaves -1 when `enable_order_hint` is 0; `as u8` is 255 (hints
    // 256 bits wide). Disabled case sends 0, matching libavcodec CBS.
    pic_params.order_hint_bits_minus_1 = if seq.enable_order_hint {
        narrow(
            "order_hint_bits_minus_1",
            u32::try_from(seq.order_hint_bits_minus_1).map_err(|_| {
                PlanToVaAv1Error::FieldOverflow {
                    field: "order_hint_bits_minus_1",
                    value: 0,
                }
            })?,
        )?
    } else {
        0
    };
    pic_params.bit_depth_idx = if color.high_bitdepth {
        if color.twelve_bit {
            2
        } else {
            1
        }
    } else {
        0
    };
    pic_params.matrix_coefficients = color.matrix_coefficients as u8;
    pic_params.seq_info_fields = seq_info_fields(seq);
    pic_params.current_frame = setup_surface;
    // Equal to `current_frame`: `apply_grain` is 0 on every frame that reaches here.
    pic_params.current_display_picture = setup_surface;
    pic_params.anchor_frames_num = 0;
    pic_params.anchor_frames_list = std::ptr::null_mut();
    // Upscaled width: libavcodec's `frame_width_minus_1`. AV1 5.9.8 reads
    // this into `UpscaledWidth` before superres divides it to `FrameWidth`.
    pic_params.frame_width_minus1 = narrow16(
        "frame_width_minus1",
        h.upscaled_width
            .checked_sub(1)
            .ok_or(PlanToVaAv1Error::FieldOverflow {
                field: "upscaled_width",
                value: 0,
            })?,
    )?;
    pic_params.frame_height_minus1 = narrow16(
        "frame_height_minus1",
        h.frame_height
            .checked_sub(1)
            .ok_or(PlanToVaAv1Error::FieldOverflow {
                field: "frame_height",
                value: 0,
            })?,
    )?;
    pic_params.ref_frame_map = ref_frame_map;
    // Header `ref_frame_idx[name]` verbatim — not `plan.refs`, where a lost
    // reference is a hole. Concealment is the substituted `ref_frame_map` slot.
    pic_params.ref_frame_idx = h.ref_frame_idx;
    pic_params.primary_ref_frame = narrow("primary_ref_frame", h.primary_ref_frame)?;
    pic_params.order_hint = narrow("order_hint", h.order_hint)?;
    pic_params.seg_info = seg_info;
    pic_params.tile_cols = narrow("tile_cols", t.tile_cols)?;
    pic_params.tile_rows = narrow("tile_rows", t.tile_rows)?;
    pic_params.width_in_sbs_minus_1 = width_in_sbs_minus_1;
    pic_params.height_in_sbs_minus_1 = height_in_sbs_minus_1;
    pic_params.context_update_tile_id =
        narrow16("context_update_tile_id", t.context_update_tile_id)?;
    pic_params.pic_info_fields = frame_flags(h);
    // Real denominator, or `SUPERRES_NUM` when superres is off. libva
    // documents 8 there and 9..=16 otherwise; 0 is outside the range.
    pic_params.superres_scale_denominator = if h.use_superres {
        narrow("superres_denom", h.superres_denom)?
    } else {
        SUPERRES_NUM
    };
    pic_params.interp_filter = h.interpolation_filter as u8;
    loop_filter(h, &mut pic_params);
    quantization(h, &mut pic_params)?;
    let c = &h.cdef_params;
    // Parser holds `CdefDamping` (coded + 3); libva wants the coded value.
    pic_params.cdef_damping_minus_3 =
        narrow("cdef_damping_minus_3", c.cdef_damping.saturating_sub(3))?;
    pic_params.cdef_bits = narrow("cdef_bits", c.cdef_bits)?;
    pic_params.cdef_y_strengths = cdef_y_strengths;
    pic_params.cdef_uv_strengths = cdef_uv_strengths;
    pic_params.loop_restoration_fields = loop_restoration(h);
    pic_params.wm = wm;
    // Zeroed: `apply_grain` is 0 here; libva documents that as set the rest
    // to zero and ignore.
    pic_params.film_grain_info.film_grain_info_fields = FilmGrainInfoFieldsAV1::default().pack();

    Ok(DecodePlanVaAv1 {
        pic_params,
        tile_groups,
        setup_slot,
        setup_id,
        substituted_refs,
    })
}

/// `ref_frame_map` by `RefPic::slot` (bitstream 0..8), not ledger slot: a
/// ledger index names the wrong reference after the first eviction. Returns
/// the map and the substituted-slot bits. Read-only on `slots`.
fn refs(
    plan: &AuPlanAv1,
    slots: &SlotMap,
    surfaces: &[u32],
    setup_surface: u32,
) -> Result<([u32; NUM_REF_SLOTS], u8), PlanToVaAv1Error> {
    let h = &*plan.header;
    let mut ref_frame_map = [VA_INVALID_SURFACE; NUM_REF_SLOTS];
    // Shown key frame: empty store. libavcodec writes `VA_INVALID_ID` for
    // every slot. `dpb_refs` is the store before this refresh; listing those
    // surfaces would disagree with that empty map.
    let publishes_store = !(h.frame_type == FrameType::KeyFrame && h.show_frame);
    if !publishes_store {
        return Ok((ref_frame_map, 0));
    }
    for r in &plan.dpb_refs {
        let ledger = slots
            .slot_of(r.id)
            .ok_or(PlanToVaAv1Error::UnresolvedReference(r.id))?;
        let surface =
            *surfaces
                .get(usize::from(ledger))
                .ok_or(PlanToVaAv1Error::SurfaceOutOfRange {
                    slot: ledger,
                    surfaces: surfaces.len(),
                })?;
        let slot = usize::from(r.slot);
        if slot >= NUM_REF_SLOTS {
            return Err(PlanToVaAv1Error::FieldOverflow {
                field: "RefPic::slot",
                value: u32::from(r.slot),
            });
        }
        ref_frame_map[slot] = surface;
    }

    // Empty slot → a live surface (`va_dec_av1.h`): planner-empty or a
    // refused conversion the caller left unbound. Prefer a resolved
    // reference; `setup_surface` is the fallback (it is about to be written).
    let mut substituted_refs = 0u8;
    let alternative = ref_frame_map
        .iter()
        .copied()
        .find(|&s| s != VA_INVALID_SURFACE)
        .unwrap_or(setup_surface);
    for (slot, entry) in ref_frame_map.iter_mut().enumerate() {
        if *entry == VA_INVALID_SURFACE {
            *entry = alternative;
            substituted_refs |= 1 << slot;
        }
    }
    Ok((ref_frame_map, substituted_refs))
}

/// One [`TileGroupVa`] per tile-group OBU, offsets rebased onto its data.
fn tiles(au: &[u8], plan: &AuPlanAv1) -> Result<Vec<TileGroupVa>, PlanToVaAv1Error> {
    let h = &*plan.header;
    // Empty/short tiles with a stored picture is a lost packet. Refuse here;
    // [`PlanToVaAv1Error::lost_tiles`] is how the caller tells damage from a defect.
    if plan.tiles.is_empty() {
        return Err(PlanToVaAv1Error::NoTiles);
    }
    let t = &h.tile_info;
    if t.tile_cols == 0 || t.tile_rows == 0 {
        return Err(PlanToVaAv1Error::NoTiles);
    }
    let grid = (t.tile_cols as usize).saturating_mul(t.tile_rows as usize);
    if t.tile_cols as usize > MAX_TILE_DIM || t.tile_rows as usize > MAX_TILE_DIM {
        return Err(PlanToVaAv1Error::TooManyTiles {
            cols: t.tile_cols,
            rows: t.tile_rows,
        });
    }
    if grid > MAX_TILES {
        return Err(PlanToVaAv1Error::Tiles(Av1TileError::TooManyTiles {
            tiles: grid,
        }));
    }
    let bitstream: Av1Bitstream =
        plan_bitstream(au, &plan.tiles, h).map_err(PlanToVaAv1Error::Tiles)?;

    let mut tile_groups: Vec<TileGroupVa> = Vec::with_capacity(plan.tiles.len());
    let mut walked = 0usize;
    // `plan_bitstream` emits one region per plan group, in order. `get`
    // rather than `[]`: a decode-thread panic is worse than a refusal.
    for (index, tg) in plan.tiles.iter().enumerate() {
        let region = bitstream
            .groups
            .get(index)
            .ok_or(PlanToVaAv1Error::TileCountMismatch {
                records: walked,
                walked: bitstream.tiles.len(),
                grid,
            })?
            .clone();
        // Malformed `tg_end < tg_start`: the walk refuses too, so saturate
        // rather than a second path.
        let count = tg.tg_end.saturating_sub(tg.tg_start).saturating_add(1);
        let mut records = Vec::with_capacity(count as usize);
        for step in 0..count {
            let tile_num = tg.tg_start.saturating_add(step);
            let payload = bitstream.tiles.get(walked).ok_or({
                PlanToVaAv1Error::TileCountMismatch {
                    records: walked,
                    walked: bitstream.tiles.len(),
                    grid,
                }
            })?;
            walked += 1;
            // Offset is relative to this group's `tile_data` buffer, not the
            // AU. Rebased here; this rung has nothing to rebase against later.
            if payload.start < region.start || payload.end > region.end {
                return Err(PlanToVaAv1Error::TileOutsideGroup { tile: walked - 1 });
            }
            records.push(VaSliceParameterBufferAV1 {
                slice_data_size: narrow32("slice_data_size", payload.end - payload.start)?,
                slice_data_offset: narrow32("slice_data_offset", payload.start - region.start)?,
                slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
                tile_row: narrow16("tile_row", tile_num / t.tile_cols)?,
                tile_column: narrow16("tile_column", tile_num % t.tile_cols)?,
                // `va_deprecated`; libavcodec fills both.
                tg_start: narrow16("tg_start", tg.tg_start)?,
                tg_end: narrow16("tg_end", tg.tg_end)?,
                anchor_frame_idx: ANCHOR_FRAME_UNUSED,
                tile_idx_in_tile_list: 0,
                va_reserved: [0; 4],
            });
        }
        tile_groups.push(TileGroupVa {
            tiles: records,
            data: region,
        });
    }
    // Header grid (`tile_cols` × `tile_rows`) vs tiles the AU carried. A
    // dropped tile group is silent; submitting would announce a short grid.
    if walked != grid || bitstream.tiles.len() != grid {
        return Err(PlanToVaAv1Error::TileCountMismatch {
            records: walked,
            walked: bitstream.tiles.len(),
            grid,
        });
    }
    Ok(tile_groups)
}

/// Coded tile sizes. Arrays are 63 long: the last tile size is derived.
/// libavcodec loops to `tile_cols` and overruns index 63 on a 64-column frame.
fn tile_sizes(
    h: &ParsedFrameHeaderAv1,
) -> Result<([u16; TILE_SBS_LEN], [u16; TILE_SBS_LEN]), PlanToVaAv1Error> {
    let t = &h.tile_info;
    let mut width_in_sbs_minus_1 = [0u16; TILE_SBS_LEN];
    let mut height_in_sbs_minus_1 = [0u16; TILE_SBS_LEN];
    for (out, coded) in width_in_sbs_minus_1
        .iter_mut()
        .zip(&t.width_in_sbs_minus_1[..(t.tile_cols as usize).min(TILE_SBS_LEN)])
    {
        *out = narrow16("width_in_sbs_minus_1", *coded)?;
    }
    for (out, coded) in height_in_sbs_minus_1
        .iter_mut()
        .zip(&t.height_in_sbs_minus_1[..(t.tile_rows as usize).min(TILE_SBS_LEN)])
    {
        *out = narrow16("height_in_sbs_minus_1", *coded)?;
    }
    Ok((width_in_sbs_minus_1, height_in_sbs_minus_1))
}

fn segmentation(h: &ParsedFrameHeaderAv1) -> VaSegmentationStructAV1 {
    let sp = &h.segmentation_params;
    let mut seg_info = VaSegmentationStructAV1::zeroed();
    seg_info.segment_info_fields = SegmentInfoFieldsAV1 {
        enabled: sp.segmentation_enabled,
        update_map: sp.segmentation_update_map,
        temporal_update: sp.segmentation_temporal_update,
        update_data: sp.segmentation_update_data,
    }
    .pack();
    for segment in 0..8 {
        let mut mask = 0u8;
        for (feature, enabled) in sp.feature_enabled[segment].iter().enumerate() {
            if *enabled {
                mask |= 1 << feature;
            }
        }
        seg_info.feature_mask[segment] = mask;
        // libva wants `FeatureData` after 5.9.14 Clip3; the parser already
        // clips on read.
        seg_info.feature_data[segment] = sp.feature_data[segment];
    }
    seg_info
}

/// Pack the CODED two-bit `sec`. AV1 5.9.19 rewrites a coded 3 to 4 in
/// place; masking the parser value with 3 turns the strongest secondary
/// filter into none. `coded_cdef_sec_strength` inverts that.
fn cdef_strengths(
    h: &ParsedFrameHeaderAv1,
) -> Result<([u8; CDEF_MAX], [u8; CDEF_MAX]), PlanToVaAv1Error> {
    let c = &h.cdef_params;
    let mut cdef_y_strengths = [0u8; CDEF_MAX];
    let mut cdef_uv_strengths = [0u8; CDEF_MAX];
    for i in 0..CDEF_MAX {
        let pri_y = narrow("cdef_y_pri_strength", c.cdef_y_pri_strength[i])?;
        let pri_uv = narrow("cdef_uv_pri_strength", c.cdef_uv_pri_strength[i])?;
        cdef_y_strengths[i] = (pri_y << 2) | coded_cdef_sec_strength(c.cdef_y_sec_strength[i]);
        cdef_uv_strengths[i] = (pri_uv << 2) | coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]);
    }
    Ok((cdef_y_strengths, cdef_uv_strengths))
}

fn global_motion(h: &ParsedFrameHeaderAv1) -> [VaWarpedMotionParamsAV1; REFS_PER_FRAME] {
    let gm = &h.global_motion_params;
    let mut wm = [VaWarpedMotionParamsAV1::zeroed(); REFS_PER_FRAME];
    for (name, entry) in wm.iter_mut().enumerate() {
        // By reference NAME, never DPB slot. Parser stores
        // `LAST_FRAME..ALTREF_FRAME`; libavcodec writes `wm[i - 1]`. By slot
        // agrees only while reference `i` sits in slot `i + 1`.
        let gm_name = LAST_FRAME + name;
        entry.wmtype = gm.gm_type[gm_name] as u32;
        // Six parameters, not eight: 5.9.24 codes six. `wmmat[6]`/`[7]` stay 0.
        entry.wmmat[..6].copy_from_slice(&gm.gm_params[gm_name]);
        // Parser `setup_shear` verdict; libva's flag is the inverse.
        entry.invalid = u8::from(!gm.warp_valid[gm_name]);
    }
    wm
}

fn seq_info_fields(seq: &ParsedSequenceHeaderAv1) -> u32 {
    let color = &seq.color_config;
    SeqInfoFieldsAV1 {
        still_picture: seq.still_picture,
        use_128x128_superblock: seq.use_128x128_superblock,
        enable_filter_intra: seq.enable_filter_intra,
        enable_intra_edge_filter: seq.enable_intra_edge_filter,
        enable_interintra_compound: seq.enable_interintra_compound,
        enable_masked_compound: seq.enable_masked_compound,
        enable_dual_filter: seq.enable_dual_filter,
        enable_order_hint: seq.enable_order_hint,
        enable_jnt_comp: seq.enable_jnt_comp,
        enable_cdef: seq.enable_cdef,
        mono_chrome: color.mono_chrome,
        color_range: color.color_range,
        subsampling_x: color.subsampling_x,
        subsampling_y: color.subsampling_y,
        chroma_sample_position: color.chroma_sample_position as u8,
        film_grain_params_present: seq.film_grain_params_present,
    }
    .pack()
}

fn frame_flags(h: &ParsedFrameHeaderAv1) -> u32 {
    PicInfoFieldsAV1 {
        frame_type: h.frame_type as u8,
        show_frame: h.show_frame,
        showable_frame: h.showable_frame,
        error_resilient_mode: h.error_resilient_mode,
        disable_cdf_update: h.disable_cdf_update,
        allow_screen_content_tools: h.allow_screen_content_tools != 0,
        force_integer_mv: h.force_integer_mv != 0,
        allow_intrabc: h.allow_intrabc,
        use_superres: h.use_superres,
        allow_high_precision_mv: h.allow_high_precision_mv,
        is_motion_mode_switchable: h.is_motion_mode_switchable,
        use_ref_frame_mvs: h.use_ref_frame_mvs,
        disable_frame_end_update_cdf: h.disable_frame_end_update_cdf,
        uniform_tile_spacing_flag: h.tile_info.uniform_tile_spacing_flag,
        allow_warped_motion: h.allow_warped_motion,
        large_scale_tile: false,
    }
    .pack()
}

fn loop_filter(h: &ParsedFrameHeaderAv1, pic_params: &mut VaDecPictureParameterBufferAV1) {
    let lf = &h.loop_filter_params;
    pic_params.filter_level = [lf.loop_filter_level[0], lf.loop_filter_level[1]];
    pic_params.filter_level_u = lf.loop_filter_level[2];
    pic_params.filter_level_v = lf.loop_filter_level[3];
    pic_params.loop_filter_info_fields = LoopFilterInfoFieldsAV1 {
        sharpness_level: lf.loop_filter_sharpness,
        mode_ref_delta_enabled: lf.loop_filter_delta_enabled,
        mode_ref_delta_update: lf.loop_filter_delta_update,
    }
    .pack();
    pic_params.ref_deltas = lf.loop_filter_ref_deltas;
    pic_params.mode_deltas = lf.loop_filter_mode_deltas;
}

/// Quantizer indices and matrices, plus the mode-control word that carries
/// the delta-q and delta-lf switches.
fn quantization(
    h: &ParsedFrameHeaderAv1,
    pic_params: &mut VaDecPictureParameterBufferAV1,
) -> Result<(), PlanToVaAv1Error> {
    let q = &h.quantization_params;
    let lf = &h.loop_filter_params;
    pic_params.base_qindex = narrow("base_qindex", q.base_q_idx)?;
    // `su(1+6)`: parser cannot emit outside -63..=63, so this cannot truncate.
    pic_params.y_dc_delta_q = q.delta_q_y_dc as i8;
    pic_params.u_dc_delta_q = q.delta_q_u_dc as i8;
    pic_params.u_ac_delta_q = q.delta_q_u_ac as i8;
    pic_params.v_dc_delta_q = q.delta_q_v_dc as i8;
    pic_params.v_ac_delta_q = q.delta_q_v_ac as i8;
    pic_params.qmatrix_fields = QmatrixFieldsAV1 {
        using_qmatrix: q.using_qmatrix,
        // No `0xFF` sentinel: libva has `using_qmatrix`, so unused matrices
        // are ignored.
        qm_y: narrow("qm_y", q.qm_y)?,
        qm_u: narrow("qm_u", q.qm_u)?,
        qm_v: narrow("qm_v", q.qm_v)?,
    }
    .pack();
    pic_params.mode_control_fields = ModeControlFieldsAV1 {
        delta_q_present_flag: q.delta_q_present,
        log2_delta_q_res: narrow("delta_q_res", q.delta_q_res)?,
        delta_lf_present_flag: lf.delta_lf_present,
        log2_delta_lf_res: lf.delta_lf_res,
        delta_lf_multi: lf.delta_lf_multi,
        tx_mode: h.tx_mode as u8,
        reference_select: h.reference_select,
        reduced_tx_set_used: h.reduced_tx_set,
        skip_mode_present: h.skip_mode_present,
    }
    .pack();
    Ok(())
}

fn loop_restoration(h: &ParsedFrameHeaderAv1) -> u16 {
    let lr = &h.loop_restoration_params;
    LoopRestorationFieldsAV1 {
        // Parser `FrameRestorationType` is the spec's; no remap.
        // [`LoopRestorationFieldsAV1`] documents libavcodec's coded-`lr_type` swap.
        yframe_restoration_type: lr.frame_restoration_type[0] as u8,
        cbframe_restoration_type: lr.frame_restoration_type[1] as u8,
        crframe_restoration_type: lr.frame_restoration_type[2] as u8,
        lr_unit_shift: lr.lr_unit_shift,
        lr_uv_shift: lr.lr_uv_shift,
    }
    .pack()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;
    use std::collections::HashMap;

    const AV1_25FPS: &[u8] = pf_bitstream::testing::AV1_25FPS;

    /// Recognisable per-slot values, so a wrong index is a wrong number.
    fn surface_table() -> Vec<u32> {
        (0..NUM_REF_SLOTS as u32 + 1).map(|i| 0x1000 + i).collect()
    }

    /// Rebuild the ledger-slot → surface table from the ledger (a released
    /// slot binds nothing), then bind the picture just converted. The next
    /// frame resolves through this table. `surface` is `None` when the slot
    /// was assigned but nothing decoded — the caller's half of [`plan_to_va_av1`].
    fn bind(
        slot_surface: &mut [u32],
        slots: &SlotMap,
        stored: Option<PicId>,
        surface: Option<u32>,
    ) {
        let live: std::collections::HashSet<u8> = slots.held().map(|(slot, _)| slot).collect();
        for (index, bound) in slot_surface.iter_mut().enumerate() {
            if !live.contains(&(index as u8)) {
                *bound = VA_INVALID_SURFACE;
            }
        }
        if let Some(slot) = stored.and_then(|id| slots.slot_of(id)) {
            slot_surface[usize::from(slot)] = surface.unwrap_or(VA_INVALID_SURFACE);
        }
    }

    /// Whole vendored vector, converted. Load-bearing indexings:
    /// `ref_frame_map` by AV1 slot (a ledger index would read a different
    /// surface) and `wm[]` by reference name (`gm_params[name + 1]`).
    #[test]
    fn the_whole_vendored_vector_converts_and_the_indexings_hold() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut slot_surface = vec![VA_INVALID_SURFACE; NUM_REF_SLOTS + 1];
        // Independent of the ledger: a wrong store index reads a surface this
        // map disagrees with.
        let mut surface_of: HashMap<u64, u32> = HashMap::new();

        let (mut frames, mut inter, mut warped, mut cdef_fixups) = (0u32, 0u32, 0u32, 0u32);
        let mut multi_ref_slot_pictures = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                let Some(id) = plan.dpb.stored else {
                    continue;
                };
                frames += 1;
                // Unique per picture, so a stale binding is a wild value.
                let setup_surface = 0x9000 + frames;
                // Snapshot before conversion: references resolve against the pre-removal table.
                let surfaces = slot_surface.clone();
                let va = plan_to_va_av1(&plan, packet, &mut slots, &surfaces, setup_surface)
                    .unwrap_or_else(|e| panic!("frame {frames}: {e}"));
                surface_of.insert(id, setup_surface);
                bind(&mut slot_surface, &slots, Some(id), Some(setup_surface));

                assert_eq!(va.pic_params.current_frame, setup_surface);
                assert_eq!(
                    va.pic_params.current_display_picture, setup_surface,
                    "no film grain means the display surface IS the decode target"
                );

                let h = &*plan.header;
                let shown_key = h.frame_type == FrameType::KeyFrame && h.show_frame;

                let mut expected = [VA_INVALID_SURFACE; NUM_REF_SLOTS];
                if !shown_key {
                    for r in &plan.dpb_refs {
                        expected[usize::from(r.slot)] = *surface_of
                            .get(&r.id)
                            .unwrap_or_else(|| panic!("frame {frames}: no surface for {}", r.id));
                    }
                }
                assert_eq!(
                    va.pic_params.ref_frame_map, expected,
                    "frame {frames}: the store must be indexed by AV1 slot and hold \
                     the surface each slot's picture decoded into"
                );
                assert_eq!(
                    va.substituted_refs, 0,
                    "frame {frames}: a stream that lost nothing must conceal nothing — \
                     a substitution here means the reference plumbing is inventing \
                     surfaces on a clean vector"
                );
                if shown_key {
                    assert!(
                        va.pic_params
                            .ref_frame_map
                            .iter()
                            .all(|&s| s == VA_INVALID_SURFACE),
                        "frame {frames}: a shown key frame publishes an empty store"
                    );
                }
                // One picture in several AV1 slots: one slot per picture cannot
                // tell slot from picture.
                let distinct_slots: std::collections::HashSet<u8> =
                    plan.dpb_refs.iter().map(|r| r.slot).collect();
                assert_eq!(
                    distinct_slots.len(),
                    plan.dpb_refs.len(),
                    "the marked store lists each slot once"
                );
                let distinct_ids: std::collections::HashSet<u64> =
                    plan.dpb_refs.iter().map(|r| r.id).collect();
                if distinct_ids.len() < plan.dpb_refs.len() {
                    multi_ref_slot_pictures += 1;
                }

                assert_eq!(
                    va.pic_params.ref_frame_idx, h.ref_frame_idx,
                    "frame {frames}: the name table is the header's own slot list"
                );
                if !h.frame_is_intra {
                    inter += 1;
                    for (name, r) in plan.refs.iter().enumerate() {
                        let r = r.expect("the clean vector loses no reference");
                        assert_eq!(
                            va.pic_params.ref_frame_idx[name], r.slot,
                            "frame {frames}: name {name} must carry its SLOT"
                        );
                        assert_eq!(
                            va.pic_params.ref_frame_map[usize::from(r.slot)],
                            surface_of[&r.id],
                            "frame {frames}: following name {name} through the slot \
                             table must reach that reference's own surface"
                        );
                    }
                }

                for name in 0..REFS_PER_FRAME {
                    let gm = &h.global_motion_params;
                    assert_eq!(
                        va.pic_params.wm[name].wmmat[..6],
                        gm.gm_params[name + 1],
                        "frame {frames}: wm[{name}] must be reference name \
                         {}'s warp, not slot {name}'s",
                        name + 1
                    );
                    assert_eq!(va.pic_params.wm[name].wmmat[6..], [0, 0]);
                    assert_eq!(va.pic_params.wm[name].wmtype, gm.gm_type[name + 1] as u32);
                    assert_eq!(
                        va.pic_params.wm[name].invalid,
                        u8::from(!gm.warp_valid[name + 1])
                    );
                    if va.pic_params.wm[name].wmtype != 0 {
                        warped += 1;
                    }
                }

                let grid = (h.tile_info.tile_cols * h.tile_info.tile_rows) as usize;
                let records: usize = va.tile_groups.iter().map(|g| g.tiles.len()).sum();
                assert_eq!(records, grid, "frame {frames}: one record per tile");
                for group in &va.tile_groups {
                    let region = &packet[group.data.clone()];
                    for tile in &group.tiles {
                        let start = tile.slice_data_offset as usize;
                        let end = start + tile.slice_data_size as usize;
                        assert!(
                            end <= region.len(),
                            "frame {frames}: a record runs past its data buffer"
                        );
                        // A tile payload, not the group's header (where the
                        // region starts and the payload does not).
                        assert!(
                            !region[start..end].is_empty(),
                            "frame {frames}: an empty tile"
                        );
                    }
                    // Payloads plus one `TileSizeBytes` per tile except the last
                    // — `tile_group_obu()` arithmetic, independent of the offsets.
                    let size_bytes = if grid > 1 {
                        h.tile_info.tile_size_bytes as usize
                    } else {
                        0
                    };
                    let payloads: usize =
                        group.tiles.iter().map(|t| t.slice_data_size as usize).sum();
                    assert_eq!(
                        payloads + (group.tiles.len() - 1) * size_bytes,
                        group.data.end - group.data.start,
                        "frame {frames}: the group's tiles and its size fields must \
                         account for the region exactly"
                    );
                    assert_eq!(
                        group.tiles[0].slice_data_offset, 0,
                        "the first tile's payload starts the tile_data region"
                    );
                }

                assert_eq!(
                    va.pic_params.frame_width_minus1 as u32,
                    h.upscaled_width - 1,
                    "frame {frames}: the width field is the UPSCALED width"
                );
                assert_eq!(va.pic_params.frame_height_minus1 as u32, h.frame_height - 1);
                assert_eq!(
                    va.pic_params.superres_scale_denominator, SUPERRES_NUM,
                    "this vector uses no superres, so the denominator is 8 — never 0"
                );
                assert_eq!(
                    va.pic_params.order_hint_bits_minus_1 as i32,
                    plan.sequence.order_hint_bits_minus_1,
                    "the vector enables order hints, so the field is the parser's"
                );
                let coded = 1usize << h.cdef_params.cdef_bits;
                for i in 0..coded {
                    let sec = va.pic_params.cdef_y_strengths[i] & 0x3;
                    let pri = va.pic_params.cdef_y_strengths[i] >> 2;
                    assert_eq!(pri as u32, h.cdef_params.cdef_y_pri_strength[i]);
                    if h.cdef_params.cdef_y_sec_strength[i] == 4 {
                        cdef_fixups += 1;
                        assert_eq!(
                            sec, 3,
                            "frame {frames}: the spec's in-place 4 is the coded 3, \
                             and masking it with 3 would send 0"
                        );
                    } else {
                        assert_eq!(sec as u32, h.cdef_params.cdef_y_sec_strength[i]);
                    }
                }
            }
        }

        assert_eq!(frames, 274, "every frame of the vector converted");
        assert!(
            inter > 0,
            "no inter frame: the name-table checks were vacuous"
        );
        assert!(
            multi_ref_slot_pictures > 0,
            "no picture ever occupied two reference slots at once, so nothing here \
             could tell an AV1-slot index from a per-picture one"
        );
        assert!(
            cdef_fixups > 0,
            "no frame coded a secondary strength needing the fixup, so the CDEF \
             packing above compared a correction against a stream that never needs it"
        );
        // This vector codes no global motion: `wm[]` proves the indexing
        // (`gm_params[name + 1]`) but every value is the identity warp.
        eprintln!(
            "frames {frames} · inter {inter} · non-identity warps {warped} \
             (0 means the warp VALUES are untested; the indexing is not) · \
             cdef fixups {cdef_fixups}"
        );
    }

    /// `show_existing_frame` has no submission — not a failure. Built by
    /// hand: the vendored vector never uses it. Exercises `dpb.stored == None`.
    #[test]
    fn a_show_existing_frame_plan_is_not_a_decode() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();
        let display_only = AuPlanAv1 {
            dpb: pf_bitstream::av1::DpbUpdate {
                stored: None,
                outputs: vec![1],
                removed: Vec::new(),
            },
            tiles: Vec::new(),
            ..key
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        assert_eq!(
            plan_to_va_av1(&display_only, first, &mut slots, &surface_table(), 7).err(),
            Some(PlanToVaAv1Error::NoDecode)
        );
        assert_eq!(slots.active(), 0, "a refusal must not touch the ledger");
    }

    /// Film grain is refused, not approximated (module docs). The gate sits
    /// after mutations so a grained mid-GOP frame does not leave the ledger
    /// one picture short of the planner.
    #[test]
    fn a_frame_that_applies_film_grain_is_refused() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();

        // Vector codes neither flag; both halves of the gate are set by hand.
        let mut seq = (*key.sequence).clone();
        seq.film_grain_params_present = true;
        let mut header = (*key.header).clone();
        header.film_grain_params.apply_grain = true;
        let grained = AuPlanAv1 {
            sequence: std::rc::Rc::new(seq.clone()),
            header: std::rc::Rc::new(header),
            ..key.clone()
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        assert_eq!(
            plan_to_va_av1(&grained, first, &mut slots, &surface_table(), 7).err(),
            Some(PlanToVaAv1Error::FilmGrain)
        );
        let stored = grained.dpb.stored.expect("the key frame is stored");
        assert_eq!(
            slots.slot_of(stored),
            Some(0),
            "the refusal must leave the ledger holding the picture the PLANNER stored \
             — the planner has no idea this rung said no, and every later frame that \
             names this picture resolves through this ledger"
        );

        // Sequence declares the tool; this frame does not apply it.
        let declared_only = AuPlanAv1 {
            sequence: std::rc::Rc::new(seq),
            ..key
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let va = plan_to_va_av1(&declared_only, first, &mut slots, &surface_table(), 7)
            .expect("a declared-but-unused film grain tool still decodes");
        assert_eq!(
            va.pic_params.film_grain_info,
            crate::va_av1::VaFilmGrainStructAV1::zeroed(),
            "apply_grain is 0, which libva documents as 'set the rest to zero'"
        );
        assert_eq!(
            va.pic_params.seq_info_fields & (1 << 15),
            1 << 15,
            "the sequence's declaration is still reported"
        );
    }

    /// `order_hint_bits_minus_1` must be 0, not 255, when order hints are off.
    /// Parser stores -1; `as u8` is 255. No vector here disables order hints.
    #[test]
    fn order_hints_off_sends_zero_not_the_parsers_minus_one() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();
        assert!(
            key.sequence.enable_order_hint,
            "the vector enables order hints; this test is about the other branch"
        );
        let mut seq = (*key.sequence).clone();
        seq.enable_order_hint = false;
        seq.order_hint_bits_minus_1 = -1;
        seq.order_hint_bits = 0;
        let plan = AuPlanAv1 {
            sequence: std::rc::Rc::new(seq),
            ..key
        };
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let va = plan_to_va_av1(&plan, first, &mut slots, &surface_table(), 7).expect("converts");
        assert_eq!(va.pic_params.order_hint_bits_minus_1, 0);
        assert_eq!(
            va.pic_params.seq_info_fields & (1 << 7),
            0,
            "and the sequence flag says so too"
        );
    }

    /// A frame that refreshes no slot gives its ledger slot straight back.
    /// Nine such frames would otherwise fill a nine-slot ledger (`SlotError::Full`)
    /// on a legal stream.
    #[test]
    fn a_frame_that_refreshes_nothing_returns_its_slot() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .clone();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let surfaces = surface_table();

        let va = plan_to_va_av1(&key, first, &mut slots, &surfaces, 7).expect("converts");
        assert_eq!(va.setup_slot, Some(0));
        assert_eq!(slots.active(), 1);

        let mut header = (*key.header).clone();
        header.refresh_frame_flags = 0;
        let ephemeral = AuPlanAv1 {
            header: std::rc::Rc::new(header),
            dpb: pf_bitstream::av1::DpbUpdate {
                stored: Some(999),
                outputs: vec![999],
                removed: Vec::new(),
            },
            ..key
        };
        let va = plan_to_va_av1(&ephemeral, first, &mut slots, &surfaces, 8).expect("converts");
        assert_eq!(
            va.setup_slot, None,
            "nothing binds the surface — only the pending output claims it"
        );
        assert_eq!(
            slots.active(),
            1,
            "the ledger is back where it was; a ninth such frame must still fit"
        );
        assert_eq!(slots.slot_of(999), None);
    }

    /// A ledger sized for another codec is refused rather than overflowed.
    #[test]
    fn a_ledger_of_the_wrong_capacity_is_refused() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("plans")
            .first()
            .expect("a frame")
            .clone();
        // Four-frame DPB cannot hold AV1's eight slots.
        let mut slots = SlotMap::new(4);
        assert_eq!(
            plan_to_va_av1(&key, first, &mut slots, &surface_table(), 7).err(),
            Some(PlanToVaAv1Error::CapacityMismatch {
                required: 9,
                capacity: 5
            })
        );
    }

    /// Short surface table is refused before the ledger is touched, so the
    /// caller's post-call bind is always in range.
    #[test]
    fn a_short_surface_table_is_refused_before_any_mutation() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let key = planner
            .plan_au(first)
            .expect("plans")
            .first()
            .expect("a frame")
            .clone();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let short = vec![0u32; NUM_REF_SLOTS];
        assert!(matches!(
            plan_to_va_av1(&key, first, &mut slots, &short, 7).err(),
            Some(PlanToVaAv1Error::SurfaceOutOfRange { .. })
        ));
        assert_eq!(slots.active(), 0);
    }

    /// Truncated AU: tile groups missing, picture still stored. Refuse, and
    /// keep the ledger in step with the planner. Skipping `slots.assign` here
    /// leaves the two one picture apart; the next frame that names it
    /// hard-errors `UnresolvedReference` and never repairs.
    #[test]
    fn a_truncated_access_unit_refuses_but_leaves_the_ledger_in_step() {
        let packets: Vec<&[u8]> = IvfIterator::new(AV1_25FPS).take(3).collect();
        assert_eq!(packets.len(), 3, "the vector has at least three packets");
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut slot_surface = vec![VA_INVALID_SURFACE; NUM_REF_SLOTS + 1];

        let key = planner.plan_au(packets[0]).expect("plans").remove(0);
        let key_id = key.dpb.stored.expect("the key frame decodes");
        plan_to_va_av1(&key, packets[0], &mut slots, &slot_surface.clone(), 0x9001)
            .expect("the key frame converts");
        bind(&mut slot_surface, &slots, Some(key_id), Some(0x9001));

        // Packet 1: hidden ALTREF then shown frame. Losing the hidden one is
        // silent: nothing displays it.
        let mut unit = planner.plan_au(packets[1]).expect("plans");
        assert_eq!(
            unit.len(),
            2,
            "packet 1 is a hidden ALTREF plus a shown frame"
        );
        let shown = unit.remove(1);
        let mut lost = unit.remove(0);
        let lost_id = lost.dpb.stored.expect("a truncated frame is still STORED");
        assert!(
            !lost.header.show_frame,
            "the frame this test loses is the hidden one"
        );
        assert!(
            !lost.tiles.is_empty(),
            "the vector's own packet carries tiles; this test takes them away"
        );
        lost.tiles.clear();
        lost.warnings
            .push(pf_bitstream::av1::PlanWarning::TruncatedAu { offset: 0 });
        assert!(
            lost.warnings
                .iter()
                .any(crate::PlanWarningAv1::is_integrity),
            "the plan this test drives must be one the rung CONCEALS"
        );

        let refusal = plan_to_va_av1(&lost, packets[1], &mut slots, &slot_surface.clone(), 0x9002)
            .expect_err("no tiles, nothing to submit");
        assert_eq!(refusal, PlanToVaAv1Error::NoTiles);
        assert!(
            refusal.lost_tiles(),
            "the caller tells damage from a defect through this predicate; a refusal \
             it does not recognise is a hard error and demotes the rung"
        );
        let ledger_slot = slots.slot_of(lost_id).expect(
            "THE REGRESSION: the planner stored this picture, so the ledger must hold \
             it too — without this every later reference to it is a hard Err that \
             never repairs",
        );
        // Slot is live; bind nothing — nothing was decoded.
        bind(&mut slot_surface, &slots, Some(lost_id), None);
        assert_eq!(
            slot_surface[usize::from(ledger_slot)],
            VA_INVALID_SURFACE,
            "a picture that never decoded must not inherit the surface of whatever \
             held its ledger slot before"
        );

        let mut substituted_somewhere = false;
        for (plan, packet, surface) in [
            (&shown, packets[1], 0x9003u32),
            (
                &planner.plan_au(packets[2]).expect("plans").remove(0),
                packets[2],
                0x9004,
            ),
        ] {
            let id = plan.dpb.stored.expect("decodes");
            assert!(
                plan.dpb_refs.iter().any(|r| r.id == lost_id),
                "this frame must reference the truncated picture or it proves nothing"
            );
            let va = plan_to_va_av1(plan, packet, &mut slots, &slot_surface.clone(), surface)
                .expect("a frame after a lost one converts — it does not hard-error");

            // Lost picture's slots get a live decoded surface, never `VA_INVALID_SURFACE`.
            for r in &plan.dpb_refs {
                let bit = 1u8 << r.slot;
                if r.id == lost_id {
                    substituted_somewhere = true;
                    assert_eq!(
                        va.substituted_refs & bit,
                        bit,
                        "slot {} holds a picture with no surface and must be reported \
                         substituted",
                        r.slot
                    );
                    assert_eq!(
                        va.pic_params.ref_frame_map[usize::from(r.slot)],
                        0x9001,
                        "and it must point at a picture that DECODED — the key \
                         frame's surface — rather than at nothing"
                    );
                } else {
                    assert_eq!(
                        va.substituted_refs & bit,
                        0,
                        "slot {} resolved; substituting it would hide a real reference",
                        r.slot
                    );
                }
            }
            bind(&mut slot_surface, &slots, Some(id), Some(surface));
        }
        assert!(
            substituted_somewhere,
            "no frame ever named the lost picture, so nothing above was checked"
        );
    }

    /// A store that resolved nothing still submits live surfaces. The decode
    /// target is the `va_dec_av1.h` alternative; drivers do not validate
    /// reference ids, so `VA_INVALID_SURFACE` must not reach them.
    #[test]
    fn a_store_with_no_surfaces_at_all_falls_back_to_the_decode_target() {
        let packets: Vec<&[u8]> = IvfIterator::new(AV1_25FPS).take(2).collect();
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);

        let key = planner.plan_au(packets[0]).expect("plans").remove(0);
        let key_id = key.dpb.stored.expect("decodes");
        plan_to_va_av1(&key, packets[0], &mut slots, &surface_table(), 0x9001).expect("converts");
        assert!(slots.slot_of(key_id).is_some());

        // Key frame holds every slot; caller bound none of them.
        let unbound = vec![VA_INVALID_SURFACE; NUM_REF_SLOTS + 1];
        let next = planner.plan_au(packets[1]).expect("plans").remove(0);
        assert_eq!(next.dpb_refs.len(), NUM_REF_SLOTS, "a full store");
        let va = plan_to_va_av1(&next, packets[1], &mut slots, &unbound, 0x9002).expect("converts");
        assert_eq!(
            va.substituted_refs, 0xff,
            "every slot of the store was concealed"
        );
        assert_eq!(
            va.pic_params.ref_frame_map, [0x9002; NUM_REF_SLOTS],
            "with nothing else live, the decode target is the substitute"
        );

        // Shown key frame: libavcodec publishes an all-invalid map; leave it.
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let va = plan_to_va_av1(&key, packets[0], &mut slots, &unbound, 0x9001).expect("converts");
        assert_eq!(va.substituted_refs, 0);
        assert_eq!(va.pic_params.ref_frame_map, [VA_INVALID_SURFACE; 8]);
    }
}
