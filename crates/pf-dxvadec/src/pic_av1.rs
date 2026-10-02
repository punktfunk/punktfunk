//! One AV1 [`AuPlan`] into the DXVA picture-parameter and tile-control records
//! `SubmitDecoderBuffers` takes — [`crate::dxva_av1`] layouts, [`crate::pic`] one
//! codec over, twin of `pf_vkdecode::pic_av1`.
//!
//! # Two arrays, two indices
//!
//! `frame_refs[7]` is indexed by reference NAME (`LAST`..`ALTREF`). Each entry
//! holds the AV1 reference SLOT (`ref_frame_idx[name]`), that reference's coded
//! size, and that reference's global motion. `RefFrameMapTextureIndex[8]` is
//! indexed by that same slot and holds the surface — the whole store, the way
//! `RefFrameList` does for H.264/H.265. The driver looks up one through the
//! other, so `Index` must be the slot, never the surface.
//!
//! Global motion is also by NAME: `gm_params[LAST_FRAME + name]`. Reading it by
//! DPB slot transposes every warp the moment reference `i` is not in slot `i+1`.
//!
//! Pin: `the_whole_vendored_vector_converts`,
//! `the_decode_target_never_aliases_a_surface_the_submission_names`.

use pf_bitstream::av1::coded_cdef_sec_strength;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::FrameType;
use pf_bitstream::av1::ParsedFrameHeader;
use pf_bitstream::av1::ParsedSequenceHeader;
use pf_bitstream::av1::PicId;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::av1::REFS_PER_FRAME;

use crate::dxva_av1::CdefAv1;
use crate::dxva_av1::CdefFlagsAv1;
use crate::dxva_av1::CdefStrength;
use crate::dxva_av1::CodingFlagsAv1;
use crate::dxva_av1::FilmGrainAv1;
use crate::dxva_av1::FilmGrainFlagsAv1;
use crate::dxva_av1::FormatFlagsAv1;
use crate::dxva_av1::GlobalMotionFlags;
use crate::dxva_av1::LoopFilterAv1;
use crate::dxva_av1::LoopFilterFlagsAv1;
use crate::dxva_av1::PicEntryAv1;
use crate::dxva_av1::PicParamsAv1;
use crate::dxva_av1::QuantizationAv1;
use crate::dxva_av1::QuantizationFlagsAv1;
use crate::dxva_av1::SegmentFeatureMask;
use crate::dxva_av1::SegmentationAv1;
use crate::dxva_av1::SegmentationFlagsAv1;
use crate::dxva_av1::TileAv1;
use crate::dxva_av1::TilesAv1;
use crate::dxva_av1::UNUSED_INDEX;
use crate::plan_bitstream;
use crate::Av1Bitstream;
use crate::Av1TileError;
use crate::SlotError;
use crate::SlotMap;

/// DXVA `widths`/`heights` array length, not the spec's tile ceiling.
pub const MAX_TILE_DIM: usize = 64;

/// libavcodec's `MAX_TILES`: `dxva2_av1.c` has a 256-entry `tiles` array and
/// refuses past it (`AVERROR(ENOSYS)`). [`MAX_TILE_DIM`] admits 4096, which no
/// AV1 level defines.
pub const MAX_TILES: usize = 256;

/// Off-frame `log2_restoration_unit_size`: libavcodec's 8, top of `dxva.h`'s
/// 6..8 range. Parser leaves `[0,0,0]` when restoration is NONE;
/// `0u16.trailing_zeros()` is 16.
const LOG2_RESTORATION_UNIT_SIZE_UNUSED: u16 = 8;

/// Unused quantiser-matrix index. The DXVA block has no `using_qmatrix` flag;
/// 0 selects matrix 0. libavcodec/`dxva.h` unused value is `0xFF`.
const QM_UNUSED: u8 = 0xFF;

/// AV1 spec `LAST_FRAME`: first reference NAME, and the offset from a
/// `ref_frame_idx` position into per-reference arrays. `INTRA_FRAME` is 0.
const LAST_FRAME: usize = 1;

#[derive(Debug, Clone)]
pub struct DecodePlanDxvaAv1 {
    pub pic_params: PicParamsAv1,
    /// One record per **tile**, not per tile group, in `tg_start..=tg_end`
    /// order. `row`/`column`/`anchor_frame` are final; `DataOffset`/`DataSize`
    /// are access-unit-relative until [`mod@crate::pack_av1`] rebases them.
    pub tiles: Vec<TileAv1>,
    /// Tile and tile-group ranges in the access unit — what the packer copies
    /// and rebases against.
    pub bitstream: Av1Bitstream,
    pub setup_slot: u8,
    pub setup_id: PicId,
    /// Pictures this frame's `refresh_frame_flags` displace while this
    /// submission still names them. Release after the decode op, never here:
    /// AV1 applies the refresh after decode (7.20), and [`SlotMap::assign`]
    /// would recycle the vacated surface into [`Self::setup_slot`].
    ///
    /// Stricter than Vulkan: `RefFrameMapTextureIndex` declares the whole
    /// store, so every still-named picture must survive, not just the seven
    /// this frame reads. Same contract as
    /// [`crate::pic::DecodePlanDxva::release_after_decode`] and
    /// `pf_vkdecode::pic_av1::DecodePlanVkAv1::release_after_decode`.
    pub release_after_decode: Vec<PicId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanToDxvaAv1Error {
    /// A `show_existing_frame` plan decodes nothing and has no submission.
    NoDecode,
    NoTiles,
    Tiles(Av1TileError),
    /// Header grid disagrees with tiles the access unit carried — a dropped
    /// tile group, which nothing else reports. Submitting would announce
    /// `cols * rows` over a shorter buffer.
    TileCountMismatch {
        /// Control records from the plan's tile-group spans.
        records: usize,
        /// Payloads the bitstream walk found.
        walked: usize,
        /// `tile_cols * tile_rows` — what picture parameters announce.
        grid: usize,
    },
    UnresolvedReference(PicId),
    TooManyTiles {
        cols: u32,
        rows: u32,
    },
    /// Wider than the DXVA field type.
    FieldOverflow {
        field: &'static str,
        value: u32,
    },
    Slot(SlotError),
}

impl From<SlotError> for PlanToDxvaAv1Error {
    fn from(e: SlotError) -> Self {
        PlanToDxvaAv1Error::Slot(e)
    }
}

impl std::fmt::Display for PlanToDxvaAv1Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanToDxvaAv1Error::NoDecode => {
                write!(f, "a show_existing_frame plan has no decode submission")
            }
            PlanToDxvaAv1Error::NoTiles => write!(f, "the frame carried no tile group"),
            PlanToDxvaAv1Error::Tiles(e) => write!(f, "tile walk: {e}"),
            PlanToDxvaAv1Error::TileCountMismatch {
                records,
                walked,
                grid,
            } => write!(
                f,
                "the frame header's tile grid is {grid} tiles; the access unit carried \
                 {walked} and produced {records} records"
            ),
            PlanToDxvaAv1Error::UnresolvedReference(id) => {
                write!(f, "reference picture {id} holds no DPB slot")
            }
            PlanToDxvaAv1Error::TooManyTiles { cols, rows } => write!(
                f,
                "{cols}x{rows} tiles exceed the {MAX_TILE_DIM}-entry picture parameters"
            ),
            PlanToDxvaAv1Error::FieldOverflow { field, value } => {
                write!(f, "{field} = {value} does not fit its DXVA field")
            }
            PlanToDxvaAv1Error::Slot(e) => write!(f, "DPB slot map: {e:?}"),
        }
    }
}

impl std::error::Error for PlanToDxvaAv1Error {}

fn narrow(field: &'static str, value: u32) -> Result<u8, PlanToDxvaAv1Error> {
    u8::try_from(value).map_err(|_| PlanToDxvaAv1Error::FieldOverflow { field, value })
}

fn narrow16(field: &'static str, value: u32) -> Result<u16, PlanToDxvaAv1Error> {
    u16::try_from(value).map_err(|_| PlanToDxvaAv1Error::FieldOverflow { field, value })
}

