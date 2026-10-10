// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file (vendor/cros-codecs/LICENSE).

// Port of vendor/cros-codecs `decoder/stateless/h264.rs` (PROVENANCE.md).
// Spec 8.2.1–8.2.5 and C.4.5.3 keep upstream structure so diffs stay
// legible. Stripped: decoder plumbing and interlaced field-split
// (the envelope gate rejects those streams).

//! Per-AU H.264 planning. [`H264Planner::plan_au`] turns one Annex-B
//! access unit (parameter sets plus the slices of one picture) into an
//! [`AuPlan`]: parsed headers, POC, per-slice reference lists
//! (long-term/MMCO included), and the DPB delta.
//!
//! A `frame_num` gap or a missing DPB reference is a [`PlanWarning`],
//! not an error: planning continues and the session requests recovery.
//! [`PlanError`] is only for AUs that cannot be planned at all.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::h264::dpb::Dpb;
use cros_codecs::codec::h264::dpb::DpbEntry;
use cros_codecs::codec::h264::dpb::DpbPicRefList;
use cros_codecs::codec::h264::dpb::MmcoError;
use cros_codecs::codec::h264::dpb::ReferencePicLists;
use cros_codecs::codec::h264::parser::MaxLongTermFrameIdx;
use cros_codecs::codec::h264::parser::Nalu;
use cros_codecs::codec::h264::parser::NaluType;
use cros_codecs::codec::h264::parser::Parser;
use cros_codecs::codec::h264::parser::Pps;
use cros_codecs::codec::h264::parser::RefPicListModification;
use cros_codecs::codec::h264::parser::Slice;
use cros_codecs::codec::h264::parser::SliceType;
use cros_codecs::codec::h264::parser::Sps;
use cros_codecs::codec::h264::picture::Field;
use cros_codecs::codec::h264::picture::FieldRank;
use cros_codecs::codec::h264::picture::IsIdr;
use cros_codecs::codec::h264::picture::PictureData;
use cros_codecs::codec::h264::picture::RcPictureData;
use cros_codecs::codec::h264::picture::Reference;
use cros_codecs::Resolution;
use tracing::trace;

pub use cros_codecs::codec::h264::parser::Level;
pub use cros_codecs::codec::h264::parser::SliceHeader;

use crate::sei;
pub use crate::sei::RecoveryPoint;

/// Backend DPB-slot identity. Live-DPB `Vec` indices shift on bumping; do not expose them.
pub type PicId = u64;

#[derive(Debug, Clone)]
pub struct AuPlan {
    pub picture: PicturePlan,
    pub slices: Vec<SlicePlan>,
    pub dpb: DpbUpdate,
    /// Marked DPB at begin-picture, not this AU's reference lists.
    ///
    /// Captured after 8.2.5.2 gap placeholders and any IDR drain, before this
    /// AU's 8.2.5 marking. A picture named here may still appear in
    /// [`DpbUpdate::removed`]: it was a valid reference for this decode and
    /// unmarked at the end.
    ///
    /// DXVA `RefFrameList` is the pictures currently marked used for
    /// reference. A long-term picture that no slice names must stay here — a
    /// driver may treat absence as "no longer a reference". Vulkan
    /// `pReferenceSlots` is this operation's slots; the native rung binds the
    /// AU's own set.
    ///
    /// 8.2.5.2 placeholders have no [`PicId`] and are omitted. Pictures held
    /// only for output (already unmarked) are omitted. Order is DPB storage
    /// order; backends may reorder.
    pub dpb_refs: Vec<RefPic>,
    pub warnings: Vec<PlanWarning>,
    /// SPS activated for this AU: the first slice's PPS's SPS. A later slice
    /// may name another PPS; that drift does not reach here. Cloned from the
    /// parser table so backends never re-parse the AU.
    pub sps: Rc<Sps>,
    /// PPS the picture began with (first slice). Same `Rc` as [`Self::sps`].
    pub pps: Rc<Pps>,
}

/// Picture parameters after 8.2.1 POC and before end-of-picture marking.
#[derive(Debug, Clone)]
pub struct PicturePlan {
    pub is_idr: bool,
    pub nal_ref_idc: u8,
    pub is_reference: bool,
    pub frame_num: u16,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    /// PicOrderCnt: min of top/bottom for a frame.
    pub pic_order_cnt: i32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Conformance-window crop (7.4.2.1.1), luma samples of the coded picture.
    pub display_crop: DisplayCrop,
    /// Colour from the active SPS VUI (E.2.1 inference when absent). Per
    /// picture, never latched at session start: a new SPS can switch colour
    /// mid-stream.
    pub colour: ColourDescription,
    pub profile_idc: u8,
    pub level_idc: Level,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub chroma_format_idc: u8,
    /// DPB size in frames (A.3.1). Backends size their slot pool from this.
    pub max_dpb_frames: usize,
    pub recovery_point: Option<RecoveryPoint>,
    /// Whether every picture this AU predicts from came off an intact chain.
    /// `true` for an IDR and for a fully-clean chain; `false` once this AU or
    /// an ancestor needed concealment. Observation only — does not change the
    /// plan, warnings, or DPB. See [`crate::clean`].
    pub references_clean: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayCrop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// H.273 code points from the active SPS VUI. Absent VUI (or its colour
/// blocks) yields E.2.1 inference: 2/2/2 unspecified, limited range — never
/// struct-zero (0 is a reserved code point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColourDescription {
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    /// `video_full_range_flag` (E.2.1 infers limited range when absent).
    pub video_full_range: bool,
}

#[derive(Debug, Clone)]
pub struct SlicePlan {
    /// Byte range of the slice NALU in the AU, start code included. Points; does not copy.
    pub data: Range<usize>,
    /// [`Self::data`] from the NAL header on, start code dropped. The three
    /// bytes before it are always `00 00 01`.
    pub nal: Range<usize>,
    pub header: SliceHeader,
    pub ref_list0: Vec<RefPic>,
    pub ref_list1: Vec<RefPic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub id: PicId,
    /// 8.2.1 field order counts. Keep the pair: a nonzero
    /// `delta_pic_order_cnt_bottom` makes them differ even on a progressive
    /// frame. After MMCO 5 these are the rebased values later AUs key by
    /// (8.2.5.4.5).
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub is_long_term: bool,
    /// `frame_num` (short-term) or `LongTermFrameIdx` (long-term). DXVA and Vulkan key by this pair.
    pub frame_num_or_lt_idx: u16,
}

#[derive(Debug, Clone, Default)]
pub struct DpbUpdate {
    /// Id assigned to this AU's picture; allocate a surface for it.
    pub stored: Option<PicId>,
    /// Display-ready pictures, in C.4.5.3 bumping order.
    pub outputs: Vec<PicId>,
    /// Never referenced again; free once displayed.
    pub removed: Vec<PicId>,
}

/// Concealment: planning continues; the session requests recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// `frame_num` skipped; at least one reference AU was lost.
    FrameNumGap { expected: u16, got: u16 },
    MissingReference {
        context: &'static str,
        detail: String,
    },
    /// NALU walk stopped early (malformed NALU or a slice of another picture).
    /// The plan covers only slices before the cut; `offset` is that byte.
    TruncatedAu { offset: usize },
    /// MMCO 5 (8.2.5.4.5): DPB drained and the current picture's stored
    /// frame_num/POC rebased to zero after the plan was captured.
    /// [`PicturePlan`] holds the pre-rebase 8.2.1 values; later AUs key the
    /// picture by the rebased pair on [`RefPic`].
    Mmco5Rebase,
    /// No VUI `bitstream_restriction`; DPB sized from A.3.1's level ceiling,
    /// which can exceed what a mainstream decoder provides. Warning, not a
    /// clamp: see [`dpb_limit`].
    LevelDerivedDpb {
        max_dpb_frames: usize,
        level_idc: u8,
    },
}

