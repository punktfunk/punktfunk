//! Native D3D11VA backend: pf-bitstream plans drive `ID3D11VideoDecoder` without FFmpeg.
//! H.264, H.265, and AV1 Profile 0 in NV12 or P010. This module enumerates, allocates,
//! and submits; [`pf_dxvadec`] owns the tested DXVA layouts, packing, conversion, profile
//! choice, alignment, and sizing. [`HandoffRing`] converts decoded surfaces to shareable
//! RGBA for the presenter, so this path is not zero-copy; see [`crate::video_d3d11`].
//!
//! [`NativeD3d11Decoder::new`] rejects unsupported codecs, shapes, profiles, or configs
//! so a caller can fall through before an AU is consumed. In-band shape
//! changes rebuild the whole [`Session`]. The pool is one decoder-only `ID3D11Texture2D`
//! array; [`pf_dxvadec::align_surface`] and [`pf_dxvadec::pool_size`] size it. Slots a
//! submission names stay live until [`NativeD3d11Decoder::release_deferred`] after the
//! decode op, so the target cannot alias a reference.
//!
//! Exclusive `&mut self` (`Send`, not `Sync`). The session field drops before the handoff
//! ring. Busy `DecoderBeginFrame` retries on a bounded budget. Concealed pictures are not
//! submitted and raise [`NativeD3d11Decoder::take_recovery_request`]; skipped H.265 RASL
//! pictures are `Ok(None)`; refusals stay demotion-eligible errors.

use anyhow::{anyhow, bail, Context as _, Result};
use pf_dxvadec::{Codec, DxvaProfile};
use std::time::{Duration, Instant};
use windows::core::{Interface, GUID};
use windows::Win32::d3d11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDecoder,
    ID3D11VideoDecoderOutputView, ID3D11VideoDevice, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VDOV_DIMENSION_TEXTURE2D, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
    D3D11_VIDEO_DECODER_BUFFER_DESC, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
    D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
    D3D11_VIDEO_DECODER_BUFFER_TYPE, D3D11_VIDEO_DECODER_CONFIG, D3D11_VIDEO_DECODER_DESC,
    D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC,
};
use windows::Win32::dxgi::{
    IDXGIResource1, DXGI_FORMAT, DXGI_SAMPLE_DESC, DXGI_SHARED_RESOURCE_READ,
    DXGI_SHARED_RESOURCE_WRITE,
};

use crate::video_color::ColorDesc;
use crate::video_d3d11::{create_device, D3d11Frame, HandoffRing, HandoffSource};
use crate::video_types::{DecodeHealth, StreamFormat};

/// Decode-pool bind flag. The pool takes this flag alone.
const BIND_DECODER: u32 = 0x200;

/// `DecoderBeginFrame` returns `E_PENDING` while the driver's decode queue is full (Intel).
/// libavcodec's `ff_dxva2_common_end_frame` waits 50 × 2 ms; this waits the same 100 ms,
/// spinning for the first 500 µs — a queue that drains within a decode costs no sleep —
/// then in 1 ms sleeps. A shorter budget turns a busy 4K decode into a demotion strike.
const BEGIN_FRAME_BUDGET: Duration = Duration::from_millis(100);
const BEGIN_FRAME_SPIN: Duration = Duration::from_micros(500);
const BEGIN_FRAME_SLEEP: Duration = Duration::from_millis(1);
const E_PENDING: i32 = 0x8000_000A_u32 as i32;

/// Pin string for this rung.
pub const DECODER_PIN: &str = "native-d3d11va";

/// Per-codec planner, chosen once at construction. Session, pool, and submission stay
/// codec-agnostic; forking them per codec would fork the machinery that is hard to get right.
enum Planner {
    H264(Box<pf_dxvadec::H264Planner>),
    H265(Box<pf_dxvadec::H265Planner>),
    Av1(Box<pf_dxvadec::Av1Planner>),
}

/// Geometry and colour of a decoded picture, for the hand-off. Split from [`Submission`]
/// because AV1 `show_existing_frame` (5.9.2) carries none of its own: the shown frame's
/// state is loaded, so [`Session::held`] is the source.
#[derive(Debug, Clone, Copy)]
struct PictureFacts {
    colour: ColorDesc,
    keyframe: bool,
    /// Whole prediction chain was fully available — see
    /// [`crate::video_d3d11::D3d11Frame::references_clean`].
    references_clean: bool,
    /// Conformance-window crop (H.264/H.265) or render size (AV1) — the blit rectangle.
    width: u32,
    height: u32,
}

/// AV1 tile and bitstream inputs. H.264/H.265 have no counterpart.
struct Av1Buffers {
    bitstream: pf_dxvadec::Av1Bitstream,
    /// One `DXVA_Tile_AV1` per tile; packer rebases `DataOffset` into the driver mapping.
    tiles: Vec<pf_dxvadec::TileAv1>,
}

struct Submission {
    pic_params: Vec<u8>,
    /// Inverse-quantization matrix, or `None` when the buffer must not be submitted
    /// (HEVC with `scaling_list_enabled_flag` clear; see `DecodePlanDxvaH265::qmatrix`).
    qmatrix: Option<Vec<u8>>,
    /// `NumMBsInBuffer` on bitstream and slice-control: coded MBs on H.264, 0 on HEVC.
    /// Both are libavcodec's values (`dxva2_h264.c` / `dxva2_hevc.c`).
    mb_count: u32,
    slice_ranges: Vec<std::ops::Range<usize>>,
    setup_slot: u8,
    /// Picture id assigned to `setup_slot`. AV1 must return it to the ledger
    /// ([`NativeD3d11Decoder::frame_av1`]); without it the leak is invisible.
    setup_id: u64,
    /// Pictures this AU retires while the submission still names their surfaces.
    /// Released after the decode op ([`NativeD3d11Decoder::release_deferred`]).
    /// Dropping it decodes into a surface the picture also predicts from.
    /// Empty on H.265: `H265Planner` snapshots `dpb_refs` after `decode_rps`.
    release_after_decode: Vec<u64>,
    codec: Codec,
    facts: PictureFacts,
    /// Integrity warning: a substitute reference. Never submitted — see [`NativeD3d11Decoder::decode`].
    concealed: bool,
    /// AV1 tile-control and bitstream set. `None` on H.264/H.265; [`NativeD3d11Decoder::fill_and_submit`] dispatches on it.
    av1: Option<Av1Buffers>,
    /// Whether this frame displays. AV1 may decode hidden references in the same unit;
    /// H.264/H.265 always true.
    show: bool,
}

/// Session identity, read from the SPS the planner just activated, not the negotiated
/// format. Every field sizes an object that cannot change after creation: coded size and
/// DPB depth size the decoder, pool, and slot map; chroma and luma depth pick the profile
/// GUID and `DXGI_FORMAT`. Any change rebuilds the session whole — a half rebuild hands
/// out missing surface indices or writes 10-bit samples into 8-bit surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamShape {
    coded_width: u32,
    coded_height: u32,
    max_dpb_frames: usize,
    chroma_format_idc: u8,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
}

impl StreamShape {
    fn bit_depth(&self) -> u8 {
        8 + self.bit_depth_luma_minus8
    }

    /// Session shape one AV1 plan implies.
    ///
    /// Coded size is the sequence header's **maximum** frame size, not this frame's.
    /// AV1 lets every frame pick a size up to that max; `DXVA_PicParams_AV1` carries
    /// both so the decoder and pool are built once. Sizing from the frame would rebuild
    /// (and drop every reference) on a downward resize, which AV1 allows without a key.
    /// DPB is eight reference slots plus the current picture: nine surfaces.
    fn of_av1(plan: &pf_dxvadec::AuPlanAv1) -> StreamShape {
        let depth = plan.picture.bit_depth.saturating_sub(8);
        StreamShape {
            coded_width: u32::from(plan.sequence.max_frame_width_minus_1) + 1,
            coded_height: u32::from(plan.sequence.max_frame_height_minus_1) + 1,
            max_dpb_frames: pf_dxvadec::NUM_REF_SLOTS,
            chroma_format_idc: plan.picture.chroma_format_idc,
            // AV1 codes one bit depth for all planes, so luma and chroma match and
            // `Session::build`'s mixed-depth refusal cannot fire.
            bit_depth_luma_minus8: depth,
            bit_depth_chroma_minus8: depth,
        }
    }
}

/// Live decoder plus everything sized to this stream. Rebuilt whole on any
/// [`StreamShape`] change: a half rebuild is a corrupt reference.
struct Session {
    decoder: ID3D11VideoDecoder,
    /// One texture array, kept for the session and used as the video-processor blit source.
    pool: ID3D11Texture2D,
    views: Vec<ID3D11VideoDecoderOutputView>,
    slots: pf_dxvadec::SlotMap,
    /// Per-surface facts for AV1 `show_existing_frame`. Stale entries are unreachable:
    /// a surface is only named through the slot map.
    held: Vec<Option<PictureFacts>>,
    shape: StreamShape,
    /// Profile chosen from this SPS chroma/depth, which may differ from the negotiated format.
    profile: DxvaProfile,
}

pub struct NativeD3d11Decoder {
    device: ID3D11Device,
    /// Live context for teardown/rebuild; the hand-off holds its own clone for the blit.
    #[allow(dead_code)]
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    /// Decoder, pool, and slot map. Declared before `handoff` so it drops first: the
    /// ring must outlive the decode surfaces it converted.
    session: Option<Session>,
    handoff: HandoffRing,
    planner: Planner,
    codec: Codec,
    /// `StatusReportFeedbackNumber`, monotonic from 1. 0 is an unwritten buffer; never a tag.
    status_id: u32,
    health: DecodeHealth,
    want_recovery: bool,
}

// SAFETY: every field is either owned plain data or a reference-counted COM interface with
// interlocked counts, so moving the whole struct to another thread and releasing it there is
// sound. D3D11's immediate context is not thread-SAFE but it is thread-AGNOSTIC: it requires
// serialised use, which `&mut self` on every method gives, not use from one fixed thread. The
// presenter never touches these objects — it reaches the shared textures through their NT
// handles on its own device. Moved, never shared; deliberately NOT `Sync`.
unsafe impl Send for NativeD3d11Decoder {}

impl NativeD3d11Decoder {
    /// Build on the presenter's adapter.
    ///
    /// Refusals fail here, before an AU: codec, negotiated shape, profile list, decoder
    /// config. A construction miss falls through with a clean stream; a first-AU miss
    /// burns the opening IDR and only exits through demotion.
    ///
    /// The decoder object is not created here: `D3D11_VIDEO_DECODER_DESC` needs the coded
    /// size from the in-band SPS. The negotiated format only proves the adapter can host
    /// a profile; the session's profile comes from `StreamShape`.
    pub fn new(
        codec: Codec,
        stream: StreamFormat,
        luid: Option<[u8; 8]>,
        hdr10_out: bool,
    ) -> Result<NativeD3d11Decoder> {
        let profile = pf_dxvadec::profile_for(codec, stream.chroma_format_idc, stream.bit_depth)
            .ok_or_else(|| {
                anyhow!(
                    "no DXVA profile for {codec:?} chroma_format_idc {} at {} bits",
                    stream.chroma_format_idc,
                    stream.bit_depth
                )
            })?;
        let (device, context) = create_device(luid)?;
        let handoff = HandoffRing::new(device.clone(), context.clone(), hdr10_out)?;
        let video_device = handoff.video_device().clone();
        let video_context: ID3D11VideoContext = context
            .cast()
            .context("context lacks ID3D11VideoContext (created without VIDEO_SUPPORT)")?;
        profile_supported(&video_device, profile)?;
        let planner = match codec {
            Codec::H264 => Planner::H264(Box::new(pf_dxvadec::H264Planner::new())),
            Codec::H265 => Planner::H265(Box::new(pf_dxvadec::H265Planner::new())),
            Codec::Av1 => Planner::Av1(Box::new(pf_dxvadec::Av1Planner::new())),
        };
        tracing::info!(
            ?codec,
            negotiated_profile = profile.name,
            chroma = stream.chroma_format_idc,
            bits = stream.bit_depth,
            "native D3D11VA decoder built (pf-dxvadec, pinned)"
        );
        Ok(NativeD3d11Decoder {
            device,
            context,
            video_device,
            video_context,
            session: None,
            handoff,
            planner,
            codec,
            status_id: 0,
            // D3D11VA has no per-picture status read (`ID3D11VideoContext` exposes none),
            // so `failed` stays 0 and `status_queries` is false. Claiming query support
            // we do not have would report "clean" for unmeasured.
            health: DecodeHealth {
                status_queries: false,
                ..DecodeHealth::default()
            },
            want_recovery: false,
        })
    }