/// Convert one planned AV1 frame: one helper per picture-parameter section,
/// assembled here.
///
/// `au` is the access unit `plan` was planned from: tile-control records need
/// per-tile byte ranges from each group's `tile_size_minus_1` walk, which the
/// plan does not store. No `status_id`: `StatusReportFeedbackNumber` stays
/// zero (fn body). Nothing mutates `slots` until every fallible step has passed.
pub fn plan_to_dxva_av1(
    au: &[u8],
    plan: &AuPlan,
    slots: &mut SlotMap,
) -> Result<DecodePlanDxvaAv1, PlanToDxvaAv1Error> {
    let setup_id = plan.dpb.stored.ok_or(PlanToDxvaAv1Error::NoDecode)?;
    if plan.tiles.is_empty() {
        return Err(PlanToDxvaAv1Error::NoTiles);
    }
    let h = &*plan.header;
    let seq = &*plan.sequence;

    let (ref_frame_map, frame_refs) = refs(plan, slots)?;
    let (tiles, tile_records, bitstream) = tiles(au, plan)?;
    let loop_filter = loop_filter(h);
    let quantization = quantization(h)?;
    let cdef = cdef(h)?;
    let segmentation = segmentation(h);
    let film_grain = film_grain(h, seq)?;

    // Hold every removal; release nothing here. A named picture this refresh
    // displaces is still in `ref_frame_map`; freeing it hands the surface to
    // `setup_slot`. `dpb.removed` is a subset of that store (planner snapshot
    // before mutation); `setup_id` cannot also be a displaced id.
    let release_after_decode: Vec<PicId> = plan
        .dpb
        .removed
        .iter()
        .copied()
        .filter(|id| *id != setup_id)
        .collect();
    let setup_slot = match slots.slot_of(setup_id) {
        Some(existing) => existing,
        None => slots.assign(setup_id)?,
    };

    let color = &seq.color_config;
    let mut pic_params = PicParamsAv1::zeroed();
    // Upscaled width. libavcodec sends coded `FrameWidth` (`avctx->width`).
    // Equal when superres is off (7.20), which is every stream here; revisit
    // with a superres vector, not by reading.
    pic_params.width = h.upscaled_width;
    pic_params.height = h.frame_height;
    pic_params.max_width = u32::from(seq.max_frame_width_minus_1) + 1;
    pic_params.max_height = u32::from(seq.max_frame_height_minus_1) + 1;
    pic_params.curr_pic_texture_index = setup_slot;
    // Real denominator, not the coded one; `SUPERRES_NUM` when superres is off.
    pic_params.superres_denom = if h.use_superres {
        narrow("superres_denom", h.superres_denom)?
    } else {
        SUPERRES_NUM
    };
    pic_params.bitdepth = if color.high_bitdepth {
        if color.twelve_bit {
            12
        } else {
            10
        }
    } else {
        8
    };
    pic_params.seq_profile = seq.seq_profile as u8;
    pic_params.tiles = tiles;
    (pic_params.coding, pic_params.format) = frame_flags(h, seq);
    pic_params.primary_ref_frame = narrow("primary_ref_frame", h.primary_ref_frame)?;
    pic_params.order_hint = narrow("order_hint", h.order_hint)?;
    pic_params.order_hint_bits = if seq.enable_order_hint {
        // Parser types this signed; a negative would be a parse bug, and
        // wrapping it unsigned here would hide that.
        narrow(
            "order_hint_bits",
            u32::try_from(seq.order_hint_bits_minus_1).map_err(|_| {
                PlanToDxvaAv1Error::FieldOverflow {
                    field: "order_hint_bits_minus_1",
                    value: 0,
                }
            })? + 1,
        )?
    } else {
        0
    };
    pic_params.frame_refs = frame_refs;
    pic_params.ref_frame_map_texture_index = ref_frame_map;
    pic_params.loop_filter = loop_filter;
    pic_params.quantization = quantization;
    pic_params.cdef = cdef;
    pic_params.interp_filter = h.interpolation_filter as u8;
    pic_params.segmentation = segmentation;
    pic_params.film_grain = film_grain;
    // Leave zero. libavcodec comments the assignment out for AV1 ("breaks
    // decoding on some drivers"); Chromium ships zero too ("crashes"). This
    // rung does not even take a number (fn docs).

    Ok(DecodePlanDxvaAv1 {
        pic_params,
        tiles: tile_records,
        bitstream,
        setup_slot,
        setup_id,
        release_after_decode,
    })
}

/// `RefFrameMapTextureIndex` by SLOT and `frame_refs` by NAME (module docs).
/// Read-only on `slots`.
fn refs(
    plan: &AuPlan,
    slots: &SlotMap,
) -> Result<([u8; NUM_REF_SLOTS], [PicEntryAv1; REFS_PER_FRAME]), PlanToDxvaAv1Error> {
    let h = &*plan.header;
    // `RefFrameMapTextureIndex` is the whole store by SLOT — same job as
    // `RefFrameList`; an LTR no slice names still has to appear.
    // A store picture this map never assigned (a rebuilt session, whose planner still holds
    // the old store) stays unused, as in H.264 and HEVC: failing here would refuse even the
    // key frame that ends it. A named reference without a surface is still refused below.
    let mut ref_frame_map = [UNUSED_INDEX; NUM_REF_SLOTS];
    for r in &plan.dpb_refs {
        match slots.slot_of(r.id) {
            Some(slot) => ref_frame_map[usize::from(r.slot)] = slot,
            None => tracing::trace!(id = r.id, "a stored AV1 picture holds no slot in this map"),
        }
    }

    // Seven reference NAMES: slot, coded size, global motion (module docs).
    // `plan.refs` is indexed by NAME; a hole keeps `UNUSED_INDEX`. Compacting
    // first renamed every name after the first loss.
    let mut frame_refs = [PicEntryAv1::zeroed(); REFS_PER_FRAME];
    let inter = !matches!(
        h.frame_type,
        FrameType::KeyFrame | FrameType::IntraOnlyFrame
    );
    if inter {
        for (name, r) in plan.refs.iter().enumerate() {
            let Some(r) = r else { continue };
            // Ledger must hold a surface, or `ref_frame_map` named nothing at
            // `r.slot` and the driver follows `Index` to an empty entry.
            slots
                .slot_of(r.id)
                .ok_or(PlanToDxvaAv1Error::UnresolvedReference(r.id))?;
            // Global motion is by reference NAME, never DPB slot.
            // `gm_params[LAST_FRAME + name]`; by slot agrees only while
            // reference `i` sits in slot `i + 1`.
            let gm_name = LAST_FRAME + name;
            let gm = &h.global_motion_params;
            frame_refs[name] = PicEntryAv1 {
                // The REFERENCE's own size, never this frame's. AV1 lets each
                // frame pick a size; the driver scales from `RefUpscaledWidth`
                // (7.11.3.3). Current-frame size makes every scaled pred unscaled.
                width: r.state.upscaled_width,
                height: r.state.frame_height,
                wmmat: gm.gm_params[gm_name],
                global_motion_flags: GlobalMotionFlags {
                    // DXVA `wminvalid` is the inverse of the parser's
                    // `setup_shear` verdict.
                    wminvalid: !gm.warp_valid[gm_name],
                    wmtype: gm.gm_type[gm_name] as u8,
                }
                .pack(),
                // AV1 reference SLOT (`ref_frame_idx[name]`, 0..8), not the
                // surface. `Index` subscripts `RefFrameMapTextureIndex`.
                index: r.slot,
                reserved16: 0,
            };
        }
    }
    Ok((ref_frame_map, frame_refs))
}

