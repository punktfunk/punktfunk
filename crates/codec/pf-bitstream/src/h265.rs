// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file (vendor/cros-codecs/LICENSE).

// Adapted from cros-codecs `decoder/stateless/h265.rs`
// (vendor/cros-codecs/PROVENANCE.md). Spec 8.3.1–8.3.4 and C.5.2 keep
// upstream structure so diffs stay legible. Stripped: decoder plumbing
// and SCC self-reference (the envelope gate rejects it).

//! Per-AU H.265 planning: [`H265Planner::plan_au`] turns one access unit
//! (Annex-B, parameter sets plus the slice segments of one picture) into an
//! [`AuPlan`] — parsed headers, POC, derived reference picture sets (including
//! the long-term entries RFI recovery uses), per-slice reference lists, and
//! the DPB delta.
//!
//! Concealment matches the H.264 layer: an RPS entry naming a picture the DPB
//! does not hold is a [`PlanWarning`], never an error. The reference is
//! substituted in place; planning continues. [`PlanError`] is for AUs that
//! cannot (or, for RASL pictures behind a skipped CRA, must not) be planned.
//!
//! [`PlanError::RaslSkipped`] is the spec skip (8.1.3 NOTE: decode nothing,
//! show nothing, stream healthy). The H.265 backend must treat it as an
//! Ok-skip of the AU, never as a recovery trigger: the H.264 session maps
//! every planner `Err` to release-unshown plus reanchor, which is wrong here.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::Cursor;
use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::h265::dpb::Dpb;
use cros_codecs::codec::h265::dpb::DpbEntry;
use cros_codecs::codec::h265::parser::Nalu;
use cros_codecs::codec::h265::parser::Parser;
use cros_codecs::codec::h265::parser::Pps;
use cros_codecs::codec::h265::parser::ShortTermRefPicSet;
use cros_codecs::codec::h265::parser::Slice;
use cros_codecs::codec::h265::parser::Sps;
use cros_codecs::codec::h265::picture::PictureData;
use cros_codecs::codec::h265::picture::Reference;
use cros_codecs::Resolution;
use tracing::trace;

pub use cros_codecs::codec::h265::parser::Level;
pub use cros_codecs::codec::h265::parser::NaluType;
pub use cros_codecs::codec::h265::parser::SliceHeader;

// Owned by `h264`: pf-vkdecode already imports these from there. Re-export
// rather than lift to a third module.
pub use crate::h264::ColourDescription;
pub use crate::h264::DisplayCrop;
pub use crate::h264::DpbUpdate;
pub use crate::h264::PicId;

use crate::sei;
pub use crate::sei::RecoveryPointHevc;

pub mod conceal;
mod refs;

pub use refs::CurrentRefs;
pub use refs::RefRpsIdxError;
pub use refs::RpsError;
pub use refs::RPS_SET_LEN;
pub use refs::RPS_UNUSED;

/// Everything a backend needs to submit one access unit.
#[derive(Debug, Clone)]
pub struct AuPlan {
    pub picture: PicturePlan,
    /// The picture's 8.3.2 reference picture sets, resolved to stored pictures.
    /// DXVA picparams and Vulkan `StdVideoDecodeH265PictureInfo` key by these.
    pub rps: RpsPlan,
    pub slices: Vec<SlicePlan>,
    pub dpb: DpbUpdate,
    /// Pictures the DPB holds marked "used for reference" when this AU decodes.
    /// A SUPERSET of [`Self::rps`]: 8.3.2's *Foll* sets stay marked for later
    /// pictures while this AU names none of them.
    ///
    /// Both rungs bind them: DXVA as `RefPicList`, Vulkan in `pReferenceSlots`
    /// after the current sets. A driver may treat absence as "no longer a
    /// reference": AMD's Vulkan driver then decodes an RFI anchor on a *Foll*
    /// picture wrong.
    ///
    /// Captured after 8.3.2 marking and C.5.2.2's pre-decode update, before the
    /// current picture is stored — never contains it, never a picture the RPS
    /// just unmarked. DPB order (oldest first); one picture, one surface.
    pub dpb_refs: Vec<RefPic>,
    pub warnings: Vec<PlanWarning>,
    /// SPS activated for this AU (the first slice's PPS's SPS). A later segment
    /// may name another PPS; that drift does not reach here. Cloned from the
    /// parser table so backends do not re-parse the AU.
    pub sps: Rc<Sps>,
    /// PPS the picture was begun with (the first slice's). Same `Rc` as
    /// [`Self::sps`] via `pps.sps`; a VPS, if present, hangs off `sps.vps`.
    pub pps: Rc<Pps>,
}

/// Per-picture parameters after 8.3.1 POC derivation and 8.3.2 RPS marking.
#[derive(Debug, Clone)]
pub struct PicturePlan {
    /// HEVC picture taxonomy lives on the NALU type, not header flags.
    pub nalu_type: NaluType,
    pub is_idr: bool,
    pub is_irap: bool,
    /// `NoRaslOutputFlag` (8.1.3): set on every IDR/BLA and on a CRA that opens
    /// the bitstream or follows an EOS. RASLs leading this IRAP are undecodable.
    pub no_rasl_output_flag: bool,
    /// False only for sub-layer non-reference NALU types. C.3.4 still stores every
    /// planned picture as short-term; same-layer pictures must not name an SLNR.
    pub is_reference: bool,
    /// `PicOrderCntVal` (8.3.1).
    pub pic_order_cnt: i32,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Conformance-window crop (7.4.3.2.1: `conf_win_*` scale by SubWidthC/
    /// SubHeightC), in luma samples of the coded picture.
    pub display_crop: DisplayCrop,
    /// Colour from the ACTIVE SPS VUI (E.3.1 inference where absent: parser
    /// defaults 2/"unspecified", limited range). Per picture, never latched:
    /// an HDR desktop can switch PQ/BT.2020 in-band with a new SPS.
    pub colour: ColourDescription,
    pub general_profile_idc: u8,
    pub level_idc: Level,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub chroma_format_idc: u8,
    /// Stream `sps_max_dec_pic_buffering_minus1 + 1`, capped at 16 — not A-2's
    /// level ceiling. Backends size their slot pool from this. See [`dpb_limit`].
    pub max_dpb_frames: usize,
    /// Bits of the inline `st_ref_pic_set()` on the first slice (0 when the RPS
    /// came from the SPS by index). Vulkan `NumBitsForSTRefPicSetInSlice`.
    pub short_term_ref_pic_set_size_bits: u32,
    pub recovery_point: Option<RecoveryPointHevc>,
    /// Every picture this AU predicts from came off a fully-available chain, so
    /// a host `USER_FLAG_RECOVERY_ANCHOR` claim can be checked. `true` for an
    /// IRAP and any picture whose whole chain is clean; `false` once this AU or
    /// anything it descends from needed concealment.
    ///
    /// Additive: the plan, warnings, and DPB do not change because of it. See
    /// [`crate::clean`] for propagation (every rule errs toward `false`).
    pub references_clean: bool,
}

/// A reference list / RPS entry: the minimum every backend picparams format needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefPic {
    pub id: PicId,
    /// Stored `PicOrderCntVal` — HEVC hardware keys references by POC, not `frame_num`.
    pub pic_order_cnt: i32,
    pub is_long_term: bool,
}

/// The three "current" 8.3.2 reference picture sets, resolved to stored pictures.
///
/// An entry the DPB lacks is flagged [`PlanWarning::MissingReference`] and
/// carries a stand-in picture, so every position the slices can index binds.
#[derive(Debug, Clone, Default)]
pub struct RpsPlan {
    /// `RefPicSetStCurrBefore`: short-term, POC below current, nearest first.
    pub st_curr_before: Vec<RefPic>,
    /// `RefPicSetStCurrAfter`: short-term, POC above current, nearest first.
    pub st_curr_after: Vec<RefPic>,
    /// `RefPicSetLtCurr`: long-term references — the RFI recovery path.
    pub lt_curr: Vec<RefPic>,
}

/// One slice segment NALU of the picture, with its reference lists fully derived.
#[derive(Debug, Clone)]
pub struct SlicePlan {
    /// Byte range of the slice NALU in the input AU, start code included.
    /// Hardware takes the raw bitstream, so the plan points instead of copying.
    pub data: Range<usize>,
    /// [`Self::data`] from the NAL header on, start code dropped. The three
    /// bytes before it are always `00 00 01`.
    pub nal: Range<usize>,
    /// Parsed slice-segment header. For a dependent segment this is COMPLETED
    /// (7.4.7.1 inherited fields already copied); backends never see a partial.
    pub header: SliceHeader,
    pub ref_list0: Vec<RefPic>,
    pub ref_list1: Vec<RefPic>,
}

/// Concealment signals: planning continues, the session layer requests recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanWarning {
    /// An RPS (or list) entry named a picture the DPB does not hold. Matching
    /// reference-list positions carry an in-place substitute.
    MissingReference {
        context: &'static str,
        detail: String,
    },
    /// NALU walk stopped early: truncated NALU with real data behind it, or a
    /// slice of another picture. Plan covers slices before the cut; `offset` is
    /// the cut's byte position in the AU.
    TruncatedAu { offset: usize },
    /// Activated SPS signals reordering (`sps_max_num_reorder_pics > 0`). Spec-
    /// legal and planned (C.5.2 honours it). Hosts emit zero-reorder only; this
    /// is the signal if that assumption breaks (H.264 `Mmco5Rebase` idiom).
    NonZeroReorder { max_num_reorder_pics: u8 },
}

impl PlanWarning {
    /// Whether the PICTURE is damaged. Twin of [`crate::h264::PlanWarning::is_integrity`];
    /// every backend conceals on this.
    ///
    /// `NonZeroReorder` is not damage: it fires on the AU that activates an SPS
    /// (opening IRAP, ABR resolution change). Treating it as concealment would
    /// release that IRAP unshown and poison [`crate::clean::CleanLedger`] on the
    /// picture that is clean by construction.
    ///
    /// Exhaustive, no wildcard — a new variant must pick a side.
    pub fn is_integrity(&self) -> bool {
        match self {
            PlanWarning::MissingReference { .. } | PlanWarning::TruncatedAu { .. } => true,
            PlanWarning::NonZeroReorder { .. } => false,
        }
    }
}

/// The AU cannot be planned at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Parse(String),
    /// Legal H.265 outside what hosts emit. Stream-integrity failure, not a gap.
    OutsideEnvelope(&'static str),
    NoActiveParamSet {
        pps_id: u8,
    },
    /// [`H265Planner::flush`] discarded decoding state; resume only at an IRAP.
    /// Flush marks the next picture "first after EOS", so any IRAP gets
    /// `NoRaslOutputFlag = 1` and CRA/BLA are full re-entry points.
    AwaitingIdr,
    /// RASL whose CRA/BLA had `NoRaslOutputFlag = 1` (open-GOP join). Spec: may
    /// reference pre-join pictures; must not decode or output (8.1.3 NOTE).
    /// State is untouched; the next AU plans normally.
    RaslSkipped {
        poc: i32,
    },
}

impl PlanError {
    /// Nothing to plan until an IRAP and its parameter sets land, as for a decoder built
    /// mid-GOP. Every decode rung treats this as idle, not a refusal. A RASL skip is not.
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
                write!(f, "flushed: waiting for an IRAP to resume planning")
            }
            PlanError::RaslSkipped { poc } => {
                write!(
                    f,
                    "RASL picture (poc {poc}) after a CRA join is not decodable"
                )
            }
        }
    }
}

impl std::error::Error for PlanError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NegotiationInfo {
    coded_resolution: Resolution,
    general_profile_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    chroma_format_idc: u8,
    max_dpb_frames: usize,
    /// Reorder depth is a negotiation fact: a mid-stream 0→N switch (same
    /// geometry) changes [`DpbUpdate::outputs`]. Omit it and outputs stall.
    max_num_reorder_pics: u8,
}

impl From<&Sps> for NegotiationInfo {
    fn from(sps: &Sps) -> Self {
        NegotiationInfo {
            coded_resolution: Resolution::from((u32::from(sps.width()), u32::from(sps.height()))),
            general_profile_idc: sps.profile_tier_level.general_profile_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: dpb_limit(sps),
            max_num_reorder_pics: sps.max_num_reorder_pics[usize::from(sps.max_sub_layers_minus1)],
        }
    }
}

/// Stream `sps_max_dec_pic_buffering_minus1[HighestTid] + 1`, capped at 16.
///
/// Not equation A-2 (`Sps::max_dpb_size`). A-2 is a CEILING on what an SPS may
/// signal (7.4.3.2.1: `0..=MaxDpbSize - 1`), not what the stream needs. C.5.2.2
/// bumps against the signalled buffering; A.4.1 bounds RPS entries by the same
/// number.
///
/// Reading A-2 as a requirement over-allocates: A-2 branches on picture size
/// vs the LEVEL's `MaxLumaPs`, so a six-picture L5.1 stream reported 16 at
/// 1080p (branch 1) and 6 at 4K. Backends added the current picture and asked
/// for 17 slots; Vulkan Video caps `maxDpbSlots` at 16.
///
/// `Dpb::needs_bumping` already keys on signalled buffering, not `max_num_pics`,
/// so widening the limit only over-allocated hardware surfaces.
///
/// Cap stays: `check_envelope` rejects `buffering > 16`, but this also runs
/// from `NegotiationInfo::from` on SPSes that have not reached the gate yet.
fn dpb_limit(sps: &Sps) -> usize {
    let buffering =
        usize::from(sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)]) + 1;
    buffering.min(16)
}

/// 8.3.2 RefPicSet, derived once per picture.
///
/// Upstream uses `[_; 16]` plus counts. `Vec`s here avoid the OOB panic a
/// hostile header can reach (a slice may name more RPS entries than the DPB).
/// Stand in for every `None` of a current RPS set: the nearest present entry of
/// the set, else the DPB reference closest in POC to the named picture.
fn fill_missing(dpb: &Dpb<PicId>, set: &mut [Option<DpbEntry<PicId>>], pocs: &[i32]) {
    for i in 0..set.len() {
        if set[i].is_some() {
            continue;
        }
        let neighbour = set[..i]
            .iter()
            .rev()
            .chain(set[i + 1..].iter())
            .flatten()
            .next()
            .cloned();
        set[i] = neighbour.or_else(|| {
            dpb.get_all_references()
                .into_iter()
                .min_by_key(|e| (e.0.borrow().pic_order_cnt_val - pocs[i]).abs())
        });
    }
}

#[derive(Default)]
struct RefPicSet {
    /// `PocStCurrBefore` / `PocStCurrAfter` / `PocStFoll` (equation 8-5).
    poc_st_curr_before: Vec<i32>,
    poc_st_curr_after: Vec<i32>,
    poc_st_foll: Vec<i32>,
    /// `PocLtCurr` / `PocLtFoll` with `delta_poc_msb_present_flag` (8-5).
    poc_lt_curr: Vec<(i32, bool)>,
    poc_lt_foll: Vec<(i32, bool)>,

    /// Resolved sets (8-6/8-7). `None` = DPB miss, positional; `fill_missing`
    /// stands a real picture in before the plan is built.
    ref_pic_set_st_curr_before: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_st_curr_after: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_lt_curr: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_st_foll: Vec<Option<DpbEntry<PicId>>>,
    ref_pic_set_lt_foll: Vec<Option<DpbEntry<PicId>>>,
}

