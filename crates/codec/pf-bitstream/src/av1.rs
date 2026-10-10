//! AV1 access-unit planning. One temporal unit in; a [`AuPlan`] per frame out.
//! The vendored parser reads syntax; this module owns reference-slot
//! bookkeeping, output state, and concealment.
//!
//! AV1 names seven reads through `ref_frame_idx`, writes eight numbered slots
//! through `refresh_frame_flags`, and may display an existing slot without
//! decoding. [`AuPlan::refs`] is therefore reference-name indexed and keeps
//! holes for missing slots. A unit may carry several frames; [`Av1Planner::plan_au`]
//! returns one plan each, in decode order.
//!
//! Backend Vulkan, DXVA, and VA-API conversions stay in their decoder crates.

use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::av1::parser::FrameHeaderObu;
use cros_codecs::codec::av1::parser::ObuAction;
use cros_codecs::codec::av1::parser::ObuType;
use cros_codecs::codec::av1::parser::ParsedObu;
use cros_codecs::codec::av1::parser::Parser;
use cros_codecs::codec::av1::parser::SequenceHeaderObu;

use crate::h264::ColourDescription;

pub mod tiles;

/// Parsed types a backend conversion names. Re-exported so backends do not reach
/// into the vendored crate — the same courtesy [`crate::h264`] does for `Sps`/`Pps`.
pub use cros_codecs::codec::av1::parser::FrameHeaderObu as ParsedFrameHeader;
pub use cros_codecs::codec::av1::parser::FrameType;
pub use cros_codecs::codec::av1::parser::SequenceHeaderObu as ParsedSequenceHeader;

/// Stable identity of a decoded picture. Backends key surface tables by this,
/// never by slot index — slots are reused.
pub type PicId = u64;

/// AV1's reference slot count (`NUM_REF_FRAMES`).
pub const NUM_REF_SLOTS: usize = 8;

/// References a single inter frame may name (`REFS_PER_FRAME`).
pub const REFS_PER_FRAME: usize = 7;

/// One named reference: picture, the slot that holds it, and that picture's own
/// header state (never the frame being decoded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub id: PicId,
    /// Slot index, 0..8. Vulkan addresses references by slot; surface-keyed
    /// backends resolve `id` through their own table.
    pub slot: u8,
    /// The reference's own header state — never the frame being decoded.
    pub state: RefState,
}

/// What one picture's own frame header said, kept for as long as that picture can
/// serve as a reference.
///
/// Vulkan (`StdVideoDecodeAV1ReferenceInfo`) and DXVA (`DXVA_PicEntry_AV1`) ask
/// about the *reference* picture, not the frame being decoded. Filling those
/// fields from the current header compiles and still predicts from a picture the
/// hardware has been told the wrong size, type, and order hint about. Recorded
/// once at store time in [`Av1Planner::refresh_slots`]; built only by [`RefState::of`].
///
/// VA-API has no per-reference struct: `ref_frame_map` is `VASurfaceID`s and the
/// driver reads answers off the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefState {
    pub order_hint: u32,
    /// This picture's `UpscaledWidth`. 7.11.3.3 scales motion from the
    /// *reference's* size; DXVA asks for it per entry. Filling it from the
    /// current header makes scaled prediction read as unscaled. VA-API reads
    /// dimensions off the surface.
    pub upscaled_width: u32,
    /// This picture's `FrameHeight`. No superres vertically, so this is the
    /// reference's coded height.
    pub frame_height: u32,
    /// This picture's frame type — a reference is routinely a different type
    /// from the frame reading it.
    pub frame_type: FrameType,
    /// `RefFrameSignBias` packed for Vulkan: bit `i` set where
    /// `RefFrameSignBias[i]` is 1 (`INTRA_FRAME` = 0 … `ALTREF_FRAME` = 7).
    /// Tells the decoder a reference lies in the future. All-zero is wrong for
    /// any stream with hidden ALTREFs.
    pub ref_frame_sign_bias: u8,
    /// This picture's `OrderHints[]`, which become `SavedOrderHints` once it is
    /// a reference (7.20). Indexed by AV1 reference-frame name, as above.
    pub saved_order_hints: [u32; NUM_REF_SLOTS],
    pub disable_frame_end_update_cdf: bool,
    pub segmentation_enabled: bool,
}

impl RefState {
    /// Read one frame header's reference-relevant state.
    ///
    /// Used both when a picture is stored and when a backend activates a slot for
    /// the picture it is about to decode. One function so the two cannot drift.
    pub fn of(header: &FrameHeaderObu) -> RefState {
        // Spec `RefFrameSignBias` is name-indexed (bit 1 = LAST_FRAME). The
        // vendored parser writes `fh.ref_frame_sign_bias[i]` in the LAST_FRAME+i
        // loop, so index 0 is LAST and index 7 is never written. Shift here;
        // `the_sign_bias_mask_is_spec_indexed_not_parser_indexed` pins it.
        let mut ref_frame_sign_bias = 0u8;
        for (i, biased) in header
            .ref_frame_sign_bias
            .iter()
            .take(REFS_PER_FRAME)
            .enumerate()
        {
            if *biased {
                ref_frame_sign_bias |= 1 << (i + 1);
            }
        }
        RefState {
            order_hint: header.order_hint,
            upscaled_width: header.upscaled_width,
            frame_height: header.frame_height,
            frame_type: header.frame_type,
            ref_frame_sign_bias,
            saved_order_hints: header.order_hints,
            disable_frame_end_update_cdf: header.disable_frame_end_update_cdf,
            segmentation_enabled: header.segmentation_params.segmentation_enabled,
        }
    }
}