/// The tile grid, one control record per TILE, and the walked byte ranges.
fn tiles(
    au: &[u8],
    plan: &AuPlan,
) -> Result<(TilesAv1, Vec<TileAv1>, Av1Bitstream), PlanToDxvaAv1Error> {
    let h = &*plan.header;
    let t = &h.tile_info;
    if t.tile_cols as usize > MAX_TILE_DIM || t.tile_rows as usize > MAX_TILE_DIM {
        return Err(PlanToDxvaAv1Error::TooManyTiles {
            cols: t.tile_cols,
            rows: t.tile_rows,
        });
    }
    let mut tiles = TilesAv1::zeroed();
    tiles.cols = narrow("tiles.cols", t.tile_cols)?;
    tiles.rows = narrow("tiles.rows", t.tile_rows)?;
    tiles.context_update_id = t.context_update_tile_id as u16;
    // Superblock COUNTS; the syntax codes `*_in_sbs_minus_1`. The `+ 1` is
    // the conversion: a minus-one value understates every tile by one SB.
    for i in 0..t.tile_cols as usize {
        tiles.widths[i] = narrow16("tiles.widths", t.width_in_sbs_minus_1[i].saturating_add(1))?;
    }
    for i in 0..t.tile_rows as usize {
        tiles.heights[i] = narrow16(
            "tiles.heights",
            t.height_in_sbs_minus_1[i].saturating_add(1),
        )?;
    }

    // One record per TILE, not per tile group: a group of four tiles is four
    // `row`/`column` pairs. Bytes from `plan_bitstream`; numbering from the
    // plan's `tg_start`/`tg_end`. Cross-check against the header GRID, not
    // between those two — they share one expression. A dropped group is silent.
    let cols = t.tile_cols.max(1);
    let rows = t.tile_rows.max(1);
    let grid = (cols as usize).saturating_mul(rows as usize);
    if grid > MAX_TILES {
        return Err(PlanToDxvaAv1Error::Tiles(Av1TileError::TooManyTiles {
            tiles: grid,
        }));
    }
    let bitstream = plan_bitstream(au, &plan.tiles, h).map_err(PlanToDxvaAv1Error::Tiles)?;
    let mut tile_records = Vec::with_capacity(bitstream.tiles.len());
    for tg in &plan.tiles {
        // Malformed `tg_end < tg_start`: the walk refuses too, so saturate
        // rather than grow a second refusal path.
        let count = tg.tg_end.saturating_sub(tg.tg_start).saturating_add(1);
        for step in 0..count {
            let tile_num = tg.tg_start.saturating_add(step);
            tile_records.push(TileAv1 {
                // Access-unit coordinates; `pack_av1` replaces both with
                // buffer-relative ones (field docs).
                data_offset: 0,
                data_size: 0,
                row: (tile_num / cols) as u16,
                column: (tile_num % cols) as u16,
                reserved16: 0,
                // Large-scale-tile reference; unused here. libavcodec writes
                // `0xFF` on every tile.
                anchor_frame: UNUSED_INDEX,
                reserved8: 0,
            });
        }
    }
    if tile_records.len() != grid || bitstream.tiles.len() != grid {
        return Err(PlanToDxvaAv1Error::TileCountMismatch {
            records: tile_records.len(),
            walked: bitstream.tiles.len(),
            grid,
        });
    }
    for (record, tile) in tile_records.iter_mut().zip(&bitstream.tiles) {
        record.data_offset =
            u32::try_from(tile.start).map_err(|_| PlanToDxvaAv1Error::FieldOverflow {
                field: "tile.DataOffset",
                value: u32::MAX,
            })?;
        record.data_size = u32::try_from(tile.end - tile.start).map_err(|_| {
            PlanToDxvaAv1Error::FieldOverflow {
                field: "tile.DataSize",
                value: u32::MAX,
            }
        })?;
    }
    Ok((tiles, tile_records, bitstream))
}

/// Loop filter, plus the loop-restoration fields DXVA keeps in the same block.
fn loop_filter(h: &ParsedFrameHeader) -> LoopFilterAv1 {
    let lf = &h.loop_filter_params;
    let mut loop_filter = LoopFilterAv1::zeroed();
    loop_filter.filter_level = [lf.loop_filter_level[0], lf.loop_filter_level[1]];
    loop_filter.filter_level_u = lf.loop_filter_level[2];
    loop_filter.filter_level_v = lf.loop_filter_level[3];
    loop_filter.sharpness_level = lf.loop_filter_sharpness;
    loop_filter.control_flags = LoopFilterFlagsAv1 {
        mode_ref_delta_enabled: lf.loop_filter_delta_enabled,
        mode_ref_delta_update: lf.loop_filter_delta_update,
        delta_lf_multi: lf.delta_lf_multi,
        delta_lf_present: lf.delta_lf_present,
    }
    .pack();
    loop_filter.ref_deltas = lf.loop_filter_ref_deltas;
    loop_filter.mode_deltas = lf.loop_filter_mode_deltas;
    loop_filter.delta_lf_res = lf.delta_lf_res;
    // DXVA wants log2 of the unit size. Parser records the size itself, and
    // only inside `UsesLr` (5.9.20) — off is `[0,0,0]`, `trailing_zeros` 16.
    // libavcodec sends 8 when off (`dxva.h` range 6..8). On, trailing_zeros
    // already is `6 + lr_unit_shift - lr_uv_shift`.
    let lr = &h.loop_restoration_params;
    for i in 0..3 {
        loop_filter.frame_restoration_type[i] = lr.frame_restoration_type[i] as u8;
        loop_filter.log2_restoration_unit_size[i] = if lr.uses_lr {
            lr.loop_restoration_size[i].trailing_zeros() as u16
        } else {
            LOG2_RESTORATION_UNIT_SIZE_UNUSED
        };
    }
    loop_filter
}

fn quantization(h: &ParsedFrameHeader) -> Result<QuantizationAv1, PlanToDxvaAv1Error> {
    let q = &h.quantization_params;
    let mut quantization = QuantizationAv1::zeroed();
    quantization.control_flags = QuantizationFlagsAv1 {
        delta_q_present: q.delta_q_present,
        delta_q_res: narrow("delta_q_res", q.delta_q_res)?,
    }
    .pack();
    quantization.base_qindex = narrow("base_qindex", q.base_q_idx)?;
    quantization.y_dc_delta_q = q.delta_q_y_dc as i8;
    quantization.u_dc_delta_q = q.delta_q_u_dc as i8;
    quantization.v_dc_delta_q = q.delta_q_v_dc as i8;
    quantization.u_ac_delta_q = q.delta_q_u_ac as i8;
    quantization.v_ac_delta_q = q.delta_q_v_ac as i8;
    // No `using_qmatrix` bit; 0xFF is "none". Parser leaves `qm_*` at 0
    // inside `if using_qmatrix`, and 0 is a valid matrix index.
    let (qm_y, qm_u, qm_v) = if q.using_qmatrix {
        (
            narrow("qm_y", q.qm_y)?,
            narrow("qm_u", q.qm_u)?,
            narrow("qm_v", q.qm_v)?,
        )
    } else {
        (QM_UNUSED, QM_UNUSED, QM_UNUSED)
    };
    quantization.qm_y = qm_y;
    quantization.qm_u = qm_u;
    quantization.qm_v = qm_v;
    Ok(quantization)
}

/// Two fields packed into a byte. `secondary` is two bits: AV1 5.9.19
/// rewrites a coded 3 to 4 in place, and `& 0x3` would turn the strongest
/// filter into none. `coded_cdef_sec_strength` inverts that.
fn cdef(h: &ParsedFrameHeader) -> Result<CdefAv1, PlanToDxvaAv1Error> {
    let c = &h.cdef_params;
    let mut cdef = CdefAv1::zeroed();
    cdef.control_flags = CdefFlagsAv1 {
        damping: narrow("cdef_damping", c.cdef_damping.saturating_sub(3))?,
        bits: narrow("cdef_bits", c.cdef_bits)?,
    }
    .pack();
    for i in 0..8 {
        cdef.y_strengths[i] = CdefStrength {
            primary: c.cdef_y_pri_strength[i] as u8,
            secondary: coded_cdef_sec_strength(c.cdef_y_sec_strength[i]),
        }
        .pack();
        cdef.uv_strengths[i] = CdefStrength {
            primary: c.cdef_uv_pri_strength[i] as u8,
            secondary: coded_cdef_sec_strength(c.cdef_uv_sec_strength[i]),
        }
        .pack();
    }
    Ok(cdef)
}