impl RefPicSet {
    fn curr_is_empty(&self) -> bool {
        self.poc_st_curr_before.is_empty()
            && self.poc_st_curr_after.is_empty()
            && self.poc_lt_curr.is_empty()
    }
}

/// Picture being planned, spanning the slice segments of one AU.
struct CurrentPicState {
    pic: PictureData,
    /// PPS of the current segment. A later segment may name another PPS; this
    /// feeds end-of-picture bumping, as upstream does.
    pps: Rc<Pps>,
    /// PPS the picture was begun with. [`H265Planner::picture_plan`] reads this
    /// so per-picture parameters cannot drift to a later segment's PPS.
    first_slice_pps: Rc<Pps>,
    id: PicId,
    rps_plan: RpsPlan,
    /// Marked DPB for THIS picture's decode, captured beside the RPS that marked
    /// it — see [`AuPlan::dpb_refs`].
    dpb_refs: Vec<RefPic>,
    /// 7.4.7.1: `slice_segment_address` must increase across a picture's segments.
    prev_segment_address: Option<u32>,
}

/// Plans H.265 access units for stateless hardware decoders.
///
/// Owns the vendored parser and DPB plus the POC/RPS state upstream keeps in
/// `H265DecoderState`. One instance per elementary stream; AUs in decode order.
pub struct H265Planner {
    parser: Parser,
    negotiation_info: NegotiationInfo,
    dpb: Dpb<PicId>,
    rps: RefPicSet,
    /// Spec `PrevTid0Pic` (8.3.1).
    prev_tid0_pic: Option<PictureData>,
    /// `MaxPicOrderCntLsb` of the ACTIVE SPS. Upstream latches the last SPS
    /// parsed; this takes the activating PPS's SPS (8.3.1 reads the active one).
    max_pic_order_cnt_lsb: i32,
    /// `NoRaslOutputFlag` of the last IRAP.
    irap_no_rasl_output_flag: bool,
    /// Next picture is first in the bitstream / follows an EOS (both feed
    /// `NoRaslOutputFlag`, 8.1.3).
    first_picture_in_bitstream: bool,
    first_picture_after_eos: bool,
    /// Last independent slice-segment header, copied into dependent segments
    /// (7.4.7.1).
    last_independent_header: Option<SliceHeader>,
    next_pic_id: PicId,
    /// Set by [`Self::flush`]: planning resumes only at an IRAP.
    awaiting_idr: bool,
    /// Outputs, `removed` baseline and the unclean marks behind
    /// [`PicturePlan::references_clean`].
    report: crate::report::AuReporter,
}

impl Default for H265Planner {
    fn default() -> Self {
        Self {
            parser: Default::default(),
            negotiation_info: Default::default(),
            dpb: Default::default(),
            rps: Default::default(),
            prev_tid0_pic: None,
            max_pic_order_cnt_lsb: 0,
            irap_no_rasl_output_flag: false,
            first_picture_in_bitstream: true,
            first_picture_after_eos: true,
            last_independent_header: None,
            next_pic_id: 0,
            awaiting_idr: false,
            report: Default::default(),
        }
    }
}

impl H265Planner {
    pub fn new() -> Self {
        Default::default()
    }