    pub fn name(&self) -> &'static str {
        DECODER_PIN
    }

    /// Hand NV12 / P010 pictures over as planar copies when the presenter imports them
    /// (`HandoffRing::set_planar`); RGB through the video processor otherwise.
    pub fn with_planar(mut self, nv12: bool, p010: bool) -> Self {
        self.handoff.set_planar(nv12, p010);
        self
    }

    pub fn health(&self) -> DecodeHealth {
        self.health
    }

    /// Drain the keyframe request raised by concealment.
    pub fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.want_recovery)
    }

    /// The gate lifted on intra refresh marks: the planner's damaged-chain marks are stale
    /// (`CleanLedger::clear`).
    pub fn forgive_unclean(&mut self) {
        match &mut self.planner {
            Planner::H264(p) => p.forgive_unclean(),
            Planner::H265(p) => p.forgive_unclean(),
            Planner::Av1(p) => p.forgive_unclean(),
        }
    }

    /// Plan, convert, and submit one access unit.
    ///
    /// `Ok(Some)` is a picture in the hand-off. `Ok(None)` is not an error: concealment
    /// (drop, request recovery) or an HEVC RASL skip after an open-GOP join. Either as
    /// `Err` would demote on the lossy links this rung exists to handle. `Err` is a
    /// decoder that could not run — streak-eligible, counted as `refused`.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<D3d11Frame>> {
        if matches!(self.planner, Planner::Av1(_)) {
            return self.decode_av1(au);
        }
        let submission = match self.plan(au) {
            Ok(Some(submission)) => submission,
            // RASL skip: no plan, no health note — the decoder was never fed.
            Ok(None) => return Ok(None),
            Err(e) => {
                self.health.note(false, true, 0);
                tracing::warn!(error = %format!("{e:#}"), "native D3D11VA refused the access unit");
                return Err(e);
            }
        };
        if submission.concealed {
            // Substitute for a lost reference. Do not submit: a wrong DPB entry poisons
            // every AU after. Deferred releases still run — the planner assigned a slot.
            self.release_deferred(&submission);
            self.health.note(true, false, 0);
            self.want_recovery = true;
            return Ok(None);
        }
        let submitted = self.submit(au, &submission);
        // Surfaces conversion held back, freed now that the decode op has been issued
        // (or failed). See [`Self::release_deferred`].
        self.release_deferred(&submission);
        let frame = match submitted {
            Ok(frame) => frame,
            Err(e) => {
                self.health.note(false, true, 0);
                tracing::warn!(error = %format!("{e:#}"), "native D3D11VA submission failed");
                return Err(e);
            }
        };
        self.health.note(false, false, 0);
        Ok(Some(frame))
    }

    /// Release surfaces this submission still named. Safe only after the decode op
    /// has been issued (or will never be). Dropping the list holds a surface per AU
    /// and hits `SlotError::Full` within the ledger depth, so every `decode` exit
    /// runs this, concealed and failed included.
    fn release_deferred(&mut self, sub: &Submission) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        for &id in &sub.release_after_decode {
            if !session.slots.release(id) {
                // Misses after a renegotiation replace the slot map while the planner
                // still reports the old ids in `removed`. Outside a rebuild, conversion
                // and ledger disagree.
                tracing::debug!(id, "a deferred release named a picture holding no surface");
            }
        }
    }

    /// One AV1 temporal unit: decode every frame, present at most one.
    ///
    /// A unit may carry several frame headers. Hidden frames (alt-refs) must decode —
    /// later pictures predict from them — and must not reach the presenter.
    ///
    /// Concealment is per unit, not per picture: a damaged frame is still converted
    /// (that assigns its DPB slot) but not submitted. Skipping conversion would
    /// desynchronise the slot map and turn later references into demotion-eligible
    /// `Err`. Nothing from the unit is presented.
    fn decode_av1(&mut self, au: &[u8]) -> Result<Option<D3d11Frame>> {
        let plans = match &mut self.planner {
            Planner::Av1(planner) => match planner.plan_au(au) {
                Ok(plans) => plans,
                Err(e) => {
                    self.health.note(false, true, 0);
                    tracing::warn!(
                        error = %format!("{e:?}"),
                        "native D3D11VA refused the AV1 temporal unit"
                    );
                    return Err(anyhow!("plan: {e:?}"));
                }
            },
            _ => bail!("decode_av1 on a non-AV1 planner"),
        };

        let mut shown = None;
        let mut concealed = false;
        for plan in &plans {
            let damaged = plan
                .warnings
                .iter()
                .any(pf_dxvadec::is_integrity_warning_av1);
            concealed |= damaged;
            match self.frame_av1(au, plan, damaged) {
                Ok(Some(frame)) => shown = Some(frame),
                Ok(None) => {}
                Err(e) => {
                    self.health.note(false, true, 0);
                    tracing::warn!(error = %format!("{e:#}"), "native D3D11VA AV1 frame failed");
                    return Err(e);
                }
            }
        }
        if concealed {
            // An earlier frame of this unit may already have been blitted. `D3d11Frame`
            // is POD (no handle, no Drop); the ring's keyed mutex is taken and released
            // around the blit, so an unconsumed slot is reused. Deferring every blit
            // to unit end would read a surface after something else could claim its slot.
            self.health.note(true, false, 0);
            self.want_recovery = true;
            return Ok(None);
        }
        self.health.note(false, false, 0);
        Ok(shown)
    }

    /// One frame of a temporal unit: converted, submitted unless `damaged`, blitted
    /// only if the unit displays it.
    ///
    /// `refresh_frame_flags == 0` is legal AV1 (shown once, never referenced) and
    /// never enters the planner's store, so it is never reported removed. Conversion
    /// still assigned a ledger slot; leaving it held exhausts the nine-slot ledger.
    /// Released here on the concealed path too.
    ///
    /// AV1 applies `refresh_frame_flags` after decode (7.20), so a frame that reads a
    /// slot its own refresh overwrites is ordinary. `plan_to_dxva_av1` hands those
    /// pictures back rather than releasing: `SlotMap::assign` would return the vacated
    /// surface to `setup_slot` and the submission would name one surface as both
    /// current and reference. Release after the decode op.
    fn frame_av1(
        &mut self,
        au: &[u8],
        plan: &pf_dxvadec::AuPlanAv1,
        damaged: bool,
    ) -> Result<Option<D3d11Frame>> {
        // `show_existing_frame` decodes nothing: it re-displays a picture an earlier
        // hidden frame put in a reference slot.
        if plan.dpb.stored.is_none() {
            return self.show_existing_av1(plan);
        }
        let sub = self.plan_frame_av1(au, plan)?;
        // Hold the `Result` so the two slot releases below run on failure too.
        // `decode_av1` keeps the session (no slot-map rebuild); an early return
        // leaked a surface per failed frame. Later frames of a failed unit are
        // abandoned: recovering a half-decoded unit is the pump's question.
        let shown = self.decode_and_present_av1(au, &sub, damaged);
        if shown.is_err() {
            // Slot map now names this picture while the surface still holds the previous
            // occupant's pixels. A later `show_existing_frame` would blit the old geometry
            // and colour. The `damaged` path already clears; the failure path did not.
            if let Some(session) = self.session.as_mut() {
                if let Some(held) = session.held.get_mut(usize::from(sub.setup_slot)) {
                    *held = None;
                }
            }
        }

        // Surfaces this frame's refresh displaced while the submission still named them.
        // Safe now: the decode op has been issued, or there is no op (damaged/failed).
        self.release_deferred(&sub);

        // Slot nothing will ask for again. After the blit, so the surface is read first.
        if plan.header.refresh_frame_flags == 0 {
            if let Some(session) = self.session.as_mut() {
                if session.slots.release(sub.setup_id) {
                    tracing::trace!(
                        id = sub.setup_id,
                        slot = sub.setup_slot,
                        "AV1 frame refreshes no reference slot — returning its surface"
                    );
                }
                if let Some(held) = session.held.get_mut(usize::from(sub.setup_slot)) {
                    *held = None;
                }
            }
        }
        shown
    }

    /// Submit one converted AV1 frame and blit it if it displays. Split from
    /// [`Self::frame_av1`] so the caller can run slot releases on the failure path.
    fn decode_and_present_av1(
        &mut self,
        au: &[u8],
        sub: &Submission,
        damaged: bool,
    ) -> Result<Option<D3d11Frame>> {
        let shown = if damaged {
            // Converted (slot map stays in step) but not submitted. Clear `held`: the
            // slot map names this picture while the surface still holds the previous
            // occupant's pixels. `None` makes `show_existing_frame` return `Ok(None)`.
            if let Some(session) = self.session.as_mut() {
                if let Some(held) = session.held.get_mut(usize::from(sub.setup_slot)) {
                    *held = None;
                }
            }
            None
        } else {
            self.decode_into(au, sub)?;
            if let Some(session) = self.session.as_mut() {
                if let Some(held) = session.held.get_mut(usize::from(sub.setup_slot)) {
                    *held = Some(sub.facts);
                }
            }
            if sub.show {
                Some(self.present(sub.setup_slot, sub.facts)?)
            } else {
                None
            }
        };
        Ok(shown)
    }

    /// Convert one AV1 frame, rebuilding the session when the sequence moved.
    ///
    /// `self.status_id` is not advanced. AV1 submissions carry a zero
    /// `StatusReportFeedbackNumber` — setting it breaks some NVIDIA drivers
    /// (libavcodec `dxva2_av1.c` comments the assignment out; Chromium ships 0) —
    /// so [`pf_dxvadec::plan_to_dxva_av1`] takes no id.
    fn plan_frame_av1(&mut self, au: &[u8], plan: &pf_dxvadec::AuPlanAv1) -> Result<Submission> {
        let session = ensure_session(
            &mut self.session,
            &self.device,
            &self.video_device,
            self.codec,
            StreamShape::of_av1(plan),
        )?;
        let dxva = pf_dxvadec::plan_to_dxva_av1(au, plan, &mut session.slots)
            .map_err(|e| anyhow!("plan → DXVA: {e}"))?;
        Ok(Submission {
            pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
            // AV1 selects matrices by index from tables the decoder already has.
            // `dxva2_av1_end_frame` passes `NULL, 0` and the generic layer skips the buffer.
            qmatrix: None,
            mb_count: 0,
            slice_ranges: Vec::new(),
            setup_slot: dxva.setup_slot,
            setup_id: dxva.setup_id,
            release_after_decode: dxva.release_after_decode,
            codec: Codec::Av1,
            facts: PictureFacts {
                colour: colour_of(plan.picture.colour),
                keyframe: plan.picture.is_key,
                references_clean: plan.picture.references_clean,
                // Render size: AV1 display region (conformance-window crop on the other
                // two). Not `upscaled_width` when superres is on. Treated as a crop to
                // match the Vulkan rung and the goldens; libavcodec uses sample aspect.
                // Clamped: 5.9.6 has no upper bound, and an unclamped crop overruns the surface.
                width: plan.picture.render_width.min(plan.picture.upscaled_width),
                height: plan.picture.render_height.min(plan.picture.frame_height),
            },
            concealed: false,
            av1: Some(Av1Buffers {
                bitstream: dxva.bitstream,
                tiles: dxva.tiles,
            }),
            show: plan.picture.show_frame,
        })
    }

    /// Blit a surface the pool already holds. Geometry and colour come from
    /// [`Session::held`]: a `show_existing_frame` header carries none (AV1 5.9.2).
    /// Empty slot is `Ok(None)`, not an error — already reported as
    /// `MissingShowExisting`, so the caller has concealed the unit.
    fn show_existing_av1(&mut self, plan: &pf_dxvadec::AuPlanAv1) -> Result<Option<D3d11Frame>> {
        let target = self.session.as_ref().and_then(|session| {
            let id = plan.dpb.outputs.first().copied()?;
            let slot = session.slots.slot_of(id)?;
            let facts = (*session.held.get(usize::from(slot))?)?;
            Some((slot, facts))
        });
        // Showing a key this way resets the reference store (7.20). Follow the
        // plan's removals or the map fills and the next assignment fails.
        if let Some(session) = self.session.as_mut() {
            for &id in &plan.dpb.removed {
                session.slots.release(id);
            }
        }
        match target {
            Some((slot, facts)) => self.present(slot, facts).map(Some),
            None => Ok(None),
        }
    }

    /// Plan one AU and convert it, rebuilding the session when shape moved.
    /// `Ok(None)` is the RASL skip and nothing else.
    fn plan(&mut self, au: &[u8]) -> Result<Option<Submission>> {
        self.status_id = self.status_id.wrapping_add(1).max(1);
        let status_id = self.status_id;
        match &mut self.planner {
            Planner::H264(planner) => {
                let plan = match planner.plan_au(au) {
                    Ok(plan) => plan,
                    // Nothing to feed until the IDR and its parameter sets land — a
                    // decoder built mid-GOP sees slices first. Idle, not a refusal.
                    Err(
                        e @ (pf_dxvadec::PlanError::AwaitingIdr
                        | pf_dxvadec::PlanError::NoActiveParamSet { .. }),
                    ) => {
                        self.want_recovery = true;
                        tracing::debug!(error = %e, "native D3D11VA idle until the next IDR");
                        return Ok(None);
                    }
                    Err(e) => bail!("plan: {e}"),
                };
                let concealed = plan.warnings.iter().any(pf_dxvadec::is_integrity_warning);
                let session = ensure_session(
                    &mut self.session,
                    &self.device,
                    &self.video_device,
                    self.codec,
                    StreamShape {
                        coded_width: plan.picture.coded_width,
                        coded_height: plan.picture.coded_height,
                        max_dpb_frames: plan.picture.max_dpb_frames,
                        chroma_format_idc: plan.picture.chroma_format_idc,
                        bit_depth_luma_minus8: plan.picture.bit_depth_luma_minus8,
                        bit_depth_chroma_minus8: plan.picture.bit_depth_chroma_minus8,
                    },
                )?;
                let dxva = pf_dxvadec::plan_to_dxva(&plan, &mut session.slots, status_id)
                    .map_err(|e| anyhow!("plan → DXVA: {e}"))?;
                Ok(Some(Submission {
                    pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
                    // H.264 always carries matrices: libavcodec submits the buffer
                    // unconditionally; the PPS lists are always meaningful (Table 7-2).
                    qmatrix: Some(pf_dxvadec::as_bytes(&dxva.qmatrix).to_vec()),
                    mb_count: dxva.mb_count,
                    slice_ranges: dxva.slice_ranges,
                    setup_slot: dxva.setup_slot,
                    setup_id: dxva.setup_id,
                    release_after_decode: dxva.release_after_decode,
                    codec: Codec::H264,
                    facts: PictureFacts {
                        colour: colour_of(plan.picture.colour),
                        keyframe: plan.picture.is_idr,
                        references_clean: plan.picture.references_clean,
                        width: plan.picture.display_crop.width,
                        height: plan.picture.display_crop.height,
                    },
                    concealed,
                    av1: None,
                    show: true,
                }))
            }
            Planner::H265(planner) => {
                let plan = match planner.plan_au(au) {
                    Ok(plan) => plan,
                    // Leading pictures after a CRA join: spec says decode and output
                    // nothing. Mapping to `Err` would beg the host for a keyframe.
                    Err(pf_dxvadec::PlanErrorH265::RaslSkipped { poc }) => {
                        tracing::debug!(poc, "RASL picture skipped after an open-GOP join");
                        return Ok(None);
                    }
                    // Same idle wait as the H.264 arm.
                    Err(
                        e @ (pf_dxvadec::PlanErrorH265::AwaitingIdr
                        | pf_dxvadec::PlanErrorH265::NoActiveParamSet { .. }),
                    ) => {
                        self.want_recovery = true;
                        tracing::debug!(error = %e, "native D3D11VA idle until the next IDR");
                        return Ok(None);
                    }
                    Err(e) => bail!("plan: {e}"),
                };
                let concealed = plan
                    .warnings
                    .iter()
                    .any(pf_dxvadec::is_integrity_warning_h265);
                let session = ensure_session(
                    &mut self.session,
                    &self.device,
                    &self.video_device,
                    self.codec,
                    StreamShape {
                        coded_width: plan.picture.coded_width,
                        coded_height: plan.picture.coded_height,
                        max_dpb_frames: plan.picture.max_dpb_frames,
                        chroma_format_idc: plan.picture.chroma_format_idc,
                        bit_depth_luma_minus8: plan.picture.bit_depth_luma_minus8,
                        bit_depth_chroma_minus8: plan.picture.bit_depth_chroma_minus8,
                    },
                )?;
                let dxva = pf_dxvadec::plan_to_dxva_h265(&plan, &mut session.slots, status_id)
                    .map_err(|e| anyhow!("plan → DXVA: {e}"))?;
                Ok(Some(Submission {
                    pic_params: pf_dxvadec::as_bytes(&dxva.pic_params).to_vec(),
                    // `None` unless scaling lists are enabled — then the buffer is not
                    // submitted, matching libavcodec.
                    qmatrix: dxva
                        .qmatrix
                        .as_ref()
                        .map(|qm| pf_dxvadec::as_bytes(qm).to_vec()),
                    // HEVC has no macroblocks; libavcodec leaves `NumMBsInBuffer` 0.
                    mb_count: 0,
                    slice_ranges: dxva.slice_ranges,
                    setup_slot: dxva.setup_slot,
                    setup_id: dxva.setup_id,
                    // Empty: `H265Planner` snapshots `dpb_refs` after `decode_rps`, so
                    // an RPS-dropped picture is never in `RefPicList`. The other two
                    // snapshot before marking and need the deferral.
                    release_after_decode: Vec::new(),
                    codec: Codec::H265,
                    facts: PictureFacts {
                        colour: colour_of(plan.picture.colour),
                        keyframe: plan.picture.is_irap,
                        references_clean: plan.picture.references_clean,
                        width: plan.picture.display_crop.width,
                        height: plan.picture.display_crop.height,
                    },
                    concealed,
                    av1: None,
                    show: true,
                }))
            }
            // An AV1 AU is a temporal unit (`Vec` of plans). Walked by [`Self::decode_av1`].
            Planner::Av1(_) => bail!(
                "an AV1 temporal unit is planned frame by frame (decode_av1), not through plan()"
            ),
        }
    }

    /// Decode one picture and hand it off. H.264/H.265: one AU is one displayed picture.
    fn submit(&mut self, au: &[u8], sub: &Submission) -> Result<D3d11Frame> {
        self.decode_into(au, sub)?;
        self.present(sub.setup_slot, sub.facts)
    }

    /// `DecoderBeginFrame` → codec buffers → `SubmitDecoderBuffers` → `DecoderEndFrame`.
    /// Writes the decode surface only. Split from the hand-off because AV1 decodes
    /// hidden alt-refs that must not be blitted. Buffer order matches libavcodec
    /// (pic params, Q matrices, bitstream, slice control): a driver may care.
    fn decode_into(&mut self, au: &[u8], sub: &Submission) -> Result<()> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("no decode session (plan should have built one)"))?;
        let view = session
            .views
            .get(usize::from(sub.setup_slot))
            .ok_or_else(|| anyhow!("setup surface {} is outside the pool", sub.setup_slot))?;

        let (retries, waited) = begin_frame(&self.video_context, &session.decoder, view)?;
        self.handoff.note_begin_frame(retries, waited);
        // Inside a frame from here; every exit must `DecoderEndFrame` or the next
        // `DecoderBeginFrame` fails and the session is wedged.
        let result = self.fill_and_submit(au, sub, session);
        // SAFETY: a COM call on the live video context, ending the frame this method began on
        // the live decoder. Its own failure is reported only when nothing worse happened.
        let ended = unsafe { self.video_context.DecoderEndFrame(&session.decoder) };
        result?;
        ended.ok().context("DecoderEndFrame")
    }

    /// Shared `VideoProcessorBlt` → shareable-RGBA hand-off for a pool surface.
    /// Takes a surface index and [`PictureFacts`], not a [`Submission`]: AV1
    /// `show_existing_frame` presents a picture submitted several AUs ago.
    fn present(&mut self, slot: u8, facts: PictureFacts) -> Result<D3d11Frame> {
        let pool = self
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("no decode session to present from"))?
            .pool
            .clone();
        self.handoff.present(HandoffSource {
            texture: &pool,
            array_slice: u32::from(slot),
            width: facts.width,
            height: facts.height,
            color: facts.colour,
            keyframe: facts.keyframe,
            references_clean: facts.references_clean,
            decoder: DECODER_PIN,
        })
    }

    fn fill_and_submit(&self, au: &[u8], sub: &Submission, session: &Session) -> Result<()> {
        match &sub.av1 {
            Some(av1) => self.fill_and_submit_av1(au, av1, sub, session),
            None => self.fill_and_submit_slices(au, sub, session),
        }
    }

    /// AV1 buffer set: picture parameters, bitstream, tile control. Three, never
    /// four — AV1 transmits no quantization matrix (`dxva2_av1_end_frame` passes
    /// `NULL, 0`). Tile records go in the SLICE_CONTROL slot. `NumMBsInBuffer` is
    /// 0 on all three (`dxva2_av1.c`); inventing a tile-count spelling diverges.
    fn fill_and_submit_av1(
        &self,
        au: &[u8],
        av1: &Av1Buffers,
        sub: &Submission,
        session: &Session,
    ) -> Result<()> {
        // libavcodec's map/fill/release order: picture parameters, bitstream, tile control.
        let pp_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
            |dst| {
                copy_into(dst, &sub.pic_params)?;
                Ok(sub.pic_params.len())
            },
        )?;

        // Packed in the driver's mapping (no staging). Returns tile records with
        // `DataOffset` rebased into that mapping, so the two steps cannot merge.
        let mut packed = None;
        let bs_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            |dst| {
                let p = pf_dxvadec::pack_av1(au, &av1.bitstream, &av1.tiles, dst)
                    .map_err(|e| anyhow!("AV1 tile pack: {e}"))?;
                let size = p.data_size as usize;
                packed = Some(p);
                Ok(size)
            },
        )?;
        let packed = packed.expect("the writer above ran or returned an error");

        let tile_bytes = pf_dxvadec::slice_bytes(&packed.tiles);
        let tc_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
            |dst| {
                copy_into(dst, tile_bytes)?;
                Ok(tile_bytes.len())
            },
        )?;

        // Descriptor table is pf-dxvadec's (CPU-tested: types, order, sizes, zero
        // `NumMBsInBuffer`). This arm only checks the writers' byte counts against it.
        let descs = pf_dxvadec::descriptors_av1(&packed);
        let written = [
            (pf_dxvadec::BUFFER_PICTURE_PARAMETERS, pp_size),
            (pf_dxvadec::BUFFER_BITSTREAM, bs_size),
            (pf_dxvadec::BUFFER_SLICE_CONTROL, tc_size),
        ];
        let mut out: Vec<D3D11_VIDEO_DECODER_BUFFER_DESC> = Vec::with_capacity(descs.len());
        for desc in &descs {
            let wrote = written
                .iter()
                .find(|(kind, _)| *kind == desc.buffer_type)
                .map(|(_, size)| *size)
                .ok_or_else(|| anyhow!("no writer for AV1 buffer type {}", desc.buffer_type))?;
            if wrote != desc.data_size as usize {
                bail!(
                    "AV1 buffer type {} was written with {wrote} bytes, the descriptor \
                     declares {}",
                    desc.buffer_type,
                    desc.data_size
                );
            }
            out.push(buffer_desc(
                buffer_kind(desc.buffer_type)?,
                desc.data_size as usize,
                desc.num_mbs_in_buffer,
            ));
        }

        // SAFETY: a COM call on the live video context with the live decoder and a slice of
        // fully-initialized descriptors that outlives the call. Every buffer named by a
        // descriptor was released back to the driver by `write_buffer` before this runs,
        // which is what makes them submittable.
        unsafe {
            self.video_context
                .SubmitDecoderBuffers(&session.decoder, &out)
        }
        .ok()
        .context("SubmitDecoderBuffers (AV1)")
    }

    fn fill_and_submit_slices(&self, au: &[u8], sub: &Submission, session: &Session) -> Result<()> {
        let mut descs: Vec<D3D11_VIDEO_DECODER_BUFFER_DESC> = Vec::with_capacity(4);

        let pp_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
            |dst| {
                copy_into(dst, &sub.pic_params)?;
                Ok(sub.pic_params.len())
            },
        )?;
        descs.push(buffer_desc(
            D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
            pp_size,
            0,
        ));

        // HEVC with scaling lists disabled submits no such buffer — libavcodec's
        // condition. Picture parameters already told the driver to ignore the matrix.
        if let Some(qmatrix) = &sub.qmatrix {
            let qm_size = write_buffer(
                &self.video_context,
                &session.decoder,
                D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
                |dst| {
                    copy_into(dst, qmatrix)?;
                    Ok(qmatrix.len())
                },
            )?;
            descs.push(buffer_desc(
                D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX,
                qm_size,
                0,
            ));
        }

        // Packed in the driver's mapping. Returns slice locations the control
        // buffer below is built from, so the two steps cannot merge.
        let mut packed = None;
        let bs_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            |dst| {
                let p = pf_dxvadec::pack(au, &sub.slice_ranges, dst)
                    .map_err(|e| anyhow!("bitstream pack: {e}"))?;
                let size = p.data_size as usize;
                packed = Some(p);
                Ok(size)
            },
        )?;
        descs.push(buffer_desc(
            D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
            bs_size,
            sub.mb_count,
        ));
        let packed = packed.expect("the writer above ran or returned an error");

        let sc_size = write_buffer(
            &self.video_context,
            &session.decoder,
            D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
            |dst| match sub.codec {
                Codec::H264 => {
                    let records = pf_dxvadec::slice_control(&packed.records);
                    let bytes = pf_dxvadec::slice_bytes(&records);
                    copy_into(dst, bytes)?;
                    Ok(bytes.len())
                }
                Codec::H265 => {
                    let records = pf_dxvadec::slice_control_h265(&packed.records);
                    let bytes = pf_dxvadec::slice_bytes(&records);
                    copy_into(dst, bytes)?;
                    Ok(bytes.len())
                }
                // `fill_and_submit` dispatched AV1 to the other arm. A fourth codec
                // must fail to compile here rather than pack tiles as H.264 slices.
                Codec::Av1 => bail!("AV1 does not submit slice-control records"),
            },
        )?;
        descs.push(buffer_desc(
            D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
            sc_size,
            sub.mb_count,
        ));

        // SAFETY: a COM call on the live video context with the live decoder and a slice of
        // fully-initialized descriptors that outlives the call. Every buffer named by a
        // descriptor was released back to the driver by `write_buffer` before this runs,
        // which is what makes them submittable.
        unsafe {
            self.video_context
                .SubmitDecoderBuffers(&session.decoder, &descs)
        }
        .ok()
        .context("SubmitDecoderBuffers")
    }
}