impl PlanWarning {
    /// Whether the picture is damaged (a substitute for something lost) rather
    /// than a spec-legal envelope fact.
    ///
    /// `true` means conceal the output and mark the picture unclean in
    /// [`crate::clean::CleanLedger`]. Lives on the enum so those two consumers
    /// cannot disagree. `Mmco5Rebase` and `LevelDerivedDpb` are not damage.
    ///
    /// Exhaustive match, no wildcard: a new variant must be classified or the
    /// build fails. A `_ => false` would report unclassified damage as clean.
    pub fn is_integrity(&self) -> bool {
        match self {
            PlanWarning::FrameNumGap { .. }
            | PlanWarning::MissingReference { .. }
            | PlanWarning::TruncatedAu { .. } => true,
            PlanWarning::Mmco5Rebase | PlanWarning::LevelDerivedDpb { .. } => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Parse(String),
    /// Legal H.264 outside what punktfunk hosts emit. Stream-integrity failure, not a feature gap.
    OutsideEnvelope(&'static str),
    NoActiveParamSet {
        pps_id: u8,
    },
    /// [`H264Planner::flush`] discarded decoding state; planning resumes only at an IDR.
    AwaitingIdr,
}

impl PlanError {
    /// Nothing to plan until an IDR and its parameter sets land, as for a decoder built
    /// mid-GOP. Every decode rung treats this as idle, not a refusal.
    pub fn awaits_idr(&self) -> bool {
        matches!(
            self,
            PlanError::AwaitingIdr | PlanError::NoActiveParamSet { .. }
        )
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Parse(msg) => write!(f, "parse error: {msg}"),
            PlanError::OutsideEnvelope(what) => {
                write!(f, "outside the punktfunk decode envelope: {what}")
            }
            PlanError::NoActiveParamSet { pps_id } => {
                write!(f, "slice references PPS {pps_id}, which has not been seen")
            }
            PlanError::AwaitingIdr => {
                write!(f, "flushed: waiting for an IDR to resume planning")
            }
        }
    }
}

impl std::error::Error for PlanError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NegotiationInfo {
    coded_resolution: Resolution,
    profile_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    chroma_format_idc: u8,
    max_dpb_frames: usize,
    interlaced: bool,
}

impl From<&Sps> for NegotiationInfo {
    fn from(sps: &Sps) -> Self {
        NegotiationInfo {
            coded_resolution: Resolution::from((sps.width(), sps.height())),
            profile_idc: sps.profile_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: dpb_limit(sps),
            interlaced: !sps.frame_mbs_only_flag,
        }
    }
}

/// Mainstream hardware DPB slots: DXVA's H.264 picparams array is 16 entries;
/// Vulkan Video reports the same. Backends need one extra slot for the picture
/// in flight, so a 16-frame DPB demands 17 and is refused.
const MAINSTREAM_MAX_DPB_SLOTS: usize = 16;

/// DPB size in frames. Same question as [`crate::h265::dpb_limit`], different
/// answer: H.264 has no unconditional SPS field. The stream states its need
/// only in the VUI `bitstream_restriction` (`max_dec_frame_buffering`, E.2.1).
/// Absent that, A.3.1's level ceiling
/// `min(MaxDpbMbs / (PicWidthInMbs * FrameHeightInMbs), 16)` is the spec
/// inference — and it saturates at 16 whenever the picture is small relative
/// to the level, which is [`MAINSTREAM_MAX_DPB_SLOTS`] + 1 hardware slots.
///
/// Do not clamp. With the restriction present the value is the stream's own
/// statement; clamping a deep DPB would corrupt output. Without it there is
/// no honest smaller number — [`PlanWarning::LevelDerivedDpb`] names that
/// case instead.
fn dpb_limit(sps: &Sps) -> usize {
    // A.3.1 cap, VUI override, and `max_num_ref_frames` floor live in the parser.
    // This is the named place the result is interpreted.
    sps.max_dpb_frames()
}

/// True when [`dpb_limit`] used A.3.1's level ceiling, not a VUI `bitstream_restriction`.
fn dpb_is_level_derived(sps: &Sps) -> bool {
    !(sps.vui_parameters_present_flag && sps.vui_parameters.bitstream_restriction_flag)
}

#[derive(Copy, Clone, Debug)]
enum RefPicList {
    RefPicList0,
    RefPicList1,
}

/// Previous reference picture, for 8.2.1 POC.
struct PrevReferencePicInfo {
    frame_num: u32,
    has_mmco_5: bool,
    top_field_order_cnt: i32,
    pic_order_cnt_msb: i32,
    pic_order_cnt_lsb: i32,
    field: Field,
}

impl Default for PrevReferencePicInfo {
    fn default() -> Self {
        Self {
            frame_num: Default::default(),
            has_mmco_5: Default::default(),
            top_field_order_cnt: Default::default(),
            pic_order_cnt_msb: Default::default(),
            pic_order_cnt_lsb: Default::default(),
            field: Field::Frame,
        }
    }
}

impl PrevReferencePicInfo {
    fn fill(&mut self, pic: &PictureData) {
        self.has_mmco_5 = pic.has_mmco_5;
        self.top_field_order_cnt = pic.top_field_order_cnt;
        self.pic_order_cnt_msb = pic.pic_order_cnt_msb;
        self.pic_order_cnt_lsb = pic.pic_order_cnt_lsb;
        self.field = pic.field;
        self.frame_num = pic.frame_num;
    }
}

/// Previous picture, for 8.2.1 POC.
#[derive(Default)]
struct PrevPicInfo {
    frame_num: u32,
    frame_num_offset: u32,
    has_mmco_5: bool,
}

impl PrevPicInfo {
    fn fill(&mut self, pic: &PictureData) {
        self.frame_num = pic.frame_num;
        self.has_mmco_5 = pic.has_mmco_5;
        self.frame_num_offset = pic.frame_num_offset;
    }
}

/// `first_mb_in_slice` must increase across a picture's slices (7.4.3).
/// Upstream's comparison is inverted and fires on every well-formed slice.
/// `None`/vacant means no slice seen yet, so the first never trips it.
enum CurrentMacroblockTracking {
    SeparateColorPlane(BTreeMap<u8, u32>),
    NonSeparateColorPlane(Option<u32>),
}

struct CurrentPicState {
    pic: PictureData,
    /// PPS of the current slice. A later slice may name another PPS; this feeds end-of-picture marking.
    pps: Rc<Pps>,
    /// PPS the picture began with. [`H264Planner::picture_plan`] reads this so
    /// per-picture parameters cannot drift to a later slice's PPS.
    first_slice_pps: Rc<Pps>,
    id: PicId,
    /// Reference lists, derived once per picture and indexed per slice.
    ref_pic_lists: ReferencePicLists,
    /// Marked DPB for this picture's decode, captured beside the lists; see [`AuPlan::dpb_refs`].
    dpb_refs: Vec<RefPic>,
    current_macroblock: CurrentMacroblockTracking,
}

/// Plans H.264 access units for a stateless hardware decoder.
/// One instance per elementary stream; feed AUs in decode order.
#[derive(Default)]
pub struct H264Planner {
    parser: Parser,
    negotiation_info: NegotiationInfo,
    dpb: Dpb<PicId>,
    prev_ref_pic_info: PrevReferencePicInfo,
    prev_pic_info: PrevPicInfo,
    max_long_term_frame_idx: MaxLongTermFrameIdx,
    next_pic_id: PicId,
    /// Set by [`Self::flush`]; planning resumes only at an IDR.
    awaiting_idr: bool,
    /// Outputs, `removed` baseline and the unclean marks behind
    /// [`PicturePlan::references_clean`].
    report: crate::report::AuReporter,
}

impl H264Planner {
    pub fn new() -> Self {
        Default::default()
    }

    /// Plan one Annex-B access unit: parameter sets plus the slices of exactly one
    /// picture. After [`PlanError`] state is best-effort; request an IDR. Outputs
    /// and removals queued by a failed AU ride out on the next plan or [`Self::flush`].
    pub fn plan_au(&mut self, au: &[u8]) -> Result<AuPlan, PlanError> {
        let mut warnings = Vec::new();
        let mut slices = Vec::new();
        let mut recovery_point = None;
        let mut current: Option<CurrentPicState> = None;
        let mut saw_nalu = false;

        let mut cursor = Cursor::new(au);
        loop {
            let nalu = match Nalu::next(&mut cursor) {
                Ok(nalu) => nalu,
                Err(_) => {
                    // Header parse failed, or end of AU. A start code past the
                    // cursor means data was cut off: warn and keep slices already
                    // planned. No start code is trailing padding — emulation
                    // prevention forbids 00 00 01 in a NALU payload.
                    let pos = (cursor.position() as usize).min(au.len());
                    if au[pos..].windows(3).any(|w| w == [0x00, 0x00, 0x01]) {
                        warnings.push(PlanWarning::TruncatedAu { offset: pos });
                    }
                    break;
                }
            };
            saw_nalu = true;
            // After `Nalu::next` the cursor is on the NAL header. `offset` is
            // start-code length, `size` is payload length: range without a copy.
            let nalu_offset = cursor.position() as usize;
            let range = (nalu_offset - nalu.offset)..(nalu_offset + nalu.size);
            debug_assert_eq!(&au[range.clone()], nalu.data.as_ref());

            match nalu.header.type_ {
                NaluType::Sps => {
                    let sps = self.parser.parse_sps(&nalu).map_err(PlanError::Parse)?;
                    Self::check_envelope(sps)?;
                }
                NaluType::Pps => {
                    self.parser.parse_pps(&nalu).map_err(PlanError::Parse)?;
                }
                NaluType::Sei => {
                    match sei::parse_recovery_point(nalu.as_ref().get(1..).unwrap_or(&[])) {
                        Ok(Some(rp)) => recovery_point = Some(rp),
                        Ok(None) => {}
                        // A broken SEI must not cost the picture it decorates.
                        Err(err) => trace!("ignoring unparseable SEI NALU: {err}"),
                    }
                }
                NaluType::Slice | NaluType::SliceIdr => {
                    // After a flush, only an IDR restarts the decoding process.
                    if current.is_none() && self.awaiting_idr && !nalu.header.idr_pic_flag {
                        return Err(PlanError::AwaitingIdr);
                    }
                    let slice = match self.parser.parse_slice_header(nalu) {
                        Ok(slice) => slice,
                        Err(err) => return Err(Self::slice_parse_error(err)),
                    };
                    match &current {
                        None => {
                            current = Some(self.begin_picture(&slice, &mut warnings)?);
                            // Only once the IDR plans: one that fails (an unknown
                            // PPS) leaves the DPB empty, and the P behind it would
                            // otherwise plan on no reference and be stored as one.
                            self.awaiting_idr = false;
                        }
                        // One picture per AU: a second `first_mb_in_slice == 0` is a mis-split.
                        Some(_) if slice.header.first_mb_in_slice == 0 => {
                            return Err(PlanError::OutsideEnvelope(
                                "more than one coded picture in one access unit",
                            ));
                        }
                        Some(cur) => {
                            // Continuation slice must share frame_num and IDR-ness
                            // with the first (7.4.3). A foreign slice and everything
                            // after it are dropped as [`PlanWarning::TruncatedAu`].
                            if u32::from(slice.header.frame_num) != cur.pic.frame_num
                                || slice.nalu.header.idr_pic_flag
                                    != matches!(cur.pic.is_idr, IsIdr::Yes { .. })
                            {
                                warnings.push(PlanWarning::TruncatedAu {
                                    offset: range.start,
                                });
                                break;
                            }
                        }
                    }
                    let cur = current.as_mut().expect("a picture was begun above");
                    slices.push(self.plan_slice(cur, slice, range, &mut warnings)?);
                }
                NaluType::SliceDpa | NaluType::SliceDpb | NaluType::SliceDpc => {
                    return Err(PlanError::OutsideEnvelope("data-partitioned slices"));
                }
                other => trace!("skipping NAL unit type {other:?}"),
            }
        }

        if !saw_nalu {
            return Err(PlanError::Parse("no NAL units in access unit".into()));
        }
        let cur = current
            .ok_or_else(|| PlanError::Parse("access unit contains no coded picture".into()))?;

        // An IDR has empty lists, and so does a concealed picture — its own
        // warnings keep it unclean, so a consumer reading the bit alone cannot
        // lift onto it.
        let concealed = warnings.iter().any(PlanWarning::is_integrity);
        let references_clean = self.report.references_clean(
            slices
                .iter()
                .flat_map(|s: &SlicePlan| s.ref_list0.iter().chain(&s.ref_list1))
                .map(|r| r.id),
            concealed,
        );
        // Before `finish_picture`: MMCO 5 rewrites stored POC after this, but
        // backends submit the 8.2.1 values.
        let picture = Self::picture_plan(&cur, recovery_point, references_clean);
        // Clone before `finish_picture` consumes `cur`.
        let pps = Rc::clone(&cur.first_slice_pps);
        let sps = Rc::clone(&pps.sps);
        let dpb_refs = cur.dpb_refs.clone();
        let stored = self.finish_picture(cur, &mut warnings)?;
        // `finish_picture` can add an integrity warning; the mark reads them all.
        let concealed = warnings.iter().any(PlanWarning::is_integrity);
        let dpb = self
            .report
            .close(stored, self.live_ids(), references_clean, concealed);

        Ok(AuPlan {
            picture,
            slices,
            dpb,
            dpb_refs,
            warnings,
            sps,
            pps,
        })
    }

    /// An intra refresh wave closed on the picture just planned and the freeze gate
    /// accepted it: that picture is whole by overwrite, which its reference chain cannot
    /// show. Only its mark goes; the half-swept pictures before it stay damaged, so a
    /// later picture or a host anchor that still leans on one is held and the client
    /// asks for the IDR. Every P this decoder meets names one active reference, the
    /// picture before it, so a healthy stream reads clean from the close on.
    pub fn forgive_unclean(&mut self) {
        self.report.forgive_last();
    }

    /// Drain the DPB and discard 8.2.1/8.2.5 state. Planning resumes only at
    /// an IDR ([`PlanError::AwaitingIdr`]). Parameter sets survive (7.4.1.2).
    pub fn flush(&mut self) -> DpbUpdate {
        let live = self.live_ids();
        self.drain_dpb();

        self.prev_ref_pic_info = Default::default();
        self.prev_pic_info = Default::default();
        self.max_long_term_frame_idx = Default::default();
        self.negotiation_info = Default::default();
        self.awaiting_idr = true;
        self.report.flush(live)
    }

    /// Reject interlaced and separate-colour-plane streams; hosts never emit them.
    fn check_envelope(sps: &Sps) -> Result<(), PlanError> {
        if !sps.frame_mbs_only_flag {
            return Err(PlanError::OutsideEnvelope(
                "interlaced stream (frame_mbs_only_flag == 0)",
            ));
        }
        if sps.separate_colour_plane_flag {
            return Err(PlanError::OutsideEnvelope(
                "separate colour plane coding (separate_colour_plane_flag == 1)",
            ));
        }
        // A.3.1 caps at 16; the VUI `max_dec_frame_buffering` is an unbounded
        // ue(v) the parser reads uncapped. Gate here at SPS activation: backends
        // size slot pools from this number.
        if dpb_limit(sps) > MAINSTREAM_MAX_DPB_SLOTS {
            return Err(PlanError::OutsideEnvelope(
                "DPB deeper than 16 frames (max_dec_frame_buffering)",
            ));
        }
        Ok(())
    }

    /// Map a missing-PPS parse string to [`PlanError::NoActiveParamSet`].
    /// The needle is the vendored parser's own wording, not ours; reword it here
    /// and every missing-PPS error degrades to `Parse`.
    fn slice_parse_error(err: String) -> PlanError {
        match err.split_once("PPS for pic_parameter_set_id ") {
            Some((_, id)) => PlanError::NoActiveParamSet {
                pps_id: id.trim().parse().unwrap_or(0),
            },
            None => PlanError::Parse(err),
        }
    }

    /// Ids currently in the DPB. 8.2.5.2 gap placeholders carry none.
    fn live_ids(&self) -> BTreeSet<PicId> {
        self.dpb
            .entries()
            .iter()
            .filter_map(|entry| entry.reference)
            .collect()
    }

    fn compute_pic_order_count(
        &mut self,
        pic: &mut PictureData,
        sps: &Sps,
    ) -> Result<(), PlanError> {
        match pic.pic_order_cnt_type {
            // Spec 8.2.1.1
            0 => {
                let prev_pic_order_cnt_msb;
                let prev_pic_order_cnt_lsb;

                if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    prev_pic_order_cnt_lsb = 0;
                    prev_pic_order_cnt_msb = 0;
                } else if self.prev_ref_pic_info.has_mmco_5 {
                    if !matches!(self.prev_ref_pic_info.field, Field::Bottom) {
                        prev_pic_order_cnt_msb = 0;
                        prev_pic_order_cnt_lsb = self.prev_ref_pic_info.top_field_order_cnt;
                    } else {
                        prev_pic_order_cnt_msb = 0;
                        prev_pic_order_cnt_lsb = 0;
                    }
                } else {
                    prev_pic_order_cnt_msb = self.prev_ref_pic_info.pic_order_cnt_msb;
                    prev_pic_order_cnt_lsb = self.prev_ref_pic_info.pic_order_cnt_lsb;
                }

                let max_pic_order_cnt_lsb = 1 << (sps.log2_max_pic_order_cnt_lsb_minus4 + 4);

                // 8.2.1.1 compares against the derived prevPicOrderCntLsb (0 or
                // previous TopFieldOrderCnt after MMCO 5) in both wrap branches.
                // Upstream reads the raw stored lsb in the first; this follows the spec.
                pic.pic_order_cnt_msb = if (pic.pic_order_cnt_lsb < prev_pic_order_cnt_lsb)
                    && (prev_pic_order_cnt_lsb - pic.pic_order_cnt_lsb >= max_pic_order_cnt_lsb / 2)
                {
                    prev_pic_order_cnt_msb + max_pic_order_cnt_lsb
                } else if (pic.pic_order_cnt_lsb > prev_pic_order_cnt_lsb)
                    && (pic.pic_order_cnt_lsb - prev_pic_order_cnt_lsb > max_pic_order_cnt_lsb / 2)
                {
                    prev_pic_order_cnt_msb - max_pic_order_cnt_lsb
                } else {
                    prev_pic_order_cnt_msb
                };

                // Saturating: `delta_pic_order_cnt_bottom` is an unbounded se(v).
                if !matches!(pic.field, Field::Bottom) {
                    pic.top_field_order_cnt =
                        pic.pic_order_cnt_msb.saturating_add(pic.pic_order_cnt_lsb);
                }

                if !matches!(pic.field, Field::Top) {
                    if matches!(pic.field, Field::Frame) {
                        pic.bottom_field_order_cnt = pic
                            .top_field_order_cnt
                            .saturating_add(pic.delta_pic_order_cnt_bottom);
                    } else {
                        pic.bottom_field_order_cnt =
                            pic.pic_order_cnt_msb.saturating_add(pic.pic_order_cnt_lsb);
                    }
                }
            }

            // Spec 8.2.1.2
            1 => {
                if self.prev_pic_info.has_mmco_5 {
                    self.prev_pic_info.frame_num_offset = 0;
                }

                if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    pic.frame_num_offset = 0;
                } else if self.prev_pic_info.frame_num > pic.frame_num {
                    pic.frame_num_offset =
                        self.prev_pic_info.frame_num_offset + sps.max_frame_num();
                } else {
                    pic.frame_num_offset = self.prev_pic_info.frame_num_offset;
                }

                let mut abs_frame_num = if sps.num_ref_frames_in_pic_order_cnt_cycle != 0 {
                    pic.frame_num_offset + pic.frame_num
                } else {
                    0
                };

                if pic.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }

                let mut expected_pic_order_cnt = 0;

                if abs_frame_num > 0 {
                    if sps.num_ref_frames_in_pic_order_cnt_cycle == 0 {
                        return Err(PlanError::Parse(
                            "invalid num_ref_frames_in_pic_order_cnt_cycle".into(),
                        ));
                    }

                    let pic_order_cnt_cycle_cnt =
                        (abs_frame_num - 1) / sps.num_ref_frames_in_pic_order_cnt_cycle as u32;
                    let frame_num_in_pic_order_cnt_cycle =
                        (abs_frame_num - 1) % sps.num_ref_frames_in_pic_order_cnt_cycle as u32;
                    expected_pic_order_cnt =
                        pic_order_cnt_cycle_cnt as i32 * sps.expected_delta_per_pic_order_cnt_cycle;

                    assert!(frame_num_in_pic_order_cnt_cycle < 255);

                    // Upstream sums the full cycle; 8.2.1.2 sums
                    // `frame_num_in_pic_order_cnt_cycle + 1` entries. Ported as-is:
                    // hosts emit pic_order_cnt_type 0 only.
                    let cycle = usize::from(sps.num_ref_frames_in_pic_order_cnt_cycle);
                    for offset in &sps.offset_for_ref_frame[..cycle] {
                        expected_pic_order_cnt += offset;
                    }
                }

                if pic.nal_ref_idc == 0 {
                    expected_pic_order_cnt += sps.offset_for_non_ref_pic;
                }

                if matches!(pic.field, Field::Frame) {
                    pic.top_field_order_cnt = expected_pic_order_cnt + pic.delta_pic_order_cnt0;

                    pic.bottom_field_order_cnt = pic.top_field_order_cnt
                        + sps.offset_for_top_to_bottom_field
                        + pic.delta_pic_order_cnt1;
                } else if !matches!(pic.field, Field::Bottom) {
                    pic.top_field_order_cnt = expected_pic_order_cnt + pic.delta_pic_order_cnt0;
                } else {
                    pic.bottom_field_order_cnt = expected_pic_order_cnt
                        + sps.offset_for_top_to_bottom_field
                        + pic.delta_pic_order_cnt0;
                }
            }

            // Spec 8.2.1.3
            2 => {
                if self.prev_pic_info.has_mmco_5 {
                    self.prev_pic_info.frame_num_offset = 0;
                }

                if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    pic.frame_num_offset = 0;
                } else if self.prev_pic_info.frame_num > pic.frame_num {
                    pic.frame_num_offset =
                        self.prev_pic_info.frame_num_offset + sps.max_frame_num();
                } else {
                    pic.frame_num_offset = self.prev_pic_info.frame_num_offset;
                }

                let pic_order_cnt = if matches!(pic.is_idr, IsIdr::Yes { .. }) {
                    0
                } else if pic.nal_ref_idc == 0 {
                    2 * (pic.frame_num_offset + pic.frame_num) as i32 - 1
                } else {
                    2 * (pic.frame_num_offset + pic.frame_num) as i32
                };

                if matches!(pic.field, Field::Frame | Field::Top) {
                    pic.top_field_order_cnt = pic_order_cnt;
                }
                if matches!(pic.field, Field::Frame | Field::Bottom) {
                    pic.bottom_field_order_cnt = pic_order_cnt;
                }
            }