    /// Plan one access unit: Annex-B bytes with VPS/SPS/PPS/SEI/AUD plus the
    /// 1..N slice-segment NALUs of exactly one picture.
    ///
    /// After a [`PlanError`] state is best-effort ([`PlanError::RaslSkipped`]
    /// leaves it intact). Request an IDR before feeding more AUs. Outputs and
    /// removals queued by a failed AU emit with the next successful plan (or
    /// [`Self::flush`]) — never discarded.
    pub fn plan_au(&mut self, au: &[u8]) -> Result<AuPlan, PlanError> {
        let mut warnings = Vec::new();
        let mut slices = Vec::new();
        let mut recovery_point = None;
        let mut current: Option<CurrentPicState> = None;
        let mut saw_nalu = false;

        // Byte past the last fully consumed NALU. The cursor cannot be this
        // anchor: after a successful `Nalu::next` it sits on the CURRENT header,
        // and a failed one leaves it mid-scan.
        let mut consumed_end = 0usize;
        let mut cursor = Cursor::new(au);
        loop {
            let nalu = match Nalu::next(&mut cursor) {
                Ok(nalu) => nalu,
                Err(_) => {
                    // End of the AU, or a NALU shorter than its two header bytes
                    // (every 6-bit HEVC type is a valid header, unlike H.264).
                    // Non-zero bytes after the last consumed NALU are cut-off
                    // data, not B.2.2 trailing_zero_8bits padding.
                    let tail = &au[consumed_end.min(au.len())..];
                    if tail.iter().any(|&b| b != 0) {
                        warnings.push(PlanWarning::TruncatedAu {
                            offset: consumed_end,
                        });
                    }
                    break;
                }
            };
            saw_nalu = true;
            // After `Nalu::next` the cursor sits on the NAL header; `offset` is
            // start-code length, `size` the payload — the AU byte range, no copy.
            let nalu_offset = cursor.position() as usize;
            let range = (nalu_offset - nalu.offset)..(nalu_offset + nalu.size);
            debug_assert_eq!(&au[range.clone()], nalu.data.as_ref());
            consumed_end = range.end;

            // Hosts emit single-layer only. nuh_layer_id > 0 is an enhancement
            // layer; half-decoding the base of an unknown stream is silent loss.
            if nalu.header.nuh_layer_id != 0 {
                return Err(PlanError::OutsideEnvelope(
                    "multilayer stream (nuh_layer_id != 0)",
                ));
            }

            match nalu.header.type_ {
                NaluType::VpsNut => {
                    self.parser.parse_vps(&nalu).map_err(PlanError::Parse)?;
                }
                NaluType::SpsNut => {
                    let sps = self.parser.parse_sps(&nalu).map_err(PlanError::Parse)?;
                    Self::check_envelope(sps)?;
                }
                NaluType::PpsNut => {
                    self.parser.parse_pps(&nalu).map_err(PlanError::Parse)?;
                }
                NaluType::PrefixSeiNut => {
                    // HEVC NAL header is two bytes. Recovery point is prefix-only
                    // (D.2.1); suffix SEI never carries it and falls through.
                    match sei::parse_recovery_point_hevc(nalu.as_ref().get(2..).unwrap_or(&[])) {
                        Ok(Some(rp)) => recovery_point = Some(rp),
                        Ok(None) => {}
                        // A broken SEI must not cost the picture it decorates.
                        Err(err) => trace!("ignoring unparseable SEI NALU: {err}"),
                    }
                }
                NaluType::EosNut => {
                    // 8.1.3: first picture after EOS gets NoRaslOutputFlag = 1.
                    self.first_picture_after_eos = true;
                }
                NaluType::EobNut => {
                    self.first_picture_in_bitstream = true;
                }
                NaluType::TrailN
                | NaluType::TrailR
                | NaluType::TsaN
                | NaluType::TsaR
                | NaluType::StsaN
                | NaluType::StsaR
                | NaluType::RadlN
                | NaluType::RadlR
                | NaluType::RaslN
                | NaluType::RaslR
                | NaluType::BlaWLp
                | NaluType::BlaWRadl
                | NaluType::BlaNLp
                | NaluType::IdrWRadl
                | NaluType::IdrNLp
                | NaluType::CraNut => {
                    // Opening with a continuation (first payload bit 0) is the
                    // tail of a previous picture. Beginning from it would
                    // fabricate a duplicate; a dependent segment would inherit
                    // the previous AU's independent header. Skip and warn.
                    let first_segment = nalu.as_ref().get(2).is_some_and(|byte| byte & 0x80 != 0);
                    if current.is_none() && !first_segment {
                        warnings.push(PlanWarning::TruncatedAu {
                            offset: range.start,
                        });
                        continue;
                    }
                    // After a flush only an IRAP restarts. Any IRAP works: flush
                    // set `first_picture_after_eos`, so CRA/BLA get
                    // NoRaslOutputFlag = 1. Cleared only once the picture begins.
                    if current.is_none() && self.awaiting_idr && !nalu.header.type_.is_irap() {
                        return Err(PlanError::AwaitingIdr);
                    }
                    let mut slice = match self.parser.parse_slice_header(nalu) {
                        Ok(slice) => slice,
                        Err(err) => {
                            // Continuation that fails to parse is a mid-AU cut:
                            // keep already-planned slices. HEVC's 2-byte header
                            // accepts every type, so the failure surfaces here
                            // (H.264's equivalent fires at the NALU header).
                            if current.is_some() {
                                warnings.push(PlanWarning::TruncatedAu {
                                    offset: range.start,
                                });
                                break;
                            }
                            return Err(Self::slice_parse_error(err));
                        }
                    };

                    // 7.4.7.1: a dependent segment inherits everything but its
                    // address. Completing the header here means SlicePlan is full
                    // and the continuity checks below see real values.
                    if slice.header.dependent_slice_segment_flag {
                        let independent =
                            self.last_independent_header.clone().ok_or_else(|| {
                                PlanError::Parse(
                                    "dependent slice segment without a preceding \
                                     independent slice segment header"
                                        .into(),
                                )
                            })?;
                        slice
                            .replace_header(independent)
                            .map_err(PlanError::Parse)?;
                    }

                    match &current {
                        None => {
                            current = Some(self.begin_picture(&slice, &mut warnings)?);
                            self.awaiting_idr = false;
                        }
                        // Contract is one picture per AU. A second first-segment
                        // means the pump is broken (upstream would start another).
                        Some(_) if slice.header.first_slice_segment_in_pic_flag => {
                            return Err(PlanError::OutsideEnvelope(
                                "more than one coded picture in one access unit",
                            ));
                        }
                        Some(cur) => {
                            // Continuation must belong to the picture the first
                            // segment began (7.4.2.4.4 same NALU type; 7.4.7.1
                            // same POC lsb). Drop a foreign slice and the tail.
                            if slice.nalu.header.type_ != cur.pic.nalu_type
                                || i32::from(slice.header.pic_order_cnt_lsb)
                                    != cur.pic.slice_pic_order_cnt_lsb
                            {
                                warnings.push(PlanWarning::TruncatedAu {
                                    offset: range.start,
                                });
                                break;
                            }
                        }
                    }
                    if !slice.header.dependent_slice_segment_flag {
                        self.last_independent_header = Some(slice.header.clone());
                    }
                    let cur = current.as_mut().expect("a picture was begun above");
                    slices.push(self.plan_slice(cur, slice, range, &mut warnings)?);
                }
                other => trace!("skipping NAL unit type {other:?}"),
            }
        }

        if !saw_nalu {
            return Err(PlanError::Parse("no NAL units in access unit".into()));
        }
        let cur = current
            .ok_or_else(|| PlanError::Parse("access unit contains no coded picture".into()))?;

        // 8.3.2 retains pictures this AU does not use (`used_by_curr_pic`
        // clear), so the slice lists decide. An IRAP's lists are empty, so this
        // is vacuously true; a concealed picture's own warnings keep it unclean.
        let concealed = warnings.iter().any(PlanWarning::is_integrity);
        let references_clean = self.report.references_clean(
            slices
                .iter()
                .flat_map(|s: &SlicePlan| s.ref_list0.iter().chain(&s.ref_list1))
                .map(|r| r.id),
            concealed,
        );
        let picture = Self::picture_plan(&cur, recovery_point, references_clean);
        let rps = cur.rps_plan.clone();
        let dpb_refs = cur.dpb_refs.clone();
        // Cloned before finish_picture consumes `cur`.
        let pps = Rc::clone(&cur.first_slice_pps);
        let sps = Rc::clone(&pps.sps);
        let stored = self.finish_picture(cur)?;
        let dpb = self
            .report
            .close(stored, self.live_ids(), references_clean, concealed);

        Ok(AuPlan {
            picture,
            rps,
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

    /// Drain the DPB: every still-buffered picture becomes display-ready and
    /// every id is released. Session calls this at teardown or a discontinuity.
    ///
    /// 8.3 decoding state is discarded; planning resumes only at an IRAP
    /// ([`PlanError::AwaitingIdr`]). Parameter sets survive (7.4.2.4).
    pub fn flush(&mut self) -> DpbUpdate {
        let live = self.live_ids();
        self.drain_dpb();

        self.rps = Default::default();
        self.prev_tid0_pic = None;
        self.negotiation_info = Default::default();
        self.last_independent_header = None;
        self.irap_no_rasl_output_flag = false;
        // Resuming picture is first after EOS: any IRAP gets NoRaslOutputFlag = 1
        // (8.1.3), which is what makes non-IDR re-entry sound.
        self.first_picture_after_eos = true;
        self.awaiting_idr = true;
        self.report.flush(live)
    }

    /// Envelope: no interlaced video, separate-colour-plane, SCC self-reference,
    /// or DPB deeper than 16. Clients only decode hosts; hosts emit none of these.
    fn check_envelope(sps: &Sps) -> Result<(), PlanError> {
        if sps.separate_colour_plane_flag {
            return Err(PlanError::OutsideEnvelope(
                "separate colour plane coding (separate_colour_plane_flag == 1)",
            ));
        }
        // HEVC has no frame_mbs_only_flag; field coding is VUI field_seq_flag
        // (and pic_struct SEI). Parser defaults the flag to 0 when VUI is absent.
        if sps.vui_parameters.field_seq_flag {
            return Err(PlanError::OutsideEnvelope(
                "field-coded stream (vui field_seq_flag == 1)",
            ));
        }
        let ptl = &sps.profile_tier_level;
        if ptl.general_interlaced_source_flag && !ptl.general_progressive_source_flag {
            return Err(PlanError::OutsideEnvelope(
                "interlaced source (general_interlaced_source_flag)",
            ));
        }
        // A.4 caps the DPB at 16. Parser reads sps_max_dec_pic_buffering_minus1
        // up to 16 (17 frames); no hardware implements deeper. Gated at SPS
        // activation because backends size slot pools from this.
        let buffering =
            usize::from(sps.max_dec_pic_buffering_minus1[usize::from(sps.max_sub_layers_minus1)])
                + 1;
        if buffering > 16 {
            return Err(PlanError::OutsideEnvelope(
                "DPB deeper than 16 frames (sps_max_dec_pic_buffering_minus1)",
            ));
        }
        if sps.scc_extension.curr_pic_ref_enabled_flag {
            return Err(PlanError::OutsideEnvelope(
                "SCC current-picture referencing (sps_curr_pic_ref_enabled_flag)",
            ));
        }
        // 7.4.3.2.1: window inside the coded size. Vendored `visible_rectangle()`
        // subtracts in u32 and panics on overflow; check in u64 (ue(v) unbounded).
        const SUB_WIDTH_C: [u64; 4] = [1, 2, 2, 1];
        const SUB_HEIGHT_C: [u64; 4] = [1, 2, 1, 1];
        if sps.conformance_window_flag {
            let idx = usize::from(sps.chroma_array_type.min(3));
            let horizontal = SUB_WIDTH_C[idx]
                * (u64::from(sps.conf_win_left_offset) + u64::from(sps.conf_win_right_offset));
            let vertical = SUB_HEIGHT_C[idx]
                * (u64::from(sps.conf_win_top_offset) + u64::from(sps.conf_win_bottom_offset));
            if horizontal >= u64::from(sps.width()) || vertical >= u64::from(sps.height()) {
                return Err(PlanError::Parse(
                    "conformance window exceeds the coded picture".into(),
                ));
            }
        }
        Ok(())
    }

    /// Map a vendored slice-header parse failure to [`PlanError::NoActiveParamSet`].
    /// The needle is the vendored parser's own wording, not ours; reword it here
    /// and the error degrades to `Parse`.
    fn slice_parse_error(err: String) -> PlanError {
        match err.split_once("PPS for pic_parameter_set_id ") {
            Some((_, id)) => PlanError::NoActiveParamSet {
                pps_id: id.trim().parse().unwrap_or(0),
            },
            None => PlanError::Parse(err),
        }
    }

    fn live_ids(&self) -> BTreeSet<PicId> {
        self.dpb.entries().iter().map(|entry| entry.1).collect()
    }

    /// Queue pictures C.5.2 declares ready. `additional` selects C.5.2.3 (after
    /// decode) over C.5.2.2 (before) — upstream's `BumpingType`.
    fn bump_as_needed(&mut self, sps: &Sps, additional: bool) {
        loop {
            let needs = if additional {
                self.dpb.needs_additional_bumping(sps)
            } else {
                self.dpb.needs_bumping(sps)
            };
            if !needs {
                break;
            }
            match self.dpb.bump(false) {
                Some(entry) => self.report.pending_outputs.push(entry.1),
                None => break,
            }
        }
    }

    fn drain_dpb(&mut self) {
        let pics = self.dpb.drain();
        self.report
            .pending_outputs
            .extend(pics.into_iter().map(|e| e.1));
        self.dpb.clear();
    }

    // 8.3.2 Note 2.
    fn st_ref_pic_set<'a>(
        hdr: &'a SliceHeader,
        sps: &'a Sps,
    ) -> Result<&'a ShortTermRefPicSet, PlanError> {
        if hdr.curr_rps_idx == sps.num_short_term_ref_pic_sets {
            Ok(&hdr.short_term_ref_pic_set)
        } else {
            sps.short_term_ref_pic_set
                .get(usize::from(hdr.curr_rps_idx))
                .ok_or_else(|| PlanError::Parse("invalid short_term_ref_pic_set_idx".into()))
        }
    }

    // 8.3.2: derivation of the five POC lists.
    fn decode_rps(
        &mut self,
        slice: &Slice,
        sps: &Sps,
        cur_pic: &PictureData,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<(), PlanError> {
        let hdr = &slice.header;

        if cur_pic.nalu_type.is_irap() && cur_pic.no_rasl_output_flag {
            self.dpb.mark_all_as_unused_for_ref();
        }

        self.rps = RefPicSet::default();

        if !slice.nalu.header.type_.is_idr() {
            let curr_st_rps = Self::st_ref_pic_set(hdr, sps)?;
            // Equation 8-5, short-term half. Saturating: a POC outside i32 is a
            // corrupt header and must become a missing-reference warning, not
            // overflow here (upstream adds unchecked).
            for i in 0..usize::from(curr_st_rps.num_negative_pics) {
                let poc = cur_pic
                    .pic_order_cnt_val
                    .saturating_add(curr_st_rps.delta_poc_s0[i]);
                if curr_st_rps.used_by_curr_pic_s0[i] {
                    self.rps.poc_st_curr_before.push(poc);
                } else {
                    self.rps.poc_st_foll.push(poc);
                }
            }
            for i in 0..usize::from(curr_st_rps.num_positive_pics) {
                let poc = cur_pic
                    .pic_order_cnt_val
                    .saturating_add(curr_st_rps.delta_poc_s1[i]);
                if curr_st_rps.used_by_curr_pic_s1[i] {
                    self.rps.poc_st_curr_after.push(poc);
                } else {
                    self.rps.poc_st_foll.push(poc);
                }
            }

            // Equation 8-5, long-term half: PocLtCurr/PocLtFoll from PocLsbLt
            // plus optional MSB cycle. RFI recovery pins an anchor here.
            let num_lt = usize::from(hdr.num_long_term_sps) + usize::from(hdr.num_long_term_pics);
            for i in 0..num_lt.min(hdr.poc_lsb_lt.len()) {
                let mut poc_lt = i64::from(hdr.poc_lsb_lt[i]);
                if hdr.delta_poc_msb_present_flag[i] {
                    poc_lt += i64::from(cur_pic.pic_order_cnt_val);
                    poc_lt -= i64::from(hdr.delta_poc_msb_cycle_lt[i])
                        * i64::from(self.max_pic_order_cnt_lsb);
                    poc_lt -=
                        i64::from(cur_pic.pic_order_cnt_val & (self.max_pic_order_cnt_lsb - 1));
                }
                let poc_lt = poc_lt.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
                if hdr.used_by_curr_pic_lt[i] {
                    self.rps
                        .poc_lt_curr
                        .push((poc_lt, hdr.delta_poc_msb_present_flag[i]));
                } else {
                    self.rps
                        .poc_lt_foll
                        .push((poc_lt, hdr.delta_poc_msb_present_flag[i]));
                }
            }
        }

        self.derive_and_mark_rps(warnings);
        Ok(())
    }

    // 8.3.2 second half (8-6/8-7 plus marking). `None` stays positional for
    // list construction; a miss the current picture needs is MissingReference.
    fn derive_and_mark_rps(&mut self, warnings: &mut Vec<PlanWarning>) {
        let mask = self.max_pic_order_cnt_lsb.wrapping_sub(1);

        // Equation 8-6.
        for &(poc, msb_present) in &self.rps.poc_lt_curr {
            let reference = if msb_present {
                self.dpb.find_ref_by_poc(poc)
            } else {
                // 7.4.7.1: at most one reference per poc_lsb when delta_poc_msb
                // is absent (RFI anchor path). Vendored find takes the oldest of
                // several matches; still pick (concealment) but flag.
                let candidates = self
                    .dpb
                    .pictures()
                    .filter(|p| p.is_ref() && (p.pic_order_cnt_val & mask) == poc)
                    .count();
                if candidates > 1 {
                    warnings.push(PlanWarning::MissingReference {
                        context: "ambiguous long-term poc_lsb (7.4.7.1 requires a unique match)",
                        detail: format!("poc lsb {poc}: {candidates} candidates"),
                    });
                }
                self.dpb.find_ref_by_poc_masked(poc, mask)
            };
            if reference.is_none() {
                warnings.push(PlanWarning::MissingReference {
                    context: "long-term RPS entry (RefPicSetLtCurr)",
                    detail: format!("poc {poc}"),
                });
            }
            self.rps.ref_pic_set_lt_curr.push(reference);
        }
        for &(poc, msb_present) in &self.rps.poc_lt_foll {
            let reference = if msb_present {
                self.dpb.find_ref_by_poc(poc)
            } else {
                self.dpb.find_ref_by_poc_masked(poc, mask)
            };
            if reference.is_none() {
                // Not used by this picture; a future RPS will warn. Trace only.
                trace!("RefPicSetLtFoll entry poc {poc} not in the DPB");
            }
            self.rps.ref_pic_set_lt_foll.push(reference);
        }

        for pic in self.rps.ref_pic_set_lt_curr.iter().flatten() {
            pic.0.borrow_mut().set_reference(Reference::LongTerm);
        }
        for pic in self.rps.ref_pic_set_lt_foll.iter().flatten() {
            pic.0.borrow_mut().set_reference(Reference::LongTerm);
        }

        // Equation 8-7.
        for &poc in &self.rps.poc_st_curr_before {
            let reference = self.dpb.find_short_term_ref_by_poc(poc);
            if reference.is_none() {
                warnings.push(PlanWarning::MissingReference {
                    context: "short-term RPS entry (RefPicSetStCurrBefore)",
                    detail: format!("poc {poc}"),
                });
            }
            self.rps.ref_pic_set_st_curr_before.push(reference);
        }
        for &poc in &self.rps.poc_st_curr_after {
            let reference = self.dpb.find_short_term_ref_by_poc(poc);
            if reference.is_none() {
                warnings.push(PlanWarning::MissingReference {
                    context: "short-term RPS entry (RefPicSetStCurrAfter)",
                    detail: format!("poc {poc}"),
                });
            }
            self.rps.ref_pic_set_st_curr_after.push(reference);
        }
        for &poc in &self.rps.poc_st_foll {
            let reference = self.dpb.find_short_term_ref_by_poc(poc);
            if reference.is_none() {
                trace!("RefPicSetStFoll entry poc {poc} not in the DPB");
            }
            self.rps.ref_pic_set_st_foll.push(reference);
        }

        // A named picture the DPB lacks takes a stand-in at its position, so the
        // decode binds a real picture wherever the slice can point: the nearest
        // present entry of its set, else the DPB reference closest in POC. The
        // MissingReference above still conceals the AU. Hardware fed an inter
        // slice with an empty list corrupts its DPB for the rest of the GOP.
        fill_missing(
            &self.dpb,
            &mut self.rps.ref_pic_set_st_curr_before,
            &self.rps.poc_st_curr_before,
        );
        fill_missing(
            &self.dpb,
            &mut self.rps.ref_pic_set_st_curr_after,
            &self.rps.poc_st_curr_after,
        );
        let lt_pocs: Vec<i32> = self.rps.poc_lt_curr.iter().map(|&(poc, _)| poc).collect();
        fill_missing(&self.dpb, &mut self.rps.ref_pic_set_lt_curr, &lt_pocs);

        // 8.3.2 step 4: DPB pictures in none of the five sets become unused.
        // Identity by Rc, not POC — a corrupt-stream collision must not keep
        // the wrong picture alive.
        let in_any_set = |pic: &Rc<RefCell<PictureData>>| {
            self.rps
                .ref_pic_set_lt_curr
                .iter()
                .chain(&self.rps.ref_pic_set_lt_foll)
                .chain(&self.rps.ref_pic_set_st_curr_before)
                .chain(&self.rps.ref_pic_set_st_curr_after)
                .chain(&self.rps.ref_pic_set_st_foll)
                .flatten()
                .any(|entry| Rc::ptr_eq(&entry.0, pic))
        };
        for entry in self.dpb.entries() {
            if !in_any_set(&entry.0) {
                entry.0.borrow_mut().set_reference(Reference::None);
            }
        }
    }

    // C.5.2.2 pre-decode DPB update. Exempt only bitstream picture 0 (DPB empty
    // by definition). An IRAP after in-band EOS still drains (or discards under
    // no_output_of_prior_pics_flag) so sequences never interleave. The flag is
    // this picture's value; upstream clears it first, which makes its check vacuous.
    fn update_dpb_before_decoding(
        &mut self,
        cur_pic: &PictureData,
        was_first_in_bitstream: bool,
        sps: &Sps,
    ) {
        if cur_pic.nalu_type.is_irap() && cur_pic.no_rasl_output_flag && !was_first_in_bitstream {
            if cur_pic.no_output_of_prior_pics_flag {
                // C.3.2: discard prior pictures without output.
                self.dpb.clear();
            } else {
                self.drain_dpb();
            }
        } else {
            self.dpb.remove_unused();
            self.bump_as_needed(sps, false);
        }
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
        // SPS-level SCC gate cannot catch a PPS that enables self-reference on
        // its own. A list containing the current picture is a backend violation.
        if pps.scc_extension.curr_pic_ref_enabled_flag {
            return Err(PlanError::OutsideEnvelope(
                "SCC current-picture referencing (pps_curr_pic_ref_enabled_flag)",
            ));
        }

        // Gate at every activation, not only when NegotiationInfo changes: the
        // parser table keeps a rejected SPS, and NegotiationInfo omits envelope
        // facts. A PPS-only rebind must not reach `visible_rectangle()`'s u32 sub.
        Self::check_envelope(&pps.sps)?;

        // 8.3.1 reads MaxPicOrderCntLsb off the active SPS. Local until accepted.
        let max_pic_order_cnt_lsb = 1i32 << (pps.sps.log2_max_pic_order_cnt_lsb_minus4 + 4);

        // PictureData constructor is 8.3.1 POC + 8.1.3 flags; no planner state.
        let pic = PictureData::new_from_slice(
            slice,
            self.first_picture_in_bitstream,
            self.first_picture_after_eos,
            self.prev_tid0_pic.as_ref(),
            max_pic_order_cnt_lsb,
        );

        if pic.nalu_type.is_rasl() && self.irap_no_rasl_output_flag {
            // 8.1.3 NOTE: RASL of a NoRaslOutputFlag IRAP may name pre-join
            // pictures; neither decode nor output. Refuse before any state
            // change — a RASL AU with a renegotiating SPS must not drain the DPB.
            return Err(PlanError::RaslSkipped {
                poc: pic.pic_order_cnt_val,
            });
        }

        // Picture accepted from here; state changes begin (incl. renegotiation).
        self.renegotiate_if_needed(&pps.sps, warnings);
        self.max_pic_order_cnt_lsb = max_pic_order_cnt_lsb;

        if pic.nalu_type.is_irap() {
            self.irap_no_rasl_output_flag = pic.no_rasl_output_flag;
        }

        let was_first_in_bitstream = self.first_picture_in_bitstream;
        self.first_picture_after_eos = false;
        self.first_picture_in_bitstream = false;

        let id = self.next_pic_id;
        self.next_pic_id += 1;

        self.decode_rps(slice, &pps.sps, &pic, warnings)?;
        self.update_dpb_before_decoding(&pic, was_first_in_bitstream, &pps.sps);

        let rps_plan = self.rps_plan();
        // Beside the RPS that marked it, never later: `finish_picture` stores the
        // current picture, which belongs to the next AU's snapshot.
        let dpb_refs = self.dpb_snapshot();

        Ok(CurrentPicState {
            pic,
            first_slice_pps: Rc::clone(&pps),
            pps,
            id,
            rps_plan,
            dpb_refs,
            prev_segment_address: None,
        })
    }

    /// Marked DPB as [`AuPlan::dpb_refs`]: every picture 8.3.2 left used for
    /// short- or long-term reference, whether or not THIS picture names it.
    fn dpb_snapshot(&self) -> Vec<RefPic> {
        self.dpb
            .get_all_references()
            .iter()
            .map(Self::to_ref_pic)
            .collect()
    }

    /// Caller ([`Self::begin_picture`]) has already run the envelope gate.
    fn renegotiate_if_needed(&mut self, sps: &Sps, warnings: &mut Vec<PlanWarning>) {
        if NegotiationInfo::from(sps) == self.negotiation_info {
            return;
        }
        // Drain so already-planned frames are display-ready before params change.
        self.drain_dpb();
        self.negotiation_info = NegotiationInfo::from(sps);
        self.dpb.set_max_num_pics(dpb_limit(sps));

        let reorder = sps.max_num_reorder_pics[usize::from(sps.max_sub_layers_minus1)];
        if reorder > 0 {
            warnings.push(PlanWarning::NonZeroReorder {
                max_num_reorder_pics: reorder,
            });
        }
    }

    fn plan_slice(
        &self,
        cur: &mut CurrentPicState,
        slice: Slice,
        data: Range<usize>,
        warnings: &mut Vec<PlanWarning>,
    ) -> Result<SlicePlan, PlanError> {
        // 7.4.7.1: slice_segment_address must increase across segments.
        if let Some(prev) = cur.prev_segment_address {
            if slice.header.segment_address <= prev && !slice.header.first_slice_segment_in_pic_flag
            {
                trace!("slice_segment_address does not increase monotonically, expect corrupted output");
            }
        }
        cur.prev_segment_address = Some(slice.header.segment_address);

        let pps = self
            .parser
            .get_pps(slice.header.pic_parameter_set_id)
            .ok_or(PlanError::NoActiveParamSet {
                pps_id: slice.header.pic_parameter_set_id,
            })?;
        cur.pps = Rc::clone(pps);

        // Mid-picture renegotiation would drop the previous slices' context.
        if NegotiationInfo::from(&*cur.pps.sps) != self.negotiation_info {
            return Err(PlanError::Parse(
                "invalid stream: mid-picture renegotiation requested".into(),
            ));
        }

        let (ref_list0, ref_list1) = self.build_ref_pic_lists(&slice.header, warnings);

        // 8.3.4: an inter slice needs num_ref_idx_l0_active entries. Empty
        // (every candidate lost) is undecodable; flag so the session recovers.
        let slice_type = slice.header.type_;
        if (slice_type.is_p() || slice_type.is_b()) && ref_list0.is_empty() {
            warnings.push(PlanWarning::MissingReference {
                context: "inter slice with no usable RefPicList0",
                detail: format!("slice_type {slice_type:?}"),
            });
        }
        if slice_type.is_b() && ref_list1.is_empty() {
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

    // 8.3.4: P/B reference lists as backend [`RefPic`]s, in-place concealment.
    fn build_ref_pic_lists(
        &self,
        hdr: &SliceHeader,
        warnings: &mut Vec<PlanWarning>,
    ) -> (Vec<RefPic>, Vec<RefPic>) {
        if !hdr.type_.is_p() && !hdr.type_.is_b() {
            return (Vec::new(), Vec::new());
        }

        // 8-8/8-10 cycle the three current sets until the list is full; all
        // three empty never terminates. Upstream only guards behind SCC flags.
        // Bail empty; plan_slice flags them.
        if self.rps.curr_is_empty() {
            return (Vec::new(), Vec::new());
        }

        let list0 = self.build_one_list(
            hdr,
            usize::from(hdr.num_ref_idx_l0_active_minus1) + 1,
            hdr.ref_pic_list_modification
                .ref_pic_list_modification_flag_l0,
            &hdr.ref_pic_list_modification.list_entry_l0,
            // Equation 8-8: list 0 leads with the past (StCurrBefore).
            [
                &self.rps.ref_pic_set_st_curr_before,
                &self.rps.ref_pic_set_st_curr_after,
                &self.rps.ref_pic_set_lt_curr,
            ],
            warnings,
        );

        let list1 = if hdr.type_.is_b() {
            self.build_one_list(
                hdr,
                usize::from(hdr.num_ref_idx_l1_active_minus1) + 1,
                hdr.ref_pic_list_modification
                    .ref_pic_list_modification_flag_l1,
                &hdr.ref_pic_list_modification.list_entry_l1,
                // Equation 8-10: list 1 leads with the future (StCurrAfter).
                [
                    &self.rps.ref_pic_set_st_curr_after,
                    &self.rps.ref_pic_set_st_curr_before,
                    &self.rps.ref_pic_set_lt_curr,
                ],
                warnings,
            )
        } else {
            Vec::new()
        };

        (list0, list1)
    }

    #[allow(clippy::type_complexity)]
    fn build_one_list(
        &self,
        hdr: &SliceHeader,
        num_active: usize,
        modification_flag: bool,
        list_entries: &[u32],
        set_order: [&Vec<Option<DpbEntry<PicId>>>; 3],
        warnings: &mut Vec<PlanWarning>,
    ) -> Vec<RefPic> {
        // Equations 8-8/8-10: RefPicListTempX cycles until NumRpsCurrTempListX.
        let temp_len = num_active.max(hdr.num_pic_total_curr as usize);
        let mut temp: Vec<Option<RefPic>> = Vec::with_capacity(temp_len);
        'fill: while temp.len() < temp_len {
            for set in set_order {
                for entry in set {
                    if temp.len() == temp_len {
                        break 'fill;
                    }
                    temp.push(entry.as_ref().map(Self::to_ref_pic));
                }
            }
        }

        // Equations 8-9/8-11: temporal list, reordered via list_entry_lX.
        let mut list: Vec<Option<RefPic>> = Vec::with_capacity(num_active);
        for r_idx in 0..num_active {
            let entry = if modification_flag {
                match list_entries
                    .get(r_idx)
                    .and_then(|&idx| temp.get(idx as usize))
                {
                    Some(entry) => *entry,
                    None => {
                        // Parser bounds list_entry below NumPicTotalCurr; this is
                        // for a future parser re-sync.
                        warnings.push(PlanWarning::MissingReference {
                            context: "ref_pic_list_modification entry out of range",
                            detail: format!("ref_idx {r_idx}"),
                        });
                        None
                    }
                }
            } else {
                temp.get(r_idx).copied().flatten()
            };
            list.push(entry);
        }

        Self::substitute_in_place(list)
    }

    /// Fill holes a lost reference leaves, preserving list positions: every
    /// `ref_idx` indexes the returned Vec 1:1. A hole takes the previous existing
    /// entry, else the first. Compacting would shift later `ref_idx`s. An all-hole
    /// list collapses to empty (caller warns); `fill_missing` leaves none.
    fn substitute_in_place(list: Vec<Option<RefPic>>) -> Vec<RefPic> {
        let first_existing = list.iter().flatten().next().copied();
        let mut out = Vec::with_capacity(list.len());
        let mut prev_existing: Option<RefPic> = None;
        for slot in &list {
            match slot {
                Some(real) => {
                    prev_existing = Some(*real);
                    out.push(*real);
                }
                None => {
                    if let Some(substitute) = prev_existing.or(first_existing) {
                        out.push(substitute);
                    }
                }
            }
        }
        out
    }

    fn to_ref_pic(entry: &DpbEntry<PicId>) -> RefPic {
        let pic = entry.0.borrow();
        RefPic {
            id: entry.1,
            pic_order_cnt: pic.pic_order_cnt_val,
            is_long_term: matches!(pic.reference(), Reference::LongTerm),
        }
    }

    fn rps_plan(&self) -> RpsPlan {
        let convert = |set: &Vec<Option<DpbEntry<PicId>>>| -> Vec<RefPic> {
            set.iter().flatten().map(Self::to_ref_pic).collect()
        };
        RpsPlan {
            st_curr_before: convert(&self.rps.ref_pic_set_st_curr_before),
            st_curr_after: convert(&self.rps.ref_pic_set_st_curr_after),
            lt_curr: convert(&self.rps.ref_pic_set_lt_curr),
        }
    }

    fn finish_picture(&mut self, cur: CurrentPicState) -> Result<PicId, PlanError> {
        let CurrentPicState { pic, pps, id, .. } = cur;

        // 8.3.1: this picture becomes PrevTid0Pic if eligible.
        if pic.valid_for_prev_tid0_pic {
            self.prev_tid0_pic = Some(pic.clone());
        }

        let sps = Rc::clone(&pps.sps);

        // Store first, then bump (C.3.4 marks short-term inside store_picture).
        self.dpb
            .store_picture(Rc::new(RefCell::new(pic)), id)
            .map_err(PlanError::Parse)?;
        self.bump_as_needed(&sps, true);

        Ok(id)
    }

    fn picture_plan(
        cur: &CurrentPicState,
        recovery_point: Option<RecoveryPointHevc>,
        references_clean: bool,
    ) -> PicturePlan {
        let pic = &cur.pic;
        // First slice's PPS defines the picture; `cur.pps` may have drifted.
        let sps = &cur.first_slice_pps.sps;
        let rect = sps.visible_rectangle();

        PicturePlan {
            nalu_type: pic.nalu_type,
            is_idr: pic.nalu_type.is_idr(),
            is_irap: pic.nalu_type.is_irap(),
            no_rasl_output_flag: pic.no_rasl_output_flag,
            is_reference: !pic.nalu_type.is_slnr(),
            pic_order_cnt: pic.pic_order_cnt_val,
            coded_width: u32::from(sps.width()),
            coded_height: u32::from(sps.height()),
            // Vendored `visible_rectangle()`: crop OFFSET in `min`, visible SIZE
            // in `max` (not an edge), SubWidthC/SubHeightC already applied.
            display_crop: DisplayCrop {
                x: rect.min.x,
                y: rect.min.y,
                width: rect.max.x,
                height: rect.max.y,
            },
            // Unconditional: parser Default already holds E.3.1 (2/2/2, limited
            // range); parse overwrites only under present flags. Fields are u32;
            // E.2 is u(8), so the casts cannot truncate.
            colour: ColourDescription {
                colour_primaries: sps.vui_parameters.colour_primaries as u8,
                transfer_characteristics: sps.vui_parameters.transfer_characteristics as u8,
                matrix_coefficients: sps.vui_parameters.matrix_coeffs as u8,
                video_full_range: sps.vui_parameters.video_full_range_flag,
            },
            general_profile_idc: sps.profile_tier_level.general_profile_idc,
            level_idc: sps.profile_tier_level.general_level_idc,
            bit_depth_luma_minus8: sps.bit_depth_luma_minus8,
            bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8,
            chroma_format_idc: sps.chroma_format_idc,
            max_dpb_frames: dpb_limit(sps),
            short_term_ref_pic_set_size_bits: pic.short_term_ref_pic_set_size_bits,
            recovery_point,
            references_clean,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::testing::split_h265_aus;

    const TEST_25FPS: &[u8] = crate::testing::H265_25FPS;
    const TEST_BEAR: &[u8] = crate::testing::H265_BEAR;
    const TEST_BBB: &[u8] = crate::testing::H265_BBB;
    const TEST_64X64_I_P_B_P: &[u8] = crate::testing::H265_64X64_I_P_B_P;

    /// Plan a vendored clip: every AU plans, no integrity warnings, every stored
    /// id reaches output once, outputs ascend POC within each IRAP period.
    fn plan_whole_clip(stream: &[u8]) -> (H265Planner, Vec<AuPlan>) {
        let aus = split_h265_aus(stream);
        let mut planner = H265Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(
                planner
                    .plan_au(au)
                    .expect("the clean vector must plan without errors"),
            );
        }

        for plan in &plans {
            assert!(
                !plan.warnings.iter().any(PlanWarning::is_integrity),
                "clean vector produced an integrity warning: {:?}",
                plan.warnings
            );
        }

        let stored: BTreeSet<PicId> = plans.iter().filter_map(|p| p.dpb.stored).collect();
        assert_eq!(stored.len(), plans.len());
        let mut emitted: Vec<PicId> = plans
            .iter()
            .flat_map(|p| p.dpb.outputs.iter().copied())
            .collect();
        emitted.extend(planner.flush().outputs);
        let output: BTreeSet<PicId> = emitted.iter().copied().collect();
        assert_eq!(
            output.len(),
            emitted.len(),
            "no picture may be output twice"
        );
        assert_eq!(
            output, stored,
            "bumping plus the final flush must output every picture"
        );

        // Within each IRAP period, ids must emerge in ascending POC (C.5.2).
        // An IRAP with NoRaslOutputFlag resets POC continuity, hence the key.
        let mut period = 0usize;
        let mut order_key: BTreeMap<PicId, (usize, i32)> = BTreeMap::new();
        for plan in &plans {
            if plan.picture.is_irap && plan.picture.no_rasl_output_flag {
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
                    "outputs must emerge in ascending POC order per IRAP period: \
                     {key:?} emitted after {last:?}"
                );
            }
            last = Some(key);
        }

        (planner, plans)
    }

    #[test]
    fn the_full_25fps_vector_plans_every_picture_and_every_pic_id_reaches_output() {
        let aus = split_h265_aus(TEST_25FPS);
        assert_eq!(aus.len(), 250, "the vendored golden: 250 pictures");
        let (_, plans) = plan_whole_clip(TEST_25FPS);
        assert_eq!(plans.len(), 250);
        assert_eq!(plans.iter().map(|p| p.slices.len()).sum::<usize>(), 250);
        assert!(plans[0].picture.is_idr);
        assert_eq!(
            plans[0].picture.pic_order_cnt, 0,
            "POC 0 at the opening IRAP"
        );
        for plan in &plans {
            for slice in &plan.slices {
                if slice.header.type_.is_p() || slice.header.type_.is_b() {
                    assert!(!slice.ref_list0.is_empty());
                }
            }
        }
    }

    #[test]
    fn the_bear_and_bbb_vectors_plan_clean_end_to_end() {
        let (_, bear) = plan_whole_clip(TEST_BEAR);
        assert!(!bear.is_empty());
        let (_, bbb) = plan_whole_clip(TEST_BBB);
        assert!(!bbb.is_empty());
    }

    /// `PF_H265_DUMP=<capture>`: print each picture's active references, to read an
    /// encoder's reference structure off a capture. Diagnostics only.
    #[test]
    #[ignore = "diagnostic: set PF_H265_DUMP to a capture"]
    fn print_active_references_of_a_capture() {
        let path = std::env::var("PF_H265_DUMP").expect("PF_H265_DUMP=<capture>");
        let bytes = std::fs::read(&path).expect("read the capture");
        let mut planner = H265Planner::new();
        for (i, au) in split_h265_aus(&bytes).iter().enumerate() {
            let plan = planner.plan_au(au).expect("plan");
            let refs: Vec<i32> = plan
                .slices
                .iter()
                .flat_map(|s| s.ref_list0.iter().chain(&s.ref_list1))
                .map(|r| r.pic_order_cnt)
                .collect();
            println!(
                "{i}: poc {} refs {refs:?} clean {} warnings {}",
                plan.picture.pic_order_cnt,
                plan.picture.references_clean,
                plan.warnings.len()
            );
        }
    }

    /// [`PicturePlan::references_clean`] on real bitstreams: two lossless
    /// conformance clips must report every picture clean. A false mark would
    /// refuse every host recovery anchor.
    ///
    /// Clips carry B-slices and reordering, so they exercise "lists, not the
    /// RPS": 8.3.2 retains unused pictures, and folding those in would condemn
    /// pictures at random.
    #[test]
    fn a_lossless_conformance_clip_reports_clean_references_on_every_picture() {
        for (name, clip) in [("bear", TEST_BEAR), ("bbb", TEST_BBB)] {
            let (_, plans) = plan_whole_clip(clip);
            assert!(!plans.is_empty(), "{name} produced no plans");
            for (i, plan) in plans.iter().enumerate() {
                assert!(
                    plan.picture.references_clean,
                    "{name} picture {i} (poc {}) must read clean on a lossless clip",
                    plan.picture.pic_order_cnt
                );
            }
        }
    }

    #[test]
    fn b_slices_get_a_future_led_list1_distinct_from_list0() {
        let aus = split_h265_aus(TEST_64X64_I_P_B_P);
        let mut planner = H265Planner::new();
        let mut b_slices_seen = 0usize;

        for au in &aus {
            let plan = planner
                .plan_au(au)
                .expect("the clean vector must plan without errors");
            for slice in &plan.slices {
                if !slice.header.type_.is_b() {
                    continue;
                }
                b_slices_seen += 1;
                assert!(!slice.ref_list0.is_empty());
                assert!(!slice.ref_list1.is_empty());
                // 8.3.4: list0 leads with the past, list1 with the future.
                assert!(slice.ref_list0[0].pic_order_cnt < plan.picture.pic_order_cnt);
                assert!(slice.ref_list1[0].pic_order_cnt > plan.picture.pic_order_cnt);
                assert!(!plan.rps.st_curr_before.is_empty());
                assert!(!plan.rps.st_curr_after.is_empty());
            }
        }

        assert!(b_slices_seen > 0, "the vector must contain B slices");
    }

    /// Minimal bit writer for the syntax the planner reads. Vendored crate has
    /// no H.265 builder (encoder is H.264-only). Headers only; no slice data
    /// after the alignment bit.
    struct BitSink {
        bytes: Vec<u8>,
        acc: u8,
        nbits: u8,
    }

    impl BitSink {
        fn new() -> Self {
            BitSink {
                bytes: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }

        fn bit(&mut self, b: u32) {
            self.acc = (self.acc << 1) | (b as u8 & 1);
            self.nbits += 1;
            if self.nbits == 8 {
                self.bytes.push(self.acc);
                self.acc = 0;
                self.nbits = 0;
            }
        }

        fn bits(&mut self, count: usize, value: u32) {
            for i in (0..count).rev() {
                self.bit((value >> i) & 1);
            }
        }

        fn ue(&mut self, value: u32) {
            let x = value + 1;
            let len = 32 - x.leading_zeros() as usize;
            self.bits(len - 1, 0);
            self.bits(len, x);
        }

        fn se(&mut self, value: i32) {
            let k = if value > 0 {
                2 * value as u32 - 1
            } else {
                (-2 * (value as i64)) as u32
            };
            self.ue(k);
        }

        /// rbsp_trailing_bits(): stop bit plus zero pad to a byte boundary.
        fn finish(mut self) -> Vec<u8> {
            self.bit(1);
            while self.nbits != 0 {
                self.bit(0);
            }
            self.bytes
        }
    }

    /// RBSP in start code + 2-byte HEVC NAL header + emulation prevention.
    fn h265_nalu_with_layer(nalu_type: u8, layer_id: u8, rbsp: &[u8]) -> Vec<u8> {
        let mut out = vec![
            0x00,
            0x00,
            0x00,
            0x01,
            (nalu_type << 1) | (layer_id >> 5),
            ((layer_id & 0x1f) << 3) | 0x01, // nuh_temporal_id_plus1 = 1
        ];
        let mut zeros = 0usize;
        for &byte in rbsp {
            if zeros >= 2 && byte <= 0x03 {
                out.push(0x03);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }

    fn h265_nalu(nalu_type: u8, rbsp: &[u8]) -> Vec<u8> {
        h265_nalu_with_layer(nalu_type, 0, rbsp)
    }

    #[derive(Clone)]
    enum VuiOpt {
        Absent,
        SignalType {
            full_range: bool,
            colour: Option<(u8, u8, u8)>,
        },
        FieldSeq,
    }

    #[derive(Clone)]
    pub(super) struct SpsOpts {
        profile_idc: u8,
        chroma_format_idc: u32,
        width: u32,
        height: u32,
        bit_depth_minus8: u32,
        /// `general_level_idc` (30 × level: 120 = L4, 153 = L5.1). A-2 keys on
        /// it; the planner does not.
        level_idc: u32,
        /// (left, right, top, bottom) conf_win offsets, in chroma units.
        conf_win: Option<(u32, u32, u32, u32)>,
        max_dec_pic_buffering_minus1: u32,
        max_num_reorder_pics: u32,
        long_term: bool,
        vui: VuiOpt,
    }

    impl Default for SpsOpts {
        fn default() -> Self {
            SpsOpts {
                profile_idc: 1, // Main
                chroma_format_idc: 1,
                width: 64,
                height: 64,
                bit_depth_minus8: 0,
                level_idc: 120, // L4
                conf_win: None,
                max_dec_pic_buffering_minus1: 4,
                max_num_reorder_pics: 0,
                long_term: false,
                vui: VuiOpt::Absent,
            }
        }
    }

    /// profile_tier_level() general block. Per-sub-layer tail follows only when
    /// max_sub_layers_minus1 > 0. Constraint/reserved bits stay zero.
    fn ptl_general(s: &mut BitSink, profile_idc: u8, level_idc: u32) {
        s.bits(2, 0);
        s.bit(0);
        s.bits(5, u32::from(profile_idc));
        s.bits(32, 0);
        s.bit(1); // general_progressive_source_flag
        s.bit(0); // general_interlaced_source_flag
        s.bit(0); // general_non_packed_constraint_flag
        s.bit(1); // general_frame_only_constraint_flag
        s.bits(31, 0);
        s.bits(12, 0); // 43 zero bits total
        s.bit(0); // general_inbld_flag / reserved
        s.bits(8, level_idc); // general_level_idc
    }

    fn synth_sps(o: &SpsOpts) -> Vec<u8> {
        let mut s = BitSink::new();
        s.bits(4, 0); // sps_video_parameter_set_id
        s.bits(3, 0); // sps_max_sub_layers_minus1
        s.bit(1); // sps_temporal_id_nesting_flag
        ptl_general(&mut s, o.profile_idc, o.level_idc);

        s.ue(0); // sps_seq_parameter_set_id
        s.ue(o.chroma_format_idc);
        if o.chroma_format_idc == 3 {
            s.bit(0); // separate_colour_plane_flag
        }
        s.ue(o.width);
        s.ue(o.height);
        match o.conf_win {
            Some((left, right, top, bottom)) => {
                s.bit(1);
                s.ue(left);
                s.ue(right);
                s.ue(top);
                s.ue(bottom);
            }
            None => s.bit(0),
        }
        s.ue(o.bit_depth_minus8); // bit_depth_luma_minus8
        s.ue(o.bit_depth_minus8); // bit_depth_chroma_minus8
        s.ue(0); // log2_max_pic_order_cnt_lsb_minus4: 4-bit POC lsb
        s.bit(1); // sps_sub_layer_ordering_info_present_flag
        s.ue(o.max_dec_pic_buffering_minus1);
        s.ue(o.max_num_reorder_pics);
        s.ue(0); // sps_max_latency_increase_plus1
        s.ue(0); // log2_min_luma_coding_block_size_minus3: 8
        s.ue(3); // log2_diff_max_min_luma_coding_block_size: CTB 64
        s.ue(0); // log2_min_luma_transform_block_size_minus2: 4
        s.ue(3); // log2_diff_max_min_luma_transform_block_size: 32
        s.ue(0); // max_transform_hierarchy_depth_inter
        s.ue(0); // max_transform_hierarchy_depth_intra
        s.bit(0); // scaling_list_enabled_flag
        s.bit(0); // amp_enabled_flag
        s.bit(0); // sample_adaptive_offset_enabled_flag
        s.bit(0); // pcm_enabled_flag
        s.ue(0); // num_short_term_ref_pic_sets
        if o.long_term {
            s.bit(1); // long_term_ref_pics_present_flag
            s.ue(0); // num_long_term_ref_pics_sps
        } else {
            s.bit(0);
        }
        s.bit(0); // sps_temporal_mvp_enabled_flag
        s.bit(0); // strong_intra_smoothing_enabled_flag
        match &o.vui {
            VuiOpt::Absent => s.bit(0),
            vui => {
                s.bit(1); // vui_parameters_present_flag
                s.bit(0); // aspect_ratio_info_present_flag
                s.bit(0); // overscan_info_present_flag
                match vui {
                    VuiOpt::SignalType { full_range, colour } => {
                        s.bit(1); // video_signal_type_present_flag
                        s.bits(3, 5); // video_format: unspecified
                        s.bit(u32::from(*full_range));
                        match colour {
                            Some((primaries, transfer, matrix)) => {
                                s.bit(1); // colour_description_present_flag
                                s.bits(8, u32::from(*primaries));
                                s.bits(8, u32::from(*transfer));
                                s.bits(8, u32::from(*matrix));
                            }
                            None => s.bit(0),
                        }
                    }
                    _ => s.bit(0), // video_signal_type_present_flag
                }
                s.bit(0); // chroma_loc_info_present_flag
                s.bit(0); // neutral_chroma_indication_flag
                s.bit(u32::from(matches!(vui, VuiOpt::FieldSeq))); // field_seq_flag
                s.bit(0); // frame_field_info_present_flag
                s.bit(0); // default_display_window_flag
                s.bit(0); // vui_timing_info_present_flag
                s.bit(0); // bitstream_restriction_flag
            }
        }
        s.bit(0); // sps_extension_present_flag
        h265_nalu(33, &s.finish())
    }

    fn synth_pps(dependent_slice_segments: bool) -> Vec<u8> {
        let mut s = BitSink::new();
        s.ue(0); // pps_pic_parameter_set_id
        s.ue(0); // pps_seq_parameter_set_id
        s.bit(u32::from(dependent_slice_segments));
        s.bit(0); // output_flag_present_flag
        s.bits(3, 0); // num_extra_slice_header_bits
        s.bit(0); // sign_data_hiding_enabled_flag
        s.bit(0); // cabac_init_present_flag
        s.ue(0); // num_ref_idx_l0_default_active_minus1
        s.ue(0); // num_ref_idx_l1_default_active_minus1
        s.se(0); // init_qp_minus26
        s.bit(0); // constrained_intra_pred_flag
        s.bit(0); // transform_skip_enabled_flag
        s.bit(0); // cu_qp_delta_enabled_flag
        s.se(0); // pps_cb_qp_offset
        s.se(0); // pps_cr_qp_offset
        s.bit(0); // pps_slice_chroma_qp_offsets_present_flag
        s.bit(0); // weighted_pred_flag
        s.bit(0); // weighted_bipred_flag
        s.bit(0); // transquant_bypass_enabled_flag
        s.bit(0); // tiles_enabled_flag
        s.bit(0); // entropy_coding_sync_enabled_flag
        s.bit(0); // pps_loop_filter_across_slices_enabled_flag
        s.bit(0); // deblocking_filter_control_present_flag
        s.bit(0); // pps_scaling_list_data_present_flag
        s.bit(0); // lists_modification_present_flag
        s.ue(0); // log2_parallel_merge_level_minus2
        s.bit(0); // slice_segment_header_extension_present_flag
        s.bit(0); // pps_extension_present_flag
        h265_nalu(34, &s.finish())
    }

    const IDR_W_RADL: u8 = 19;
    const TRAIL_R: u8 = 1;
    const CRA_NUT: u8 = 21;
    const RASL_N: u8 = 8;

    #[derive(Clone)]
    struct SliceOpts {
        nalu_type: u8,
        layer_id: u8,
        /// Continuation: `Some((address, address_bits, dependent))`.
        segment: Option<(u32, usize, bool)>,
        /// Flag bit is written only when the PPS enables dependent segments.
        pps_dependent_enabled: bool,
        slice_type: u32, // 2 = I, 1 = P, 0 = B
        poc_lsb: u32,
        /// Short-term RPS: (delta_poc_sX_minus1, used_by_curr_pic) pairs.
        neg: Vec<(u32, bool)>,
        pos: Vec<(u32, bool)>,
        /// When the SPS set long_term_ref_pics_present_flag:
        /// (poc_lsb_lt, used_by_curr_pic_lt, delta_poc_msb_cycle_lt).
        lt: Vec<(u32, bool, Option<u32>)>,
        sps_long_term: bool,
        num_ref_idx_l0: u32,
        num_ref_idx_l1: u32,
        no_output_of_prior_pics: bool,
    }

    impl Default for SliceOpts {
        fn default() -> Self {
            SliceOpts {
                nalu_type: TRAIL_R,
                layer_id: 0,
                segment: None,
                pps_dependent_enabled: false,
                slice_type: 1,
                poc_lsb: 0,
                neg: Vec::new(),
                pos: Vec::new(),
                lt: Vec::new(),
                sps_long_term: false,
                num_ref_idx_l0: 1,
                num_ref_idx_l1: 1,
                no_output_of_prior_pics: false,
            }
        }
    }

    fn synth_slice(o: &SliceOpts) -> Vec<u8> {
        let is_irap = (16..=23).contains(&o.nalu_type);
        let is_idr = o.nalu_type == IDR_W_RADL || o.nalu_type == 20;
        let mut s = BitSink::new();
        s.bit(u32::from(o.segment.is_none())); // first_slice_segment_in_pic_flag
        if is_irap {
            s.bit(u32::from(o.no_output_of_prior_pics));
        }
        s.ue(0); // slice_pic_parameter_set_id
        let mut dependent = false;
        if let Some((address, bits, dep)) = o.segment {
            if o.pps_dependent_enabled {
                s.bit(u32::from(dep));
            }
            s.bits(bits, address);
            dependent = dep;
        }
        if !dependent {
            s.ue(o.slice_type);
            if !is_idr {
                s.bits(4, o.poc_lsb);
                s.bit(0); // short_term_ref_pic_set_sps_flag
                          // st_ref_pic_set(stRpsIdx = 0): no inter-RPS prediction flag.
                s.ue(o.neg.len() as u32);
                s.ue(o.pos.len() as u32);
                for &(delta_minus1, used) in &o.neg {
                    s.ue(delta_minus1);
                    s.bit(u32::from(used));
                }
                for &(delta_minus1, used) in &o.pos {
                    s.ue(delta_minus1);
                    s.bit(u32::from(used));
                }
                if o.sps_long_term {
                    s.ue(o.lt.len() as u32); // num_long_term_pics
                    for &(poc_lsb_lt, used, msb) in &o.lt {
                        s.bits(4, poc_lsb_lt);
                        s.bit(u32::from(used));
                        match msb {
                            Some(cycle) => {
                                s.bit(1);
                                s.ue(cycle);
                            }
                            None => s.bit(0),
                        }
                    }
                }
            }
            if o.slice_type != 2 {
                s.bit(1); // num_ref_idx_active_override_flag
                s.ue(o.num_ref_idx_l0 - 1);
                if o.slice_type == 0 {
                    s.ue(o.num_ref_idx_l1 - 1);
                    s.bit(0); // mvd_l1_zero_flag
                }
                s.ue(0); // five_minus_max_num_merge_cand
            }
            s.se(0); // slice_qp_delta
        }
        h265_nalu_with_layer(o.nalu_type, o.layer_id, &s.finish())
    }

    fn idr_slice() -> Vec<u8> {
        synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            ..Default::default()
        })
    }

    pub(super) fn trail_p(poc_lsb: u32, neg: &[(u32, bool)], num_ref_idx_l0: u32) -> Vec<u8> {
        synth_slice(&SliceOpts {
            poc_lsb,
            neg: neg.to_vec(),
            num_ref_idx_l0,
            ..Default::default()
        })
    }

    fn param_sets(sps: &SpsOpts) -> Vec<u8> {
        let mut au = synth_sps(sps);
        au.extend(synth_pps(false));
        au
    }

    pub(super) fn opening_idr_au(sps: &SpsOpts) -> Vec<u8> {
        let mut au = param_sets(sps);
        au.extend(idr_slice());
        au
    }

    #[test]
    fn an_idr_opens_planning_with_poc_zero_full_crop_and_inferred_colour() {
        let mut planner = H265Planner::new();
        let plan = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();

        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert!(plan.picture.is_idr && plan.picture.is_irap);
        assert!(plan.picture.no_rasl_output_flag);
        assert!(plan.picture.is_reference);
        assert_eq!(plan.picture.pic_order_cnt, 0);
        assert_eq!(plan.picture.nalu_type, NaluType::IdrWRadl);
        assert_eq!(
            (plan.picture.coded_width, plan.picture.coded_height),
            (64, 64)
        );
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 0,
                y: 0,
                width: 64,
                height: 64
            }
        );
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 2,
                transfer_characteristics: 2,
                matrix_coefficients: 2,
                video_full_range: false,
            },
            "E.3.1 inference: 'unspecified' code points + limited range, never a raw 0"
        );
        assert_eq!(plan.picture.general_profile_idc, 1);
        assert_eq!(plan.picture.level_idc, Level::L4);
        assert_eq!(plan.picture.chroma_format_idc, 1);
        assert_eq!(
            plan.picture.max_dpb_frames, 5,
            "the DPB is the stream's sps_max_dec_pic_buffering_minus1 + 1, NOT A-2's \
             level ceiling (which would say 16 for a 64x64 L4 stream)"
        );
        // Zero-reorder: the picture is display-ready in its own plan.
        assert_eq!(plan.dpb.outputs, vec![plan.dpb.stored.unwrap()]);
        assert!(plan.rps.st_curr_before.is_empty());
        assert!(plan.rps.lt_curr.is_empty());
        assert_eq!(plan.picture.short_term_ref_pic_set_size_bits, 0);
    }