fn segmentation(h: &ParsedFrameHeader) -> SegmentationAv1 {
    let s = &h.segmentation_params;
    let mut segmentation = SegmentationAv1::zeroed();
    segmentation.control_flags = SegmentationFlagsAv1 {
        enabled: s.segmentation_enabled,
        update_map: s.segmentation_update_map,
        update_data: s.segmentation_update_data,
        temporal_update: s.segmentation_temporal_update,
    }
    .pack();
    for seg in 0..8 {
        let e = &s.feature_enabled[seg];
        segmentation.feature_mask[seg] = SegmentFeatureMask {
            alt_q: e[0],
            alt_lf_y_v: e[1],
            alt_lf_y_h: e[2],
            alt_lf_u: e[3],
            alt_lf_v: e[4],
            ref_frame: e[5],
            skip: e[6],
            globalmv: e[7],
        }
        .pack();
        segmentation.feature_data[seg] = s.feature_data[seg];
    }
    segmentation
}

/// Same film-grain gate as Vulkan: sequence present AND frame apply.
/// Otherwise the block stays zero.
fn film_grain(
    h: &ParsedFrameHeader,
    seq: &ParsedSequenceHeader,
) -> Result<FilmGrainAv1, PlanToDxvaAv1Error> {
    let mut film_grain = FilmGrainAv1::zeroed();
    if !(seq.film_grain_params_present && h.film_grain_params.apply_grain) {
        return Ok(film_grain);
    }
    let fg = &h.film_grain_params;
    film_grain.control_flags = FilmGrainFlagsAv1 {
        apply_grain: true,
        scaling_shift_minus8: fg.grain_scaling_minus_8,
        chroma_scaling_from_luma: fg.chroma_scaling_from_luma,
        ar_coeff_lag: narrow("ar_coeff_lag", fg.ar_coeff_lag)?,
        ar_coeff_shift_minus6: fg.ar_coeff_shift_minus_6,
        grain_scale_shift: fg.grain_scale_shift,
        overlap_flag: fg.overlap_flag,
        clip_to_restricted_range: fg.clip_to_restricted_range,
        matrix_coeff_is_identity: seq.color_config.matrix_coefficients as u32 == 0,
    }
    .pack();
    film_grain.grain_seed = fg.grain_seed;
    // DXVA wants [value, scaling] pairs; parser/Vulkan keep parallel
    // arrays. Over-count is refused, not truncated: fewer points is
    // different grain, not less grain.
    let pts = |name: &'static str, n: u8, cap: usize| -> Result<usize, PlanToDxvaAv1Error> {
        if usize::from(n) > cap {
            return Err(PlanToDxvaAv1Error::FieldOverflow {
                field: name,
                value: u32::from(n),
            });
        }
        Ok(usize::from(n))
    };
    let ny = pts(
        "num_y_points",
        fg.num_y_points,
        film_grain.scaling_points_y.len(),
    )?;
    let ncb = pts(
        "num_cb_points",
        fg.num_cb_points,
        film_grain.scaling_points_cb.len(),
    )?;
    let ncr = pts(
        "num_cr_points",
        fg.num_cr_points,
        film_grain.scaling_points_cr.len(),
    )?;
    for i in 0..ny {
        film_grain.scaling_points_y[i] = [fg.point_y_value[i], fg.point_y_scaling[i]];
    }
    for i in 0..ncb {
        film_grain.scaling_points_cb[i] = [fg.point_cb_value[i], fg.point_cb_scaling[i]];
    }
    for i in 0..ncr {
        film_grain.scaling_points_cr[i] = [fg.point_cr_value[i], fg.point_cr_scaling[i]];
    }
    film_grain.num_y_points = fg.num_y_points;
    film_grain.num_cb_points = fg.num_cb_points;
    film_grain.num_cr_points = fg.num_cr_points;
    film_grain
        .ar_coeffs_y
        .copy_from_slice(&fg.ar_coeffs_y_plus_128[..24]);
    film_grain
        .ar_coeffs_cb
        .copy_from_slice(&fg.ar_coeffs_cb_plus_128[..25]);
    film_grain
        .ar_coeffs_cr
        .copy_from_slice(&fg.ar_coeffs_cr_plus_128[..25]);
    film_grain.cb_mult = fg.cb_mult;
    film_grain.cb_luma_mult = fg.cb_luma_mult;
    film_grain.cr_mult = fg.cr_mult;
    film_grain.cr_luma_mult = fg.cr_luma_mult;
    film_grain.cb_offset = fg.cb_offset as i16;
    film_grain.cr_offset = fg.cr_offset as i16;
    Ok(film_grain)
}

/// The packed `coding` and `format` words.
fn frame_flags(h: &ParsedFrameHeader, seq: &ParsedSequenceHeader) -> (u32, u8) {
    let color = &seq.color_config;
    let coding = CodingFlagsAv1 {
        use_128x128_superblock: seq.use_128x128_superblock,
        intra_edge_filter: seq.enable_intra_edge_filter,
        interintra_compound: seq.enable_interintra_compound,
        masked_compound: seq.enable_masked_compound,
        warped_motion: h.allow_warped_motion,
        dual_filter: seq.enable_dual_filter,
        jnt_comp: seq.enable_jnt_comp,
        screen_content_tools: h.allow_screen_content_tools != 0,
        integer_mv: h.force_integer_mv != 0,
        cdef: seq.enable_cdef,
        restoration: seq.enable_restoration,
        film_grain: seq.film_grain_params_present,
        intrabc: h.allow_intrabc,
        high_precision_mv: h.allow_high_precision_mv,
        switchable_motion_mode: h.is_motion_mode_switchable,
        filter_intra: seq.enable_filter_intra,
        disable_frame_end_update_cdf: h.disable_frame_end_update_cdf,
        disable_cdf_update: h.disable_cdf_update,
        reference_mode: h.reference_select,
        skip_mode: h.skip_mode_present,
        reduced_tx_set: h.reduced_tx_set,
        superres: h.use_superres,
        tx_mode: h.tx_mode as u8,
        use_ref_frame_mvs: h.use_ref_frame_mvs,
        enable_ref_frame_mvs: seq.enable_ref_frame_mvs,
        // Literal 1, not `refresh_frame_flags != 0`. libavcodec writes 1;
        // Chromium writes `!(show_existing && KEY)`, which is also 1 here
        // (`NoDecode` in the caller). A no-refresh frame is legal AV1.
        reference_frame_update: true,
    }
    .pack();
    let format = FormatFlagsAv1 {
        frame_type: h.frame_type as u8,
        show_frame: h.show_frame,
        showable_frame: h.showable_frame,
        subsampling_x: color.subsampling_x,
        subsampling_y: color.subsampling_y,
        mono_chrome: color.mono_chrome,
    }
    .pack();
    (coding, format)
}