            _ => {
                return Err(PlanError::Parse(format!(
                    "invalid pic_order_cnt_type: {}",
                    sps.pic_order_cnt_type
                )))
            }
        }

        match pic.field {
            Field::Frame => {
                pic.pic_order_cnt =
                    std::cmp::min(pic.top_field_order_cnt, pic.bottom_field_order_cnt);
            }
            Field::Top => {
                pic.pic_order_cnt = pic.top_field_order_cnt;
            }
            Field::Bottom => {
                pic.pic_order_cnt = pic.bottom_field_order_cnt;
            }
        }

        Ok(())
    }

    /// Queue pictures C.4.5.3 bumping declares ready for output.
    fn bump_as_needed(&mut self, current_pic: &PictureData) {
        let bumped = self.dpb.bump_as_needed(current_pic);
        self.report
            .pending_outputs
            .extend(bumped.into_iter().flatten());
    }

    fn drain_dpb(&mut self) {
        let pics = self.dpb.drain();
        self.report
            .pending_outputs
            .extend(pics.into_iter().flatten());
    }

    /// Complementary first field, if any. Always `None` under the envelope
    /// (DPB never goes interlaced); kept as ported so the upstream diff stays small.
    fn find_first_field(
        &self,
        hdr: &SliceHeader,
    ) -> Result<Option<(RcPictureData, PicId)>, String> {
        let mut prev_field = None;

        if self.dpb.interlaced() {
            if let Some(last_dpb_entry) = self.dpb.entries().last() {
                let last_pic = last_dpb_entry.pic.borrow();

                if !matches!(last_pic.field, Field::Frame)
                    && matches!(last_pic.field_rank(), FieldRank::Single)
                {
                    if let Some(id) = &last_dpb_entry.reference {
                        prev_field = Some((last_dpb_entry.pic.clone(), *id));
                    }
                }
            }
        }

        let prev_field = match prev_field {
            None => return Ok(None),
            Some(prev_field) => prev_field,
        };

        let prev_field_pic = prev_field.0.borrow();

        if prev_field_pic.frame_num != u32::from(hdr.frame_num) {
            return Err(format!(
                "the previous field's frame_num value {} differs from the current one's {}",
                prev_field_pic.frame_num, hdr.frame_num
            ));
        }

        let cur_field = if hdr.bottom_field_flag {
            Field::Bottom
        } else {
            Field::Top
        };

        if !hdr.field_pic_flag || cur_field == prev_field_pic.field {
            let field = prev_field_pic.field;
            return Err(format!(
                "expected complementary field {:?}, got {:?}",
                field.opposite(),
                field
            ));
        }

        drop(prev_field_pic);
        Ok(Some(prev_field))
    }

    // 8.2.4.3.1 Modification process of reference picture lists for short-term
    // reference pictures
    #[allow(clippy::too_many_arguments)]
    fn short_term_pic_list_modification<'a>(
        cur_pic: &PictureData,
        dpb: &'a Dpb<PicId>,
        ref_pic_list_x: &mut DpbPicRefList<'a, PicId>,
        num_ref_idx_lx_active_minus1: u8,
        max_pic_num: i32,
        rplm: &RefPicListModification,
        pic_num_lx_pred: &mut i32,
        ref_idx_lx: &mut usize,
    ) -> Result<(), String> {
        let pic_num_lx_no_wrap;
        // 7.4.3.1: `0..MaxPicNum - 1`. Bounding it here keeps every sum below
        // within `2 * MaxPicNum`; an unbounded ue(v) would overflow them.
        if i64::from(rplm.abs_diff_pic_num_minus1) >= i64::from(max_pic_num) {
            return Err(format!(
                "abs_diff_pic_num_minus1 {} exceeds MaxPicNum {max_pic_num}",
                rplm.abs_diff_pic_num_minus1
            ));
        }
        let abs_diff_pic_num = rplm.abs_diff_pic_num_minus1 as i32 + 1;
        let modification_of_pic_nums_idc = rplm.modification_of_pic_nums_idc;

        if modification_of_pic_nums_idc == 0 {
            if *pic_num_lx_pred - abs_diff_pic_num < 0 {
                pic_num_lx_no_wrap = *pic_num_lx_pred - abs_diff_pic_num + max_pic_num;
            } else {
                pic_num_lx_no_wrap = *pic_num_lx_pred - abs_diff_pic_num;
            }
        } else if modification_of_pic_nums_idc == 1 {
            if *pic_num_lx_pred + abs_diff_pic_num >= max_pic_num {
                pic_num_lx_no_wrap = *pic_num_lx_pred + abs_diff_pic_num - max_pic_num;
            } else {
                pic_num_lx_no_wrap = *pic_num_lx_pred + abs_diff_pic_num;
            }
        } else {
            return Err(format!(
                "unexpected value for modification_of_pic_nums_idc {modification_of_pic_nums_idc:?}"
            ));
        }

        *pic_num_lx_pred = pic_num_lx_no_wrap;

        let pic_num_lx = if pic_num_lx_no_wrap > cur_pic.pic_num {
            pic_num_lx_no_wrap - max_pic_num
        } else {
            pic_num_lx_no_wrap
        };

        let handle = dpb
            .find_short_term_with_pic_num(pic_num_lx)
            .ok_or_else(|| format!("no ShortTerm reference found with pic_num {pic_num_lx}"))?;

        if *ref_idx_lx >= ref_pic_list_x.len() {
            return Err("invalid ref_idx_lx index".into());
        }
        ref_pic_list_x.insert(*ref_idx_lx, handle);
        *ref_idx_lx += 1;

        let mut nidx = *ref_idx_lx;

        for cidx in *ref_idx_lx..=usize::from(num_ref_idx_lx_active_minus1) + 1 {
            if cidx == ref_pic_list_x.len() {
                break;
            }

            let target = &ref_pic_list_x[cidx].pic;

            if target.borrow().pic_num_f(max_pic_num) != pic_num_lx {
                ref_pic_list_x[nidx] = ref_pic_list_x[cidx];
                nidx += 1;
            }
        }

        while ref_pic_list_x.len() > (usize::from(num_ref_idx_lx_active_minus1) + 1) {
            ref_pic_list_x.pop();
        }

        Ok(())
    }

    // 8.2.4.3.2 Modification process of reference picture lists for long-term
    // reference pictures
    fn long_term_pic_list_modification<'a>(
        dpb: &'a Dpb<PicId>,
        ref_pic_list_x: &mut DpbPicRefList<'a, PicId>,
        num_ref_idx_lx_active_minus1: u8,
        max_long_term_frame_idx: MaxLongTermFrameIdx,
        rplm: &RefPicListModification,
        ref_idx_lx: &mut usize,
    ) -> Result<(), String> {
        let long_term_pic_num = rplm.long_term_pic_num;

        let handle = dpb
            .find_long_term_with_long_term_pic_num(long_term_pic_num)
            .ok_or_else(|| {
                format!("no LongTerm reference found with long_term_pic_num {long_term_pic_num}")
            })?;

        if *ref_idx_lx >= ref_pic_list_x.len() {
            return Err("invalid ref_idx_lx index".into());
        }
        ref_pic_list_x.insert(*ref_idx_lx, handle);
        *ref_idx_lx += 1;

        let mut nidx = *ref_idx_lx;

        for cidx in *ref_idx_lx..=usize::from(num_ref_idx_lx_active_minus1) + 1 {
            if cidx == ref_pic_list_x.len() {
                break;
            }

            let target = &ref_pic_list_x[cidx].pic;
            if target.borrow().long_term_pic_num_f(max_long_term_frame_idx) != long_term_pic_num {
                ref_pic_list_x[nidx] = ref_pic_list_x[cidx];
                nidx += 1;
            }
        }

        while ref_pic_list_x.len() > (usize::from(num_ref_idx_lx_active_minus1) + 1) {
            ref_pic_list_x.pop();
        }

        Ok(())
    }

    fn modify_ref_pic_list(
        &self,
        cur_pic: &PictureData,
        hdr: &SliceHeader,
        ref_pic_list_type: RefPicList,
        ref_pic_list_indices: &[usize],
    ) -> Result<DpbPicRefList<'_, PicId>, String> {
        let (ref_pic_list_modification_flag_lx, num_ref_idx_lx_active_minus1, rplm) =
            match ref_pic_list_type {
                RefPicList::RefPicList0 => (
                    hdr.ref_pic_list_modification_flag_l0,
                    hdr.num_ref_idx_l0_active_minus1,
                    &hdr.ref_pic_list_modification_l0,
                ),
                RefPicList::RefPicList1 => (
                    hdr.ref_pic_list_modification_flag_l1,
                    hdr.num_ref_idx_l1_active_minus1,
                    &hdr.ref_pic_list_modification_l1,
                ),
            };

        let mut ref_pic_list: Vec<_> = ref_pic_list_indices
            .iter()
            .map(|&i| &self.dpb.entries()[i])
            .take(usize::from(num_ref_idx_lx_active_minus1) + 1)
            .collect();

        if !ref_pic_list_modification_flag_lx {
            return Ok(ref_pic_list);
        }

        let mut pic_num_lx_pred = cur_pic.pic_num;
        let mut ref_idx_lx = 0;

        for modification in rplm {
            let idc = modification.modification_of_pic_nums_idc;

            match idc {
                0 | 1 => {
                    Self::short_term_pic_list_modification(
                        cur_pic,
                        &self.dpb,
                        &mut ref_pic_list,
                        num_ref_idx_lx_active_minus1,
                        hdr.max_pic_num as i32,
                        modification,
                        &mut pic_num_lx_pred,
                        &mut ref_idx_lx,
                    )?;
                }
                2 => Self::long_term_pic_list_modification(
                    &self.dpb,
                    &mut ref_pic_list,
                    num_ref_idx_lx_active_minus1,
                    self.max_long_term_frame_idx,
                    modification,
                    &mut ref_idx_lx,
                )?,
                3 => break,
                _ => return Err(format!("unexpected modification_of_pic_nums_idc {idc:?}")),
            }
        }

        Ok(ref_pic_list)
    }

    /// Like [`Self::modify_ref_pic_list`], but a failed modification becomes
    /// [`PlanWarning::MissingReference`] plus the unmodified 8.2.4.2 initial list.
    /// Upstream aborts; a named-but-lost picture is concealment, not a session kill.
    fn modified_or_initial_list(
        &self,
        cur_pic: &PictureData,
        hdr: &SliceHeader,
        ref_pic_list_type: RefPicList,
        ref_pic_list_indices: &[usize],
        warnings: &mut Vec<PlanWarning>,
    ) -> DpbPicRefList<'_, PicId> {
        match self.modify_ref_pic_list(cur_pic, hdr, ref_pic_list_type, ref_pic_list_indices) {
            Ok(list) => list,
            Err(detail) => {
                warnings.push(PlanWarning::MissingReference {
                    context: "ref_pic_list_modification",
                    detail,
                });
                let num_ref_idx_lx_active_minus1 = match ref_pic_list_type {
                    RefPicList::RefPicList0 => hdr.num_ref_idx_l0_active_minus1,
                    RefPicList::RefPicList1 => hdr.num_ref_idx_l1_active_minus1,
                };
                ref_pic_list_indices
                    .iter()
                    .map(|&i| &self.dpb.entries()[i])
                    .take(usize::from(num_ref_idx_lx_active_minus1) + 1)
                    .collect()
            }
        }
    }

    /// RefPicList0/1 for one slice (8.2.4), as backend [`RefPic`]s.
    fn create_ref_pic_lists(
        &self,
        cur_pic: &PictureData,
        hdr: &SliceHeader,
        ref_pic_lists: &ReferencePicLists,
        warnings: &mut Vec<PlanWarning>,
    ) -> (Vec<RefPic>, Vec<RefPic>) {
        let ref_pic_list0 = match hdr.slice_type {
            SliceType::P | SliceType::Sp => self.modified_or_initial_list(
                cur_pic,
                hdr,
                RefPicList::RefPicList0,
                &ref_pic_lists.ref_pic_list_p0,
                warnings,
            ),
            SliceType::B => self.modified_or_initial_list(
                cur_pic,
                hdr,
                RefPicList::RefPicList0,
                &ref_pic_lists.ref_pic_list_b0,
                warnings,
            ),
            _ => Vec::new(),
        };

        let ref_pic_list1 = match hdr.slice_type {
            SliceType::B => self.modified_or_initial_list(
                cur_pic,
                hdr,
                RefPicList::RefPicList1,
                &ref_pic_lists.ref_pic_list_b1,
                warnings,
            ),
            _ => Vec::new(),
        };

        (
            Self::to_ref_pics(&ref_pic_list0, warnings),
            Self::to_ref_pics(&ref_pic_list1, warnings),
        )
    }

    /// Convert a reference list to [`RefPic`]s, preserving `ref_idx` positions.
    ///
    /// An 8.2.5.2 gap placeholder has no id, so it is replaced in place by the
    /// previous existing entry (else the first). Compacting would shift every
    /// later `ref_idx`. An all-placeholder list collapses to empty (caller warns).
    fn to_ref_pics(list: &[&DpbEntry<PicId>], warnings: &mut Vec<PlanWarning>) -> Vec<RefPic> {
        // Slot carries frame_num too: a long-term substitute is relabelled short-term.
        let mut slots: Vec<Option<(RefPic, u16)>> = Vec::with_capacity(list.len());
        for entry in list {
            let pic = entry.pic.borrow();
            match entry.reference {
                Some(id) => {
                    let is_long_term = matches!(pic.reference(), Reference::LongTerm);
                    let frame_num_or_lt_idx = if is_long_term {
                        // `long_term_frame_idx` is ue(v); spec bounds it via
                        // max_long_term_frame_idx, the parser does not. Saturate, do not truncate.
                        u16::try_from(pic.long_term_frame_idx).unwrap_or(u16::MAX)
                    } else {
                        pic.frame_num as u16
                    };
                    slots.push(Some((
                        RefPic {
                            id,
                            top_field_order_cnt: pic.top_field_order_cnt,
                            bottom_field_order_cnt: pic.bottom_field_order_cnt,
                            is_long_term,
                            frame_num_or_lt_idx,
                        },
                        pic.frame_num as u16,
                    )));
                }
                None => {
                    warnings.push(PlanWarning::MissingReference {
                        context: "non-existing picture (frame_num gap placeholder) in list",
                        detail: format!("frame_num {}", pic.frame_num),
                    });
                    slots.push(None);
                }
            }
        }

        let first_existing = slots.iter().flatten().next().copied();
        let mut out = Vec::with_capacity(slots.len());
        let mut prev_existing: Option<(RefPic, u16)> = None;
        for slot in &slots {
            match slot {
                Some(real) => {
                    prev_existing = Some(*real);
                    out.push(real.0);
                }
                None => {
                    // Keep the index. Placeholders are short-term (8.2.5.2); label
                    // the substitute short-term with its own frame_num.
                    if let Some((substitute, frame_num)) = prev_existing.or(first_existing) {
                        out.push(RefPic {
                            id: substitute.id,
                            top_field_order_cnt: substitute.top_field_order_cnt,
                            bottom_field_order_cnt: substitute.bottom_field_order_cnt,
                            is_long_term: false,
                            frame_num_or_lt_idx: frame_num,
                        });
                    }
                }
            }
        }
        out
    }

    /// Every operation runs, so one that names a lost picture cannot skip the marking after it.
    /// Unmarking a picture this DPB never held (MMCO 1/2) leaves it as the encoder's is: no error.
    /// The first other failure is returned once the list is done.
    fn handle_memory_management_ops(&mut self, pic: &mut PictureData) -> Result<(), MmcoError> {
        let markings = pic.ref_pic_marking.clone();
        let mut first_err = None;

        for marking in &markings.inner {
            let op = marking.memory_management_control_operation;
            let done = match op {
                0 => break,
                1 => self.dpb.mmco_op_1(pic, marking),
                2 => self.dpb.mmco_op_2(pic, marking),
                3 => self.dpb.mmco_op_3(pic, marking),
                4 => {
                    self.max_long_term_frame_idx = self.dpb.mmco_op_4(marking);
                    Ok(())
                }
                5 => {
                    self.max_long_term_frame_idx = self.dpb.mmco_op_5(pic);
                    Ok(())
                }
                6 => {
                    self.dpb.mmco_op_6(pic, marking);
                    Ok(())
                }
                other => Err(MmcoError::UnknownMmco(other)),
            };
            match done {
                Err(MmcoError::NoShortTermPic) if matches!(op, 1 | 2) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
                Ok(()) => {}
            }
        }

        first_err.map_or(Ok(()), Err)
    }

    fn reference_pic_marking(&mut self, pic: &mut PictureData, sps: &Sps) -> Result<(), MmcoError> {
        // 8.2.5.1
        if matches!(pic.is_idr, IsIdr::Yes { .. }) {
            self.dpb.mark_all_as_unused_for_ref();

            if pic.ref_pic_marking.long_term_reference_flag {
                pic.set_reference(Reference::LongTerm, false);
                pic.long_term_frame_idx = 0;
                self.max_long_term_frame_idx = MaxLongTermFrameIdx::Idx(0);
            } else {
                pic.set_reference(Reference::ShortTerm, false);
                self.max_long_term_frame_idx = MaxLongTermFrameIdx::NoLongTermFrameIndices;
            }

            return Ok(());
        }

        if pic.ref_pic_marking.adaptive_ref_pic_marking_mode_flag {
            self.handle_memory_management_ops(pic)?;
        } else {
            self.dpb.sliding_window_marking(pic, sps);
        }

        Ok(())
    }

    fn apply_sps(&mut self, sps: &Sps, warnings: &mut Vec<PlanWarning>) {
        self.negotiation_info = NegotiationInfo::from(sps);

        let max_dpb_frames = dpb_limit(sps);
        // Level-derived and larger than mainstream slots. Warn, do not clamp;
        // see [`dpb_limit`].
        if dpb_is_level_derived(sps) && max_dpb_frames + 1 > MAINSTREAM_MAX_DPB_SLOTS {
            warnings.push(PlanWarning::LevelDerivedDpb {
                max_dpb_frames,
                level_idc: sps.level_idc as u8,
            });
        }
        let interlaced = !sps.frame_mbs_only_flag;
        let max_num_order_frames = sps.max_num_order_frames() as usize;
        // 8.2.1.3: `pic_order_cnt_type` 2 makes output order the decoding order,
        // whatever the VUI states. NVENC codes it and states no bound.
        let max_num_reorder_frames =
            if sps.pic_order_cnt_type == 2 || max_num_order_frames > max_dpb_frames {
                0
            } else {
                max_num_order_frames
            };

        self.dpb.set_limits(max_dpb_frames, max_num_reorder_frames);
        self.dpb.set_interlaced(interlaced);
    }

    fn negotiation_possible(sps: &Sps, old_negotiation_info: &NegotiationInfo) -> bool {
        let negotiation_info = NegotiationInfo::from(sps);
        *old_negotiation_info != negotiation_info
    }

    fn renegotiate_if_needed(
        &mut self,
        sps: &Sps,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<(), PlanError> {
        if Self::negotiation_possible(sps, &self.negotiation_info) {
            Self::check_envelope(sps)?;
            // Drain before SPS parameters change under pictures still in the DPB.
            self.drain_dpb();
            self.apply_sps(sps, warnings);
        }

        Ok(())
    }

    fn handle_frame_num_gap(
        &mut self,
        sps: &Sps,
        frame_num: u32,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<(), PlanError> {
        if self.dpb.is_empty() {
            return Ok(());
        }

        trace!("frame_num gap detected");

        // Upstream refuses a gap when `gaps_in_frame_num_value_allowed_flag` is
        // unset. The caller already warned; 8.2.5.2 still runs so later
        // frame_num/pic_num bookkeeping stays spec-true.
        let mut unused_short_term_frame_num =
            (self.prev_ref_pic_info.frame_num + 1) % sps.max_frame_num();
        while unused_short_term_frame_num != frame_num {
            let max_frame_num = sps.max_frame_num();

            let mut pic = PictureData::new_non_existing(unused_short_term_frame_num, 0);
            self.compute_pic_order_count(&mut pic, sps)?;

            self.dpb
                .update_pic_nums(unused_short_term_frame_num, max_frame_num, &pic);

            self.dpb.sliding_window_marking(&mut pic, sps);

            self.bump_as_needed(&pic);

            // Envelope keeps the DPB progressive; no interlaced field-split.
            if let Err(err) = self.dpb.store_picture(pic.into_rc(), None) {
                // Full DPB: stop inserting placeholders rather than error the AU.
                // pic_num bookkeeping degrades; recovery from the warning heals it.
                warnings.push(PlanWarning::MissingReference {
                    context: "frame_num gap placeholder dropped (DPB full)",
                    detail: err.to_string(),
                });
                break;
            }

            unused_short_term_frame_num += 1;
            unused_short_term_frame_num %= max_frame_num;
        }

        Ok(())
    }

    fn init_current_pic(
        &mut self,
        slice: &Slice,
        sps: &Sps,
        first_field: Option<&RcPictureData>,
    ) -> Result<PictureData, PlanError> {
        let mut pic = PictureData::new_from_slice(slice, sps, 0, first_field);
        self.compute_pic_order_count(&mut pic, sps)?;

        if matches!(pic.is_idr, IsIdr::Yes { .. }) {
            // C.4.5.3 bumping, clause 2: IDR with no_output_of_prior_pics_flag != 1
            // (and not inferred 1; C.4.4).
            if !pic.ref_pic_marking.no_output_of_prior_pics_flag {
                self.drain_dpb();
            } else {
                // C.4.4: no_output_of_prior_pics_flag == 1 (or inferred): empty
                // the DPB without output.
                self.dpb.clear();
            }
        }

        self.dpb
            .update_pic_nums(u32::from(slice.header.frame_num), sps.max_frame_num(), &pic);

        Ok(pic)
    }

    fn begin_picture(
        &mut self,
        slice: &Slice,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<CurrentPicState, PlanError> {
        let hdr = &slice.header;
        let pps = Rc::clone(self.parser.get_pps(hdr.pic_parameter_set_id).ok_or(
            PlanError::NoActiveParamSet {
                pps_id: hdr.pic_parameter_set_id,
            },
        )?);

        self.renegotiate_if_needed(&pps.sps, warnings)?;

        let first_field = self.find_first_field(hdr).map_err(PlanError::Parse)?;

        let id = self.next_pic_id;
        self.next_pic_id += 1;

        if slice.nalu.header.idr_pic_flag {
            self.prev_ref_pic_info.frame_num = 0;
        }

        let frame_num = u32::from(hdr.frame_num);

        let current_macroblock = match pps.sps.separate_colour_plane_flag {
            true => CurrentMacroblockTracking::SeparateColorPlane(Default::default()),
            false => CurrentMacroblockTracking::NonSeparateColorPlane(None),
        };

        if frame_num != self.prev_ref_pic_info.frame_num
            && frame_num != (self.prev_ref_pic_info.frame_num + 1) % pps.sps.max_frame_num()
        {
            if !self.dpb.is_empty() {
                warnings.push(PlanWarning::FrameNumGap {
                    expected: ((self.prev_ref_pic_info.frame_num + 1) % pps.sps.max_frame_num())
                        as u16,
                    got: hdr.frame_num,
                });
            }
            self.handle_frame_num_gap(&pps.sps, frame_num, warnings)?;
        }

        let pic = self.init_current_pic(slice, &pps.sps, first_field.as_ref().map(|f| &f.0))?;
        let ref_pic_lists = self.dpb.build_ref_pic_lists(&pic);
        // Snapshot now, beside the 8.2.4 lists. `finish_picture` runs 8.2.5
        // marking; that DPB is what the next AU decodes against, not this one.
        let dpb_refs = self.dpb_snapshot();

        Ok(CurrentPicState {
            pic,
            first_slice_pps: Rc::clone(&pps),
            pps,
            id,
            ref_pic_lists,
            dpb_refs,
            current_macroblock,
        })
    }

    /// Marked DPB as [`AuPlan::dpb_refs`]: used-for-reference and carrying a
    /// [`PicId`]. Unmarked-but-held-for-output pictures and 8.2.5.2 placeholders
    /// are omitted.
    fn dpb_snapshot(&self) -> Vec<RefPic> {
        self.dpb
            .entries()
            .iter()
            .filter_map(|entry| {
                let id = entry.reference?;
                let pic = entry.pic.borrow();
                let is_long_term = match pic.reference() {
                    Reference::LongTerm => true,
                    Reference::ShortTerm => false,
                    Reference::None => return None,
                };
                Some(RefPic {
                    id,
                    top_field_order_cnt: pic.top_field_order_cnt,
                    bottom_field_order_cnt: pic.bottom_field_order_cnt,
                    is_long_term,
                    // Same pair-key as `to_ref_pics`: DXVA `FrameNumList` is
                    // LongTermFrameIdx for long-term, frame_num for short-term.
                    frame_num_or_lt_idx: if is_long_term {
                        u16::try_from(pic.long_term_frame_idx).unwrap_or(u16::MAX)
                    } else {
                        pic.frame_num as u16
                    },
                })
            })
            .collect()
    }

    // 7.4.3: first_mb_in_slice increases monotonically across the picture.
    fn check_first_mb_in_slice(current_macroblock: &mut CurrentMacroblockTracking, slice: &Slice) {
        let first_mb_in_slice = slice.header.first_mb_in_slice;
        match current_macroblock {
            CurrentMacroblockTracking::SeparateColorPlane(current_macroblock) => {
                match current_macroblock.entry(slice.header.colour_plane_id) {
                    Entry::Vacant(entry) => {
                        entry.insert(first_mb_in_slice);
                    }
                    Entry::Occupied(mut entry) => {
                        let current_macroblock = entry.get_mut();
                        if first_mb_in_slice <= *current_macroblock {
                            trace!(
                                "first_mb_in_slice does not increase monotonically, expect \
                                 corrupted output"
                            );
                        }
                        *current_macroblock = first_mb_in_slice;
                    }
                }
            }
            CurrentMacroblockTracking::NonSeparateColorPlane(current_macroblock) => {
                if current_macroblock.is_some_and(|current| first_mb_in_slice <= current) {
                    trace!(
                        "first_mb_in_slice does not increase monotonically, expect corrupted \
                         output"
                    );
                }
                *current_macroblock = Some(first_mb_in_slice);
            }
        }
    }

    fn plan_slice(
        &self,
        cur: &mut CurrentPicState,
        slice: Slice,
        data: Range<usize>,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<SlicePlan, PlanError> {
        Self::check_first_mb_in_slice(&mut cur.current_macroblock, &slice);

        let pps = self
            .parser
            .get_pps(slice.header.pic_parameter_set_id)
            .ok_or(PlanError::NoActiveParamSet {
                pps_id: slice.header.pic_parameter_set_id,
            })?;
        cur.pps = Rc::clone(pps);

        // No mid-picture renegotiation: it would drop the previous slices' context.
        if Self::negotiation_possible(&cur.pps.sps, &self.negotiation_info) {
            return Err(PlanError::Parse(
                "invalid stream: inter-picture renegotiation requested".into(),
            ));
        }

        let (ref_list0, ref_list1) =
            self.create_ref_pic_lists(&cur.pic, &slice.header, &cur.ref_pic_lists, warnings);

        // 8.2.4.2.1: an inter slice needs a usable reference. An all-placeholder
        // DPB yields an empty list with no per-entry warning; flag it here.
        let slice_type = slice.header.slice_type;
        if matches!(slice_type, SliceType::P | SliceType::Sp | SliceType::B) && ref_list0.is_empty()
        {
            warnings.push(PlanWarning::MissingReference {
                context: "inter slice with no usable RefPicList0",
                detail: format!("slice_type {slice_type:?}"),
            });
        }
        if matches!(slice_type, SliceType::B) && ref_list1.is_empty() {
            warnings.push(PlanWarning::MissingReference {
                context: "B slice with no usable RefPicList1",
                detail: format!("slice_type {slice_type:?}"),
            });
        }

        Ok(SlicePlan {
            nal: data.start + slice.nalu.offset..data.end,
            data,
            header: slice.header,
            ref_list0,
            ref_list1,
        })
    }

    fn add_to_ready_queue(&mut self, pic: PictureData, id: PicId) {
        if matches!(pic.field, Field::Frame) {
            self.report.pending_outputs.push(id);
        } else if let FieldRank::Second(..) = pic.field_rank() {
            self.report.pending_outputs.push(id);
        }
    }

    fn finish_picture(
        &mut self,
        cur: CurrentPicState,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<PicId, PlanError> {
        let CurrentPicState {
            mut pic, pps, id, ..
        } = cur;

        if matches!(pic.reference(), Reference::ShortTerm | Reference::LongTerm) {
            // Upstream aborts on a failed MMCO; a named-but-lost picture is a
            // warning here and the rest of the marking state stays usable.
            if let Err(err) = self.reference_pic_marking(&mut pic, &pps.sps) {
                warnings.push(PlanWarning::MissingReference {
                    context: "reference picture marking (MMCO)",
                    detail: err.to_string(),
                });
            }
            self.prev_ref_pic_info.fill(&pic);
        }

        self.prev_pic_info.fill(&pic);

        if pic.has_mmco_5 {
            warnings.push(PlanWarning::Mmco5Rebase);
            // C.4.5.3 bumping, clause 3: MMCO 5 (C.4.4).
            self.drain_dpb();
        }

        // C.4.5.3 bumping, clauses 1, 4, 5, 6.
        self.bump_as_needed(&pic);

        // C.4.5.1 / C.4.5.2: store a complementary-ref second field, a
        // reference picture, or a non-reference that still has an empty
        // buffer after bumping; otherwise queue for output.
        if pic.is_second_field_of_complementary_ref_pair()
            || pic.is_ref()
            || self.dpb.has_empty_frame_buffer()
        {
            // Upstream field-splits when interlaced; the envelope keeps that path unreachable.
            self.dpb
                .store_picture(pic.into_rc(), Some(id))
                .map_err(|err| PlanError::Parse(err.to_string()))?;
        } else {
            self.add_to_ready_queue(pic, id);
        }

        // Not C.4.5.3: that outputs on a full DPB only, which shows a zero-reorder
        // stream `max_dec_frame_buffering` pictures late.
        let ready = self.dpb.bump_past_reorder_bound();
        self.report
            .pending_outputs
            .extend(ready.into_iter().flatten());

        Ok(id)
    }

    fn picture_plan(
        cur: &CurrentPicState,
        recovery_point: Option<RecoveryPoint>,
        references_clean: bool,
    ) -> PicturePlan {
        let pic = &cur.pic;
        // First slice's PPS defines the picture; `cur.pps` may have drifted.
        let sps = &cur.first_slice_pps.sps;
        let rect = sps.visible_rectangle();

        PicturePlan {
            is_idr: matches!(pic.is_idr, IsIdr::Yes { .. }),
            nal_ref_idc: pic.nal_ref_idc,
            is_reference: pic.is_ref(),
            frame_num: pic.frame_num as u16,
            top_field_order_cnt: pic.top_field_order_cnt,
            bottom_field_order_cnt: pic.bottom_field_order_cnt,
            pic_order_cnt: pic.pic_order_cnt,
            coded_width: sps.width(),
            coded_height: sps.height(),
            // Vendored `visible_rectangle()`: `min` is crop offset, `max` is
            // visible size, not an edge. Subtracting double-counts and can underflow.
            display_crop: DisplayCrop {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.max.x,
                height: rect.max.y,
            },
            // Unconditional: the parser's `Default` VUI already holds E.2.1
            // inference (2/2/2, limited range); parse only overwrites under the
            // present flags.
            colour: ColourDescription {
                colour_primaries: sps.vui_parameters.colour_primaries,
                transfer_characteristics: sps.vui_parameters.transfer_characteristics,
                matrix_coefficients: sps.vui_parameters.matrix_coefficients,
                video_full_range: sps.vui_parameters.video_full_range_flag,
            },
            profile_idc: sps.profile_idc,
            level_idc: sps.level_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: dpb_limit(sps),
            recovery_point,
            references_clean,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use cros_codecs::codec::h264::parser::VuiParams;
    use cros_codecs::codec::h264::synthesizer::Synthesizer;

    use super::*;
    use crate::testing::h264::authored_sps_pps;
    use crate::testing::h264::base_sps;
    use crate::testing::h264::param_set_au;
    use crate::testing::h264::write_idr_slice;
    use crate::testing::h264::write_p_slice;
    use crate::testing::h264::write_slice;
    use crate::testing::h264::SliceSpec;
    use crate::testing::split_h264_aus;

    const TEST_25FPS: &[u8] = crate::testing::H264_25FPS;
    const TEST_64X64_I_P_B_P_HIGH: &[u8] = crate::testing::H264_64X64_I_P_B_P_HIGH;

    /// `nal` starts at the NAL header whatever prefix the encoder wrote, so
    /// the three bytes before it are always `00 00 01`.
    #[test]
    fn a_slices_nal_range_skips_a_three_or_four_byte_start_code() {
        let au = split_h264_aus(TEST_25FPS)[0];
        let plan = H264Planner::new().plan_au(au).expect("plans");
        let first = &plan.slices[0];
        assert_eq!(first.nal.start - first.data.start, 3);

        let mut four = au[..first.data.start].to_vec();
        four.push(0);
        four.extend_from_slice(&au[first.data.start..]);
        let plan = H264Planner::new().plan_au(&four).expect("plans");
        let first = &plan.slices[0];
        assert_eq!(first.nal.start - first.data.start, 4);
        for slice in &plan.slices {
            assert_eq!(slice.nal.end, slice.data.end);
            assert_eq!(four[slice.nal.start - 3..slice.nal.start], [0, 0, 1]);
        }
    }

    #[test]
    fn the_full_25fps_vector_plans_every_picture_and_every_pic_id_reaches_output() {
        let aus = split_h264_aus(TEST_25FPS);
        let mut planner = H264Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(
                planner
                    .plan_au(au)
                    .expect("the clean vector must plan without errors"),
            );
        }

        assert_eq!(plans.len(), 250);
        assert_eq!(plans.iter().map(|p| p.slices.len()).sum::<usize>(), 500);
        assert!(plans[0].picture.is_idr);
        assert!(plans.iter().all(|p| p.warnings.is_empty()));
        assert_eq!(
            (plans[0].picture.coded_width, plans[0].picture.coded_height),
            (320, 240),
            "coded size comes from the vector's SPS"
        );

        for plan in &plans {
            if plan.picture.is_idr {
                assert_eq!(plan.picture.pic_order_cnt, 0, "POC must reset at an IDR");
            }
            for slice in &plan.slices {
                if slice.header.slice_type.is_p() || slice.header.slice_type.is_b() {
                    assert!(!slice.ref_list0.is_empty());
                }
            }
        }

        let stored: BTreeSet<PicId> = plans.iter().filter_map(|p| p.dpb.stored).collect();
        assert_eq!(stored.len(), 250);
        let mut emitted: Vec<PicId> = plans
            .iter()
            .flat_map(|p| p.dpb.outputs.iter().copied())
            .collect();
        emitted.extend(planner.flush().outputs);
        let output: BTreeSet<PicId> = emitted.iter().copied().collect();
        assert_eq!(
            output, stored,
            "bumping plus the final flush must output every picture"
        );

        // Order, not just coverage: within an IDR period, ids emerge in
        // ascending POC (C.4.5.3). POC resets at IDR, hence the period key.
        let mut period = 0usize;
        let mut order_key: BTreeMap<PicId, (usize, i32)> = BTreeMap::new();
        for plan in &plans {
            if plan.picture.is_idr {
                period += 1;
            }
            order_key.insert(
                plan.dpb.stored.unwrap(),
                (period, plan.picture.pic_order_cnt),
            );
        }
        let mut last: Option<(usize, i32)> = None;
        for id in &emitted {
            let key = order_key[id];
            if let Some(last) = last {
                assert!(
                    key > last,
                    "outputs must emerge in ascending POC order per IDR period: \
                     {key:?} emitted after {last:?}"
                );
            }
            last = Some(key);
        }
    }

    #[test]
    fn b_slices_get_a_poc_ordered_list1_distinct_from_list0() {
        let aus = split_h264_aus(TEST_64X64_I_P_B_P_HIGH);
        let mut planner = H264Planner::new();
        let mut b_slices_seen = 0usize;

        for au in &aus {
            let plan = planner
                .plan_au(au)
                .expect("the clean vector must plan without errors");
            for slice in &plan.slices {
                if !slice.header.slice_type.is_b() {
                    continue;
                }
                b_slices_seen += 1;
                assert!(!slice.ref_list0.is_empty());
                assert!(!slice.ref_list1.is_empty());

                let ids0: Vec<PicId> = slice.ref_list0.iter().map(|r| r.id).collect();
                let ids1: Vec<PicId> = slice.ref_list1.iter().map(|r| r.id).collect();
                assert_ne!(ids0, ids1, "list1 must not be list0's ordering");

                // 8.2.4.2.3: list0 leads with the past, list1 with the future
                // (a frame's PicOrderCnt is the min of its field order counts).
                let poc = |r: &RefPic| r.top_field_order_cnt.min(r.bottom_field_order_cnt);
                assert!(poc(&slice.ref_list0[0]) < plan.picture.pic_order_cnt);
                assert!(poc(&slice.ref_list1[0]) > plan.picture.pic_order_cnt);
            }
        }

        assert!(b_slices_seen > 0, "the vector must contain B slices");
    }

    /// Picture-level warnings, dropping [`PlanWarning::LevelDerivedDpb`].
    /// [`base_sps`] is 64×64 with no VUI restriction, so A.3.1 saturates
    /// (`MaxDpbMbs(L1)=396` / 16 MBs) and every plan carries that warning.
    fn picture_warnings(plan: &AuPlan) -> Vec<&PlanWarning> {
        plan.warnings
            .iter()
            .filter(|w| !matches!(w, PlanWarning::LevelDerivedDpb { .. }))
            .collect()
    }

    #[test]
    fn mmco_marks_a_picture_long_term_later_lists_carry_it_and_mmco2_evicts_it() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());

        // AU1 marks itself long-term: MMCO 4 admits index 0, MMCO 6 assigns it.
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(4, 1), (6, 0)]));
        let au2 = write_p_slice(2, 4, 1, 2, None);
        // AU3 MMCO 2 unmarks long_term_pic_num 0. List depth 3 so presence is visible.
        let au3 = write_p_slice(3, 6, 1, 3, Some(&[(2, 0)]));
        let au4 = write_p_slice(4, 8, 0, 3, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();
        let p2 = planner.plan_au(&au2).unwrap();
        let p3 = planner.plan_au(&au3).unwrap();
        let p4 = planner.plan_au(&au4).unwrap();
        for plan in [&p0, &p1, &p2, &p3, &p4] {
            assert!(
                picture_warnings(plan).is_empty(),
                "authored stream must plan clean: {plan:?}"
            );
        }
        assert!(p0.picture.is_idr);
        assert_eq!(p2.picture.pic_order_cnt, 4);

        let idr_id = p0.dpb.stored.unwrap();
        let lt_id = p1.dpb.stored.unwrap();

        // After AU1's marking, AU2's list is [short-term IDR, long-term AU1]
        // (8.2.4.2.1: long-term after short-term).
        let list0 = &p2.slices[0].ref_list0;
        assert_eq!(list0.len(), 2);
        assert!(!list0[0].is_long_term);
        assert_eq!(list0[0].id, idr_id);
        assert!(list0[1].is_long_term);
        assert_eq!(list0[1].id, lt_id);
        assert_eq!(
            list0[1].frame_num_or_lt_idx, 0,
            "LongTermFrameIdx, not frame_num"
        );

        // Marking is end-of-picture (8.2.5): AU3's own list is built before
        // MMCO 2 and must still hold the long-term picture.
        let p2_id = p2.dpb.stored.unwrap();
        let p3_id = p3.dpb.stored.unwrap();
        assert_eq!(
            p3.slices[0]
                .ref_list0
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(p2_id, false), (idr_id, false), (lt_id, true)],
            "AU3 still sees the long-term ref; its own MMCO 2 applies only at finish"
        );

        // AU4: three short-terms, descending PicNum (8.2.4.2.1); the unmarked picture is gone.
        assert_eq!(
            p4.slices[0]
                .ref_list0
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(p3_id, false), (p2_id, false), (idr_id, false)],
            "the unmarked picture must have left, short-terms sorted by PicNum"
        );

        let flush = planner.flush();
        assert!(flush.outputs.contains(&lt_id));
        assert!(flush.removed.contains(&lt_id));
    }

    #[test]
    fn unmarking_a_lost_long_term_picture_is_clean_and_the_marking_after_it_runs() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        // AU1 drops long_term_pic_num 0, which only a lost AU had marked (an LTR recovery
        // rejecting the other slot), then pins itself long-term with MMCO 4 + 6.
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(2, 0), (4, 1), (6, 0)]));
        let au2 = write_p_slice(2, 4, 1, 2, None);

        let mut planner = H264Planner::new();
        planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();
        let p2 = planner.plan_au(&au2).unwrap();
        assert!(
            picture_warnings(&p1).is_empty(),
            "nothing lost reaches AU1: {p1:?}"
        );
        let lt_id = p1.dpb.stored.unwrap();
        assert!(
            p2.slices[0]
                .ref_list0
                .iter()
                .any(|r| r.id == lt_id && r.is_long_term),
            "MMCO 6 after the failed MMCO 2 must still run"
        );
    }

    #[test]
    fn the_dpb_snapshot_holds_a_marked_long_term_reference_no_slice_of_the_au_names() {
        // Long-term still marked, but this AU's list is too short to name it.
        // A RefFrameList built from derived lists would drop it; a driver may discard it.
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        // AU1 pins itself long-term (MMCO 4 admits index 0, MMCO 6 assigns it).
        let au1 = write_p_slice(1, 2, 1, 1, Some(&[(4, 1), (6, 0)]));
        // AU2 activates one reference: 8.2.4.2.1 puts the short-term IDR first, so
        // the truncated list never names the long-term picture.
        let au2 = write_p_slice(2, 4, 1, 1, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();
        let p2 = planner.plan_au(&au2).unwrap();
        for plan in [&p0, &p1, &p2] {
            assert!(
                picture_warnings(plan).is_empty(),
                "must plan clean: {plan:?}"
            );
        }
        let idr_id = p0.dpb.stored.unwrap();
        let lt_id = p1.dpb.stored.unwrap();

        assert_eq!(
            p2.slices[0]
                .ref_list0
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![idr_id],
            "the AU's own list reaches only the short-term picture"
        );
        // Snapshot still reports both; the long-term entry is keyed by LongTermFrameIdx.
        assert_eq!(
            p2.dpb_refs
                .iter()
                .map(|r| (r.id, r.is_long_term, r.frame_num_or_lt_idx))
                .collect::<Vec<_>>(),
            vec![(idr_id, false, 0), (lt_id, true, 0)]
        );

        // IDR sees an empty DPB; AU1 sees the IDR still short-term (marking is end-of-picture).
        assert!(p0.dpb_refs.is_empty());
        assert_eq!(
            p1.dpb_refs
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(idr_id, false)]
        );
    }

    #[test]
    fn the_dpb_snapshot_matches_the_planners_own_live_ids_across_the_whole_vector() {
        // Every snapshot id must already be live; a nameless surface cannot be resolved.
        // The converse is false: the DPB holds unmarked pictures for output.
        let mut planner = H264Planner::new();
        let mut plans = Vec::new();
        for au in split_h264_aus(TEST_25FPS) {
            let plan = planner.plan_au(au).expect("plan");
            plans.push(plan);
        }
        let mut live: BTreeSet<PicId> = BTreeSet::new();
        for plan in &plans {
            for r in &plan.dpb_refs {
                assert!(
                    live.contains(&r.id),
                    "snapshot names {} which no earlier plan stored",
                    r.id
                );
            }
            let mut ids: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            let count = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), count, "the snapshot must not repeat a picture");
            // Current picture is stored after the snapshot.
            assert!(!ids.contains(&plan.dpb.stored.unwrap()));
            live.insert(plan.dpb.stored.unwrap());
            for id in &plan.dpb.removed {
                live.remove(id);
            }
        }
        // IPPP… keeps references until the DPB is full, so most AUs have a non-empty snapshot.
        let non_empty = plans.iter().filter(|p| !p.dpb_refs.is_empty()).count();
        assert!(non_empty >= 240, "only {non_empty} AUs carried a snapshot");
    }

    #[test]
    fn a_dropped_reference_au_degrades_to_gap_warnings_and_planning_continues() {
        let aus = split_h264_aus(TEST_25FPS);

        // Find a non-IDR reference not followed by an IDR (an IDR would hide the gap).
        let mut planner = H264Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(planner.plan_au(au).unwrap());
        }
        let dropped = plans
            .iter()
            .enumerate()
            .position(|(i, p)| {
                p.picture.is_reference
                    && !p.picture.is_idr
                    && plans.get(i + 1).is_some_and(|next| !next.picture.is_idr)
            })
            .expect("the vector contains a droppable reference picture");

        // Drop that AU: warn, do not error. Every list entry must be a stored PicId.
        let mut planner = H264Planner::new();
        let mut gap_seen = false;
        let mut missing_seen = false;
        let mut planned = 0usize;
        let mut stored_so_far: BTreeSet<PicId> = BTreeSet::new();
        for (i, au) in aus.iter().enumerate() {
            if i == dropped {
                continue;
            }
            let plan = planner
                .plan_au(au)
                .expect("a lost reference AU must degrade to warnings, not errors");
            planned += 1;
            gap_seen |= plan
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::FrameNumGap { .. }));
            missing_seen |= plan
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::MissingReference { .. }));
            stored_so_far.insert(plan.dpb.stored.unwrap());
            for slice in &plan.slices {
                for entry in slice.ref_list0.iter().chain(&slice.ref_list1) {
                    assert!(
                        stored_so_far.contains(&entry.id),
                        "every emitted reference must be a real stored PicId"
                    );
                }
            }
        }

        assert_eq!(planned, 249);
        assert!(
            gap_seen,
            "the AU after the drop must report the frame_num gap"
        );
        // 8.2.5.2 placeholders are unresolvable; planning around them must warn too.
        assert!(missing_seen);
    }

    /// A concealed `frame_num` gap taints every descendant's `references_clean`,
    /// even when those later plans raise no warning of their own.
    #[test]
    fn a_concealed_picture_makes_every_descendant_report_unclean_references() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert!(
            p0.picture.references_clean,
            "an IDR references nothing, so it is clean by construction"
        );

        let p1 = planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        assert!(picture_warnings(&p1).is_empty());
        assert!(p1.picture.references_clean);

        // frame_num 2 never arrives; 8.2.5.2 inserts a placeholder. This picture's
        // references were intact; the damage is its own concealment.
        let p3 = planner.plan_au(&write_p_slice(3, 6, 1, 3, None)).unwrap();
        assert!(p3
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::FrameNumGap { .. })));

        let p4 = planner.plan_au(&write_p_slice(4, 8, 1, 1, None)).unwrap();
        assert!(
            picture_warnings(&p4).is_empty(),
            "p4's own plan raises nothing — which is exactly why the bit is needed"
        );
        assert!(
            !p4.picture.references_clean,
            "p4 predicts from the concealed chain, so it must not read as clean"
        );

        let p5 = planner.plan_au(&write_p_slice(5, 10, 1, 1, None)).unwrap();
        assert!(picture_warnings(&p5).is_empty());
        assert!(!p5.picture.references_clean, "the rot keeps travelling");
    }

    #[test]
    fn an_idr_reports_clean_references_however_damaged_the_run_before_it() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        planner.plan_au(&au0).unwrap();
        planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        // frame_num 2 lost.
        let p3 = planner.plan_au(&write_p_slice(3, 6, 1, 3, None)).unwrap();
        assert!(p3
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::FrameNumGap { .. })));
        let p4 = planner.plan_au(&write_p_slice(4, 8, 1, 1, None)).unwrap();
        assert!(!p4.picture.references_clean);

        let mut idr = param_set_au(&sps, &pps);
        idr.extend(write_idr_slice());
        let p5 = planner.plan_au(&idr).unwrap();
        assert!(p5.picture.references_clean, "an IDR is always clean");
        let p6 = planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        assert!(
            p6.picture.references_clean,
            "the damaged chain died with the IDR's DPB flush"
        );
    }

    #[test]
    fn a_healthy_stream_reports_clean_references_on_every_picture() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        assert!(planner.plan_au(&au0).unwrap().picture.references_clean);
        // log2_max_frame_num_minus4 = 0 and pic_order_cnt_lsb is u(4): both wrap at 16.
        for n in 1..16u32 {
            let plan = planner
                .plan_au(&write_p_slice(n, (n * 2) % 16, 1, 1, None))
                .unwrap();
            assert!(
                picture_warnings(&plan).is_empty(),
                "frame {n} should plan cleanly: {:?}",
                picture_warnings(&plan)
            );
            assert!(plan.picture.references_clean, "frame {n} must read clean");
        }
    }

    #[test]
    fn a_gap_placeholder_inside_a_ref_list_is_substituted_in_place_not_compacted() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        au0.extend(write_idr_slice());
        let au1 = write_p_slice(1, 2, 1, 1, None);
        // frame_num 2 never fed; AU3's 3-deep list holds the 8.2.5.2 placeholder at the head.
        let au3 = write_p_slice(3, 6, 1, 3, None);

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();
        let p3 = planner.plan_au(&au3).unwrap();

        assert!(p3
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::FrameNumGap { .. })));
        assert!(p3
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::MissingReference { .. })));

        // Descending PicNum: [placeholder(2), P1(1), IDR(0)]. Head substitutes
        // as P1; the two real entries keep their ref_idx.
        let id0 = p0.dpb.stored.unwrap();
        let id1 = p1.dpb.stored.unwrap();
        let list0 = &p3.slices[0].ref_list0;
        assert_eq!(
            list0.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![id1, id1, id0],
            "substitution must preserve list length and positions"
        );
        assert!(list0.iter().all(|r| !r.is_long_term));
    }

    #[test]
    fn a_recovery_point_sei_in_the_au_lands_on_the_picture_plan() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au0, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps, &mut au0, true).unwrap();
        // SEI NALU: recovery point, recovery_frame_cnt = 5, exact_match = 1.
        au0.extend([0x00, 0x00, 0x00, 0x01, 0x06, 0x06, 0x02, 0x34, 0x40, 0x80]);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let plan = planner.plan_au(&au0).unwrap();
        assert_eq!(
            plan.picture.recovery_point,
            Some(RecoveryPoint {
                recovery_frame_cnt: 5,
                exact_match: true,
                broken_link: false
            })
        );

        // Next AU has no SEI; the field must not stick.
        let au1 = write_p_slice(1, 2, 1, 1, None);
        let plan = planner.plan_au(&au1).unwrap();
        assert_eq!(plan.picture.recovery_point, None);
    }

    #[test]
    fn an_interlaced_sps_is_rejected_as_outside_the_envelope() {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .resolution(64, 64)
            .frame_mbs_only_flag(false)
            .mb_adaptive_frame_field_flag(false)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
            .build();
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au, true).unwrap();

        let mut planner = H264Planner::new();
        assert!(matches!(
            planner.plan_au(&au),
            Err(PlanError::OutsideEnvelope(_))
        ));
    }

    #[test]
    fn a_dpb_deeper_than_16_frames_is_rejected_as_outside_the_envelope() {
        // VUI `max_dec_frame_buffering` is the only path past A.3.1's 16-frame cap.
        // SpsBuilder has no setter; fields are public so construct Sps directly.
        let sps = Sps {
            profile_idc: Profile::Main as u8,
            level_idc: Level::L4,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            max_num_ref_frames: 4,
            vui_parameters_present_flag: true,
            vui_parameters: VuiParams {
                bitstream_restriction_flag: true,
                max_dec_frame_buffering: 17,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au, true).unwrap();

        let err = H264Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("DPB")),
            "{err:?}"
        );
    }

    /// SPS at `width`×`height` and `level`. `declared` None uses A.3.1; Some sets
    /// VUI `max_dec_frame_buffering`.
    fn sps_at(width: u32, height: u32, level: Level, declared: Option<u32>) -> Sps {
        Sps {
            profile_idc: Profile::Main as u8,
            level_idc: level,
            frame_mbs_only_flag: true,
            direct_8x8_inference_flag: true,
            // A.3.1 floor `max_dpb_frames` applies; not the value under test.
            max_num_ref_frames: 3,
            pic_width_in_mbs_minus1: (width / 16 - 1) as u16,
            pic_height_in_map_units_minus1: (height / 16 - 1) as u16,
            vui_parameters_present_flag: declared.is_some(),
            vui_parameters: VuiParams {
                bitstream_restriction_flag: declared.is_some(),
                max_dec_frame_buffering: declared.unwrap_or(0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Consumer-side pin: `dpb_limit + 1 <= 16` for the (picture, level, VUI)
    /// triples hosts actually emit. Producer-side HEVC equivalent:
    /// `pf-encode`'s `rfi_dpb_fits_a_mainstream_vulkan_decoder`.
    #[test]
    fn every_reachable_h264_stream_fits_a_mainstream_slot_pool() {
        // (picture, level, VUI max_dec_frame_buffering)
        let measured = [
            ((1280, 720), Level::L3_2, 3),
            ((1280, 720), Level::L4_1, 1),
            ((1920, 1080), Level::L4_2, 3),
            ((2560, 1440), Level::L5_1, 3),
            ((3840, 2160), Level::L5_2, 3),
        ];
        for ((w, h), level, declared) in measured {
            let sps = sps_at(w, h, level, Some(declared));
            assert!(
                !dpb_is_level_derived(&sps),
                "{w}x{h} L{:?}: the measured encoders all write the VUI restriction — \
                 an SPS that carries it must never be treated as level-derived",
                level
            );
            let slots = dpb_limit(&sps) + 1;
            assert!(
                slots <= MAINSTREAM_MAX_DPB_SLOTS,
                "{w}x{h} L{level:?} declaring {declared} needs {slots} DPB slots, \
                 mainstream hardware caps at {MAINSTREAM_MAX_DPB_SLOTS}"
            );
        }
    }

    /// Same resolutions at levels that saturate A.3.1 at 16 frames, with no VUI
    /// restriction. The planner must warn rather than open a 17-slot session.
    #[test]
    fn the_level_ceiling_alone_would_reproduce_96_and_is_warned_about() {
        // (picture, a level that saturates A.3.1's ceiling for it)
        let cliff = [
            ((1280, 720), Level::L5),
            ((1280, 720), Level::L5_2),
            ((1920, 1080), Level::L5_1),
            ((2560, 1440), Level::L6),
            ((3840, 2160), Level::L6_2),
        ];
        for ((w, h), level) in cliff {
            let sps = sps_at(w, h, level, None);
            assert!(dpb_is_level_derived(&sps), "{w}x{h} L{level:?}");
            assert_eq!(
                dpb_limit(&sps),
                16,
                "{w}x{h} L{level:?} should saturate A.3.1's 16-frame ceiling"
            );
            assert!(
                dpb_limit(&sps) + 1 > MAINSTREAM_MAX_DPB_SLOTS,
                "{w}x{h} L{level:?} is the #96 arithmetic and must be recognised as such"
            );

            // SPS activates on the first slice; the warning rides on the IDR plan.
            let sps = Rc::new(sps);
            let pps = PpsBuilder::new(Rc::clone(&sps))
                .pic_parameter_set_id(0)
                .pic_init_qp(26)
                .build();
            let mut au = param_set_au(&sps, &pps);
            au.extend(write_idr_slice());

            let plan = H264Planner::new()
                .plan_au(&au)
                .unwrap_or_else(|e| panic!("{w}x{h} L{level:?} should plan, got {e:?}"));
            assert!(
                plan.warnings.contains(&PlanWarning::LevelDerivedDpb {
                    max_dpb_frames: 16,
                    level_idc: level as u8,
                }),
                "{w}x{h} L{level:?}: expected LevelDerivedDpb, got {:?}",
                plan.warnings
            );
        }
    }

    #[test]
    fn a_proportionate_level_fits_even_without_a_vui_restriction() {
        let proportionate = [
            ((1280, 720), Level::L3_2, 5),
            ((1280, 720), Level::L4_1, 9),
            ((1920, 1080), Level::L4_2, 4),
            ((2560, 1440), Level::L5_1, 12),
            ((3840, 2160), Level::L5_2, 5),
        ];
        for ((w, h), level, expected) in proportionate {
            let sps = sps_at(w, h, level, None);
            assert_eq!(dpb_limit(&sps), expected, "{w}x{h} L{level:?}");
            let slots = dpb_limit(&sps) + 1; // + the picture in flight
            assert!(slots <= MAINSTREAM_MAX_DPB_SLOTS, "{w}x{h} L{level:?}");
        }
    }

    #[test]
    fn an_mmco_5_is_planned_with_a_rebase_warning_not_rejected() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());
        // MMCO 5 takes no argument (Table 7-9).
        let au1 = write_slice(&SliceSpec {
            frame_num: 1,
            poc_lsb: 2,
            mmco: Some(&[5]),
            ..Default::default()
        });

        let mut planner = H264Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        let p1 = planner.plan_au(&au1).unwrap();

        assert!(p1.warnings.contains(&PlanWarning::Mmco5Rebase));
        // Plan holds pre-rebase 8.2.1 values; stored frame_num/POC are zeroed for later AUs.
        assert_eq!(p1.picture.frame_num, 1);
        assert_eq!(p1.picture.pic_order_cnt, 2);
        // C.4.5.3 clause 3 drain: the IDR is display-ready.
        assert!(p1.dpb.outputs.contains(&p0.dpb.stored.unwrap()));
    }

    #[test]
    fn a_separate_colour_plane_sps_is_rejected_as_outside_the_envelope() {
        // SpsBuilder has no setter; synthesizer writes the flag for High + chroma_format_idc 3.
        let sps = Sps {
            profile_idc: Profile::High as u8,
            chroma_format_idc: 3,
            separate_colour_plane_flag: true,
            frame_mbs_only_flag: true,
            ..Default::default()
        };
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps, &mut au, true).unwrap();

        let err = H264Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("separate")),
            "{err:?}"
        );
    }

    #[test]
    fn display_crop_reports_the_conformance_window_offset_and_size() {
        // chroma_format_idc 1 (Main) CropUnitX/Y = 2 with frame_mbs_only:
        // offsets top 2 / bottom 2 / left 4 / right 2 → 4/4/8/4 luma samples.
        let sps = base_sps()
            .resolution(64, 64)
            .frame_crop_offsets(2, 2, 4, 2)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 8,
                y: 4,
                width: 52,
                height: 56
            }
        );

        // crop_left 100 = 200 luma samples on a 320-wide picture. A max-minus-min
        // derivation underflows; the plan must report offset 200, size 120.
        let sps = base_sps()
            .resolution(320, 240)
            .frame_crop_offsets(0, 0, 100, 0)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 200,
                y: 0,
                width: 120,
                height: 240
            }
        );
    }

    /// 64×64 SPS with VUI colour fields. SpsBuilder has no colour setters;
    /// unwrap and mutate. The synthesizer writes `video_signal_type` from the struct.
    fn sps_with_vui_colour(
        signal_type: bool,
        full_range: bool,
        description: Option<(u8, u8, u8)>,
    ) -> Rc<Sps> {
        let mut sps = Rc::try_unwrap(base_sps().resolution(64, 64).build()).expect("freshly built");
        sps.vui_parameters_present_flag = true;
        sps.vui_parameters.video_signal_type_present_flag = signal_type;
        sps.vui_parameters.video_full_range_flag = full_range;
        if let Some((primaries, transfer, matrix)) = description {
            sps.vui_parameters.colour_description_present_flag = true;
            sps.vui_parameters.colour_primaries = primaries;
            sps.vui_parameters.transfer_characteristics = transfer;
            sps.vui_parameters.matrix_coefficients = matrix;
        }
        Rc::new(sps)
    }

    fn plan_one_idr(sps: &Rc<Sps>) -> AuPlan {
        let pps = PpsBuilder::new(Rc::clone(sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au = param_set_au(sps, &pps);
        au.extend(write_idr_slice());
        H264Planner::new().plan_au(&au).unwrap()
    }

    #[test]
    fn an_sps_without_vui_plans_the_e211_unspecified_colour() {
        let (sps, pps) = authored_sps_pps();
        assert!(
            !sps.vui_parameters_present_flag,
            "the base SPS carries no VUI"
        );
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: false,
            },
            "E.2.1 inference: 'unspecified' code points + limited range, never a raw 0"
        );
    }

    #[test]
    fn an_explicit_colour_description_rides_the_plan_and_follows_a_new_sps() {
        // BT.2020 / PQ (primaries 9, transfer 16, matrix 9).
        let hdr = sps_with_vui_colour(true, false, Some((9, 16, 9)));
        let plan = plan_one_idr(&hdr);
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                video_full_range: false,
            }
        );

        // Colour follows the SPS of each picture, not the session's first.
        // Pps snapshots its SPS at PPS-parse time; hosts re-send SPS+PPS so
        // the new content activates on the next IDR.
        let (sdr_sps, sdr_pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sdr_sps, &sdr_pps);
        au0.extend(write_idr_slice());
        let mut planner = H264Planner::new();
        let plan0 = planner.plan_au(&au0).unwrap();
        assert_eq!(plan0.picture.colour.matrix_coefficients, 2);

        let hdr_pps = PpsBuilder::new(Rc::clone(&hdr))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut au1 = param_set_au(&hdr, &hdr_pps);
        au1.extend(write_idr_slice());
        let plan1 = planner.plan_au(&au1).unwrap();
        assert_eq!(
            plan1.picture.colour.matrix_coefficients, 9,
            "the replacing SPS's colour lands on its own picture, not latched"
        );
    }

    #[test]
    fn a_vui_without_colour_description_keeps_unspecified_but_honours_the_range_flag() {
        // video_signal_type present, full-range, no colour description:
        // code points stay E.2.1 unspecified; the range flag rides.
        let plan = plan_one_idr(&sps_with_vui_colour(true, true, None));
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: true,
            }
        );
    }

    #[test]
    fn a_malformed_nalu_mid_au_truncates_with_a_warning_keeping_prior_slices() {
        let (sps, pps) = authored_sps_pps();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        // NAL type 24 (reserved): the vendored header parser rejects it.
        au.extend([0x00, 0x00, 0x00, 0x01, 0x18, 0xAA, 0xBB]);
        au.extend(write_idr_slice()); // real data behind the cut, never reached

        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert_eq!(
            plan.slices.len(),
            1,
            "only the slice before the cut is planned"
        );
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    #[test]
    fn a_foreign_slice_in_the_au_is_dropped_with_a_truncated_au_warning() {
        let (sps, pps) = authored_sps_pps();
        let mut au = param_set_au(&sps, &pps);
        au.extend(write_idr_slice());
        // Continuation slices of another picture (non-IDR, frame_num 1); drop them.
        for first_mb in [8, 9] {
            au.extend(write_slice(&SliceSpec {
                first_mb,
                frame_num: 1,
                poc_lsb: 2,
                ..Default::default()
            }));
        }

        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert!(plan.picture.is_idr);
        assert_eq!(plan.slices.len(), 1, "the foreign slices are not planned");
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    #[test]
    fn outputs_queued_during_a_failed_au_surface_in_the_next_successful_plan() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let id0 = planner.plan_au(&au0).unwrap().dpb.stored.unwrap();

        // Errors after IDR begin already drained the DPB (id0 queued):
        // continuation slice names PPS 1, which was never sent.
        let mut bad_au = write_idr_slice();
        bad_au.extend(write_slice(&SliceSpec {
            first_mb: 8,
            pps_id: 1,
            ..Default::default()
        }));
        assert!(matches!(
            planner.plan_au(&bad_au),
            Err(PlanError::NoActiveParamSet { pps_id: 1 })
        ));

        let plan = planner.plan_au(&write_idr_slice()).unwrap();
        assert!(plan.dpb.outputs.contains(&id0));
        assert!(plan.dpb.removed.contains(&id0));
    }

    #[test]
    fn flush_resets_decoding_state_and_refuses_non_idr_until_an_idr_arrives() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let mut planner = H264Planner::new();
        let id0 = planner.plan_au(&au0).unwrap().dpb.stored.unwrap();
        let id1 = planner
            .plan_au(&write_p_slice(1, 2, 1, 1, None))
            .unwrap()
            .dpb
            .stored
            .unwrap();

        let flushed = planner.flush();
        assert!(flushed.outputs.contains(&id0) && flushed.outputs.contains(&id1));
        assert_eq!(flushed.removed, vec![id0, id1]);

        assert!(matches!(
            planner.plan_au(&write_p_slice(2, 4, 1, 1, None)),
            Err(PlanError::AwaitingIdr)
        ));

        // IDR restarts planning; parameter sets survived the flush (7.4.1.2).
        let plan = planner.plan_au(&write_idr_slice()).unwrap();
        assert!(plan.picture.is_idr);
        assert_eq!(plan.picture.pic_order_cnt, 0);
        assert!(picture_warnings(&plan).is_empty());

        let plan = planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        assert!(picture_warnings(&plan).is_empty());
        assert_eq!(plan.slices[0].ref_list0.len(), 1);
    }

    /// An IDR that fails to plan restarts nothing. Clearing the latch on sight
    /// of the IDR let the P behind it plan on an empty DPB and be stored as a
    /// reference for everything after.
    #[test]
    fn an_idr_that_fails_to_plan_keeps_the_planner_awaiting_an_idr() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());
        let mut planner = H264Planner::new();
        planner.plan_au(&au0).unwrap();
        planner.flush();

        // PPS 3 was never sent.
        assert!(matches!(
            planner.plan_au(&write_slice(&SliceSpec {
                idr: true,
                ref_idc: 3,
                pps_id: 3,
                ..Default::default()
            })),
            Err(PlanError::NoActiveParamSet { pps_id: 3 })
        ));
        assert!(matches!(
            planner.plan_au(&write_p_slice(1, 2, 1, 1, None)),
            Err(PlanError::AwaitingIdr)
        ));

        let plan = planner.plan_au(&write_idr_slice()).unwrap();
        assert!(plan.picture.is_idr);
        let plan = planner.plan_au(&write_p_slice(1, 2, 1, 1, None)).unwrap();
        assert!(picture_warnings(&plan).is_empty());
        assert!(plan.picture.references_clean);
    }

    /// The clean bit is a statement about the whole chain, this picture
    /// included. A concealed picture's empty list must not read as clean.
    #[test]
    fn a_concealed_picture_reports_unclean_references_of_its_own() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());
        let mut planner = H264Planner::new();
        planner.plan_au(&au0).unwrap();
        // frame_num 1 lost: the gap conceals this picture.
        let plan = planner.plan_au(&write_p_slice(2, 4, 1, 1, None)).unwrap();
        assert!(plan.warnings.iter().any(PlanWarning::is_integrity));
        assert!(!plan.picture.references_clean);
    }

    #[test]
    fn picture_plan_parameters_come_from_the_first_slices_pps() {
        // Two SPS with the same negotiation size but different crops; PPS 1 uses the cropped one.
        let sps0 = base_sps().resolution(64, 64).build();
        let sps1 = base_sps()
            .seq_parameter_set_id(1)
            .resolution(64, 64)
            .frame_crop_offsets(2, 2, 4, 2)
            .build();
        let pps0 = PpsBuilder::new(Rc::clone(&sps0))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let pps1 = PpsBuilder::new(Rc::clone(&sps1))
            .pic_parameter_set_id(1)
            .pic_init_qp(26)
            .build();

        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps0, &mut au, true).unwrap();
        Synthesizer::<'_, Sps, _>::synthesize(3, &sps1, &mut au, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps0, &mut au, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, &pps1, &mut au, true).unwrap();
        au.extend(write_idr_slice());
        // Continuation slice may legally name another PPS.
        au.extend(write_slice(&SliceSpec {
            idr: true,
            ref_idc: 3,
            first_mb: 8,
            pps_id: 1,
            ..Default::default()
        }));

        let plan = H264Planner::new().plan_au(&au).unwrap();
        assert!(picture_warnings(&plan).is_empty());
        assert_eq!(plan.slices.len(), 2);
        // Picture parameters come from the first slice's PPS (uncropped SPS 0).
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 0,
                y: 0,
                width: 64,
                height: 64
            }
        );
        // Accessors follow the first slice; drifting to PPS 1 would desync from `picture`.
        assert_eq!(plan.pps.pic_parameter_set_id, 0);
        assert_eq!(plan.sps.seq_parameter_set_id, 0);
        assert!(
            Rc::ptr_eq(&plan.sps, &plan.pps.sps),
            "the SPS accessor is the PPS's own SPS, not a second copy"
        );
        assert!(
            !plan.sps.frame_cropping_flag,
            "SPS 0, not the cropped SPS 1"
        );
    }

    #[test]
    fn the_plans_parameter_set_accessors_carry_the_activated_content() {
        let (sps, pps) = authored_sps_pps();
        let mut au0 = param_set_au(&sps, &pps);
        au0.extend(write_idr_slice());

        let plan = H264Planner::new().plan_au(&au0).unwrap();
        // Parser re-parses in-band sets, so pointer identity with the authored
        // structs is not expected. Spot-check the fields backends consume.
        assert_eq!(plan.sps.seq_parameter_set_id, sps.seq_parameter_set_id);
        assert_eq!(plan.sps.profile_idc, sps.profile_idc);
        assert_eq!(plan.sps.level_idc, sps.level_idc);
        assert_eq!(plan.sps.max_num_ref_frames, sps.max_num_ref_frames);
        assert_eq!(plan.sps.width(), sps.width());
        assert_eq!(plan.sps.height(), sps.height());
        assert_eq!(plan.pps.pic_parameter_set_id, pps.pic_parameter_set_id);
        assert_eq!(plan.pps.seq_parameter_set_id, pps.seq_parameter_set_id);
        assert_eq!(plan.pps.pic_init_qp_minus26, pps.pic_init_qp_minus26);
        assert!(Rc::ptr_eq(&plan.sps, &plan.pps.sps));
    }

    /// AU index each picture is shown at, minus the AU index it was decoded at.
    /// Forty AUs cross the `frame_num` wrap and, where coded, the POC wrap.
    fn output_lags(sps: Rc<Sps>) -> Vec<usize> {
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let mut planner = H264Planner::new();
        let mut stored = Vec::new();
        let mut lags = Vec::new();
        for i in 0..40u32 {
            let mut au = if i == 0 {
                param_set_au(&sps, &pps)
            } else {
                Vec::new()
            };
            au.extend(write_slice(&SliceSpec {
                idr: i == 0,
                frame_num: i % 16,
                poc_lsb: (2 * i) % 16,
                poc_type_2: sps.pic_order_cnt_type == 2,
                ..Default::default()
            }));
            let plan = planner.plan_au(&au).unwrap();
            stored.push(plan.dpb.stored.unwrap());
            for shown in &plan.dpb.outputs {
                let decoded_at = stored.iter().position(|id| id == shown).unwrap();
                lags.push(i as usize - decoded_at);
            }
        }
        lags
    }

    /// A host states `max_num_reorder_frames = 0`; the session is one AU in, one
    /// picture out.
    #[test]
    fn a_zero_reorder_stream_shows_each_picture_in_its_own_au() {
        let sps = base_sps()
            .resolution(64, 64)
            .bitstream_restriction(0)
            .build();
        assert_eq!(output_lags(sps), vec![0; 40]);
    }

    /// NVENC's shape: no VUI restriction, `pic_order_cnt_type` 2. 8.2.1.3 makes
    /// output order the decoding order, so nothing waits.
    #[test]
    fn poc_type_2_shows_each_picture_in_its_own_au_with_no_stated_bound() {
        let sps = base_sps().resolution(64, 64).pic_order_cnt_type(2).build();
        assert_eq!(output_lags(sps), vec![0; 40]);
    }

    /// No VUI restriction, `pic_order_cnt_type` 0: E.2.1 infers the bound as the
    /// DPB depth. The spec's answer, kept for streams that may reorder.
    #[test]
    fn a_stream_that_states_no_bound_waits_for_a_full_dpb() {
        let sps = base_sps().resolution(64, 64).build();
        let depth = sps.max_dpb_frames();
        assert_eq!(output_lags(sps), vec![depth; 40 - depth]);
    }
}