/// pf-bitstream H.273 code points as the presenter's [`ColorDesc`].
///
/// Per picture, never latched at session start: an HDR desktop can switch to
/// PQ/BT.2020 in-band with a new SPS. pf-bitstream applies E.2.1 "unspecified"
/// inference, so these are always meaningful code points.
fn colour_of(colour: pf_dxvadec::ColourDescription) -> ColorDesc {
    ColorDesc {
        primaries: colour.colour_primaries,
        transfer: colour.transfer_characteristics,
        matrix: colour.matrix_coefficients,
        full_range: colour.video_full_range,
    }
}

/// Does this adapter expose any HEVC decode profile? Asked before the codec caps go
/// on the wire; no decoder is built here.
pub fn adapter_decodes_hevc(luid: Option<[u8; 8]>) -> bool {
    let Ok((device, _)) = create_device(luid) else {
        return false;
    };
    let Ok(video) = device.cast::<ID3D11VideoDevice>() else {
        return false;
    };
    profile_supported(&video, pf_dxvadec::config::HEVC_VLD_MAIN).is_ok()
        || profile_supported(&video, pf_dxvadec::config::HEVC_VLD_MAIN10).is_ok()
}

/// Does the adapter expose this decode profile for this surface format?
///
/// Checked at construction, not first AU: discovering it mid-stream costs the
/// opening IDR and only exits through demotion.
fn profile_supported(video: &ID3D11VideoDevice, profile: DxvaProfile) -> Result<()> {
    let wanted = GUID::from_u128(profile.guid);
    // SAFETY: COM calls on the live video device; the count bounds the loop and each profile
    // is returned by value.
    let profiles: Vec<GUID> = unsafe {
        let n = video.GetVideoDecoderProfileCount();
        (0..n)
            .filter_map(|i| video.GetVideoDecoderProfile(i).ok())
            .collect()
    };
    if !profiles.contains(&wanted) {
        bail!("adapter exposes no {} decode profile", profile.name);
    }
    // SAFETY: same live device; the arguments are a borrowed local GUID and a plain format
    // enum.
    let ok = unsafe { video.CheckVideoDecoderFormat(&wanted, profile.dxgi_format as DXGI_FORMAT) }
        .map(|b| b.as_bool())
        .unwrap_or(false);
    if !ok {
        bail!(
            "adapter's {} profile cannot decode into DXGI format {}",
            profile.name,
            profile.dxgi_format
        );
    }
    Ok(())
}