/// `SUPERRES_NUM` (AV1 spec): the denominator that means "no upscaling".
const SUPERRES_NUM: u8 = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptors::descriptors_av1;
    use crate::descriptors::BUFFER_BITSTREAM;
    use crate::descriptors::BUFFER_PICTURE_PARAMETERS;
    use crate::descriptors::BUFFER_SLICE_CONTROL;
    use crate::dxva::BITSTREAM_ALIGN;
    use crate::pack_av1::pack_av1;
    use crate::pack_av1::packed_size_av1;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::Av1Planner;

    const AV1_25FPS: &[u8] = pf_bitstream::testing::AV1_25FPS;

    /// Host 4K AV1, vendored for **two tiles**. `test-25fps.ivf.av1` is
    /// `tile_cols = tile_rows = 1` on every frame, so the one-tile test can
    /// only read index 0. This stream is `tile_cols = 1, tile_rows = 2` on
    /// all 60 frames, both tiles in one Tile Group OBU.
    const LOWDELAY_3840X2160_AV1: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-3840x2160.ivf.av1");

    /// Convert, then apply the deferred releases
    /// ([`DecodePlanDxvaAv1::release_after_decode`]). Caller's half of the
    /// contract; same helper the Vulkan tests carry.
    fn convert(au: &[u8], plan: &AuPlan, slots: &mut SlotMap) -> DecodePlanDxvaAv1 {
        let dx = plan_to_dxva_av1(au, plan, slots).expect("the clean vector converts");
        for &id in &dx.release_after_decode {
            assert!(
                slots.release(id),
                "a deferred release named picture {id}, which holds no surface"
            );
        }
        dx
    }

    /// A rebuilt session's fresh map holds none of the planner's stored pictures. The key
    /// frame that starts it over still converts; failing it would wedge every frame after.
    #[test]
    fn a_key_frame_converts_into_a_fresh_map_beside_an_old_store() {
        let mut planner = Av1Planner::new();
        let mut old = SlotMap::new(NUM_REF_SLOTS);
        let packets: Vec<&[u8]> = IvfIterator::new(AV1_25FPS).take(4).collect();
        for &packet in &packets {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_some() {
                    convert(packet, &plan, &mut old);
                }
            }
        }
        let mut fresh = SlotMap::new(NUM_REF_SLOTS);
        let key = packets[0];
        let plans = planner.plan_au(key).expect("the key frame plans again");
        let plan = plans
            .iter()
            .find(|p| p.dpb.stored.is_some())
            .expect("a decoded key frame");
        assert!(
            !plan.dpb_refs.is_empty(),
            "the store still holds the old pictures"
        );
        plan_to_dxva_av1(key, plan, &mut fresh).expect("the key frame converts");
    }

    /// Decode target never shares a surface with a picture the submission names.
    ///
    /// AV1 applies `refresh_frame_flags` after decode (7.20), so a frame that
    /// overwrites a slot it still names is ordinary. Releasing inside the
    /// conversion hands that surface to `setup_slot` (`SlotMap::assign` takes
    /// the lowest free slot). Assert the whole store: `RefFrameMapTextureIndex`
    /// declares every occupied slot, not just the seven this frame reads.
    #[test]
    fn the_decode_target_never_aliases_a_surface_the_submission_names() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut deferring, mut deferred) = (0u32, 0u32, 0u32);
        let mut peak = 0usize;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                // Not `convert` — this test applies the deferred releases
                // itself, after checking each one.
                let dx = plan_to_dxva_av1(packet, &plan, &mut slots).expect("the vector converts");
                frames += 1;
                peak = peak.max(slots.active());

                // `#[repr(packed)]` — copy the fields out before reading them.
                let curr = dx.pic_params.curr_pic_texture_index;
                let store = dx.pic_params.ref_frame_map_texture_index;
                assert!(
                    store.iter().all(|surface| *surface != curr),
                    "frame {frames}: surface {curr} is both CurrPicTextureIndex and a \
                     RefFrameMapTextureIndex entry — the frame decodes into a picture \
                     it predicts from"
                );
                // Planner invariant the deferral rests on: `dpb.removed` is a
                // subset of the pre-decode store `ref_frame_map` was built from.
                // Checking deferred ids against `dpb_refs` is vacuous (they
                // were filtered from `removed`). A planner change must fail here.
                for &id in &plan.dpb.removed {
                    assert!(
                        plan.dpb_refs.iter().any(|r| r.id == id),
                        "frame {frames}: the planner removed picture {id}, which the \
                         pre-decode store never held — `ref_frame_map` is built from \
                         that store, so a removal outside it is a picture this \
                         conversion could release without aliasing, and the blanket \
                         deferral above stops being justified"
                    );
                }
                for &id in &dx.release_after_decode {
                    assert!(
                        plan.dpb.removed.contains(&id),
                        "frame {frames}: deferred picture {id} is not in this plan's \
                         removed list"
                    );
                    assert_ne!(
                        id, dx.setup_id,
                        "frame {frames}: the picture being decoded must never be \
                         deferred — releasing it returns the surface being written"
                    );
                    assert!(
                        slots.slot_of(id).is_some(),
                        "frame {frames}: a deferred picture must still hold its surface"
                    );
                }
                if !dx.release_after_decode.is_empty() {
                    deferring += 1;
                    deferred += dx.release_after_decode.len() as u32;
                }
                for &id in &dx.release_after_decode {
                    assert!(slots.release(id));
                }
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            deferring, 268,
            "268 of 274 frames of this vector displace a picture their own submission \
             names; at zero this test compares an empty list against itself and the \
             deferral could be deleted without a single assertion noticing"
        );
        assert_eq!(deferred, 268, "one displaced picture per frame here");
        // `NUM_REF_SLOTS + 1`: the spare holds a displaced picture one frame
        // longer. Peak above `SlotMap::capacity()` would name a missing surface.
        assert!(
            peak <= slots.capacity(),
            "peak {peak} surfaces held exceeds the {} the pool allocates",
            slots.capacity()
        );
        eprintln!(
            "frames {frames} · deferring {deferring} · peak surfaces held {peak}/{}",
            slots.capacity()
        );
    }

    /// Whole vector converted and packed: the CPU stand-in for the hardware leg.
    ///
    /// Load-bearing: bytes each `DXVA_Tile_AV1` addresses must equal that tile's
    /// AU payload. A record covering the whole tile-group OBU passes every other
    /// check here and hands the driver OBU/frame/tile-group headers as entropy.
    /// Weaker than it looks — `pack_av1` computes offset from the same ranges —
    /// so `tile_group_obu()` accounting is checked separately below.
    #[test]
    fn the_whole_vendored_vector_packs_into_a_three_buffer_submission() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut dst = vec![0u8; 1 << 20];
        let mut frames = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                // Poison so a record is only "right" by pointing at bytes this
                // pack actually wrote.
                dst.fill(0xCC);
                let packed = pack_av1(packet, &dx.bitstream, &dx.tiles, &mut dst).expect("packs");

                assert_eq!(
                    packed.data_size as usize % BITSTREAM_ALIGN,
                    0,
                    "frame {frames}: the bitstream buffer is padded to the granule"
                );
                assert_eq!(packed.tiles.len(), dx.bitstream.tiles.len());

                for (record, tile) in packed.tiles.iter().zip(&dx.bitstream.tiles) {
                    // `#[repr(packed)]` — copy the fields out before using them.
                    let (offset, size) = (record.data_offset as usize, record.data_size as usize);
                    assert!(
                        offset + size <= packed.data_size as usize,
                        "frame {frames}: a tile record runs past the buffer's DataSize"
                    );
                    assert_eq!(
                        &dst[offset..offset + size],
                        &packet[tile.clone()],
                        "frame {frames}: the bytes a tile record addresses must BE that \
                         tile's payload"
                    );
                    assert!(
                        plan.tiles
                            .iter()
                            .all(|tg| tile.start != tg.data.start || tile.end != tg.data.end),
                        "frame {frames}: a tile record covers a whole tile-group OBU"
                    );
                }

                // Per-group accounting the byte comparison cannot make (fn docs).
                // `TileSizeBytes` is coded only when the frame has more than
                // one tile.
                let size_bytes =
                    if plan.header.tile_info.tile_cols * plan.header.tile_info.tile_rows > 1 {
                        plan.header.tile_info.tile_size_bytes as usize
                    } else {
                        0
                    };
                for group in &dx.bitstream.groups {
                    let in_group: Vec<_> = dx
                        .bitstream
                        .tiles
                        .iter()
                        .filter(|t| group.start <= t.start && t.end <= group.end)
                        .collect();
                    assert!(!in_group.is_empty(), "frame {frames}: an empty tile group");
                    let payloads: usize = in_group.iter().map(|t| t.end - t.start).sum();
                    assert_eq!(
                        payloads + (in_group.len() - 1) * size_bytes,
                        group.end - group.start,
                        "frame {frames}: the group's {} tiles plus its {} size fields \
                         must account for the region EXACTLY — a short sum is a tile \
                         boundary read in the wrong place, which every offset after it \
                         inherits",
                        in_group.len(),
                        in_group.len() - 1
                    );
                }

                let descs = descriptors_av1(&packed);
                assert_eq!(
                    descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
                    vec![
                        BUFFER_PICTURE_PARAMETERS,
                        BUFFER_BITSTREAM,
                        BUFFER_SLICE_CONTROL,
                    ],
                    "frame {frames}: AV1 submits three buffers and never a matrix"
                );
                // Only PicParams size is independent of `descriptors_av1`
                // arithmetic. The other two are checked against the bytes
                // written and the SDK record size.
                assert_eq!(descs[0].data_size, 912, "DXVA_PicParams_AV1, measured");
                assert_eq!(
                    descs[1].data_size as usize % BITSTREAM_ALIGN,
                    0,
                    "frame {frames}: the bitstream descriptor states the PADDED size"
                );
                assert!(
                    descs[1].data_size as usize >= packed_size_av1(&dx.bitstream),
                    "frame {frames}: the bitstream descriptor is at least the tile data"
                );
                assert_eq!(
                    descs[2].data_size as usize,
                    size_of::<TileAv1>() * dx.tiles.len(),
                    "frame {frames}: sixteen bytes per TILE"
                );
                assert!(descs.iter().all(|d| d.num_mbs_in_buffer == 0));
            }
        }
        assert_eq!(frames, 274);
    }

    /// Convert every frame and check what a driver reads.
    ///
    /// Anti-vacuity matters as much as the checks: a run that never saw an
    /// inter frame, or never saw the store hold a picture the frame does not
    /// name, would pass while exercising none of the interesting code.
    #[test]
    fn the_whole_vendored_vector_converts() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut inter, mut store_beyond_refs) = (0u32, 0u32, 0u32);
        let mut gm_by_slot_would_differ = 0u32;
        let mut index_by_surface_would_differ = 0u32;
        let mut ref_size_would_differ = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                // Count before conversion, while the ledger still holds what
                // the conversion reads (it releases displaced pictures on the
                // way out). Visible only where surface ≠ slot.
                for r in plan.refs.iter().flatten() {
                    let surface = slots.slot_of(r.id).expect("a named reference is held");
                    if surface != r.slot {
                        index_by_surface_would_differ += 1;
                    }
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;

                // Tile records are payload ranges after `tile_size_minus_1`,
                // never the whole tile-group OBU.
                assert_eq!(dx.tiles.len(), dx.bitstream.tiles.len());
                for (rec, range) in dx.tiles.iter().zip(&dx.bitstream.tiles) {
                    assert_eq!(rec.data_offset as usize, range.start);
                    assert_eq!(rec.data_size as usize, range.end - range.start);
                    assert!(range.end <= packet.len());
                    assert!(dx
                        .bitstream
                        .groups
                        .iter()
                        .any(|g| g.start <= range.start && range.end <= g.end));
                }
                for tg in &plan.tiles {
                    // Strictly inside its OBU, never at the first byte: the
                    // OBU header alone is one or two bytes.
                    assert!(dx
                        .tiles
                        .iter()
                        .all(|rec| rec.data_offset as usize != tg.data.start));
                }

                // Named slots resolve to a real surface; empty stays UNUSED.
                // 0 is a valid surface, so a leftover 0 would point at a live picture.
                let named = dx
                    .pic_params
                    .ref_frame_map_texture_index
                    .iter()
                    .filter(|i| **i != UNUSED_INDEX)
                    .count();
                assert_eq!(named, plan.dpb_refs.len());
                let referenced = plan.refs.iter().flatten().count();
                if named > referenced {
                    store_beyond_refs += 1;
                }

                if referenced > 0 {
                    inter += 1;
                    for (name, r) in plan.refs.iter().enumerate() {
                        let e = dx.pic_params.frame_refs[name];
                        let Some(named_ref) = r else {
                            assert_eq!(
                                e.index, UNUSED_INDEX,
                                "an unnamed reference must stay unused, not read as \
                                 slot 0"
                            );
                            continue;
                        };
                        assert_eq!(
                            e.index, named_ref.slot,
                            "reference name {name} must carry ref_frame_idx[{name}] — \
                             the SLOT — because `Index` subscripts \
                             RefFrameMapTextureIndex; a surface index there predicts \
                             from whatever sits in the slot of that number"
                        );
                        assert_ne!(
                            dx.pic_params.ref_frame_map_texture_index[usize::from(e.index)],
                            UNUSED_INDEX,
                            "reference name {name} points at an empty slot"
                        );
                        // The reference's own size, not this frame's. This
                        // vector never resizes, so pin against `RefPic::state`.
                        let (w, h) = (e.width, e.height);
                        assert_eq!(
                            (w, h),
                            (named_ref.state.upscaled_width, named_ref.state.frame_height),
                            "reference name {name} must carry its OWN coded size"
                        );
                        if named_ref.state.upscaled_width != plan.header.upscaled_width
                            || named_ref.state.frame_height != plan.header.frame_height
                        {
                            ref_size_would_differ += 1;
                        }
                        let gm = &plan.header.global_motion_params;
                        // `PicEntryAv1` is `#[repr(packed)]`; copy fields out
                        // before compare — a reference may be unaligned.
                        let (wmmat, flags) = (e.wmmat, e.global_motion_flags);
                        assert_eq!(
                            wmmat,
                            gm.gm_params[LAST_FRAME + name],
                            "reference name {name} must carry gm_params[LAST_FRAME \
                             + {name}], not the entry at its DPB slot"
                        );
                        assert_eq!(
                            flags,
                            GlobalMotionFlags {
                                wminvalid: !gm.warp_valid[LAST_FRAME + name],
                                wmtype: gm.gm_type[LAST_FRAME + name] as u8,
                            }
                            .pack()
                        );
                        // Count where name-index and slot-index disagree.
                        let slot = usize::from(named_ref.slot);
                        if gm.gm_params[LAST_FRAME + name] != gm.gm_params[slot]
                            || gm.gm_type[LAST_FRAME + name] != gm.gm_type[slot]
                        {
                            gm_by_slot_would_differ += 1;
                        }
                    }
                }
                assert_eq!(dx.pic_params.curr_pic_texture_index, dx.setup_slot);

                // Both native rungs take AV1 render size as a display crop and
                // clamp it: 5.9.6 puts no upper bound on `render_width_minus_1`.
                // This vector never exercises the clamp.
                assert!(
                    plan.picture.render_width <= plan.picture.upscaled_width
                        && plan.picture.render_height <= plan.picture.frame_height,
                    "frame {frames}: this vector's render region fits inside the \
                     decoded picture, so the display-size clamp is inert on it"
                );
            }
        }

        assert_eq!(frames, 274);
        eprintln!("gm reads where name and slot disagree: {gm_by_slot_would_differ}");
        eprintln!(
            "reference entries where the surface is not the slot: \
             {index_by_surface_would_differ}"
        );
        assert!(
            index_by_surface_would_differ > 0,
            "no reference of this vector ever sat in a slot whose number differs from \
             its surface index, so `Index` cannot be told from a surface index here — \
             which is exactly how the surface read shipped"
        );
        assert_eq!(
            ref_size_would_differ, 0,
            "this vector never resizes, so `frame_refs[].width` cannot be told from \
             the current frame's width by VALUE; it is pinned against \
             `RefPic::state` instead, and this counter says so rather than leaving \
             the reader to wonder"
        );
        assert!(
            gm_by_slot_would_differ > 0,
            "reading global motion by DPB SLOT never disagreed with reading it by \
             reference NAME on this vector, so the assertions above cannot tell the \
             two apart — which is how the slot read shipped in the first place"
        );
        assert!(inter > 0, "a 274-frame vector must have inter frames");
        assert!(
            store_beyond_refs > 0,
            "the reference store never held a picture the frame did not name — so \
             this run never exercised the difference between RefFrameMapTextureIndex \
             (the whole store) and frame_refs (what this frame uses), which is the \
             distinction the Ally X class of bug lives in"
        );
    }

    /// Chroma deblocking reaches `filter_level_u`/`filter_level_v`, and
    /// `log2_restoration_unit_size` is never the parser's silence.
    ///
    /// Frame 0 codes levels `[1, 7, 8, 12]`. Dropping only `[2]`/`[3]` is
    /// invisible to luma. Off restoration leaves the parser at `[0,0,0]`;
    /// `trailing_zeros` is 16, outside `dxva.h`'s 6..8. libavcodec sends 8.
    #[test]
    fn the_chroma_loop_filter_levels_and_the_restoration_unit_size_reach_the_driver() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut with_chroma_lf, mut with_lr) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let lf = &plan.header.loop_filter_params;
                // `#[repr(packed)]` — copy the block out before reading its fields.
                let sent = dx.pic_params.loop_filter;
                assert_eq!(
                    (
                        sent.filter_level[0],
                        sent.filter_level[1],
                        sent.filter_level_u,
                        sent.filter_level_v
                    ),
                    (
                        lf.loop_filter_level[0],
                        lf.loop_filter_level[1],
                        lf.loop_filter_level[2],
                        lf.loop_filter_level[3]
                    ),
                    "frame {frames}: DXVA splits AV1's four levels into a luma PAIR \
                     plus two named chroma fields — U is index 2 and V is index 3"
                );
                if lf.loop_filter_level[2] != 0 || lf.loop_filter_level[3] != 0 {
                    with_chroma_lf += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        (sent.filter_level, sent.filter_level_u, sent.filter_level_v),
                        ([1, 7], 8, 12),
                        "frame 0's levels, and the pair whose loss the Vulkan rung's \
                         frame-0 divergence was reproduced from"
                    );
                }

                let lr = &plan.header.loop_restoration_params;
                let sizes = sent.log2_restoration_unit_size;
                if lr.uses_lr {
                    with_lr += 1;
                    assert_eq!(lr.loop_restoration_size, [128, 128, 128]);
                    assert_eq!(sizes, [7, 7, 7], "6 + lr_unit_shift, per plane");
                } else {
                    assert_eq!(
                        sizes, [LOG2_RESTORATION_UNIT_SIZE_UNUSED; 3],
                        "frame {frames}: restores nothing, so the size is \
                         libavcodec's 8 — never the parser's zero read as 16"
                    );
                }
                assert!(
                    sizes.iter().all(|s| (6..=8).contains(s)),
                    "frame {frames}: log2_restoration_unit_size is 6, 7 or 8"
                );
            }
        }

        assert_eq!(frames, 274);
        assert_eq!(
            with_chroma_lf, 123,
            "123 of 274 frames of this vector deblock chroma; at zero the levels \
             above are all zero anyway and this test could not tell a dropped pair \
             from a carried one"
        );
        assert_eq!(
            with_lr, 3,
            "three frames use loop restoration, so both branches of the size are \
             exercised"
        );
    }

    /// Packed CDEF strength bytes carry the coded secondary strength.
    ///
    /// `CdefStrength::pack` gives `secondary` two bits. The parser holds the
    /// post-fixup value (4 where the stream coded 3, 5.9.19). `& 0x3` turns
    /// the strongest filter into none. Assert the packed byte: truncation is
    /// what `pack` does, and a struct-level check would not see it.
    #[test]
    fn cdef_secondary_strengths_survive_the_two_bit_pack() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut corrected_frames) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let raw = &plan.header.cdef_params;
                // `#[repr(packed)]` — copy the arrays out before indexing them.
                let cdef = dx.pic_params.cdef;
                let (y, uv) = (cdef.y_strengths, cdef.uv_strengths);
                let mut corrected = false;
                for i in 0..8 {
                    let want_y = coded_cdef_sec_strength(raw.cdef_y_sec_strength[i]);
                    let want_uv = coded_cdef_sec_strength(raw.cdef_uv_sec_strength[i]);
                    assert_eq!(
                        (y[i] >> 6, uv[i] >> 6),
                        (want_y, want_uv),
                        "frame {frames}: the secondary strength must survive the \
                         two-bit field — the parser's 4 packs to 0"
                    );
                    assert_eq!(
                        (y[i] & 0x3f, uv[i] & 0x3f),
                        (
                            raw.cdef_y_pri_strength[i] as u8,
                            raw.cdef_uv_pri_strength[i] as u8
                        ),
                        "the PRIMARY strengths are not fixed up by the spec and must \
                         reach the driver untouched"
                    );
                    if raw.cdef_y_sec_strength[i] == 4 || raw.cdef_uv_sec_strength[i] == 4 {
                        corrected = true;
                    }
                }
                if corrected {
                    corrected_frames += 1;
                }
                if frames == 1 {
                    assert_eq!(
                        (y[3] >> 6, uv[0] >> 6),
                        (3, 3),
                        "frame 0 codes the strongest secondary strength twice, and \
                         the uncorrected conversion packed both as 0"
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

    /// A key frame names no reference, and must say so with the unused sentinel
    /// rather than with slot 0.
    #[test]
    fn a_key_frame_names_no_reference() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plans = planner.plan_au(first).expect("the first unit plans");
        let plan = plans.first().expect("a frame");
        assert!(plan.picture.is_key, "the vector opens on a key frame");
        let dx = plan_to_dxva_av1(first, plan, &mut slots).expect("converts");
        assert!(dx
            .pic_params
            .frame_refs
            .iter()
            .all(|e| e.index == UNUSED_INDEX));
    }

    /// Tile sizes are superblock COUNTS, not the coded minus-one values.
    ///
    /// Nothing else sees this: offsets, sizes, descriptors stay right, the
    /// picture decodes, each tile is one SB too small. This vector is one
    /// tile, so width is the whole frame: `ceil(320/64)=5` by `ceil(240/64)=4`.
    /// Minus-one would say 4 by 3.
    #[test]
    fn the_tile_sizes_are_superblock_counts_not_the_coded_minus_one() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let t = &plan.header.tile_info;
                // `#[repr(packed)]` — copy the block out before reading its arrays.
                let tiles = dx.pic_params.tiles;
                assert_eq!((tiles.cols, tiles.rows), (1, 1), "this vector is one tile");
                let sb = if plan.sequence.use_128x128_superblock {
                    128
                } else {
                    64
                };
                assert_eq!(
                    (tiles.widths[0], tiles.heights[0]),
                    (
                        plan.header.frame_width.div_ceil(sb) as u16,
                        plan.header.frame_height.div_ceil(sb) as u16
                    ),
                    "frame {frames}: the single tile spans the whole frame in \
                     superblocks — libav sends `width_in_sbs_minus_1[i] + 1`"
                );
                assert_eq!(
                    (tiles.widths[0], tiles.heights[0]),
                    (
                        t.width_in_sbs_minus_1[0] as u16 + 1,
                        t.height_in_sbs_minus_1[0] as u16 + 1
                    ),
                    "frame {frames}: and that is the coded value plus one"
                );
                // Past the grid the arrays stay zero. A phantom `1` is a tile
                // the frame has none of. (`#[repr(packed)]`: copy before iterate.)
                let (widths, heights) = (tiles.widths, tiles.heights);
                assert!(widths[1..].iter().all(|w| *w == 0));
                assert!(heights[1..].iter().all(|h| *h == 0));
            }
        }
        assert_eq!(frames, 274);
    }

    /// Same tile arrays with a second tile live — the case a one-tile vector
    /// cannot reach. Writing only index 0 and leaving `1..` zero passes the
    /// sibling test and drops half the height here.
    ///
    /// `tile_rows = 2` with `height_in_sbs_minus_1 = [16, 16]` on a 2160-line
    /// frame is 17+17=34 superblocks of 64 (2176, the padded height).
    #[test]
    fn a_two_tile_frame_fills_both_row_entries_and_leaves_the_rest_zero() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut frames = 0u32;

        for packet in IvfIterator::new(LOWDELAY_3840X2160_AV1) {
            for plan in planner
                .plan_au(packet)
                .expect("the low-delay 4K stream plans")
            {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let t = &plan.header.tile_info;
                // `#[repr(packed)]` — copy the block out before reading its arrays.
                let tiles = dx.pic_params.tiles;
                let (widths, heights) = (tiles.widths, tiles.heights);
                assert_eq!(
                    (tiles.cols, tiles.rows),
                    (1, 2),
                    "frame {frames}: this stream is vendored FOR its second tile row. \
                     One tile here means it was regenerated below 4K (1440p and down \
                     measured single-tile) and this test has quietly become a duplicate \
                     of the vendored vector's"
                );
                let sb = if plan.sequence.use_128x128_superblock {
                    128
                } else {
                    64
                };
                assert_eq!(
                    (widths[0], 0u16),
                    (plan.header.frame_width.div_ceil(sb) as u16, 0u16),
                    "frame {frames}: the single tile COLUMN spans the whole width"
                );
                // Both rows, each coded-plus-one, read independently so a
                // broadcast of entry 0 still has to get entry 1's own value.
                assert_eq!(
                    (heights[0], heights[1]),
                    (
                        t.height_in_sbs_minus_1[0] as u16 + 1,
                        t.height_in_sbs_minus_1[1] as u16 + 1
                    ),
                    "frame {frames}: each tile row's height is its OWN \
                     `height_in_sbs_minus_1 + 1`"
                );
                assert_eq!(
                    u32::from(heights[0]) + u32::from(heights[1]),
                    plan.header.frame_height.div_ceil(sb),
                    "frame {frames}: the two tile rows must tile the frame exactly — a \
                     short second row is a frame with missing lines, which is precisely \
                     the shape the host once shipped over the wire"
                );
                // Past the grid stays zero: a phantom entry is a tile the
                // frame has not.
                assert!(widths[1..].iter().all(|w| *w == 0));
                assert!(heights[2..].iter().all(|h| *h == 0));

                // Two tile records from one tile group. On the one-tile
                // vector, one record per group is indistinguishable from one
                // per tile.
                assert_eq!(
                    plan.tiles.len(),
                    1,
                    "frame {frames}: both tiles arrive in one Tile Group OBU"
                );
                assert_eq!(
                    dx.tiles.len(),
                    2,
                    "frame {frames}: one record per TILE, not per tile group"
                );
                assert_eq!(
                    (dx.tiles[0].row, dx.tiles[0].column),
                    (0, 0),
                    "frame {frames}: tile 0 is row 0"
                );
                assert_eq!(
                    (dx.tiles[1].row, dx.tiles[1].column),
                    (1, 0),
                    "frame {frames}: tile 1 is the SECOND ROW of a single column — a \
                     (0, 1) here means rows and columns are transposed, which one \
                     square tile grid could never show"
                );
                assert!(
                    dx.tiles.iter().all(|r| r.data_size > 0),
                    "frame {frames}: every tile record must span real bytes; a \
                     zero-length second record is the whole bottom half of the frame \
                     missing"
                );
            }
        }
        assert_eq!(frames, 60, "the low-delay 4K stream is 60 coded frames");
    }

    /// Three fields whose correct value is a sentinel or a constant, on every
    /// frame — none of which any other assertion here would notice.
    ///
    /// * `StatusReportFeedbackNumber` zero: libavcodec comments the AV1
    ///   assignment out; Chromium ships zero too. This rung takes no number.
    /// * `qm_y`/`qm_u`/`qm_v` `0xFF` with no matrix: no `using_qmatrix` bit,
    ///   parser leaves 0 (a valid matrix).
    /// * `reference_frame_update` 1, which libavcodec writes as a literal.
    #[test]
    fn the_three_fields_whose_right_answer_is_a_constant() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut frames, mut without_qmatrix, mut without_refresh) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, &plan, &mut slots);
                frames += 1;
                let pp = &dx.pic_params;
                let status = pp.status_report_feedback_number;
                assert_eq!(
                    status, 0,
                    "frame {frames}: AV1 submits a zero StatusReportFeedbackNumber"
                );

                let q = pp.quantization;
                let (qm_y, qm_u, qm_v) = (q.qm_y, q.qm_u, q.qm_v);
                if plan.header.quantization_params.using_qmatrix {
                    assert_eq!(
                        (qm_y, qm_u, qm_v),
                        (
                            plan.header.quantization_params.qm_y as u8,
                            plan.header.quantization_params.qm_u as u8,
                            plan.header.quantization_params.qm_v as u8
                        )
                    );
                } else {
                    without_qmatrix += 1;
                    assert_eq!(
                        (qm_y, qm_u, qm_v),
                        (QM_UNUSED, QM_UNUSED, QM_UNUSED),
                        "frame {frames}: with no quantiser matrix the indices are the \
                         0xFF sentinel — 0 is matrix zero, which the driver would \
                         dequantize against"
                    );
                }

                // `reference_frame_update` is bit 22 of the coding flags — read back
                // through `pack` rather than spelled as a magic mask.
                let coding = pp.coding;
                let on = CodingFlagsAv1 {
                    reference_frame_update: true,
                    ..Default::default()
                }
                .pack();
                assert_eq!(coding & on, on, "frame {frames}: libav writes a literal 1");
                if plan.header.refresh_frame_flags == 0 {
                    without_refresh += 1;
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            without_qmatrix, 274,
            "no frame of this vector uses a quantiser matrix, so the sentinel is what \
             the driver reads on every one of them — at zero this test proves nothing"
        );
        // This vector has no no-refresh frame, so `reference_frame_update`
        // cannot be told from `refresh_frame_flags != 0` by value.
        assert_eq!(without_refresh, 0);
    }

    /// Every picture a temporal unit decodes is still addressable once the
    /// unit ends.
    ///
    /// Precondition of the Windows AV1 parity harness: it takes a whole
    /// temporal unit and finds a hidden frame's pixels via the slot map after
    /// the unit is done. Sound only if a unit never displaces a picture it
    /// decoded — a fact about this vector, not about AV1.
    #[test]
    fn no_unit_of_the_vector_displaces_a_picture_it_decoded_itself() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let (mut units, mut multi_frame) = (0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            let plans = planner.plan_au(packet).expect("the clean vector plans");
            units += 1;
            let mut decoded = Vec::new();
            for plan in &plans {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let dx = convert(packet, plan, &mut slots);
                decoded.push((dx.setup_id, dx.setup_slot));
            }
            if decoded.len() > 1 {
                multi_frame += 1;
            }
            for (id, slot) in decoded {
                assert_eq!(
                    slots.slot_of(id),
                    Some(slot),
                    "unit {units}: picture {id} left surface {slot} before its own \
                     unit finished, so a per-unit readback could not find it"
                );
            }
        }
        assert_eq!(units, 250);
        assert_eq!(
            multi_frame, 24,
            "24 units carry a hidden frame as well as the shown one — at zero this \
             check never saw the case it exists for"
        );
    }

    /// A frame whose tile groups do not add up to its tile grid is refused.
    ///
    /// Stands in for a dropped tile group: the OBU walk never sees it, so no
    /// `TruncatedAu` warning. Simulated by shrinking the tile-group plan.
    #[test]
    fn a_frame_short_of_its_tile_grid_is_refused_rather_than_submitted() {
        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plans = planner.plan_au(first).expect("the first unit plans");
        let mut plan = plans.into_iter().next().expect("a frame");

        // Untouched frame converts, so the refusal below is about the tiles.
        plan_to_dxva_av1(first, &plan, &mut slots).expect("the untouched frame converts");

        let header = std::rc::Rc::make_mut(&mut plan.header);
        header.tile_info.tile_cols = 2;
        header.tile_info.tile_rows = 1;
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        assert_eq!(
            plan_to_dxva_av1(first, &plan, &mut slots).err(),
            Some(PlanToDxvaAv1Error::TileCountMismatch {
                records: 1,
                walked: 1,
                grid: 2,
            })
        );
    }
}