    #[test]
    fn a_p_slice_references_the_idr_short_term_and_outputs_immediately() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let idr_id = p0.dpb.stored.unwrap();

        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);
        assert_eq!(p1.picture.pic_order_cnt, 1);
        assert_eq!(
            p1.slices[0].ref_list0,
            vec![RefPic {
                id: idr_id,
                pic_order_cnt: 0,
                is_long_term: false
            }]
        );
        assert_eq!(p1.rps.st_curr_before.len(), 1);
        assert_eq!(p1.rps.st_curr_before[0].id, idr_id);
        assert!(p1.rps.st_curr_after.is_empty());
        // Inline RPS: Vulkan NumBitsForSTRefPicSetInSlice is nonzero.
        assert!(p1.picture.short_term_ref_pic_set_size_bits > 0);
        assert!(p1.dpb.outputs.contains(&p1.dpb.stored.unwrap()));
    }

    #[test]
    fn long_term_rps_entries_carry_the_rfi_reference_shape() {
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        let p0 = planner.plan_au(&au0).unwrap();
        let idr_id = p0.dpb.stored.unwrap();

        let p1 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 1,
                neg: vec![(0, true)],
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            }))
            .unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);
        let p1_id = p1.dpb.stored.unwrap();

        // Recovery slice: previous picture short-term, IDR (poc 0) long-term.
        let p2 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 2,
                neg: vec![(0, true)],
                lt: vec![(0, true, None)],
                sps_long_term: true,
                num_ref_idx_l0: 2,
                ..Default::default()
            }))
            .unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);

        // 8.3.4: short-term current entries lead, long-term follow.
        assert_eq!(
            p2.slices[0].ref_list0,
            vec![
                RefPic {
                    id: p1_id,
                    pic_order_cnt: 1,
                    is_long_term: false
                },
                RefPic {
                    id: idr_id,
                    pic_order_cnt: 0,
                    is_long_term: true
                },
            ]
        );
        assert_eq!(p2.rps.lt_curr.len(), 1);
        assert_eq!(p2.rps.lt_curr[0].id, idr_id);
        assert!(p2.rps.lt_curr[0].is_long_term);

        // delta_poc_msb_present: same anchor resolves by full POC (8.3.2).
        let p3 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 3,
                neg: vec![(0, true)],
                lt: vec![(0, true, Some(0))],
                sps_long_term: true,
                num_ref_idx_l0: 2,
                ..Default::default()
            }))
            .unwrap();
        assert!(p3.warnings.is_empty(), "{:?}", p3.warnings);
        assert_eq!(p3.rps.lt_curr.len(), 1);
        assert_eq!(p3.rps.lt_curr[0].id, idr_id);
    }

    #[test]
    fn the_dpb_snapshot_holds_a_foll_long_term_anchor_the_current_rps_never_names() {
        // *Foll* sets: an anchor pinned long-term for later pictures, marked
        // in the DPB, in none of this picture's current sets. A DXVA RefPicList
        // built from current sets alone would drop it between pin and use.
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        let p0 = planner.plan_au(&au0).unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        assert!(
            p0.dpb_refs.is_empty(),
            "an opening IDR has no DPB behind it"
        );

        let p1 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 1,
                neg: vec![(0, true)],
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            }))
            .unwrap();
        let p1_id = p1.dpb.stored.unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);

        // used_by_curr_pic_lt_flag = 0: IDR in RefPicSetLtFoll, unused here.
        let p2 = planner
            .plan_au(&synth_slice(&SliceOpts {
                poc_lsb: 2,
                neg: vec![(0, true)],
                lt: vec![(0, false, None)],
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            }))
            .unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);
        assert!(
            p2.rps.lt_curr.is_empty(),
            "the anchor is Foll, not Curr, for this picture"
        );
        assert_eq!(
            p2.slices[0]
                .ref_list0
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![p1_id]
        );
        assert_eq!(
            p2.dpb_refs
                .iter()
                .map(|r| (r.id, r.is_long_term))
                .collect::<Vec<_>>(),
            vec![(idr_id, true), (p1_id, false)]
        );
        assert!(
            !p2.dpb.removed.contains(&idr_id),
            "a Foll entry must not be retired"
        );
    }

    #[test]
    fn the_dpb_snapshot_is_a_superset_of_the_current_rps_across_the_whole_vector() {
        let (_, plans) = plan_whole_clip(TEST_25FPS);
        assert_eq!(plans.len(), 250);
        for (i, plan) in plans.iter().enumerate() {
            let mut ids: Vec<PicId> = plan.dpb_refs.iter().map(|r| r.id).collect();
            let count = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), count, "AU {i}: the snapshot repeats a picture");
            // Every current-set entry is a marked DPB picture, same marking.
            for rp in plan
                .rps
                .st_curr_before
                .iter()
                .chain(&plan.rps.st_curr_after)
                .chain(&plan.rps.lt_curr)
            {
                let found = plan
                    .dpb_refs
                    .iter()
                    .find(|d| d.id == rp.id)
                    .unwrap_or_else(|| panic!("AU {i}: RPS entry {} is not marked", rp.id));
                assert_eq!(found.is_long_term, rp.is_long_term);
                assert_eq!(found.pic_order_cnt, rp.pic_order_cnt);
            }
            // Current picture is stored after the snapshot is taken.
            assert!(!ids.contains(&plan.dpb.stored.unwrap()));
        }
    }

    #[test]
    fn an_rps_that_drops_a_reference_retires_it_from_the_dpb() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();

        // p2 names only poc 1: the IDR leaves every set, already output, removed.
        let p2 = planner.plan_au(&trail_p(2, &[(0, true)], 1)).unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);
        assert_eq!(p2.slices[0].ref_list0[0].id, p1_id);
        assert!(p2.dpb.removed.contains(&idr_id));
        assert!(!p2.dpb.removed.contains(&p1_id));
    }

    #[test]
    fn a_missing_reference_is_substituted_in_place_not_compacted() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();

        // poc 2 lost. p3 still names 2, 1, 0; list0 head substitutes in place
        // so the two real entries keep their ref_idx positions.
        let p3 = planner
            .plan_au(&trail_p(3, &[(0, true), (0, true), (0, true)], 3))
            .unwrap();
        assert!(
            p3.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::MissingReference { .. })),
            "{:?}",
            p3.warnings
        );
        let ids: Vec<PicId> = p3.slices[0].ref_list0.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![p1_id, p1_id, idr_id],
            "substitution must preserve list length and positions"
        );
        // The RPS carries the stand-in at the lost position, so the decode binds it.
        let rps_ids: Vec<PicId> = p3.rps.st_curr_before.iter().map(|r| r.id).collect();
        assert_eq!(rps_ids, vec![p1_id, p1_id, idr_id]);
    }

    /// Every current reference lost: the list still binds a real picture (the DPB
    /// reference nearest the lost POC) instead of an empty list, and the AU is
    /// still flagged concealed.
    #[test]
    fn an_all_missing_current_set_stands_in_the_nearest_dpb_picture() {
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();
        // poc 2 lost; p3 names only poc 2.
        let p3 = planner.plan_au(&trail_p(3, &[(0, true)], 1)).unwrap();
        assert!(
            p3.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::MissingReference { .. })),
            "{:?}",
            p3.warnings
        );
        assert!(
            !p3.warnings.iter().any(|w| matches!(
                w,
                PlanWarning::MissingReference { context, .. } if context.contains("no usable")
            )),
            "{:?}",
            p3.warnings
        );
        let ids: Vec<PicId> = p3.slices[0].ref_list0.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![p1_id]);
        let rps_ids: Vec<PicId> = p3.rps.st_curr_before.iter().map(|r| r.id).collect();
        assert_eq!(rps_ids, vec![p1_id]);
    }

    #[test]
    fn a_rasl_behind_a_join_cra_is_skipped_and_the_trailing_picture_plans() {
        // CRA opening the stream: open-GOP join, NoRaslOutputFlag = 1.
        let mut au0 = param_sets(&SpsOpts::default());
        au0.extend(synth_slice(&SliceOpts {
            nalu_type: CRA_NUT,
            slice_type: 2,
            poc_lsb: 0,
            ..Default::default()
        }));
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&au0).unwrap();
        assert!(p0.picture.is_irap && !p0.picture.is_idr);
        assert!(p0.picture.no_rasl_output_flag);
        assert_eq!(p0.picture.pic_order_cnt, 0);
        let cra_id = p0.dpb.stored.unwrap();

        // RASL leading picture (poc -1) must be refused without wedging.
        let rasl = synth_slice(&SliceOpts {
            nalu_type: RASL_N,
            poc_lsb: 15,
            neg: vec![(0, true)],
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&rasl),
            Err(PlanError::RaslSkipped { poc: -1 })
        ));

        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(p1.warnings.is_empty(), "{:?}", p1.warnings);
        assert_eq!(p1.slices[0].ref_list0[0].id, cra_id);
    }

    #[test]
    fn a_rasl_behind_a_mid_stream_cra_plans_normally() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let _idr_id = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let p1_id = p1.dpb.stored.unwrap();

        // Continuous CRA: NoRaslOutputFlag = 0; unused RPS entries stay StFoll.
        let mut cra = synth_slice(&SliceOpts {
            nalu_type: CRA_NUT,
            slice_type: 2,
            poc_lsb: 4,
            neg: vec![(2, false)],
            ..Default::default()
        });
        let mut au = param_sets(&SpsOpts::default());
        au.append(&mut cra);
        let p2 = planner.plan_au(&au).unwrap();
        assert!(p2.warnings.is_empty(), "{:?}", p2.warnings);
        assert!(!p2.picture.no_rasl_output_flag);
        let cra_id = p2.dpb.stored.unwrap();

        let p3 = planner
            .plan_au(&synth_slice(&SliceOpts {
                nalu_type: RASL_N,
                poc_lsb: 2,
                neg: vec![(0, true)],
                pos: vec![(1, true)],
                num_ref_idx_l0: 2,
                ..Default::default()
            }))
            .unwrap();
        assert!(p3.warnings.is_empty(), "{:?}", p3.warnings);
        assert_eq!(p3.picture.pic_order_cnt, 2);
        assert_eq!(
            p3.slices[0]
                .ref_list0
                .iter()
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            vec![p1_id, cra_id],
            "list0: the past (StCurrBefore) then the future (StCurrAfter)"
        );
        assert!(
            !p3.picture.is_reference,
            "RASL_N is a sub-layer non-reference type"
        );
    }

    #[test]
    fn a_recovery_point_sei_lands_on_the_picture_plan_and_does_not_stick() {
        let mut au0 = param_sets(&SpsOpts::default());
        // Prefix SEI type 39: recovery_poc_cnt = 0, exact = 0, broken = 0.
        au0.extend(h265_nalu(39, &[0x06, 0x01, 0x90, 0x80]));
        au0.extend(idr_slice());

        let mut planner = H265Planner::new();
        let plan = planner.plan_au(&au0).unwrap();
        assert_eq!(
            plan.picture.recovery_point,
            Some(RecoveryPointHevc {
                recovery_poc_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );

        let plan = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert_eq!(plan.picture.recovery_point, None);
    }

    #[test]
    fn the_conformance_window_scales_by_the_chroma_format() {
        // 4:2:0: SubWidthC = SubHeightC = 2.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                conf_win: Some((2, 1, 1, 2)),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 4,
                y: 2,
                width: 58,
                height: 58
            }
        );

        // 4:4:4: offsets are luma samples.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                profile_idc: 4,
                chroma_format_idc: 3,
                conf_win: Some((2, 1, 1, 2)),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(plan.picture.chroma_format_idc, 3);
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 2,
                y: 1,
                width: 61,
                height: 61
            }
        );

        // 4:2:2: SubWidthC = 2, SubHeightC = 1.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                profile_idc: 4,
                chroma_format_idc: 2,
                conf_win: Some((1, 1, 1, 1)),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.display_crop,
            DisplayCrop {
                x: 2,
                y: 1,
                width: 60,
                height: 62
            }
        );
    }

    #[test]
    fn main10_depths_ride_the_plan() {
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                profile_idc: 2,
                bit_depth_minus8: 2,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(plan.picture.general_profile_idc, 2);
        assert_eq!(plan.picture.bit_depth_luma_minus8, 2);
        assert_eq!(plan.picture.bit_depth_chroma_minus8, 2);
        assert_eq!(plan.picture.chroma_format_idc, 1);
    }

    #[test]
    fn explicit_vui_colour_rides_the_plan_and_the_range_flag_stands_alone() {
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                vui: VuiOpt::SignalType {
                    full_range: false,
                    colour: Some((9, 16, 9)),
                },
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            plan.picture.colour,
            ColourDescription {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 9,
                video_full_range: false,
            }
        );

        // video_signal_type present, full-range, no colour description: E.3.1
        // "unspecified" code points, range flag rides.
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts {
                vui: VuiOpt::SignalType {
                    full_range: true,
                    colour: None,
                },
                ..Default::default()
            }))
            .unwrap();
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
    fn a_field_coded_stream_is_rejected_as_outside_the_envelope() {
        let err = H265Planner::new()
            .plan_au(&synth_sps(&SpsOpts {
                vui: VuiOpt::FieldSeq,
                ..Default::default()
            }))
            .unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("field")),
            "{err:?}"
        );
    }

    #[test]
    fn a_dpb_deeper_than_16_frames_is_rejected_as_outside_the_envelope() {
        // Parser reads sps_max_dec_pic_buffering_minus1 up to 16 (17 frames).
        let err = H265Planner::new()
            .plan_au(&synth_sps(&SpsOpts {
                max_dec_pic_buffering_minus1: 16,
                ..Default::default()
            }))
            .unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("DPB")),
            "{err:?}"
        );
    }

    #[test]
    fn a_multilayer_nalu_is_rejected_as_outside_the_envelope() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            layer_id: 1,
            ..Default::default()
        }));
        let err = H265Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("nuh_layer_id")),
            "{err:?}"
        );
    }

    #[test]
    fn flush_resets_decoding_state_and_refuses_non_irap_until_one_arrives() {
        let mut planner = H265Planner::new();
        let id0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap()
            .dpb
            .stored
            .unwrap();
        let id1 = planner
            .plan_au(&trail_p(1, &[(0, true)], 1))
            .unwrap()
            .dpb
            .stored
            .unwrap();

        let flushed = planner.flush();
        assert!(flushed.outputs.is_empty(), "both pictures already output");
        assert_eq!(flushed.removed, vec![id0, id1]);

        assert!(matches!(
            planner.plan_au(&trail_p(2, &[(0, true)], 1)),
            Err(PlanError::AwaitingIdr)
        ));

        // Flush gave the CRA NoRaslOutputFlag = 1. Parameter sets survived (7.4.2.4).
        let plan = planner
            .plan_au(&synth_slice(&SliceOpts {
                nalu_type: CRA_NUT,
                slice_type: 2,
                poc_lsb: 0,
                ..Default::default()
            }))
            .unwrap();
        assert!(plan.picture.is_irap);
        assert!(plan.picture.no_rasl_output_flag);
        assert_eq!(plan.picture.pic_order_cnt, 0);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);

        let plan = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.slices[0].ref_list0.len(), 1);
    }

    #[test]
    fn a_foreign_slice_in_the_au_is_dropped_with_a_truncated_au_warning() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(idr_slice());
        // TRAIL continuation after an IDR: 7.4.2.4.4 requires one NALU type.
        au.extend(synth_slice(&SliceOpts {
            segment: Some((0, 0, false)),
            poc_lsb: 1,
            neg: vec![(0, true)],
            ..Default::default()
        }));

        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(plan.picture.is_idr);
        assert_eq!(plan.slices.len(), 1, "the foreign slice is not planned");
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    #[test]
    fn a_truncated_continuation_slice_degrades_to_a_warning() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(idr_slice());
        // Second IDR segment cut mid-header: parse fails, prior slice survives.
        let mut cont = synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            segment: Some((0, 0, false)),
            ..Default::default()
        });
        cont.truncate(cont.len() - 1);
        let cut_at = au.len();
        au.extend(&cont[..7.min(cont.len())]);

        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert_eq!(plan.slices.len(), 1);
        assert!(
            plan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::TruncatedAu { offset } if *offset == cut_at)),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn two_pictures_in_one_au_is_outside_the_envelope() {
        let mut au = param_sets(&SpsOpts::default());
        au.extend(idr_slice());
        au.extend(idr_slice());
        let err = H265Planner::new().plan_au(&au).unwrap_err();
        assert!(
            matches!(err, PlanError::OutsideEnvelope(what) if what.contains("one access unit")),
            "{err:?}"
        );
    }

    #[test]
    fn dependent_slice_segments_plan_with_the_completed_header() {
        // 128×64, 64-sample CTB: two CTBs, so segment addresses exist (1 bit).
        let sps = SpsOpts {
            width: 128,
            ..Default::default()
        };
        let mut au = synth_sps(&sps);
        au.extend(synth_pps(true));
        au.extend(idr_slice());
        au.extend(synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            segment: Some((1, 1, true)),
            pps_dependent_enabled: true,
            ..Default::default()
        }));

        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.slices.len(), 2);
        let dependent = &plan.slices[1].header;
        assert!(dependent.dependent_slice_segment_flag);
        assert_eq!(dependent.segment_address, 1);
        assert!(
            dependent.type_.is_i(),
            "the dependent header inherited the independent slice's type"
        );
    }

    #[test]
    fn reordering_streams_plan_with_the_envelope_fact_flagged() {
        let sps = SpsOpts {
            max_num_reorder_pics: 2,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&opening_idr_au(&sps)).unwrap();
        assert!(
            p0.warnings.contains(&PlanWarning::NonZeroReorder {
                max_num_reorder_pics: 2
            }),
            "{:?}",
            p0.warnings
        );
        assert!(p0.dpb.outputs.is_empty());
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(p1.dpb.outputs.is_empty());

        let flushed = planner.flush();
        assert_eq!(
            flushed.outputs,
            vec![p0.dpb.stored.unwrap(), p1.dpb.stored.unwrap()]
        );
    }

    #[test]
    fn poc_msb_wraps_across_the_lsb_boundary() {
        // 4-bit POC lsb (MaxPicOrderCntLsb 16): 0 → 7 → 14 → 2 wraps to POC 18.
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let p1 = planner.plan_au(&trail_p(7, &[(6, true)], 1)).unwrap();
        assert_eq!(p1.picture.pic_order_cnt, 7);
        let p2 = planner.plan_au(&trail_p(14, &[(6, true)], 1)).unwrap();
        assert_eq!(p2.picture.pic_order_cnt, 14);
        let p3 = planner.plan_au(&trail_p(2, &[(3, true)], 1)).unwrap();
        assert!(p3.warnings.is_empty(), "{:?}", p3.warnings);
        assert_eq!(p3.picture.pic_order_cnt, 18);
        assert_eq!(p3.slices[0].ref_list0[0].pic_order_cnt, 14);
    }

    #[test]
    fn the_plans_parameter_set_accessors_carry_the_activated_content() {
        let plan = H265Planner::new()
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        assert_eq!(plan.sps.seq_parameter_set_id, 0);
        assert_eq!(plan.sps.width(), 64);
        assert_eq!(plan.sps.height(), 64);
        assert_eq!(plan.pps.pic_parameter_set_id, 0);
        assert_eq!(plan.pps.seq_parameter_set_id, 0);
        assert!(
            Rc::ptr_eq(&plan.sps, &plan.pps.sps),
            "the SPS accessor is the PPS's own SPS, not a second copy"
        );
    }

    #[test]
    fn outputs_queued_during_a_failed_au_surface_in_the_next_successful_plan() {
        let sps = SpsOpts {
            max_num_reorder_pics: 1,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&opening_idr_au(&sps)).unwrap();
        let id0 = p0.dpb.stored.unwrap();
        assert!(p0.dpb.outputs.is_empty(), "held back by the reorder depth");
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let id1 = p1.dpb.stored.unwrap();
        assert_eq!(p1.dpb.outputs, vec![id0]);

        // Errors after begin_picture drained the DPB (p1 queued): two first-segments.
        let mut bad_au = idr_slice();
        bad_au.extend(idr_slice());
        assert!(matches!(
            planner.plan_au(&bad_au),
            Err(PlanError::OutsideEnvelope(_))
        ));

        let plan = planner.plan_au(&idr_slice()).unwrap();
        assert!(plan.dpb.outputs.contains(&id1));
        assert!(plan.dpb.removed.contains(&id1));
    }

    #[test]
    fn a_dropped_reference_au_degrades_to_warnings_and_planning_continues() {
        let aus = split_h265_aus(TEST_25FPS);

        // Droppable AU: non-IRAP reference not followed by an IRAP (that would
        // reset state and hide the loss).
        let mut planner = H265Planner::new();
        let mut plans = Vec::new();
        for au in &aus {
            plans.push(planner.plan_au(au).unwrap());
        }
        let dropped = plans
            .iter()
            .enumerate()
            .position(|(i, p)| {
                p.picture.is_reference
                    && !p.picture.is_irap
                    && plans.get(i + 1).is_some_and(|next| !next.picture.is_irap)
            })
            .expect("the vector contains a droppable reference picture");

        // Same stream minus that AU must warn, not error. Every list entry must
        // resolve to a stored PicId (substitution never leaks a hole).
        let mut planner = H265Planner::new();
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
            for entry in plan
                .rps
                .st_curr_before
                .iter()
                .chain(&plan.rps.st_curr_after)
                .chain(&plan.rps.lt_curr)
            {
                assert!(stored_so_far.contains(&entry.id));
            }
        }

        assert_eq!(planned, aus.len() - 1);
        assert!(
            missing_seen,
            "an AU after the drop must report the missing reference"
        );
    }

    #[test]
    fn a_first_slice_naming_an_unseen_pps_is_a_no_active_param_set_error() {
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        // First slice names PPS 1, never sent: no picture to conceal around.
        let mut s = BitSink::new();
        s.bit(1); // first_slice_segment_in_pic_flag
        s.ue(1); // slice_pic_parameter_set_id — never seen
        let au = h265_nalu(TRAIL_R, &s.finish());
        assert!(matches!(
            planner.plan_au(&au),
            Err(PlanError::NoActiveParamSet { pps_id: 1 })
        ));
    }

    /// Parser indexed 16-deep long-term arrays with a count it read up to 32.
    /// A hostile header must be a parse error, not a panic.
    #[test]
    fn a_hostile_long_term_count_is_a_parse_error_not_a_panic() {
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        planner.plan_au(&au0).unwrap();

        let hostile = synth_slice(&SliceOpts {
            poc_lsb: 1,
            neg: vec![(0, true)],
            lt: vec![(0, true, None); 17], // num_long_term_pics = 17 > the 16 slots
            sps_long_term: true,
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&hostile),
            Err(PlanError::Parse(_))
        ));
        let plan = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    fn eos_nalu() -> Vec<u8> {
        h265_nalu(36, &[])
    }

    /// C.5.2.2 exempts only bitstream picture 0. An IRAP after in-band EOS must
    /// drain (or discard under no_output_of_prior_pics_flag) the previous sequence.
    #[test]
    fn an_eos_then_idr_drains_the_previous_sequence_before_the_new_one() {
        let sps = SpsOpts {
            max_num_reorder_pics: 2,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let p0 = planner.plan_au(&opening_idr_au(&sps)).unwrap();
        let id0 = p0.dpb.stored.unwrap();
        let p1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let id1 = p1.dpb.stored.unwrap();
        assert!(p1.dpb.outputs.is_empty(), "held back by the reorder depth");
        let p2 = planner
            .plan_au(&trail_p(2, &[(0, true), (0, true)], 1))
            .unwrap();
        let id2 = p2.dpb.stored.unwrap();
        assert_eq!(p2.dpb.outputs, vec![id0], "depth 2 releases poc 0 here");

        // EOS + IDR in one AU: pocs 1 and 2 come out here, never interleaved.
        let mut au = eos_nalu();
        au.extend(idr_slice());
        let p3 = planner.plan_au(&au).unwrap();
        assert!(p3.picture.no_rasl_output_flag, "EOS gave the IDR the flag");
        assert_eq!(p3.dpb.outputs, vec![id1, id2]);
        for id in [id0, id1, id2] {
            assert!(p3.dpb.removed.contains(&id));
        }

        // no_output_of_prior_pics_flag = 1: leftovers discarded without output (C.3.2).
        let mut planner = H265Planner::new();
        planner.plan_au(&opening_idr_au(&sps)).unwrap();
        let q1 = planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();
        let q1_id = q1.dpb.stored.unwrap();
        let mut au = eos_nalu();
        au.extend(synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            no_output_of_prior_pics: true,
            ..Default::default()
        }));
        let q2 = planner.plan_au(&au).unwrap();
        assert!(
            !q2.dpb.outputs.contains(&q1_id),
            "no_output_of_prior_pics discards without output"
        );
        assert!(q2.dpb.removed.contains(&q1_id));
    }

    /// parse_sps stores a rejected SPS; NegotiationInfo omits envelope facts. A
    /// later PPS-only rebind must re-run the envelope gate at activation.
    #[test]
    fn a_rejected_sps_cannot_be_activated_through_a_pps_only_rebind() {
        // Hostile window, same geometry. Without the activation gate this
        // reaches visible_rectangle()'s unchecked u32 subtraction.
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let hostile = synth_sps(&SpsOpts {
            conf_win: Some((100, 0, 0, 0)), // 200 luma samples of a 64-wide picture
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&hostile),
            Err(PlanError::Parse(_))
        ));
        let mut rebind = synth_pps(false);
        rebind.extend(idr_slice());
        match planner.plan_au(&rebind) {
            Err(PlanError::Parse(msg)) => assert!(msg.contains("conformance window"), "{msg}"),
            other => panic!("the rebind must not activate the rejected SPS: {other:?}"),
        }

        // 17-frame DPB, same geometry. Renegotiation runs only after
        // `check_envelope`, so a NegotiationInfo difference cannot stand in.
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        let hostile = synth_sps(&SpsOpts {
            max_dec_pic_buffering_minus1: 16,
            ..Default::default()
        });
        assert!(matches!(
            planner.plan_au(&hostile),
            Err(PlanError::OutsideEnvelope(_))
        ));
        let mut rebind = synth_pps(false);
        rebind.extend(idr_slice());
        assert!(matches!(
            planner.plan_au(&rebind),
            Err(PlanError::OutsideEnvelope(what)) if what.contains("DPB")
        ));
    }

    /// Reported DPB is the stream's declared depth, not A-2's level ceiling.
    /// A-2 branches on picture size vs `MaxLumaPs`, so a six-picture L5.1 stream
    /// reported 16 at 1080p and 6 at 4K; backends then asked for 17 slots.
    /// Host side: `pf-encode::rfi_dpb_fits_a_mainstream_vulkan_decoder`.
    #[test]
    fn the_reported_dpb_is_the_streams_own_depth_and_fits_a_16_slot_decoder() {
        /// Lowest `VkVideoCapabilitiesKHR::maxDpbSlots` among target devices.
        const VULKAN_MAX_DPB_SLOTS: usize = 16;

        // L5.1, coded 1920×1088, six pictures declared.
        let field = SpsOpts {
            width: 1920,
            height: 1088,
            level_idc: 153,
            max_dec_pic_buffering_minus1: 5,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let at_1080p = planner.plan_au(&opening_idr_au(&field)).unwrap();
        assert_eq!(
            at_1080p.picture.max_dpb_frames, 6,
            "sps_max_dec_pic_buffering_minus1 = 5 means six pictures — five references \
             plus the current one. A-2 would have said 16 here, because 1920x1088 = \
             2088960 luma samples fall under MaxLumaPs(L5.1) >> 2 = 2228224."
        );
        // One slot per DPB picture plus one in flight (`crate::slots`).
        let slots_for = |plan: &AuPlan| plan.picture.max_dpb_frames + 1;
        assert!(
            slots_for(&at_1080p) <= VULKAN_MAX_DPB_SLOTS,
            "{} DPB frames need {} slots, over the {VULKAN_MAX_DPB_SLOTS} a mainstream \
             Vulkan Video decoder offers — this is the refusal that cost HEVC",
            at_1080p.picture.max_dpb_frames,
            slots_for(&at_1080p)
        );

        // Same stream, other resolutions. A-2 would differ; the stream must not.
        for (w, h) in [(1280, 720), (2560, 1440), (3840, 2176)] {
            let mut planner = H265Planner::new();
            let plan = planner
                .plan_au(&opening_idr_au(&SpsOpts {
                    width: w,
                    height: h,
                    ..field.clone()
                }))
                .unwrap();
            assert_eq!(
                plan.picture.max_dpb_frames, at_1080p.picture.max_dpb_frames,
                "{w}x{h}: the DPB requirement is a property of the stream, not of which \
                 A-2 branch the picture size lands in"
            );
        }

        // Declared depth, plus one in-flight slot, fits a 16-slot device.
        for minus1 in 0..=14u32 {
            let mut planner = H265Planner::new();
            let plan = planner
                .plan_au(&opening_idr_au(&SpsOpts {
                    max_dec_pic_buffering_minus1: minus1,
                    ..field.clone()
                }))
                .unwrap();
            assert_eq!(plan.picture.max_dpb_frames, minus1 as usize + 1);
            assert!(slots_for(&plan) <= VULKAN_MAX_DPB_SLOTS);
        }

        // A.4 allows a 16-picture DPB; 16 + in-flight is 17 slots. Refuse it:
        // decoding with fewer slots would silently corrupt references.
        let mut planner = H265Planner::new();
        let deepest = planner
            .plan_au(&opening_idr_au(&SpsOpts {
                max_dec_pic_buffering_minus1: 15,
                ..field.clone()
            }))
            .unwrap();
        assert_eq!(deepest.picture.max_dpb_frames, 16);
        assert_eq!(slots_for(&deepest), VULKAN_MAX_DPB_SLOTS + 1);
        let mut planner = H265Planner::new();
        assert!(
            matches!(
                planner.plan_au(&opening_idr_au(&SpsOpts {
                    max_dec_pic_buffering_minus1: 16,
                    ..field.clone()
                })),
                Err(PlanError::OutsideEnvelope(what)) if what.contains("DPB")
            ),
            "a 17-picture DPB is past A.4's cap and must be refused at activation, not \
             clamped silently into a slot pool that cannot hold it"
        );
    }

    /// RASL refusal runs before renegotiation, so a RASL AU with a new SPS
    /// must not drain the DPB on its way out.
    #[test]
    fn a_skipped_rasl_carrying_a_renegotiating_sps_leaves_the_dpb_intact() {
        let mut au0 = param_sets(&SpsOpts::default());
        au0.extend(synth_slice(&SliceOpts {
            nalu_type: CRA_NUT,
            slice_type: 2,
            ..Default::default()
        }));
        let mut planner = H265Planner::new();
        let cra_id = planner.plan_au(&au0).unwrap().dpb.stored.unwrap();

        // RASL AU re-sends 128×64 parameter sets (renegotiation) but is refused.
        let mut rasl_au = param_sets(&SpsOpts {
            width: 128,
            ..Default::default()
        });
        rasl_au.extend(synth_slice(&SliceOpts {
            nalu_type: RASL_N,
            poc_lsb: 15,
            neg: vec![(0, true)],
            ..Default::default()
        }));
        assert!(matches!(
            planner.plan_au(&rasl_au),
            Err(PlanError::RaslSkipped { .. })
        ));

        let mut au = param_sets(&SpsOpts::default());
        au.extend(trail_p(1, &[(0, true)], 1));
        let plan = planner.plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.slices[0].ref_list0[0].id, cra_id);
    }

    /// An AU that opens with a continuation is a mis-split tail. Beginning a
    /// picture from it would fabricate a duplicate; a dependent segment would
    /// inherit a previous AU's independent header.
    #[test]
    fn a_leading_continuation_segment_is_skipped_not_fabricated_into_a_picture() {
        let sps = SpsOpts {
            width: 128, // two CTBs, so continuation addresses exist
            ..Default::default()
        };
        let mut au0 = synth_sps(&sps);
        au0.extend(synth_pps(true));
        au0.extend(idr_slice());
        let mut planner = H265Planner::new();
        planner.plan_au(&au0).unwrap();

        let mut au = synth_slice(&SliceOpts {
            nalu_type: IDR_W_RADL,
            slice_type: 2,
            segment: Some((1, 1, true)),
            pps_dependent_enabled: true,
            ..Default::default()
        });
        au.extend(trail_p(1, &[(0, true)], 1));
        let plan = planner.plan_au(&au).unwrap();
        assert_eq!(plan.slices.len(), 1, "only the real picture is planned");
        assert_eq!(plan.picture.pic_order_cnt, 1);
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));

        let mut au = synth_slice(&SliceOpts {
            segment: Some((1, 1, false)),
            pps_dependent_enabled: true,
            poc_lsb: 1,
            neg: vec![(0, true)],
            ..Default::default()
        });
        au.extend(trail_p(2, &[(0, true)], 1));
        let plan = planner.plan_au(&au).unwrap();
        assert_eq!(plan.slices.len(), 1);
        assert_eq!(plan.picture.pic_order_cnt, 2);
        assert!(plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::TruncatedAu { .. })));
    }

    /// Cursor-based truncation never sees a start code behind it. Cut-off data
    /// at the AU tail must warn; B.2.2 zero padding must not.
    #[test]
    fn cut_off_data_at_the_au_tail_warns_while_zero_padding_does_not() {
        let mut au = opening_idr_au(&SpsOpts::default());
        let cut_at = au.len();
        au.extend([0x00, 0x00, 0x01, 0x02]); // a start code + half a NAL header
        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::TruncatedAu { offset } if *offset == cut_at)),
            "{:?}",
            plan.warnings
        );

        let mut au = opening_idr_au(&SpsOpts::default());
        au.extend([0x00, 0x00, 0x00, 0x00]);
        let plan = H265Planner::new().plan_au(&au).unwrap();
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    /// Mid-stream SPS that raises reorder depth on unchanged geometry must still
    /// renegotiate and flag NonZeroReorder.
    #[test]
    fn a_mid_stream_reorder_increase_flags_the_envelope_fact() {
        let mut planner = H265Planner::new();
        let p0 = planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        assert!(p0.warnings.is_empty(), "{:?}", p0.warnings);
        planner.plan_au(&trail_p(1, &[(0, true)], 1)).unwrap();

        let mut au = param_sets(&SpsOpts {
            max_num_reorder_pics: 3,
            ..Default::default()
        });
        au.extend(idr_slice());
        let plan = planner.plan_au(&au).unwrap();
        assert!(
            plan.warnings.contains(&PlanWarning::NonZeroReorder {
                max_num_reorder_pics: 3
            }),
            "{:?}",
            plan.warnings
        );
    }

    /// Two references sharing a poc_lsb make the MSB-less long-term lookup
    /// ambiguous (7.4.7.1). The RFI path must warn rather than pick silently.
    #[test]
    fn an_ambiguous_long_term_poc_lsb_warns_instead_of_silently_picking() {
        let sps = SpsOpts {
            long_term: true,
            ..Default::default()
        };
        let mut planner = H265Planner::new();
        let mut au0 = param_sets(&sps);
        au0.extend(idr_slice());
        planner.plan_au(&au0).unwrap(); // poc 0

        let lt_slice = |poc_lsb: u32, neg: Vec<(u32, bool)>, lt| {
            synth_slice(&SliceOpts {
                poc_lsb,
                neg,
                lt,
                sps_long_term: true,
                ..Default::default()
            })
        };
        // 0, 7, 14, 16: msb wrap makes POC 0 and 16 share poc_lsb 0.
        planner
            .plan_au(&lt_slice(7, vec![(6, true)], vec![]))
            .unwrap();
        planner
            .plan_au(&lt_slice(14, vec![(6, true), (6, true)], vec![]))
            .unwrap();
        let p3 = planner
            .plan_au(&lt_slice(0, vec![(1, true), (13, true)], vec![]))
            .unwrap();
        assert_eq!(p3.picture.pic_order_cnt, 16, "msb wrap");

        // MSB-less long-term entry naming lsb 0 is ambiguous (POC 0 and 16).
        let p4 = planner
            .plan_au(&lt_slice(1, vec![(0, true)], vec![(0, true, None)]))
            .unwrap();
        assert!(
            p4.warnings.iter().any(|w| matches!(
                w,
                PlanWarning::MissingReference { context, .. } if context.contains("ambiguous")
            )),
            "{:?}",
            p4.warnings
        );
        assert_eq!(p4.rps.lt_curr.len(), 1, "still resolved (concealment)");
    }

    /// An IRAP AU that fails before its picture begins must not unlatch
    /// AwaitingIdr.
    #[test]
    fn a_failed_resume_irap_keeps_the_awaiting_gate_latched() {
        let mut planner = H265Planner::new();
        planner
            .plan_au(&opening_idr_au(&SpsOpts::default()))
            .unwrap();
        planner.flush();

        // CRA names an unseen PPS: resume fails before begin_picture.
        let mut s = BitSink::new();
        s.bit(1); // first_slice_segment_in_pic_flag
        s.bit(0); // no_output_of_prior_pics_flag
        s.ue(1); // slice_pic_parameter_set_id — never seen
        let bad_cra = h265_nalu(CRA_NUT, &s.finish());
        assert!(matches!(
            planner.plan_au(&bad_cra),
            Err(PlanError::NoActiveParamSet { pps_id: 1 })
        ));

        assert!(matches!(
            planner.plan_au(&trail_p(1, &[(0, true)], 1)),
            Err(PlanError::AwaitingIdr)
        ));

        let plan = planner.plan_au(&idr_slice()).unwrap();
        assert!(plan.picture.is_idr);
        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
    }

    const VPS_NUT: u8 = 32;
    const SPS_NUT: u8 = 33;
    const PPS_NUT: u8 = 34;

    /// Trailing bits so the parser reads past the guarded field. A truncated
    /// NALU would be an error for the wrong reason.
    fn padded(mut s: BitSink) -> Vec<u8> {
        for _ in 0..64 {
            s.bits(8, 0xff);
        }
        s.finish()
    }

    /// `{vps,sps}_max_sub_layers_minus1` is u(3) so 7 is representable; 7.4.3
    /// stops at 6 and the arrays are six/seven deep. Also keeps
    /// `max_num_reorder_pics[max_sub_layers_minus1]` in bounds.
    #[test]
    fn a_param_set_claiming_eight_sub_layers_is_a_parse_error_not_a_panic() {
        let mut s = BitSink::new();
        s.bits(4, 0); // vps_video_parameter_set_id
        s.bit(1); // vps_base_layer_internal_flag
        s.bit(1); // vps_base_layer_available_flag
        s.bits(6, 0); // vps_max_layers_minus1
        s.bits(3, 7); // vps_max_sub_layers_minus1 — one past the spec's 6
        s.bit(1); // vps_temporal_id_nesting_flag
        s.bits(16, 0xffff); // vps_reserved_0xffff_16bits
        ptl_general(&mut s, 1, 120);
        let vps = h265_nalu(VPS_NUT, &padded(s));
        assert!(matches!(
            H265Planner::new().plan_au(&vps),
            Err(PlanError::Parse(_))
        ));

        let mut s = BitSink::new();
        s.bits(4, 0); // sps_video_parameter_set_id
        s.bits(3, 7); // sps_max_sub_layers_minus1
        s.bit(1); // sps_temporal_id_nesting_flag
        ptl_general(&mut s, 1, 120);
        let sps = h265_nalu(SPS_NUT, &padded(s));
        assert!(matches!(
            H265Planner::new().plan_au(&sps),
            Err(PlanError::Parse(_))
        ));
    }

    /// PPS fields ahead of the tile / scaling-list block, all zero.
    fn pps_prefix() -> BitSink {
        let mut s = BitSink::new();
        s.ue(0); // pps_pic_parameter_set_id
        s.ue(0); // pps_seq_parameter_set_id
        s.bits(2, 0); // dependent_slice_segments_enabled / output_flag_present
        s.bits(3, 0); // num_extra_slice_header_bits
        s.bits(2, 0); // sign_data_hiding_enabled / cabac_init_present
        s.ue(0); // num_ref_idx_l0_default_active_minus1
        s.ue(0); // num_ref_idx_l1_default_active_minus1
        s.se(0); // init_qp_minus26
        s.bits(3, 0); // constrained_intra_pred / transform_skip / cu_qp_delta_enabled
        s.se(0); // pps_cb_qp_offset
        s.se(0); // pps_cr_qp_offset
        s.bits(4, 0); // chroma_qp_offsets / weighted_pred / weighted_bipred / bypass
        s
    }

    /// PPS fields after the tile block, all zero, plus rbsp_trailing_bits().
    fn pps_tail(mut s: BitSink) -> Vec<u8> {
        s.bits(2, 0); // loop_filter_across_slices / deblocking_filter_control_present
        s.bits(2, 0); // pps_scaling_list_data_present / lists_modification_present
        s.ue(0); // log2_parallel_merge_level_minus2
        s.bits(2, 0); // slice_segment_header_extension / pps_extension_present
        s.finish()
    }

    /// Equation 7-42 subtracts `scaling_list_pred_matrix_id_delta` from matrixId
    /// in u32. Unbounded, it underflows into an OOB read; 7.4.5 caps the delta.
    #[test]
    fn a_scaling_list_predicting_from_a_negative_matrix_is_a_parse_error_not_a_panic() {
        let mut s = pps_prefix();
        s.bits(2, 0); // tiles_enabled / entropy_coding_sync_enabled
        s.bits(2, 0); // loop_filter_across_slices / deblocking_filter_control_present
        s.bit(1); // pps_scaling_list_data_present_flag
        s.bit(0); // scaling_list_pred_mode_flag[0][0]
        s.ue(1); // scaling_list_pred_matrix_id_delta[0][0] — refMatrixId = 0 - 1
        let mut au = synth_sps(&SpsOpts::default());
        au.extend(h265_nalu(PPS_NUT, &padded(s)));
        assert!(matches!(
            H265Planner::new().plan_au(&au),
            Err(PlanError::Parse(_))
        ));
    }

    /// Tile counts bounded by CTB size only ran past the width/height arrays on
    /// a wide SPS. Table A.8 caps them at 20 columns and 22 rows for every level.
    #[test]
    fn a_pps_with_more_tiles_than_any_level_allows_is_a_parse_error_not_a_panic() {
        // 2048×2048 luma, 64×64 CTBs: 32 CTBs each way. Picture bound admits 31.
        let sps = SpsOpts {
            width: 2048,
            height: 2048,
            ..Default::default()
        };
        for (columns, rows) in [(25, 0), (0, 25)] {
            let mut s = pps_prefix();
            s.bit(1); // tiles_enabled_flag
            s.bit(0); // entropy_coding_sync_enabled_flag
            s.ue(columns); // num_tile_columns_minus1
            s.ue(rows); // num_tile_rows_minus1
            s.bit(1); // uniform_spacing_flag
            let mut au = synth_sps(&sps);
            au.extend(h265_nalu(PPS_NUT, &padded(s)));
            assert!(
                matches!(H265Planner::new().plan_au(&au), Err(PlanError::Parse(_))),
                "{columns} columns / {rows} rows must be refused"
            );
        }
    }

    /// Entry-point maximum multiplied tile counts in u8 (20 × 22 overflows);
    /// `entry_point_offset_minus1` is 32 deep however large that maximum is.
    #[test]
    fn a_slice_claiming_more_entry_points_than_the_header_holds_is_a_parse_error_not_a_panic() {
        let sps = SpsOpts {
            width: 2048,
            height: 2048,
            ..Default::default()
        };
        let mut s = pps_prefix();
        s.bit(1); // tiles_enabled_flag
        s.bit(0); // entropy_coding_sync_enabled_flag
        s.ue(19); // num_tile_columns_minus1 — Table A.8's ceiling, and legal here
        s.ue(21); // num_tile_rows_minus1
        s.bit(1); // uniform_spacing_flag
        s.bit(0); // loop_filter_across_tiles_enabled_flag
        let mut au = synth_sps(&sps);
        au.extend(h265_nalu(PPS_NUT, &pps_tail(s)));

        let mut s = BitSink::new();
        s.bit(1); // first_slice_segment_in_pic_flag
        s.bit(0); // no_output_of_prior_pics_flag
        s.ue(0); // slice_pic_parameter_set_id
        s.ue(2); // slice_type: I
        s.se(0); // slice_qp_delta
        s.ue(35); // num_entry_point_offsets — 440 tiles would allow it, 32 slots do not
        au.extend(h265_nalu(IDR_W_RADL, &padded(s)));

        assert!(matches!(
            H265Planner::new().plan_au(&au),
            Err(PlanError::Parse(_))
        ));
    }
}