/// Build the session if none, or rebuild when shape moved.
///
/// Shape is the SPS, never the negotiated format. Decoder, pool, slot map, and
/// profile all derive from it ([`StreamShape`]). A partial rebuild hands out
/// missing indices or the wrong sample width. `CapacityMismatch` forces the
/// DPB-depth leg of the same rule.
fn ensure_session<'a>(
    slot: &'a mut Option<Session>,
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    codec: Codec,
    shape: StreamShape,
) -> Result<&'a mut Session> {
    let matches = slot.as_ref().is_some_and(|s| s.shape == shape);
    if !matches {
        if let Some(old) = slot.as_ref() {
            // Log the old profile: an in-band 8-bit → 10-bit flip is a rebuild
            // that also changes it.
            tracing::info!(
                was = ?old.shape,
                was_profile = old.profile.name,
                now = ?shape,
                "stream renegotiated — rebuilding the native D3D11VA decode session"
            );
        }
        // Drop first so the old pool's VRAM is free. A 4K pool is ~100 MB; holding
        // two while allocating is how a rebuild fails on a small card.
        *slot = None;
        *slot = Some(Session::build(device, video_device, codec, shape)?);
    }
    Ok(slot.as_mut().expect("built or already matching"))
}

impl Session {
    fn build(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        codec: Codec,
        shape: StreamShape,
    ) -> Result<Session> {
        // One `DXGI_FORMAT` is one sample width for both planes. Mixed luma/chroma
        // depth has no surface this backend can allocate. Refused, not approximated.
        if shape.bit_depth_chroma_minus8 != shape.bit_depth_luma_minus8 {
            bail!(
                "luma is {}-bit and chroma is {}-bit; no DXGI decode format carries both",
                shape.bit_depth(),
                8 + shape.bit_depth_chroma_minus8
            );
        }
        // From the SPS, not the negotiated format latched at construction.
        let profile = pf_dxvadec::profile_for(codec, shape.chroma_format_idc, shape.bit_depth())
            .ok_or_else(|| {
                anyhow!(
                    "no DXVA profile for {codec:?} chroma_format_idc {} at {} bits",
                    shape.chroma_format_idc,
                    shape.bit_depth()
                )
            })?;
        profile_supported(video_device, profile)?;
        let guid = GUID::from_u128(profile.guid);
        // `DXGI_FORMAT` is a type alias here; the profile's raw code point is the format.
        let format = profile.dxgi_format as DXGI_FORMAT;
        let coded_width = shape.coded_width;
        let coded_height = shape.coded_height;
        // Surfaces aligned to the codec granule; the decoder is told the coded size.
        // libavcodec's split. They are not interchangeable: a driver may reject an
        // over-large `SampleHeight` or return a different config list.
        let aligned_width = pf_dxvadec::align_surface(coded_width, codec);
        let aligned_height = pf_dxvadec::align_surface(coded_height, codec);
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: guid,
            SampleWidth: coded_width,
            SampleHeight: coded_height,
            OutputFormat: format,
        };

        // Enumerate driver configs; `pick_config` (unit-tested) picks a short-format
        // one. Hand the driver's struct back untouched: re-synthesising from the three
        // fields selection reads would drop `Config*` members a driver may care about.

        // SAFETY: COM calls on the live video device with a borrowed local descriptor; the
        // count bounds the loop and each config is written into a local that outlives its
        // call.
        let configs: Vec<D3D11_VIDEO_DECODER_CONFIG> = unsafe {
            let count = video_device
                .GetVideoDecoderConfigCount(&desc)
                .context("GetVideoDecoderConfigCount")?;
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..count {
                let mut config = D3D11_VIDEO_DECODER_CONFIG::default();
                if video_device
                    .GetVideoDecoderConfig(&desc, i, &mut config)
                    .ok()
                    .is_ok()
                {
                    out.push(config);
                }
            }
            out
        };
        let facts: Vec<pf_dxvadec::ConfigFacts> = configs
            .iter()
            .map(|c| pf_dxvadec::ConfigFacts {
                bitstream_raw: c.ConfigBitstreamRaw,
                no_encryption: c.guidConfigBitstreamEncryption == GUID::zeroed(),
                min_render_target_buffers: c.ConfigMinRenderTargetBuffCount,
            })
            .collect();
        let index = pf_dxvadec::pick_config(codec, &facts).ok_or_else(|| {
            anyhow!(
                "{} offers no short-format ({}) decoder config among {} — this rung \
                 implements the short slice format only, and this adapter offers none",
                profile.name,
                pf_dxvadec::short_slice_config(codec),
                facts.len()
            )
        })?;
        let config = configs[index];

        // SAFETY: a COM call on the live video device over two borrowed local descriptors;
        // the returned decoder is owned by this `Session`.
        let decoder = unsafe { video_device.CreateVideoDecoder(&desc, &config) }
            .context("CreateVideoDecoder")?;

        let slots = pf_dxvadec::SlotMap::new(shape.max_dpb_frames);
        let pool_size =
            pf_dxvadec::pool_size(slots.capacity(), facts[index].min_render_target_buffers);