/// One CDEF secondary strength as hardware wants it: the coded two-bit syntax
/// element, `0..=3`.
///
/// AV1 5.9.19 reads `cdef_y_sec_strength[i]` as `f(2)` then mutates it in place
/// (`== 3` becomes `4`). The vendored parser follows the spec, so it holds
/// `0, 1, 2` or `4`. Every decode API wants the value *before* that fixup and
/// applies the expansion itself; sending `4` overflows a two-bit field and the
/// strength reads back as 0 — no secondary CDEF on the blocks that asked for
/// the strongest, before the frame is stored as a reference.
///
/// `0..=2` pass through; a hand-built coded `3` passes through. Wider values
/// are clamped, not masked: `& 3` is the truncation this exists to prevent.
/// Pinned by [`crate::av1::tests::the_cdef_secondary_strength_is_the_coded_value`].
pub fn coded_cdef_sec_strength(parsed: u32) -> u8 {
    match parsed {
        0..=2 => parsed as u8,
        _ => 3,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DpbUpdate {
    /// Id assigned to this AU's picture — allocate a surface. `None` for a
    /// `show_existing_frame`, which decodes nothing.
    pub stored: Option<PicId>,
    /// Display-ready pictures, in output order.
    pub outputs: Vec<PicId>,
    /// Pictures no slot holds any more; free once displayed.
    pub removed: Vec<PicId>,
}

/// One tile-group OBU as a byte range in the access unit.
///
/// AV1 hands the hardware whole tile-group OBUs, not H.264/H.265 slice records.
/// The range is the OBU's data; backends concatenate in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilePlan {
    pub data: Range<usize>,
    pub tg_start: u32,
    pub tg_end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PicturePlan {
    pub frame_type: FrameType,
    /// Key frame — the stream's re-anchor point.
    pub is_key: bool,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub order_hint: u32,
    /// Post-superres width; `frame_width` is the coded width before upscaling.
    pub upscaled_width: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    /// The display region — AV1's counterpart to a conformance window.
    pub render_width: u32,
    pub render_height: u32,
    pub bit_depth: u8,
    /// 0 = monochrome, 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4 — H.264 `chroma_format_idc`
    /// so a backend's format decision is one function for all three codecs.
    pub chroma_format_idc: u8,
    /// Colour signalling, per picture, never latched: a host can switch an HDR
    /// desktop to PQ/BT.2020 in band.
    pub colour: ColourDescription,
    /// Every picture this frame predicts from came off a fully-available reference
    /// chain, so a host `USER_FLAG_RECOVERY_ANCHOR` can be corroborated. `true` for
    /// a key or intra-only frame and for any clean chain; `false` once this frame
    /// or anything it descends from needed concealment.
    ///
    /// On a `show_existing_frame` this describes the picture being displayed —
    /// the only thing such a frame puts on the screen. See [`crate::clean`].
    pub references_clean: bool,
}

#[derive(Debug, Clone)]
pub struct AuPlan {
    pub picture: PicturePlan,
    pub tiles: Vec<TilePlan>,
    /// References this frame names, indexed by AV1 reference *name*: position `i`
    /// is `ref_frame_idx[i]` (`LAST_FRAME + i`).
    ///
    /// `None` is a hole: the named slot held nothing, also reported as
    /// [`PlanWarning::MissingReference`]. A compacted `Vec` of survivors renumbers
    /// every name after the first loss, and a backend that reads position-as-name
    /// then predicts from the wrong picture. Repeats are preserved: several names
    /// may point at one slot.
    pub refs: [Option<RefPic>; REFS_PER_FRAME],
    pub dpb: DpbUpdate,
    /// Every slot that holds a picture as this AU decodes — the marked DPB DXVA
    /// and VA-API conversions want, a superset of [`Self::refs`]. Slot order, each
    /// slot once.
    pub dpb_refs: Vec<RefPic>,
    pub warnings: Vec<PlanWarning>,
    pub sequence: Rc<SequenceHeaderObu>,
    /// The frame header this plan was built from, whole.
    ///
    /// [`Self::picture`] is the client digest. A hardware backend needs nearly all
    /// of the header: AV1 puts tile, quant, segmentation, loop filter, CDEF, loop
    /// restoration, global motion and film grain in the per-frame header, not a
    /// parameter set. Same reason H.264/H.265 plans carry the activated SPS/PPS:
    /// build driver structs from what was parsed, never by re-reading the AU.
    pub header: Rc<FrameHeaderObu>,
}

/// Concealment signals: planning continues, the session layer requests recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// A frame named a slot holding no picture. No AV1 process empties a slot
    /// behind the stream's back, so the reference was lost upstream.
    MissingReference { slot: u8, ref_index: u8 },
    /// An error-resilient frame coded the order hint it expects in `slot`
    /// (5.9.2 `ref_order_hint`) and the resident picture's differs: the frame
    /// the encoder predicts from was lost and an older one still sits there.
    /// The only AV1 syntax that can expose a lost inter frame.
    StaleReference { slot: u8, ref_index: u8 },
    /// `show_existing_frame` named an empty slot — nothing to display.
    MissingShowExisting { slot: u8 },
    /// The OBU walk stopped early. The plan covers what was read; `offset` is
    /// where it stopped.
    TruncatedAu { offset: usize },
}

impl PlanWarning {
    /// Does this warning mean the picture is damaged? Same contract as
    /// [`crate::h264::PlanWarning::is_integrity`]; every backend conceals on this.
    ///
    /// Every AV1 variant is damage: the codec has no reorder envelope and no MMCO
    /// to report, so the only warnings are missing or wrong pictures and a walk
    /// that stopped early. `MissingShowExisting` is damage too — the stream chose
    /// a picture that was lost, so the screen keeps the previous one. Exhaustive,
    /// no wildcard.
    pub fn is_integrity(&self) -> bool {
        match self {
            PlanWarning::MissingReference { .. }
            | PlanWarning::StaleReference { .. }
            | PlanWarning::MissingShowExisting { .. }
            | PlanWarning::TruncatedAu { .. } => true,
        }
    }
}

/// Why an access unit cannot be planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// No frame header — nothing to decode or display.
    NoFrame,
    /// A frame arrived before any sequence header. Every dimension, depth and
    /// colour value lives there.
    NoSequenceHeader,
    Parse(String),
    /// A frame outside this decoder's envelope.
    Unsupported(&'static str),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::NoFrame => write!(f, "the access unit carried no frame header"),
            PlanError::NoSequenceHeader => {
                write!(f, "a frame arrived before any sequence header")
            }
            PlanError::Parse(e) => write!(f, "AV1 parse: {e}"),
            PlanError::Unsupported(what) => write!(f, "outside the envelope: {what}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// The AV1 planner: vendored parser plus this crate's reference ledger.
pub struct Av1Planner {
    parser: Parser,
    /// Slot → the picture it holds. AV1's whole reference model.
    slots: [Option<RefPic>; NUM_REF_SLOTS],
    next_id: PicId,
    sequence: Option<Rc<SequenceHeaderObu>>,
    /// Resident pictures that came off a broken reference chain. Empty on a
    /// healthy stream; see [`crate::clean::CleanLedger`].
    clean: crate::clean::CleanLedger,
}

impl Default for Av1Planner {
    fn default() -> Self {
        Self::new()
    }
}

impl Av1Planner {
    pub fn new() -> Av1Planner {
        Av1Planner {
            parser: Parser::default(),
            slots: [None; NUM_REF_SLOTS],
            next_id: 1,
            sequence: None,
            clean: Default::default(),
        }
    }

    pub fn dpb_refs(&self) -> Vec<RefPic> {
        self.slots.iter().flatten().copied().collect()
    }

    /// An intra refresh wave finished on a picture the freeze gate accepted: forget the
    /// unclean marks, which a chain through half-refreshed pictures cannot clear itself.
    /// Whole-ledger, unlike H.264/H.265: an inter frame names seven slots and which of
    /// them a wave host's frames leave on older pictures is not measured.
    pub fn forgive_unclean(&mut self) {
        self.clean.clear();
    }

    /// Plan one temporal unit. May carry several frames, so this returns a `Vec`
    /// where the H.264/H.265 siblings return one plan.
    ///
    /// A unit may hold a hidden frame and the `show_existing_frame` that displays
    /// it, or several scalability-layer frames. Planning only the last header
    /// would drop the others with nothing to say so. Plans come back in decode
    /// order; each carries its own reference set and store update.
    pub fn plan_au(&mut self, au: &[u8]) -> Result<Vec<AuPlan>, PlanError> {
        let mut warnings = Vec::new();
        let mut plans: Vec<AuPlan> = Vec::new();
        let mut pending: Option<(FrameHeaderObu, Vec<TilePlan>)> = None;
        let mut consumed = 0usize;

        while consumed < au.len() {
            let action = match self.parser.read_obu(&au[consumed..]) {
                Ok(action) => action,
                Err(e) => {
                    // A malformed OBU with data behind it is concealment, not a
                    // parse failure — same as a truncated NALU walk — but only
                    // once something has been read. Nothing at all is a hard error.
                    if pending.is_some() || !plans.is_empty() {
                        warnings.push(PlanWarning::TruncatedAu { offset: consumed });
                        break;
                    }
                    return Err(PlanError::Parse(e));
                }
            };
            let obu = match action {
                ObuAction::Process(obu) => obu,
                ObuAction::Drop(n) => {
                    consumed += n as usize;
                    continue;
                }
            };
            let used = obu.bytes_used;
            // Payload as a range in this AU so a backend can hand the driver
            // bytes without re-parsing.
            let obu_start = consumed;
            consumed += used;

            // A new header ends the previous frame; its tile groups are all in.
            // Plan it before the parse: planning runs the 7.20 update, and the
            // new header reads the order hints and sizes that update writes.
            if matches!(obu.header.obu_type, ObuType::Frame | ObuType::FrameHeader) {
                if let Some((h, t)) = pending.take() {
                    plans.push(self.plan_one(h, t, std::mem::take(&mut warnings))?);
                }
            }

            match self.parser.parse_obu(obu) {
                Ok(ParsedObu::SequenceHeader(seq)) => self.sequence = Some(seq),
                Ok(ParsedObu::FrameHeader(fh)) => pending = Some((fh, Vec::new())),
                Ok(ParsedObu::Frame(frame)) => {
                    // A Frame OBU is a header plus its first tile group. It stays
                    // open like a bare header: 5.10 lets further tile-group OBUs
                    // follow it.
                    let tile = TilePlan {
                        data: obu_start..consumed,
                        tg_start: frame.tile_group.tg_start,
                        tg_end: frame.tile_group.tg_end,
                    };
                    pending = Some((frame.header, vec![tile]));
                }
                Ok(ParsedObu::TileGroup(tg)) => {
                    let tile = TilePlan {
                        data: obu_start..consumed,
                        tg_start: tg.tg_start,
                        tg_end: tg.tg_end,
                    };
                    match pending.as_mut() {
                        Some((_, tiles)) => tiles.push(tile),
                        // Tiles with no header ahead of them: the header was
                        // lost. Dropped, not guessed — no picture to attach to.
                        None => warnings.push(PlanWarning::TruncatedAu { offset: obu_start }),
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if pending.is_some() || !plans.is_empty() {
                        warnings.push(PlanWarning::TruncatedAu { offset: obu_start });
                        break;
                    }
                    return Err(PlanError::Parse(e));
                }
            }
        }

        if let Some((h, t)) = pending.take() {
            plans.push(self.plan_one(h, t, std::mem::take(&mut warnings))?);
        }
        if plans.is_empty() {
            return Err(PlanError::NoFrame);
        }
        // Warnings raised after the last frame was planned (a truncated tail)
        // still belong to this AU; attach them to the frame they cut short.
        if !warnings.is_empty() {
            if let Some(last) = plans.last_mut() {
                last.warnings.append(&mut warnings);
            }
        }
        Ok(plans)
    }

    fn plan_one(
        &mut self,
        header: FrameHeaderObu,
        tiles: Vec<TilePlan>,
        warnings: Vec<PlanWarning>,
    ) -> Result<AuPlan, PlanError> {
        let sequence = self.sequence.clone().ok_or(PlanError::NoSequenceHeader)?;
        self.plan_frame(header, sequence, tiles, warnings)
    }

    fn plan_frame(
        &mut self,
        header: FrameHeaderObu,
        sequence: Rc<SequenceHeaderObu>,
        tiles: Vec<TilePlan>,
        mut warnings: Vec<PlanWarning>,
    ) -> Result<AuPlan, PlanError> {
        // Shared with the plan: backends need the whole header and there is no
        // reason for each to own a copy of a struct this size.
        let header = Rc::new(header);
        let dpb_refs = self.dpb_refs();

        // Displays a slot; decodes nothing.
        if header.show_existing_frame {
            let slot = header.frame_to_show_map_idx;
            let shown = self.slots.get(usize::from(slot)).copied().flatten();
            if shown.is_none() {
                warnings.push(PlanWarning::MissingShowExisting { slot });
            }
            // The parser loaded the shown key frame's state (7.21) and left
            // the 7.20 slot write to us, as for a decoded frame. Skipping it
            // leaves stale sizes and order hints behind every later inter
            // frame. A shown non-key frame refreshes nothing; the call is a no-op.
            if let Err(e) = self.parser.ref_frame_update(&header) {
                return Err(PlanError::Parse(e));
            }
            // Showing a key frame this way resets the whole reference store
            // (7.20). Same slot writer as an ordinary refresh so removals have
            // one place.
            let removed = if header.frame_type == FrameType::KeyFrame {
                match shown {
                    // Every refreshed slot takes the *shown* picture's state
                    // (7.20), not this header's — a show_existing_frame header
                    // carries none of its own.
                    Some(pic) => self.refresh_slots(0xff, pic.id, pic.state),
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };
            // The only picture on screen is the one displayed; a missing slot
            // already warned and shows nothing, which is damage — report unclean.
            let references_clean = match shown {
                Some(pic) => self.clean.references_clean([pic.id]),
                None => false,
            };
            let picture = picture_plan(&header, &sequence, references_clean);
            // Key-frame show_existing rewrote every slot with `pic.id` (7.20).
            // Nothing new is stored; only residency to re-bound.
            self.clean
                .retain_live(self.slots.iter().flatten().map(|p| p.id));
            return Ok(AuPlan {
                picture,
                tiles,
                refs: [None; REFS_PER_FRAME],
                dpb: DpbUpdate {
                    stored: None,
                    outputs: shown.map(|p| p.id).into_iter().collect(),
                    removed,
                },
                dpb_refs,
                header: header.clone(),
                warnings,
                sequence,
            });
        }

        let mut refs = [None; REFS_PER_FRAME];
        if !matches!(
            header.frame_type,
            FrameType::KeyFrame | FrameType::IntraOnlyFrame
        ) {
            // An error-resilient frame codes the order hint it expects in every
            // slot (5.9.2). A lost inter frame leaves the older picture in its
            // slot, so occupancy alone never sees the loss; the hint does. The
            // Vulkan host codes its recovery frame this way.
            let hints_coded = header.error_resilient_mode && sequence.enable_order_hint;
            for (ref_index, &slot) in header.ref_frame_idx.iter().enumerate() {
                // Seven references; the cast cannot truncate.
                let ref_index_u8 = ref_index as u8;
                match self.slots.get(usize::from(slot)).copied().flatten() {
                    Some(pic) => {
                        refs[ref_index] = Some(pic);
                        if hints_coded
                            && header.ref_order_hint[usize::from(slot)] != pic.state.order_hint
                        {
                            warnings.push(PlanWarning::StaleReference {
                                slot,
                                ref_index: ref_index_u8,
                            });
                        }
                    }
                    None => warnings.push(PlanWarning::MissingReference {
                        slot,
                        ref_index: ref_index_u8,
                    }),
                }
            }
        }

        let id = self.next_id;
        self.next_id += 1;

        // The parser keeps its own reference state (sizes, order hints) and
        // must be updated even if our ledger is unhappy, or later inter frames
        // fail to parse.
        if let Err(e) = self.parser.ref_frame_update(&header) {
            return Err(PlanError::Parse(e));
        }
        let removed = self.refresh_slots(header.refresh_frame_flags, id, RefState::of(&header));

        // Resolved names, and no damage of this frame's own: a hole or a stale
        // slot is a warning above, and a consumer reading the bit alone must
        // not lift onto a concealed picture. Vacuous for key and intra-only
        // (`CleanLedger::references_clean`).
        let references_clean = self
            .clean
            .references_clean(refs.iter().flatten().map(|r| r.id))
            && !warnings.iter().any(PlanWarning::is_integrity);

        let picture = picture_plan(&header, &sequence, references_clean);
        let outputs = if header.show_frame {
            vec![id]
        } else {
            Vec::new()
        };
        // After `refresh_slots`, so the live set includes this frame's writes.
        // `concealed` uses the one classification (`PlanWarning::is_integrity`)
        // so the ledger and the consumer cannot disagree.
        self.clean.note_stored(
            id,
            references_clean,
            warnings.iter().any(PlanWarning::is_integrity),
        );
        self.clean
            .retain_live(self.slots.iter().flatten().map(|p| p.id));

        Ok(AuPlan {
            picture,
            tiles,
            refs,
            dpb: DpbUpdate {
                stored: Some(id),
                outputs,
                removed,
            },
            dpb_refs,
            header: header.clone(),
            warnings,
            sequence,
        })
    }

    /// Write `id` into every slot `refresh_frame_flags` names, and report
    /// pictures that no longer occupy *any* slot.
    ///
    /// One picture routinely occupies several slots (a key frame refreshes all
    /// eight). Overwriting one slot does not mean the picture is gone; reporting
    /// it removed while another slot still holds it would free a surface the
    /// next frame still references.
    fn refresh_slots(
        &mut self,
        refresh_frame_flags: u32,
        id: PicId,
        state: RefState,
    ) -> Vec<PicId> {
        let mut displaced: Vec<PicId> = Vec::new();
        for slot in 0..NUM_REF_SLOTS {
            if refresh_frame_flags & (1 << slot) == 0 {
                continue;
            }
            if let Some(old) = self.slots[slot] {
                if !displaced.contains(&old.id) {
                    displaced.push(old.id);
                }
            }
            self.slots[slot] = Some(RefPic {
                id,
                // Eight slots; the cast cannot truncate.
                slot: slot as u8,
                state,
            });
        }
        displaced.retain(|gone| !self.slots.iter().flatten().any(|held| held.id == *gone));
        displaced
    }
}

/// What an ISOBMFF `av1C` record and a colour description take from a sequence header.
/// Colour codes are ITU-T H.273; 2 (unspecified) when the header codes none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceInfo {
    pub profile: u8,
    /// Operating point 0's `seq_level_idx`.
    pub level_idx0: u8,
    /// Operating point 0's `seq_tier`.
    pub tier0: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub mono_chrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
    pub max_width: u32,
    pub max_height: u32,
}

/// The first sequence header in `obus`, a low-overhead temporal unit or any run of sized
/// OBUs. None when it carries none, or the walk hits a malformed OBU first.
pub fn sequence_info(obus: &[u8]) -> Option<SequenceInfo> {
    let mut parser = Parser::default();
    let mut consumed = 0usize;
    while consumed < obus.len() {
        let obu = match parser.read_obu(&obus[consumed..]).ok()? {
            ObuAction::Process(obu) => obu,
            ObuAction::Drop(n) => {
                consumed += n as usize;
                continue;
            }
        };
        consumed += obu.bytes_used;
        if obu.header.obu_type != ObuType::SequenceHeader {
            continue;
        }
        let ParsedObu::SequenceHeader(seq) = parser.parse_obu(obu).ok()? else {
            return None;
        };
        let color = &seq.color_config;
        let op0 = &seq.operating_points[0];
        return Some(SequenceInfo {
            profile: seq.seq_profile as u8,
            level_idx0: op0.seq_level_idx,
            tier0: op0.seq_tier,
            high_bitdepth: color.high_bitdepth,
            twelve_bit: color.twelve_bit,
            mono_chrome: color.mono_chrome,
            subsampling_x: color.subsampling_x,
            subsampling_y: color.subsampling_y,
            chroma_sample_position: color.chroma_sample_position as u8,
            color_primaries: color.color_primaries as u8,
            transfer_characteristics: color.transfer_characteristics as u8,
            matrix_coefficients: color.matrix_coefficients as u8,
            full_range: color.color_range,
            max_width: u32::from(seq.max_frame_width_minus_1) + 1,
            max_height: u32::from(seq.max_frame_height_minus_1) + 1,
        });
    }
    None
}

fn picture_plan(
    header: &FrameHeaderObu,
    sequence: &SequenceHeaderObu,
    references_clean: bool,
) -> PicturePlan {
    let color = &sequence.color_config;
    let bit_depth = if color.high_bitdepth {
        if color.twelve_bit {
            12
        } else {
            10
        }
    } else {
        8
    };
    // AV1 spells sampling as two flags plus monochrome; backends decide
    // formats in H.264's vocabulary, so the translation happens once here.
    let chroma_format_idc = match (color.mono_chrome, color.subsampling_x, color.subsampling_y) {
        (true, _, _) => 0,
        (false, true, true) => 1,
        (false, true, false) => 2,
        (false, false, false) => 3,
        // 4:4:0 (subsampling_y only) has no AV1 profile; report it as 4 rather
        // than silently calling it 4:2:0, and let the backend refuse it.
        (false, false, true) => 4,
    };
    PicturePlan {
        frame_type: header.frame_type,
        is_key: header.frame_type == FrameType::KeyFrame,
        show_frame: header.show_frame,
        showable_frame: header.showable_frame,
        order_hint: header.order_hint,
        upscaled_width: header.upscaled_width,
        frame_width: header.frame_width,
        frame_height: header.frame_height,
        render_width: header.render_width,
        render_height: header.render_height,
        bit_depth,
        chroma_format_idc,
        colour: ColourDescription {
            colour_primaries: color.color_primaries as u8,
            transfer_characteristics: color.transfer_characteristics as u8,
            matrix_coefficients: color.matrix_coefficients as u8,
            video_full_range: color.color_range,
        },
        references_clean,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::AV1_25FPS;
    use cros_codecs::bitstream_utils::IvfIterator;

    /// `PF_AV1_DUMP=<capture>` with its `.idx`: print each frame's type, entropy source and
    /// references, to read an encoder's recovery frames off a capture. Diagnostics only.
    #[test]
    #[ignore = "diagnostic: set PF_AV1_DUMP to a capture"]
    fn print_frame_headers_of_a_capture() {
        let path = std::env::var("PF_AV1_DUMP").expect("PF_AV1_DUMP=<capture>");
        let bytes = std::fs::read(&path).expect("read the capture");
        let idx = std::fs::read_to_string(format!("{path}.idx")).expect("read the .idx");
        let mut planner = Av1Planner::new();
        for (i, line) in idx.lines().enumerate() {
            let f: Vec<usize> = line
                .split_whitespace()
                .take(2)
                .filter_map(|s| s.parse().ok())
                .collect();
            let [off, len] = f[..] else { continue };
            match planner.plan_au(&bytes[off..off + len]) {
                Ok(plans) => {
                    for p in plans {
                        let h = &p.header;
                        println!(
                            "{i}: {:?} er {} primary {} refs {:?} refresh {:#04x} warnings {:?}",
                            h.frame_type,
                            h.error_resilient_mode,
                            h.primary_ref_frame,
                            h.ref_frame_idx,
                            h.refresh_frame_flags,
                            p.warnings
                        );
                    }
                }
                Err(e) => println!("{i}: {e}"),
            }
        }
    }

    /// An SVT-AV1 320x180 keyframe's temporal delimiter and sequence-header OBU: Main, level 0,
    /// 8-bit 4:2:0, no colour description, studio range. The Apple client's AV1Tests uses it too.
    const SVT_KEY_HEAD: [u8; 15] = [
        0x12, 0x00, 0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xfe, 0xcc, 0x4a, 0xf9, 0x00, 0x40,
    ];

    #[test]
    fn sequence_info_reads_what_av1c_needs() {
        let want = SequenceInfo {
            profile: 0,
            level_idx0: 0,
            tier0: 0,
            high_bitdepth: false,
            twelve_bit: false,
            mono_chrome: false,
            subsampling_x: true,
            subsampling_y: true,
            chroma_sample_position: 0,
            color_primaries: 2,
            transfer_characteristics: 2,
            matrix_coefficients: 2,
            full_range: false,
            max_width: 320,
            max_height: 180,
        };
        assert_eq!(sequence_info(&SVT_KEY_HEAD), Some(want), "a temporal unit");
        assert_eq!(
            sequence_info(&SVT_KEY_HEAD[2..]),
            Some(want),
            "the bare OBU"
        );
        for cut in 2..SVT_KEY_HEAD.len() {
            assert_eq!(sequence_info(&SVT_KEY_HEAD[..cut]), None, "cut at {cut}");
        }
        let first = IvfIterator::new(AV1_25FPS).next().expect("a temporal unit");
        assert!(
            sequence_info(first).is_some(),
            "the vendored vector's first unit"
        );
    }

    /// Walk the whole vector and check the plan is self-consistent at every frame.
    ///
    /// The extra frames are hidden — decoded, not displayed, referenced later. A
    /// planner that took only the last header in each unit would drop those
    /// references; damage would surface later as missing-reference concealment
    /// on frames that were never damaged.
    ///
    /// A header reads the reference state the frame before it wrote (7.20), also
    /// when both share a temporal unit. 24 units of this vector carry two frames.
    #[test]
    fn order_hints_name_the_pictures_the_planner_resolved() {
        let mut planner = Av1Planner::new();
        let mut checked = 0usize;
        for (unit, packet) in IvfIterator::new(AV1_25FPS).enumerate() {
            for plan in planner.plan_au(packet).unwrap() {
                for (name, r) in plan.refs.iter().enumerate() {
                    let Some(r) = r else { continue };
                    // `order_hints` is indexed by reference frame; `INTRA_FRAME` is 0.
                    assert_eq!(
                        plan.header.order_hints[name + 1],
                        r.state.order_hint,
                        "unit {unit}, reference name {name}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 0);
    }

    /// This vector has no `show_existing_frame`, so [`Av1Planner::plan_frame`]'s
    /// display-only path (including the key-frame slot reset) is untested here.
    #[test]
    fn the_whole_vendored_vector_plans_and_the_frame_count_is_the_parsers() {
        let mut planner = Av1Planner::new();
        let (mut units, mut frames, mut shown, mut show_existing) = (0u32, 0u32, 0u32, 0u32);
        let mut multi_frame_units = 0u32;
        let mut warnings = 0usize;
        let mut max_refs = 0usize;

        for packet in IvfIterator::new(AV1_25FPS) {
            units += 1;
            let plans = planner
                .plan_au(packet)
                .unwrap_or_else(|e| panic!("temporal unit {units}: {e}"));
            if plans.len() > 1 {
                multi_frame_units += 1;
            }
            for plan in &plans {
                frames += 1;
                warnings += plan.warnings.len();
                shown += plan.dpb.outputs.len() as u32;
                if plan.dpb.stored.is_none() {
                    show_existing += 1;
                    assert!(
                        plan.tiles.is_empty(),
                        "a show_existing_frame decodes nothing and can carry no tiles"
                    );
                }
                max_refs = max_refs.max(plan.refs.iter().flatten().count());

                for tile in &plan.tiles {
                    assert!(
                        tile.data.start < tile.data.end && tile.data.end <= packet.len(),
                        "frame {frames}: tile range {:?} is not inside a {}-byte unit",
                        tile.data,
                        packet.len()
                    );
                }
                for (name, r) in plan.refs.iter().enumerate() {
                    let Some(r) = r else { continue };
                    assert!(usize::from(r.slot) < NUM_REF_SLOTS);
                    assert_eq!(
                        r.slot, plan.header.ref_frame_idx[name],
                        "frame {frames}: reference name {name} holds the picture in \
                         slot {}, but ref_frame_idx[{name}] names slot {}",
                        r.slot, plan.header.ref_frame_idx[name]
                    );
                    assert!(
                        plan.dpb_refs.iter().any(|d| d.id == r.id),
                        "frame {frames}: reference {} is not in the marked store",
                        r.id
                    );
                }
            }
        }

        assert_eq!(units, 250, "the vendored vector is 250 temporal units");
        assert_eq!(
            frames, 274,
            "the parser's own golden is 274 frames; a planner that sees fewer is \
             dropping frames a multi-frame temporal unit carried"
        );
        assert_eq!(
            multi_frame_units, 24,
            "the 24 units carrying two frames are the whole reason plan_au returns a \
             vector; if this reaches 0 the count above is being met some other way"
        );
        assert_eq!(
            warnings, 0,
            "a clean conformance vector must plan without concealment"
        );
        assert_eq!(
            shown, 250,
            "one displayed frame per temporal unit — the other 24 are hidden"
        );
        assert_eq!(
            show_existing, 0,
            "this vector uses no show_existing_frame; if that ever changes, the \
             display-only path stops being untested and the doc above must say so"
        );
        assert_eq!(
            max_refs, REFS_PER_FRAME,
            "an inter frame names all seven references"
        );
    }

    /// A picture can hold several slots at once; losing one of them must not
    /// report the picture as removed. See [`Av1Planner::refresh_slots`].
    #[test]
    fn a_picture_held_by_several_slots_is_not_removed_until_the_last_one_goes() {
        let mut planner = Av1Planner::new();
        let at = |order_hint: u32| RefState {
            order_hint,
            ..RefState::of(&FrameHeaderObu::default())
        };
        let removed = planner.refresh_slots(0xff, 1, at(0));
        assert!(removed.is_empty(), "nothing was there to displace");
        assert_eq!(planner.dpb_refs().len(), NUM_REF_SLOTS);

        let removed = planner.refresh_slots(0b0000_0001, 2, at(1));
        assert!(
            removed.is_empty(),
            "picture 1 still occupies seven slots — reporting it removed would free \
             a surface every later frame still references"
        );

        let removed = planner.refresh_slots(0b1111_1110, 3, at(2));
        assert_eq!(removed, vec![1], "reported once, not once per slot");

        let removed = planner.refresh_slots(0b0000_0001, 4, at(3));
        assert_eq!(removed, vec![2]);
    }

    /// A lost reference must leave a hole at its own name, not shorten the list.
    ///
    /// With a `Vec` of survivors, dropping the picture behind name 2 slides
    /// names 3..6 down one, and a backend that reads position-as-name then
    /// predicts LAST from the picture GOLDEN should have supplied.
    #[test]
    fn a_lost_reference_leaves_its_name_empty_and_does_not_renumber_the_others() {
        // First unit is a key frame: gives the parser its sequence header
        // (`ref_frame_update` needs one) and fills all eight slots.
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let sequence = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .sequence
            .clone();
        assert_eq!(planner.dpb_refs().len(), NUM_REF_SLOTS);

        // Empty the slot name 2 will point at — a reference lost upstream.
        planner.slots[5] = None;

        let header = FrameHeaderObu {
            frame_type: FrameType::InterFrame,
            ref_frame_idx: [0, 1, 5, 3, 4, 2, 6],
            // Refresh nothing: this frame is here to be planned, not to disturb
            // the ledger the assertions read.
            refresh_frame_flags: 0,
            ..Default::default()
        };
        let plan = planner
            .plan_frame(header, sequence, Vec::new(), Vec::new())
            .expect("an inter frame with a lost reference still plans");

        assert_eq!(
            plan.warnings,
            vec![PlanWarning::MissingReference {
                slot: 5,
                ref_index: 2
            }]
        );
        assert!(plan.refs[2].is_none(), "the lost name stays empty");
        let named: Vec<Option<u8>> = plan.refs.iter().map(|r| r.map(|p| p.slot)).collect();
        assert_eq!(
            named,
            vec![Some(0), Some(1), None, Some(3), Some(4), Some(2), Some(6)],
            "every surviving name must still sit at ITS OWN index — a compacted \
             list would read [0, 1, 3, 4, 2, 6] and rename four references"
        );
    }

    /// A lost inter frame leaves the older picture in its slot, so the slot is
    /// occupied and nothing else in the syntax can tell. An error-resilient
    /// frame codes the order hint it expects per slot (5.9.2); a mismatch is
    /// the loss, and the frame must read unclean.
    #[test]
    fn an_error_resilient_frame_exposes_a_stale_slot_through_its_order_hint() {
        let mut planner = Av1Planner::new();
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let sequence = planner
            .plan_au(first)
            .expect("the key frame plans")
            .first()
            .expect("a frame")
            .sequence
            .clone();
        assert!(sequence.enable_order_hint, "the vector codes order hints");
        // Every slot holds the key frame at order hint 0. The encoder's frame
        // expects the picture at hint 5 in slot 5: that frame was lost.
        let mut ref_order_hint = [0u32; NUM_REF_SLOTS];
        ref_order_hint[5] = 5;
        let header = FrameHeaderObu {
            frame_type: FrameType::InterFrame,
            error_resilient_mode: true,
            ref_order_hint,
            ref_frame_idx: [0, 1, 5, 3, 4, 2, 6],
            refresh_frame_flags: 0,
            ..Default::default()
        };
        let plan = planner
            .plan_frame(header, sequence.clone(), Vec::new(), Vec::new())
            .expect("a stale reference is concealment, not a plan error");
        assert_eq!(
            plan.warnings,
            vec![PlanWarning::StaleReference {
                slot: 5,
                ref_index: 2
            }]
        );
        assert!(
            plan.refs[2].is_some(),
            "the slot is occupied — by the wrong picture"
        );
        assert!(
            !plan.picture.references_clean,
            "a frame predicting from a stale slot must not lift a freeze"
        );

        // Without error resilience the hints are not coded and the same frame
        // plans clean: the check must not invent damage.
        let header = FrameHeaderObu {
            frame_type: FrameType::InterFrame,
            ref_order_hint,
            ref_frame_idx: [0, 1, 5, 3, 4, 2, 6],
            refresh_frame_flags: 0,
            ..Default::default()
        };
        let plan = planner
            .plan_frame(header, sequence, Vec::new(), Vec::new())
            .expect("plans");
        assert!(plan.warnings.is_empty());
        assert!(plan.picture.references_clean);
    }

    /// `RefFrameSignBias` must come out spec-indexed (bit 1 = `LAST_FRAME`);
    /// the vendored parser's array is not.
    ///
    /// Recomputed from `order_hints` — which the parser *does* index by
    /// reference name — through 5.9.3 `get_relative_dist`. Transcribed because
    /// cros-codecs' `helpers` module is private. Without the shift, ALTREF's
    /// bias lands on GOLDEN and `INTRA_FRAME` (bit 0) picks up LAST's.
    #[test]
    fn the_sign_bias_mask_is_spec_indexed_not_parser_indexed() {
        /// AV1 5.9.3 `get_relative_dist`, verbatim.
        fn get_relative_dist(enable_order_hint: bool, bits: i32, a: i32, b: i32) -> i32 {
            if !enable_order_hint {
                return 0;
            }
            let diff = a - b;
            let m = 1 << (bits - 1);
            (diff & (m - 1)) - (diff & m)
        }

        let mut planner = Av1Planner::new();
        let (mut frames, mut nonzero_masks, mut future_refs) = (0u32, 0u32, 0u32);
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                let h = &*plan.header;
                let seq = &*plan.sequence;
                let bits = seq.order_hint_bits_minus_1 + 1;
                let state = RefState::of(h);

                let mut expected = 0u8;
                if !h.frame_is_intra {
                    for name in 1..=REFS_PER_FRAME {
                        let dist = get_relative_dist(
                            seq.enable_order_hint,
                            bits,
                            h.order_hints[name] as i32,
                            h.order_hint as i32,
                        );
                        if dist > 0 {
                            expected |= 1 << name;
                            future_refs += 1;
                        }
                    }
                }
                assert_eq!(
                    state.ref_frame_sign_bias, expected,
                    "frame {frames}: sign-bias mask {:#010b} does not match the \
                     spec's own RefFrameSignBias[1..8] {expected:#010b}",
                    state.ref_frame_sign_bias
                );
                assert_eq!(
                    state.ref_frame_sign_bias & 1,
                    0,
                    "bit 0 is INTRA_FRAME and the spec never sets it — a set bit \
                     there is the parser's off-by-one leaking through"
                );
                if state.ref_frame_sign_bias != 0 {
                    nonzero_masks += 1;
                }
            }
        }
        assert_eq!(frames, 274);
        assert!(
            nonzero_masks > 0 && future_refs > 0,
            "this vector is the hidden-ALTREF one: if no frame ever biased a \
             reference into the future, this test compared zero against zero and \
             the shift above is untested"
        );
        eprintln!("frames {frames} · frames with a future reference {nonzero_masks}");
    }

    #[test]
    fn a_reference_carries_its_own_frame_type() {
        let mut planner = Av1Planner::new();
        let mut mixed = 0u32;
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                for r in plan.refs.iter().flatten() {
                    if r.state.frame_type != plan.header.frame_type {
                        mixed += 1;
                    }
                }
            }
        }
        assert!(
            mixed > 0,
            "no frame of the vector ever referenced a picture of a DIFFERENT frame \
             type, so nothing here can tell the reference's own type from the \
             current frame's — the exact substitution this field exists to prevent"
        );
    }

    /// [`coded_cdef_sec_strength`] inverts the spec's in-place fixup.
    ///
    /// The mapping is three lines; what makes it load-bearing is that the
    /// parser hands out `4` on real frames. If a re-synced vector stopped
    /// coding a secondary strength of 3, the two-bit hardware fields would
    /// be untested again.
    #[test]
    fn the_cdef_secondary_strength_is_the_coded_value() {
        assert_eq!(
            [0, 1, 2, 3, 4].map(coded_cdef_sec_strength),
            [0, 1, 2, 3, 3],
            "0..=2 pass through, the spec's 4 is the coded 3, and a hand-built 3 is \
             already coded"
        );

        let mut planner = Av1Planner::new();
        let (mut frames, mut needing_fixup, mut strengths) = (0u32, 0u32, 0u32);
        let mut frame0_raw: Vec<u32> = Vec::new();
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                let cdef = &plan.header.cdef_params;
                let coded = 1usize << cdef.cdef_bits;
                let mut any = false;
                for i in 0..coded {
                    for raw in [cdef.cdef_y_sec_strength[i], cdef.cdef_uv_sec_strength[i]] {
                        assert!(
                            raw <= 2 || raw == 4,
                            "frame {frames}: the parser can only hold 0, 1, 2 or the \
                             fixed-up 4 — {raw} means the vendored parse changed"
                        );
                        assert!(
                            coded_cdef_sec_strength(raw) <= 3,
                            "the corrected value must fit the two bits every hardware \
                             API gives it"
                        );
                        if raw == 4 {
                            any = true;
                            strengths += 1;
                        }
                    }
                }
                if any {
                    needing_fixup += 1;
                }
                if frames == 1 {
                    frame0_raw = cdef.cdef_y_sec_strength[..coded]
                        .iter()
                        .chain(cdef.cdef_uv_sec_strength[..coded].iter())
                        .copied()
                        .collect();
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            frame0_raw,
            vec![1, 2, 0, 4, 4, 0, 0, 0],
            "frame 0's four luma then four chroma secondary strengths — the first \
             frame the parity leg hashes, and it needs the correction"
        );
        assert_eq!(
            needing_fixup, 68,
            "68 of 274 frames of this vector carry a secondary strength the spec \
             fixed up; at zero the correction above is untested by any real stream"
        );
        eprintln!(
            "frames {frames} · frames needing the fixup {needing_fixup} · strengths \
             corrected {strengths}"
        );
    }

    #[test]
    fn an_access_unit_with_no_frame_is_refused() {
        let mut planner = Av1Planner::new();
        // A lone temporal delimiter: a valid OBU, no frame.
        assert_eq!(
            planner.plan_au(&[0x12, 0x00]).err(),
            Some(PlanError::NoFrame)
        );
    }

    /// A truncated access unit never panics the decode thread.
    ///
    /// `plan_au` degrades malformation to [`PlanWarning::TruncatedAu`] or
    /// [`PlanError`]. The contract is the crate's stated posture, not a
    /// specific verdict: a short AU is a plan error or a warning, and whatever
    /// plans come back stay inside the bytes handed in.
    #[test]
    fn a_truncated_access_unit_is_a_plan_error_not_a_panic() {
        let mut planned = 0usize;
        let mut rejected = 0usize;

        for packet in IvfIterator::new(AV1_25FPS).take(12) {
            for denom in [2usize, 3, 4, 8] {
                let cut = packet.len() - packet.len() / denom;
                // Fresh planner per cut: a short unit must fail cleanly on its
                // own terms, not because a planner carried state across one.
                let mut planner = Av1Planner::new();
                match planner.plan_au(&packet[..cut]) {
                    Ok(plans) => {
                        planned += 1;
                        for plan in &plans {
                            for tile in &plan.tiles {
                                assert!(
                                    tile.data.start <= tile.data.end && tile.data.end <= cut,
                                    "tile range {:?} escapes a {cut}-byte truncated unit",
                                    tile.data
                                );
                            }
                        }
                    }
                    Err(_) => rejected += 1,
                }
            }
        }

        assert!(
            planned + rejected == 48,
            "every cut must reach a verdict; got {planned} planned + {rejected} rejected"
        );
        assert!(
            rejected > 0,
            "no truncated unit was rejected - the test proves nothing"
        );
    }

    /// The two OBU header bits a host controls and this parser used to assert on.
    /// `parse_obu_header` runs before any type dispatch, so both are reachable on the
    /// first byte of any unit; an abort there takes the decoding thread with it.
    /// Bit 0 is `obu_reserved_1bit`, bit 1 `obu_has_size_field`.
    #[test]
    fn a_reserved_obu_header_bit_is_an_error_not_a_panic() {
        let first = IvfIterator::new(AV1_25FPS).next().expect("a first packet");

        for (label, byte) in [
            ("obu_reserved_1bit set", first[0] | 0x01),
            ("obu_has_size_field cleared", first[0] & !0x02),
        ] {
            let mut au = first.to_vec();
            au[0] = byte;
            let mut planner = Av1Planner::new();
            assert!(
                planner.plan_au(&au).is_err(),
                "{label} must be a plan error, not a panic"
            );
        }

        // The unmodified unit still plans, or the two cases above prove nothing.
        assert!(
            Av1Planner::new().plan_au(first).is_ok(),
            "the pristine first packet must still plan"
        );
    }
}