        // `PUNKTFUNK_DXVA_SHARED_POOL=1` shares the pool by NT handle; `=srv` also binds it
        // for sampling. A probe for a zero-copy pool: nothing imports it yet.
        let shared_pool = std::env::var("PUNKTFUNK_DXVA_SHARED_POOL").ok();
        let shared = (D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE) as u32;
        let (bind_extra, misc): (u32, u32) = match shared_pool.as_deref() {
            Some("1") => (0, shared),
            Some("srv") => (D3D11_BIND_SHADER_RESOURCE as u32, shared),
            _ => (0, 0),
        };
        let pool_desc = D3D11_TEXTURE2D_DESC {
            Width: aligned_width,
            Height: aligned_height,
            MipLevels: 1,
            ArraySize: pool_size,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: BIND_DECODER | bind_extra,
            CPUAccessFlags: 0,
            MiscFlags: misc,
        };
        let mut pool = None;
        // SAFETY: a `?`-checked `CreateTexture2D` on the live device, over a fully-initialized
        // stack descriptor and a live `Option` out-param.
        unsafe { device.CreateTexture2D(&pool_desc, None, Some(&mut pool)) }
            .ok()
            .context("create the D3D11VA decode surface pool")?;
        let pool: ID3D11Texture2D = pool.expect("CreateTexture2D succeeded");
        if misc != 0 {
            log_pool_share(&pool, shared_pool.as_deref().unwrap_or_default());
        }

        // One output view per array slice. `DecoderBeginFrame` targets the view;
        // `ArraySlice` is the DXVA surface index, so `views[i]` is DPB slot i.
        let mut views = Vec::with_capacity(pool_size as usize);
        for slice in 0..pool_size {
            let mut view_desc = D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC {
                DecodeProfile: guid,
                ViewDimension: D3D11_VDOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            view_desc.Anonymous.Texture2D.ArraySlice = slice;
            let mut view = None;
            // SAFETY: COM calls on the live video device with the pool texture just created
            // and a borrowed local descriptor; the out-param is checked before use.
            unsafe {
                video_device.CreateVideoDecoderOutputView(&pool, &view_desc, Some(&mut view))
            }
            .ok()
            .context("CreateVideoDecoderOutputView")?;
            views.push(view.expect("output view created"));
        }

        tracing::info!(
            profile = profile.name,
            coded_width,
            coded_height,
            aligned_width,
            aligned_height,
            bit_depth = shape.bit_depth(),
            chroma_format_idc = shape.chroma_format_idc,
            pool_size,
            dpb_slots = slots.capacity(),
            config_bitstream_raw = config.ConfigBitstreamRaw,
            "native D3D11VA decode session built"
        );
        Ok(Session {
            decoder,
            pool,
            views,
            slots,
            held: vec![None; pool_size as usize],
            shape,
            profile,
        })
    }
}

/// Probe log for a shared pool: whether the NT handle a presenter would import exists.
fn log_pool_share(pool: &ID3D11Texture2D, mode: &str) {
    let handle = pool
        .cast::<IDXGIResource1>()
        .context("pool lacks IDXGIResource1")
        .and_then(|r| {
            // SAFETY: a COM call on the live pool; the returned NT handle is closed below.
            unsafe {
                r.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE as u32,
                    None,
                )
            }
            .context("CreateSharedHandle")
        });
    match handle {
        Ok(h) => {
            // SAFETY: `h` was returned above and is closed exactly once here.
            unsafe {
                let _ = windows::Win32::handleapi::CloseHandle(h);
            }
            tracing::info!(mode, "D3D11VA decode pool shared by NT handle (probe)");
        }
        Err(e) => tracing::warn!(mode, error = %format!("{e:#}"),
            "D3D11VA decode pool not shareable (probe)"),
    }
}

/// What a busy `DecoderBeginFrame` waits for next, `elapsed` into its budget.
#[derive(Debug, PartialEq, Eq)]
enum BusyWait {
    Yield,
    Sleep(Duration),
    GiveUp,
}

fn busy_wait_after(elapsed: Duration) -> BusyWait {
    if elapsed >= BEGIN_FRAME_BUDGET {
        BusyWait::GiveUp
    } else if elapsed < BEGIN_FRAME_SPIN {
        BusyWait::Yield
    } else {
        BusyWait::Sleep(BEGIN_FRAME_SLEEP)
    }
}

/// `DecoderBeginFrame` with the `E_PENDING` retry: hardware busy, not a failure. Returns
/// the retry count and the time they cost, for the hand-off window log.
fn begin_frame(
    context: &ID3D11VideoContext,
    decoder: &ID3D11VideoDecoder,
    view: &ID3D11VideoDecoderOutputView,
) -> Result<(u32, Duration)> {
    let start = Instant::now();
    let mut retries = 0u32;
    loop {
        // SAFETY: a COM call on the live video context with the live decoder and output view;
        // the content-key arguments are the "no protected content" pair (size 0, null).
        let hr = unsafe { context.DecoderBeginFrame(decoder, view, 0, None) };
        if hr.0 != E_PENDING {
            return hr
                .ok()
                .with_context(|| format!("DecoderBeginFrame (after {retries} pending retries)"))
                .map(|()| (retries, start.elapsed()));
        }
        retries += 1;
        match busy_wait_after(start.elapsed()) {
            BusyWait::Yield => std::thread::yield_now(),
            BusyWait::Sleep(d) => std::thread::sleep(d),
            BusyWait::GiveUp => bail!(
                "DecoderBeginFrame stayed E_PENDING for {retries} attempts over {:?}",
                start.elapsed()
            ),
        }
    }
}

/// Map one decoder buffer, let `write` fill it, release it back.
///
/// Release is unconditional: a buffer left mapped wedges later `GetDecoderBuffer`
/// of the same type. Returns bytes written, for `DataSize`.
fn write_buffer(
    context: &ID3D11VideoContext,
    decoder: &ID3D11VideoDecoder,
    kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
    write: impl FnOnce(&mut [u8]) -> Result<usize>,
) -> Result<usize> {
    let mut size = 0u32;
    let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: a COM call on the live video context and decoder; both out-params are locals
    // that outlive the call, and neither is read before the HRESULT is checked.
    unsafe { context.GetDecoderBuffer(decoder, kind, &mut size, &mut ptr) }
        .ok()
        .with_context(|| format!("GetDecoderBuffer({kind:?})"))?;
    if ptr.is_null() {
        bail!("GetDecoderBuffer({kind:?}) returned a null mapping");
    }
    // SAFETY: `GetDecoderBuffer` succeeded and reported a non-null pointer to a mapping of
    // `size` bytes that the driver keeps valid until the matching `ReleaseDecoderBuffer`
    // below — which runs before this borrow can escape, because the slice is confined to
    // `write`'s call. Write-only, so uninitialized driver memory is never read; `u8` has no
    // alignment requirement, and a decoder buffer never approaches `isize::MAX`.
    let dst = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), size as usize) };
    let written = write(dst);
    // SAFETY: releases exactly the buffer mapped above, on the same live context and decoder.
    let released = unsafe { context.ReleaseDecoderBuffer(decoder, kind) };
    let written = written.with_context(|| format!("filling the {kind:?} decoder buffer"))?;
    released
        .ok()
        .with_context(|| format!("ReleaseDecoderBuffer({kind:?})"))?;
    Ok(written)
}

/// pf-dxvadec `BUFFER_*` as the windows-rs constant of the same name.
///
/// A match on the four constants, not a numeric cast: the code points are
/// asserted in `pf_dxvadec::descriptors`, and named constants here never assume
/// the Windows type's representation.
fn buffer_kind(code: u32) -> Result<D3D11_VIDEO_DECODER_BUFFER_TYPE> {
    Ok(match code {
        pf_dxvadec::BUFFER_PICTURE_PARAMETERS => D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS,
        pf_dxvadec::BUFFER_INVERSE_QUANTIZATION_MATRIX => {
            D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX
        }
        pf_dxvadec::BUFFER_SLICE_CONTROL => D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL,
        pf_dxvadec::BUFFER_BITSTREAM => D3D11_VIDEO_DECODER_BUFFER_BITSTREAM,
        other => bail!("unknown DXVA buffer type {other}"),
    })
}

/// Submission descriptor for one filled buffer.
///
/// `mb_count` is `NumMBsInBuffer` and is not uniformly 0. libavcodec writes
/// `mb_width * mb_height` on H.264 bitstream and slice-control; HEVC writes 0
/// on those two; AV1 writes 0 on all three. Pic params and Q matrices take 0.
fn buffer_desc(
    kind: D3D11_VIDEO_DECODER_BUFFER_TYPE,
    size: usize,
    mb_count: u32,
) -> D3D11_VIDEO_DECODER_BUFFER_DESC {
    D3D11_VIDEO_DECODER_BUFFER_DESC {
        BufferType: kind,
        DataSize: size as u32,
        NumMBsInBuffer: mb_count,
        ..Default::default()
    }
}

fn copy_into(dst: &mut [u8], src: &[u8]) -> Result<()> {
    if src.len() > dst.len() {
        bail!(
            "a {}-byte DXVA buffer does not fit the driver's {}-byte mapping",
            src.len(),
            dst.len()
        );
    }
    dst[..src.len()].copy_from_slice(src);
    Ok(())
}

#[cfg(test)]
mod busy_wait {
    use super::*;

    /// Spin first, 1 ms sleeps after, give up at the libavcodec budget.
    #[test]
    fn busy_wait_spins_then_sleeps_then_gives_up() {
        let us = Duration::from_micros;
        assert_eq!(busy_wait_after(Duration::ZERO), BusyWait::Yield);
        assert_eq!(busy_wait_after(BEGIN_FRAME_SPIN - us(1)), BusyWait::Yield);
        assert_eq!(
            busy_wait_after(BEGIN_FRAME_SPIN),
            BusyWait::Sleep(BEGIN_FRAME_SLEEP)
        );
        assert_eq!(
            busy_wait_after(BEGIN_FRAME_BUDGET - us(1)),
            BusyWait::Sleep(BEGIN_FRAME_SLEEP)
        );
        assert_eq!(busy_wait_after(BEGIN_FRAME_BUDGET), BusyWait::GiveUp);
    }
}

#[cfg(test)]
mod parity {
    //! Frame-hash parity against libavcodec software decode.
    //!
    //! `#[ignore]`d: needs a real D3D11 video device. On Windows:
    //! `cargo test -p pf-client-core --lib video_d3d11_native -- --ignored --nocapture`
    //! Pin a GPU with `PF_DXVA_ADAPTER=<adapter description substring>`.
    //!
    //! Hashes the decode surface before `VideoProcessorBlt`. Goldens are libavcodec's
    //! (same files as `pf-vkdecode`'s `gpu_parity`), not the FFmpeg D3D11VA rung.
    //! This rung presents at decode time and never consults display order; the harness
    //! reorders by `PicId` against the planner's output list. Hosts emit zero-reorder
    //! low-delay, so production does not see this; the vendored vectors do reorder.
    //!
    //! Crop at the aligned pool height: chroma starts at `RowPitch * texture_height`,
    //! not display height. Reading at display height smears the chroma plane.

    use std::collections::HashMap;

    use pf_dxvadec::H264Planner;
    use pf_dxvadec::H265Planner;
    use sha2::Digest;
    use windows::Win32::d3d11::ID3D11Resource;
    use windows::Win32::d3d11::D3D11_CPU_ACCESS_READ;
    use windows::Win32::d3d11::D3D11_MAPPED_SUBRESOURCE;
    use windows::Win32::d3d11::D3D11_MAP_READ;
    use windows::Win32::d3d11::D3D11_USAGE_STAGING;
    use windows::Win32::dxgi::CreateDXGIFactory1;
    use windows::Win32::dxgi::IDXGIFactory1;
    use windows::Win32::dxgi::DXGI_ADAPTER_DESC1;

    use super::*;

    const TEST_25FPS_H264: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    const TEST_25FPS_H265: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );

    /// libavcodec NV12 hashes. Same files as the Vulkan rung, not a copy.
    const GOLDENS_H264: &str = include_str!("../../pf-vkdecode/tests/data/test-25fps.nv12.sha256");

    /// Host low-delay H.264: `max_num_reorder_frames = 0` and DPB depth equal to the
    /// reference count, so sliding-window unmark and eviction land in one AU. The
    /// vendored vector cannot reach that shape. Provenance is in the golden header.
    const LOWDELAY_H264: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h264");
    const GOLDENS_LOWDELAY: &str =
        include_str!("../../pf-vkdecode/tests/data/lowdelay-640x480.nv12.sha256");
    const LOWDELAY_FRAME_COUNT: usize = 120;
    const GOLDENS_H265: &str =
        include_str!("../../pf-vkdecode/tests/data/test-25fps-h265.nv12.sha256");

    /// Host low-delay HEVC. `H265Planner` snapshots `dpb_refs` after `decode_rps`,
    /// so `plan_to_dxva_h265` still releases inline. Provenance is in the golden header.
    const LOWDELAY_H265: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h265");
    const GOLDENS_LOWDELAY_H265: &str =
        include_str!("../../pf-vkdecode/tests/data/lowdelay-640x480-h265.nv12.sha256");

    /// Separate from [`LOWDELAY_FRAME_COUNT`]: two encoder runs; a length change
    /// must fail on its own leg.
    const LOWDELAY_H265_FRAME_COUNT: usize = 120;

    const FRAME_COUNT: usize = 250;

    /// Main 10: 50 frames of 320x240 HEVC 4:2:0, hashed as tightly packed P010.
    /// Provenance and why P010 (not `yuv420p10le`) are in the golden header.
    const TEST_MAIN10_H265: &[u8] = include_bytes!("../../pf-vkdecode/tests/data/test-main10.h265");
    const GOLDENS_MAIN10: &str =
        include_str!("../../pf-vkdecode/tests/data/test-main10.p010.sha256");
    const MAIN10_FRAME_COUNT: usize = 50;

    /// Vendored AV1 vector — IVF, not an elementary stream. Same file as `pf-vkdecode`.
    const TEST_25FPS_AV1: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// libavcodec per-delivered-frame NV12 hashes for the AV1 vector (320x240).
    const GOLDENS_AV1: &str =
        include_str!("../../pf-vkdecode/tests/data/test-25fps-av1.nv12.sha256");

    /// 250 temporal units, 274 decoded frames, 250 shown. The 24 hidden pictures
    /// are why the AV1 leg is not a third copy of the other two.
    const AV1_UNIT_COUNT: usize = 250;
    const AV1_DECODED_COUNT: usize = 274;
    const AV1_SHOWN_COUNT: usize = 250;

    const DISPLAY_AV1: (u32, u32) = (320, 240);

    /// Host AV1 at 4K: the only resolution this encoder emits more than one tile
    /// (`tile_cols = 1, tile_rows = 2`, both in one Tile Group OBU). A file fixture
    /// is not the wire path — it covers decode, not fragmentation or reassembly.
    const LOWDELAY_AV1: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-3840x2160.ivf.av1");
    const GOLDENS_LOWDELAY_AV1: &str =
        include_str!("../../pf-vkdecode/tests/data/lowdelay-3840x2160-av1.nv12.sha256");

    /// Three counts, never derived from each other. This host emits one shown
    /// frame per unit, the opposite of the vendored 250 / 274 / 250.
    const LOWDELAY_AV1_UNIT_COUNT: usize = 60;
    const LOWDELAY_AV1_DECODED_COUNT: usize = 60;
    const LOWDELAY_AV1_SHOWN_COUNT: usize = 60;
    const DISPLAY_LOWDELAY_AV1: (u32, u32) = (3840, 2160);

    fn golden_hashes(file: &'static str) -> Vec<&'static str> {
        file.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    fn sha256_hex(data: &[u8]) -> String {
        use std::fmt::Write as _;
        sha2::Sha256::digest(data)
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// Byte offsets of every Annex-B NAL header. Emulation prevention means
    /// `00 00 01` cannot appear inside a payload. Hand-rolled: `pf-client-core`
    /// does not depend on the vendored parser; AU-count asserts keep it honest.
    fn nal_headers(stream: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] == [0x00, 0x00, 0x01] {
                out.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    /// Split into access units given `(is_slice, starts_a_picture)`. A new AU
    /// begins at a non-VCL after slices, or at a first-of-picture slice when the
    /// current AU already has slices — pf-bitstream's rule, once for both codecs.
    fn split_aus(stream: &[u8], classify: impl Fn(&[u8], usize) -> (bool, bool)) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let mut au_start = 0usize;
        let mut au_has_slice = false;
        for header in nal_headers(stream) {
            let (is_slice, first_in_picture) = classify(stream, header);
            // Start code owning this header: three bytes, plus the optional
            // leading zero of the four-byte form.
            let mut start = header - 3;
            if start > 0 && stream[start - 1] == 0x00 {
                start -= 1;
            }
            if au_has_slice && (!is_slice || first_in_picture) {
                aus.push(&stream[au_start..start]);
                au_start = start;
                au_has_slice = false;
            }
            au_has_slice |= is_slice;
        }
        aus.push(&stream[au_start..]);
        aus
    }

    /// One-byte NAL header; `nal_unit_type` in the low 5 bits (1 = non-IDR, 5 = IDR).
    /// `first_mb_in_slice == 0` is the top bit of the next byte.
    fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = matches!(s[h] & 0x1f, 1 | 5);
            let first = is_slice && s.get(h + 1).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// Two-byte NAL header; `nal_unit_type` in bits 1..7 of the first byte.
    /// Slice if type `< 32`; `first_slice_segment_in_pic_flag` is the top bit at `+2`.
    fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = (s[h] >> 1) & 0x3f < 32;
            let first = is_slice && s.get(h + 2).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// IVF frames in file order: 32-byte `DKIF` header, then `[u32 size][u64 pts][size]`.
    /// Hand-rolled for the same reason as `nal_headers`; unit-count asserts keep it honest.
    fn split_ivf(stream: &[u8]) -> Vec<&[u8]> {
        assert_eq!(
            &stream[0..4],
            b"DKIF",
            "the vendored AV1 vector must be an IVF file"
        );
        let header = usize::from(u16::from_le_bytes([stream[6], stream[7]]));
        let mut out = Vec::new();
        let mut at = header;
        while at + 12 <= stream.len() {
            let size = u32::from_le_bytes(
                stream[at..at + 4]
                    .try_into()
                    .expect("four bytes make a u32"),
            ) as usize;
            at += 12;
            assert!(
                at + size <= stream.len(),
                "an IVF frame header claims {size} bytes past the end of the file"
            );
            out.push(&stream[at..at + size]);
            at += size;
        }
        out
    }

    /// Decode order and display order as `PicId`s, from a planner run alongside the
    /// decoder. The planner is deterministic, so these ids match the rung's.
    struct Order {
        /// One id per decoded picture, in submission order. One per AU on H.264/H.265;
        /// one per frame on AV1, where a unit can carry more than one.
        decode: Vec<u64>,
        /// Same ids in the planner's output (bumping) order, flush included.
        display: Vec<u64>,
        /// Ids each access unit decodes. AV1 only: the production entry takes whole
        /// temporal units, so the harness needs this without a test accessor. Empty
        /// on H.264/H.265, where [`Order::decode`] is already one id per unit.
        per_unit: Vec<Vec<u64>>,
    }

    fn order_h264(aus: &[&[u8]]) -> Order {
        let mut planner = H264Planner::new();
        let mut order = Order {
            decode: Vec::new(),
            display: Vec::new(),
            per_unit: Vec::new(),
        };
        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            assert_eq!(
                (plan.picture.display_crop.x, plan.picture.display_crop.y),
                (0, 0),
                "AU {index}: this rung hands the blit a size and no origin, so a \
                 non-zero conformance-window offset would be cropped from the wrong \
                 corner — by the rung, not just by this harness"
            );
            order.decode.push(
                plan.dpb.stored.unwrap_or_else(|| {
                    panic!("AU {index}: every picture of this vector is stored")
                }),
            );
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.display.extend(planner.flush().outputs);
        order
    }

    fn order_h265(aus: &[&[u8]]) -> Order {
        let mut planner = H265Planner::new();
        let mut order = Order {
            decode: Vec::new(),
            display: Vec::new(),
            per_unit: Vec::new(),
        };
        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            assert_eq!(
                (plan.picture.display_crop.x, plan.picture.display_crop.y),
                (0, 0),
                "AU {index}: a non-zero conformance-window offset is cropped from the \
                 wrong corner by this rung"
            );
            order.decode.push(
                plan.dpb.stored.unwrap_or_else(|| {
                    panic!("AU {index}: every picture of this vector is stored")
                }),
            );
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.display.extend(planner.flush().outputs);
        order
    }

    /// AV1 decode and display orders. One decoded picture per frame, not per unit.
    /// `display` is the planner's output list; AV1 has no bumping, so no flush.
    fn order_av1(units: &[&[u8]], render: (u32, u32)) -> Order {
        let mut planner = pf_dxvadec::Av1Planner::new();
        let mut order = Order {
            decode: Vec::new(),
            display: Vec::new(),
            per_unit: Vec::new(),
        };
        for (index, unit) in units.iter().enumerate() {
            let plans = planner
                .plan_au(unit)
                .unwrap_or_else(|e| panic!("unit {index}: the clean vector must plan, got {e:?}"));
            let mut this_unit = Vec::new();
            for plan in &plans {
                assert!(
                    plan.warnings.is_empty(),
                    "unit {index}: a clean vector must plan without warnings, got {:?}",
                    plan.warnings
                );
                assert_eq!(
                    (plan.picture.render_width, plan.picture.render_height),
                    render,
                    "unit {index}: the goldens are the {render:?} render region"
                );
                if let Some(id) = plan.dpb.stored {
                    order.decode.push(id);
                    this_unit.push(id);
                }
                order.display.extend(plan.dpb.outputs.iter().copied());
            }
            order.per_unit.push(this_unit);
        }
        order
    }

    /// LUID of the adapter whose description contains `PF_DXVA_ADAPTER`. Prints
    /// every enumerated description so a run always says which GPU answered.
    fn pinned_adapter() -> Option<[u8; 8]> {
        let want = std::env::var("PF_DXVA_ADAPTER").ok();
        // SAFETY: DXGI factory creation takes no pointer and returns an owned factory
        // or an error; the `Ok` binding is what proves one came back.
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            eprintln!("adapters: CreateDXGIFactory1 failed");
            return None;
        };
        let mut chosen = None;
        for i in 0.. {
            // SAFETY: a COM call on the live factory; `Ok` proves an adapter came back.
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
                break;
            };
            // SAFETY: `DXGI_ADAPTER_DESC1` is plain-old-data, so all-zeroes is valid.
            let mut desc: DXGI_ADAPTER_DESC1 = unsafe { std::mem::zeroed() };
            // SAFETY: a COM call on the adapter just enumerated, filling the zeroed
            // local through the out-param; checked before the descriptor is read.
            if unsafe { adapter.GetDesc1(&mut desc) }.is_err() {
                continue;
            }
            let end = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            let name = String::from_utf16_lossy(&desc.Description[..end]);
            let mut luid = [0u8; 8];
            luid[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
            luid[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
            let hit = want
                .as_deref()
                .is_some_and(|w| name.to_lowercase().contains(&w.to_lowercase()));
            eprintln!(
                "adapter {i}: {name}{}",
                if hit { "  <= pinned" } else { "" }
            );
            if hit && chosen.is_none() {
                chosen = Some(luid);
            }
        }
        if want.is_some() && chosen.is_none() {
            panic!("PF_DXVA_ADAPTER matched no adapter (see the list above)");
        }
        chosen
    }

    /// GPU→CPU readback of one decode-pool slice, cropped to `display` and packed
    /// tightly as NV12/P010 — the layout the goldens hash.
    struct Readback {
        ctx: ID3D11DeviceContext,
        staging: Option<ID3D11Texture2D>,
    }

    impl Readback {
        fn read(
            &mut self,
            device: &ID3D11Device,
            pool: &ID3D11Texture2D,
            slice: u32,
            display: (u32, u32),
        ) -> Vec<u8> {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: `GetDesc` fills a plain-old-data descriptor through an out-param
            // on a live texture and returns nothing to check.
            unsafe { pool.GetDesc(&mut desc) };

            if self.staging.is_none() {
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: desc.Width,
                    Height: desc.Height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: desc.Format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ as u32,
                    MiscFlags: 0,
                };
                let mut t: Option<ID3D11Texture2D> = None;
                // SAFETY: one `?`-checked call on the live device over a fully
                // initialised stack descriptor and a live `Option` out-param.
                unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut t)) }
                    .ok()
                    .expect("create the readback staging texture");
                self.staging = t;
            }
            let staging = self.staging.clone().expect("staging texture");

            let (width, height) = display;
            assert!(
                width <= desc.Width && height <= desc.Height,
                "the display region {width}x{height} does not fit the {}x{} pool surface",
                desc.Width,
                desc.Height
            );
            let ten_bit = desc.Format == pf_dxvadec::DXGI_FORMAT_P010;
            let bytes_per_sample = if ten_bit { 2 } else { 1 };
            let row_bytes = width as usize * bytes_per_sample;

            // SAFETY: `src` and `dst` are the same device's textures of identical
            // format and dimensions, so the single-subresource copy on the immediate
            // context is valid; `slice` is the array slice the decoder just wrote and
            // `MipLevels == 1` makes it the subresource index. `Map(D3D11_MAP_READ)`
            // on a STAGING texture blocks until that copy has retired and yields
            // `pData` valid for the whole resource: for NV12/P010 the luma plane is
            // `desc.Height` rows at `RowPitch` and the chroma plane follows at byte
            // offset `RowPitch * desc.Height`, so `total` below is exactly the mapped
            // extent and every sub-slice read is inside it. `Unmap` pairs the `Map`.
            unsafe {
                let src: ID3D11Resource = pool.cast().expect("pool -> resource");
                let dst: ID3D11Resource = staging.cast().expect("staging -> resource");
                self.ctx
                    .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, slice, None);
                let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                self.ctx
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                    .ok()
                    .expect("Map the readback staging texture");
                let pitch = map.RowPitch as usize;
                let aligned_h = desc.Height as usize;
                let total = pitch * (aligned_h + aligned_h.div_ceil(2));
                let mapped = std::slice::from_raw_parts(map.pData as *const u8, total);
                // Chroma starts at the aligned height, never the display height —
                // the pool surface is taller than the picture.
                let chroma_off = pitch * aligned_h;
                let mut out = Vec::with_capacity(row_bytes * (height as usize).div_ceil(2) * 3);
                for y in 0..height as usize {
                    out.extend_from_slice(&mapped[y * pitch..y * pitch + row_bytes]);
                }
                for y in 0..(height as usize).div_ceil(2) {
                    let row = chroma_off + y * pitch;
                    out.extend_from_slice(&mapped[row..row + row_bytes]);
                }
                self.ctx.Unmap(&staging, 0);
                out
            }
        }
    }

    /// Decode `aus` through a real `NativeD3d11Decoder` and compare the planner's
    /// display order against libavcodec's goldens.
    fn parity_run(
        codec: Codec,
        stream: StreamFormat,
        aus: &[&[u8]],
        order: &Order,
        goldens: &[&str],
        expected_aus: usize,
        label: &str,
    ) {
        assert_eq!(
            aus.len(),
            expected_aus,
            "{label}: the vector must split into {expected_aus} access units — a \
             different count means this file's splitter disagrees with pf-bitstream's, \
             and nothing below it is meaningful"
        );
        assert_eq!(
            order.display.len(),
            goldens.len(),
            "{label}: the planner outputs {} pictures, the goldens carry {}",
            order.display.len(),
            goldens.len()
        );

        let luid = pinned_adapter();
        let mut decoder = NativeD3d11Decoder::new(codec, stream, luid, false)
            .unwrap_or_else(|e| panic!("{label}: the box must host this profile — {e:#}"));
        let mut readback = Readback {
            ctx: decoder.context.clone(),
            staging: None,
        };

        let mut by_id: HashMap<u64, String> = HashMap::new();
        for (index, au) in aus.iter().enumerate() {
            let sub = decoder
                .plan(au)
                .unwrap_or_else(|e| panic!("AU {index}: plan failed — {e:#}"))
                .unwrap_or_else(|| panic!("AU {index}: this vector has no skipped pictures"));
            assert!(
                !sub.concealed,
                "AU {index}: a clean vector must need no concealment"
            );
            let display = (sub.facts.width, sub.facts.height);
            let slice = u32::from(sub.setup_slot);
            decoder
                .submit(au, &sub)
                .unwrap_or_else(|e| panic!("AU {index}: submit failed — {e:#}"));
            // This harness drives `plan` + `submit`, so it must apply the deferred
            // releases `decode` would. On a low-delay stream nearly every AU defers;
            // dropping them exhausts the ledger within DPB depth.
            decoder.release_deferred(&sub);
            let session = decoder.session.as_ref().expect("submit built a session");
            let pool = session.pool.clone();
            let bytes = readback.read(&decoder.device, &pool, slice, display);
            by_id.insert(order.decode[index], sha256_hex(&bytes));
        }

        let mut mismatches = 0usize;
        for (n, (id, golden)) in order.display.iter().zip(goldens.iter()).enumerate() {
            let got = by_id
                .get(id)
                .unwrap_or_else(|| panic!("display frame {n} names PicId {id}, never decoded"));
            if got != golden {
                if mismatches < 10 {
                    eprintln!("{label}: display frame {n} (PicId {id}): {got} != {golden}");
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches,
            0,
            "{label}: {mismatches}/{} frames diverge from libavcodec (first 10 above; \
             frame 0 is intra-only — if IT mismatches suspect the readback geometry \
             (pitch/crop/plane offset) rather than the decode)",
            goldens.len()
        );
        eprintln!(
            "{label}: {} frames bit-identical to libavcodec software decode",
            goldens.len()
        );
    }

    /// AV1 leg of [`parity_run`]. A temporal unit is not a picture.
    ///
    /// Drives [`NativeD3d11Decoder::decode_av1`] (the production entry) so the unit
    /// loop, slot-map bookkeeping, `show` suppression, and [`Session::held`] run.
    /// Every decoded surface is hashed (including hidden); comparison walks the
    /// planner's output list, so hidden pictures are never looked up — a golden of
    /// what libavcodec delivers cannot contain them. Hidden-frame pixels are
    /// reached through production state (`per_unit` / slot map / `held`), the same
    /// pair `show_existing_frame` reads.
    ///
    /// The vendored vector has no `show_existing_frame`; that path stays unexercised.
    fn av1_parity_run(units: &[&[u8]], order: &Order, goldens: &[&str]) {
        av1_parity_run_against(
            units,
            order,
            goldens,
            AV1_UNIT_COUNT,
            AV1_DECODED_COUNT,
            AV1_SHOWN_COUNT,
            "AV1",
        );
    }

    /// [`av1_parity_run`] with caller-supplied counts. The three are parameters,
    /// never derived: a harness that computed "hidden = 0" from either stream
    /// would silently stop checking the other.
    fn av1_parity_run_against(
        units: &[&[u8]],
        order: &Order,
        goldens: &[&str],
        unit_count: usize,
        decoded_count: usize,
        shown_count: usize,
        label: &str,
    ) {
        assert_eq!(
            units.len(),
            unit_count,
            "{label}: the IVF reader disagrees with the stream's temporal-unit count"
        );
        assert_eq!(order.decode.len(), decoded_count);
        assert_eq!(order.per_unit.len(), units.len());
        assert_eq!(order.display.len(), goldens.len());

        let luid = pinned_adapter();
        let mut decoder = NativeD3d11Decoder::new(Codec::Av1, StreamFormat::SDR_420_8, luid, false)
            .unwrap_or_else(|e| panic!("{label}: the box must host AV1 Profile 0 — {e:#}"));
        let mut readback = Readback {
            ctx: decoder.context.clone(),
            staging: None,
        };

        let mut by_id: HashMap<u64, String> = HashMap::new();
        let mut decoded = 0usize;
        let mut presented = 0usize;
        for (index, unit) in units.iter().enumerate() {
            let frame = decoder
                .decode_av1(unit)
                .unwrap_or_else(|e| panic!("unit {index}: decode failed — {e:#}"));
            if frame.is_some() {
                presented += 1;
            }

            // Everything the unit decoded, including withheld pictures.
            // Cannot hash `frame` — that is only the shown one.
            for &id in &order.per_unit[index] {
                let (slot, facts, pool) = {
                    let session = decoder
                        .session
                        .as_ref()
                        .expect("the first unit built a session");
                    let slot = session.slots.slot_of(id).unwrap_or_else(|| {
                        panic!("unit {index}: picture {id} holds no surface after its own unit")
                    });
                    let facts = session.held[usize::from(slot)].unwrap_or_else(|| {
                        panic!(
                            "unit {index}: surface {slot} holds picture {id} and no facts — \
                             `show_existing_frame` would have nothing to blit"
                        )
                    });
                    (slot, facts, session.pool.clone())
                };
                let bytes = readback.read(
                    &decoder.device,
                    &pool,
                    u32::from(slot),
                    (facts.width, facts.height),
                );
                by_id.insert(id, sha256_hex(&bytes));
                decoded += 1;
            }
        }
        assert_eq!(decoded, decoded_count);
        assert_eq!(
            presented, shown_count,
            "{label}: every unit of this stream shows exactly one frame, so the \
             production path must have handed back {shown_count} pictures"
        );
        let hidden = decoded_count - presented;
        assert_eq!(
            hidden,
            decoded_count - shown_count,
            "{label}: the rung must have decoded {} frames it never handed back — this \
             counts what `decode_av1` RETURNED against what it decoded, so a mismatch \
             on the vendored vector means the `!sub.show` suppression is not working \
             (or it stopped hiding frames, which `the_av1_vector_hides_frames…` would \
             catch first). On a stream with no hidden frames both sides are zero and \
             this is a tautology — deliberately, so one harness serves both shapes",
            decoded_count - shown_count
        );

        let mut mismatches = 0usize;
        for (n, (id, golden)) in order.display.iter().zip(goldens.iter()).enumerate() {
            let got = by_id
                .get(id)
                .unwrap_or_else(|| panic!("display frame {n} names PicId {id}, never decoded"));
            if got != golden {
                if mismatches < 10 {
                    eprintln!("{label}: display frame {n} (PicId {id}): {got} != {golden}");
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches,
            0,
            "{label}: {mismatches}/{} frames diverge from libavcodec (first 10 above; frame \
             0 is a key frame — if IT mismatches suspect the readback geometry \
             (pitch/crop/plane offset) or the tile records rather than the reference \
             handling)",
            goldens.len()
        );
        eprintln!(
            "{label}: {} delivered frames bit-identical to libavcodec, {hidden} hidden \
             frames decoded and withheld",
            goldens.len()
        );
    }

    /// Diagnostic: one line per display frame, verdict beside plan facts.
    /// Asserts nothing. Set `PF_AV1_DUMP=<tag>` to write a few frames' raw NV12
    /// to the temp directory (surfaces are recycled, so capture inside the loop).
    #[test]
    #[ignore = "diagnostic, needs a Windows D3D11 video device (see module docs)"]
    fn av1_divergence_map() {
        let units = split_ivf(TEST_25FPS_AV1);
        let order = order_av1(&units, DISPLAY_AV1);
        let goldens = golden_hashes(GOLDENS_AV1);

        let mut facts: HashMap<u64, String> = HashMap::new();
        let mut hidden: std::collections::HashSet<u64> = std::collections::HashSet::new();
        {
            let mut planner = pf_dxvadec::Av1Planner::new();
            for unit in &units {
                for plan in planner.plan_au(unit).expect("the clean vector plans") {
                    let Some(id) = plan.dpb.stored else { continue };
                    let h = &*plan.header;
                    if !h.show_frame {
                        hidden.insert(id);
                    }
                    let mut refs = String::new();
                    for r in plan.refs.iter() {
                        match r {
                            Some(r) => {
                                refs.push_str(&format!("{}/{} ", r.slot, r.id));
                            }
                            None => refs.push_str("-/- "),
                        }
                    }
                    facts.insert(
                        id,
                        format!(
                            "ft={} show={} oh={:3} pri={} refresh={:#06x} grain={} seg={} \
                             sr={} warp={} refmvs={} skip={} refsel={} tiles={}x{} \
                             lf={:?} lfsharp={} lfdelta={}{} refd={:?} moded={:?} \
                             cdefbits={} lr={:?} refs=[{}]",
                            h.frame_type as u8,
                            u8::from(h.show_frame),
                            h.order_hint,
                            h.primary_ref_frame,
                            h.refresh_frame_flags,
                            u8::from(h.film_grain_params.apply_grain),
                            u8::from(h.segmentation_params.segmentation_enabled),
                            u8::from(h.use_superres),
                            u8::from(h.allow_warped_motion),
                            u8::from(h.use_ref_frame_mvs),
                            u8::from(h.skip_mode_present),
                            u8::from(h.reference_select),
                            h.tile_info.tile_cols,
                            h.tile_info.tile_rows,
                            h.loop_filter_params.loop_filter_level,
                            h.loop_filter_params.loop_filter_sharpness,
                            u8::from(h.loop_filter_params.loop_filter_delta_enabled),
                            u8::from(h.loop_filter_params.loop_filter_delta_update),
                            h.loop_filter_params.loop_filter_ref_deltas,
                            h.loop_filter_params.loop_filter_mode_deltas,
                            h.cdef_params.cdef_bits,
                            h.loop_restoration_params.frame_restoration_type,
                            refs.trim_end(),
                        ),
                    );
                }
            }
        }

        let luid = pinned_adapter();
        let mut decoder = NativeD3d11Decoder::new(Codec::Av1, StreamFormat::SDR_420_8, luid, false)
            .expect("the box must host AV1 Profile 0");
        let mut readback = Readback {
            ctx: decoder.context.clone(),
            staging: None,
        };
        // Surfaces are recycled; capture dumps inside the loop or early pictures
        // are gone by the end of the run.
        let dump_tag = std::env::var("PF_AV1_DUMP").ok();
        let wanted: Vec<u64> = if dump_tag.is_some() {
            [3usize, 4, 10, 63, 64]
                .iter()
                .filter_map(|&n| order.display.get(n).copied())
                .collect()
        } else {
            Vec::new()
        };
        let mut by_id: HashMap<u64, String> = HashMap::new();
        for (index, unit) in units.iter().enumerate() {
            decoder.decode_av1(unit).expect("decode");
            for &id in &order.per_unit[index] {
                let (slot, f, pool) = {
                    let session = decoder.session.as_ref().expect("session");
                    let slot = session.slots.slot_of(id).expect("slot");
                    let f = session.held[usize::from(slot)].expect("facts");
                    (slot, f, session.pool.clone())
                };
                let bytes =
                    readback.read(&decoder.device, &pool, u32::from(slot), (f.width, f.height));
                if wanted.contains(&id) {
                    let tag = dump_tag.as_deref().unwrap_or("x");
                    let path = std::env::temp_dir().join(format!("pf-nv12-{tag}-pic{id}.bin"));
                    std::fs::write(&path, &bytes).expect("write the dump");
                    eprintln!("dumped pic {id} -> {}", path.display());
                }
                by_id.insert(id, sha256_hex(&bytes));
            }
        }

        eprintln!("=== MAP BEGIN ===");
        for (n, (id, golden)) in order.display.iter().zip(goldens.iter()).enumerate() {
            let got = by_id.get(id).expect("decoded");
            eprintln!(
                "disp {n:3} pic {id:3} {} | {}",
                if got == golden { "OK " } else { "BAD" },
                facts.get(id).map(String::as_str).unwrap_or("?")
            );
        }
        eprintln!("=== HIDDEN ===");
        let mut h: Vec<u64> = hidden.into_iter().collect();
        h.sort_unstable();
        for id in h {
            eprintln!(
                "hidden  pic {id:3}     | {}",
                facts.get(&id).map(String::as_str).unwrap_or("?")
            );
        }
        eprintln!("=== MAP END ===");
    }

    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn av1_every_delivered_frame_hashes_bit_identical_to_libavcodec() {
        let units = split_ivf(TEST_25FPS_AV1);
        let order = order_av1(&units, DISPLAY_AV1);
        av1_parity_run(&units, &order, &golden_hashes(GOLDENS_AV1));
    }

    /// Host AV1 at the only resolution that emits more than one tile (`tile_rows = 2`).
    /// The vendored vector is 1×1 tiles. A file, not the wire path; see [`LOWDELAY_AV1`].
    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn low_delay_host_av1_every_frame_hashes_bit_identical_to_libavcodec() {
        let units = split_ivf(LOWDELAY_AV1);
        let order = order_av1(&units, DISPLAY_LOWDELAY_AV1);
        av1_parity_run_against(
            &units,
            &order,
            &golden_hashes(GOLDENS_LOWDELAY_AV1),
            LOWDELAY_AV1_UNIT_COUNT,
            LOWDELAY_AV1_DECODED_COUNT,
            LOWDELAY_AV1_SHOWN_COUNT,
            "AV1 (low-delay host stream, 4K two-tile)",
        );
    }

    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h264_aus(TEST_25FPS_H264);
        let order = order_h264(&aus);
        parity_run(
            Codec::H264,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_H264),
            FRAME_COUNT,
            "H.264",
        );
    }

    /// Host low-delay H.264. The vendored vector cannot reach CurrPic/RefFrameList
    /// aliasing; this stream does. See [`LOWDELAY_H264`].
    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn low_delay_host_h264_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h264_aus(LOWDELAY_H264);
        let order = order_h264(&aus);
        parity_run(
            Codec::H264,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_LOWDELAY),
            LOWDELAY_FRAME_COUNT,
            "H.264 (low-delay host stream)",
        );
    }

    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn h265_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(TEST_25FPS_H265);
        let order = order_h265(&aus);
        parity_run(
            Codec::H265,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_H265),
            FRAME_COUNT,
            "H.265",
        );
    }

    /// Host low-delay HEVC. The vendored vector reorders, so it never puts an RPS
    /// drop and its eviction in one AU. This stream does. See [`LOWDELAY_H265`].
    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn low_delay_host_h265_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(LOWDELAY_H265);
        let order = order_h265(&aus);
        parity_run(
            Codec::H265,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_LOWDELAY_H265),
            LOWDELAY_H265_FRAME_COUNT,
            "H.265 (low-delay host stream)",
        );
    }

    /// Ten-bit path. D3D11VA has no per-picture status, so a Main10 stream decoding
    /// to garbage logs as clean as a correct one. P010 rows are `width * 2`; HEVC's
    /// 128-line granule pads 240 to 256, so chroma does not start at display height.
    #[test]
    #[ignore = "needs a Windows D3D11 video device (see module docs)"]
    fn main10_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(TEST_MAIN10_H265);
        let order = order_h265(&aus);
        parity_run(
            Codec::H265,
            StreamFormat {
                chroma_format_idc: 1,
                bit_depth: 10,
            },
            &aus,
            &order,
            &golden_hashes(GOLDENS_MAIN10),
            MAIN10_FRAME_COUNT,
            "HEVC Main 10",
        );
    }

    // CPU guards — not `#[ignore]`d. CI notices splitter/golden drift from pf-bitstream.

    #[test]
    fn the_local_splitter_agrees_with_the_planner_on_both_vectors() {
        let h264 = split_h264_aus(TEST_25FPS_H264);
        assert_eq!(h264.len(), FRAME_COUNT, "H.264 vector access units");
        let order = order_h264(&h264);
        assert_eq!(order.decode.len(), FRAME_COUNT);
        assert_eq!(
            order.display.len(),
            golden_hashes(GOLDENS_H264).len(),
            "the H.264 planner's output count must match the golden count"
        );

        let h265 = split_h265_aus(TEST_25FPS_H265);
        assert_eq!(h265.len(), FRAME_COUNT, "H.265 vector access units");
        let order = order_h265(&h265);
        assert_eq!(order.decode.len(), FRAME_COUNT);
        assert_eq!(
            order.display.len(),
            golden_hashes(GOLDENS_H265).len(),
            "the H.265 planner's output count must match the golden count"
        );
    }

    #[test]
    fn the_main10_vector_really_is_ten_bit() {
        let aus = split_h265_aus(TEST_MAIN10_H265);
        assert_eq!(
            aus.len(),
            MAIN10_FRAME_COUNT,
            "the Main 10 vector is {MAIN10_FRAME_COUNT} access units"
        );
        let order = order_h265(&aus);
        assert_eq!(
            order.display.len(),
            golden_hashes(GOLDENS_MAIN10).len(),
            "the planner's output count must match the Main 10 golden count"
        );

        // A regenerated 8-bit vector would make the Main10 GPU leg a second 8-bit
        // run under a ten-bit name, and it would pass if the goldens were regenerated too.
        let mut planner = H265Planner::new();
        let plan = planner
            .plan_au(aus[0])
            .expect("the Main 10 vector's first access unit must plan");
        assert_eq!(
            (
                plan.picture.chroma_format_idc,
                plan.picture.bit_depth_luma_minus8,
                plan.picture.bit_depth_chroma_minus8
            ),
            (1, 2, 2),
            "the Main 10 vector must be 4:2:0 at ten bits"
        );
        assert_eq!(
            (plan.picture.coded_width, plan.picture.coded_height),
            (320, 240),
            "the golden frame size is 320x240"
        );
    }

    #[test]
    fn the_ivf_reader_agrees_with_the_planner_and_the_av1_goldens() {
        let units = split_ivf(TEST_25FPS_AV1);
        assert_eq!(units.len(), AV1_UNIT_COUNT, "AV1 temporal units");
        let order = order_av1(&units, DISPLAY_AV1);
        assert_eq!(
            order.decode.len(),
            AV1_DECODED_COUNT,
            "the AV1 vector decodes 274 frames"
        );
        assert_eq!(
            order.display.len(),
            golden_hashes(GOLDENS_AV1).len(),
            "the AV1 planner's output count must match the golden count"
        );
        assert_eq!(order.display.len(), AV1_SHOWN_COUNT);
    }

    #[test]
    fn the_av1_vector_hides_frames_and_that_is_what_makes_this_leg_different() {
        // An AU is a temporal unit; 24 of these carry two frames and the extra is
        // never delivered. If a regenerated vector stopped, `av1_parity_run` would
        // still pass while proving nothing the H.264 leg does not.
        let units = split_ivf(TEST_25FPS_AV1);
        let mut planner = pf_dxvadec::Av1Planner::new();
        let (mut frames, mut multi_frame_units, mut shown) = (0usize, 0usize, 0usize);
        for unit in &units {
            let plans = planner.plan_au(unit).expect("the clean vector plans");
            if plans.len() > 1 {
                multi_frame_units += 1;
            }
            for plan in &plans {
                frames += 1;
                if plan.picture.show_frame {
                    shown += 1;
                }
                assert!(
                    plan.dpb.stored.is_some(),
                    "this vector uses no show_existing_frame"
                );
            }
        }
        assert_eq!(frames, AV1_DECODED_COUNT);
        assert_eq!(shown, AV1_SHOWN_COUNT);
        assert_eq!(
            multi_frame_units,
            AV1_DECODED_COUNT - AV1_SHOWN_COUNT,
            "24 units must carry a hidden frame as well as the shown one"
        );
    }

    #[test]
    fn both_vendored_vectors_really_do_reorder() {
        // The harness reorders because these vectors do. If they stop, hashing in
        // decode order would be simpler and the docs would be stale.
        for (name, order) in [
            ("H.264", order_h264(&split_h264_aus(TEST_25FPS_H264))),
            ("H.265", order_h265(&split_h265_aus(TEST_25FPS_H265))),
        ] {
            assert_ne!(
                order.decode, order.display,
                "{name}: this vector no longer reorders — the harness's PicId \
                 indirection is now unnecessary and its docs are wrong"
            );
        }
    }
}
