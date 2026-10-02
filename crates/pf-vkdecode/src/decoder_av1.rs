//! Native AV1 Vulkan Video decode over the `pf_bitstream` AV1 planner.
//!
//! Per temporal unit: `plan_au` (several frames) → `plan_to_vk_av1` →
//! tile-payload ring upload → record (barriers, `vkCmdBeginVideoCodingKHR`
//! with every bound DPB slot, one-shot session RESET, a caps-gated
//! `RESULT_STATUS_ONLY` query around `vkCmdDecodeVideoKHR`) → submit on the
//! decode queue under the caller's [`crate::QueueLock`] with a per-image timeline
//! signal. `show_existing_frame` settles DPB and output with no GPU submit.
//!
//! `referenceNameSlotIndices` are DPB slot indices, not `pReferenceSlots`
//! positions; inconsistent bindings fail closed. AV1 §7.20: a slot this frame
//! reads cannot become its target until the decode is recorded —
//! [`crate::DecodePlanVkAv1::release_after_decode`] delays that recycle. A named
//! reference with no image is fatal: latch recovery and wait for the next key
//! frame; never submit [`REFERENCE_NAME_UNUSED`].
//!
//! [`plan_bitstream`] uploads concatenated tile payloads only
//! (`frameHeaderOffset` 0). `pTileOffsets` / `pTileSizes` are 256-long
//! zero-tailed arrays (the driver reads the full arrays); `tileCount` is the
//! real count. Result-status queries only when the queue family advertises them.

use ash::vk;
use ash::vk::native as hh;
use pf_bitstream::av1::tiles::plan_bitstream;
use pf_bitstream::av1::tiles::Av1TileError;
use pf_bitstream::av1::AuPlan;
use pf_bitstream::av1::Av1Planner;
use pf_bitstream::av1::PlanWarning;
use pf_bitstream::av1::NUM_REF_SLOTS;
use pf_bitstream::h264::DisplayCrop;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::caps::derive_caps;
use crate::caps::query_caps;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps_av1::Av1ProfileKey;
use crate::decoder::core::session_extent;
use crate::decoder::core::PendingPic;
use crate::decoder::core::ScopeRef;
use crate::decoder::core::SessionState;
use crate::decoder::core::VkCodec;
use crate::decoder::core::VkDecoder;
use crate::decoder::DecodedVkFrame;
use crate::decoder::VkDecodeError;
use crate::device::DecodeDevice;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::pic_av1::plan_to_vk_av1;
use crate::pic_av1::VkRefAv1;
use crate::pic_av1::REFERENCE_NAME_UNUSED;
use crate::recovery::RecoveryMark;
use crate::ring::pack_av1_tiles;
use crate::ring::PackedAv1Tiles;
use crate::session_av1::ParamsActionAv1;
use crate::session_av1::SessionConfigAv1;
use crate::session_av1::VideoSessionAv1;

/// Eight `NUM_REF_FRAMES` plus the picture being decoded. Codec constant, not
/// an SPS field: an AV1 session never renegotiates DPB depth.
const REQUIRED_SLOTS: u32 = NUM_REF_SLOTS as u32 + 1;

/// 256: RADV reads that many entries regardless of `tileCount`.
pub(crate) const AV1_MAX_NUM_TILES: usize = 256;

/// Per-tile offsets/sizes. Arrays are 256 (what the driver reads); `count` is
/// the real tile count. ash's `tile_offsets()`/`tile_sizes()` set `tileCount`
/// from slice length, so a slice would fuse the two numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubmittedTiles {
    pub(crate) offsets: [u32; AV1_MAX_NUM_TILES],
    pub(crate) sizes: [u32; AV1_MAX_NUM_TILES],
    pub(crate) count: u32,
}

/// Packed-tile offsets/sizes as `u32`. A miss here is silent corruption: the
/// hardware would start mid-tile.
fn submitted_tiles(packed: &PackedAv1Tiles) -> Result<SubmittedTiles, Av1TileError> {
    if packed.segments.len() > AV1_MAX_NUM_TILES {
        return Err(Av1TileError::TooManyTiles {
            tiles: packed.segments.len(),
        });
    }
    let mut tiles = SubmittedTiles {
        offsets: [0; AV1_MAX_NUM_TILES],
        sizes: [0; AV1_MAX_NUM_TILES],
        count: packed.segments.len() as u32,
    };
    for (i, (segment, offset)) in packed.segments.iter().zip(&packed.offsets).enumerate() {
        let size = u32::try_from(segment.len()).map_err(|_| Av1TileError::Overflow)?;
        // Offset + size must not wrap: Vulkan would read past the packed buffer.
        offset.checked_add(size).ok_or(Av1TileError::Overflow)?;
        tiles.offsets[i] = *offset;
        tiles.sizes[i] = size;
    }
    Ok(tiles)
}

/// Always 0: the buffer holds tile payloads only. The driver takes the frame
/// header from `pStdPictureInfo`.
const FRAME_HEADER_OFFSET: u32 = 0;

/// `VkVideoDecodeAV1PictureInfoKHR` for the decode op.
///
/// Assign `tileCount` after both setters. ash's `tile_offsets()`/`tile_sizes()`
/// each set it from slice length; the arrays are 256 long, so a setter win would
/// tell the driver there are 256 tiles.
fn av1_picture_info<'a>(
    std_pic: &'a hh::StdVideoDecodeAV1PictureInfo,
    reference_name_slot_indices: [i32; pf_bitstream::av1::REFS_PER_FRAME],
    tiles: &'a SubmittedTiles,
) -> vk::VideoDecodeAV1PictureInfoKHR<'a> {
    let mut info = vk::VideoDecodeAV1PictureInfoKHR::default()
        .std_picture_info(std_pic)
        .reference_name_slot_indices(reference_name_slot_indices)
        .frame_header_offset(FRAME_HEADER_OFFSET)
        .tile_offsets(&tiles.offsets)
        .tile_sizes(&tiles.sizes);
    info.tile_count = tiles.count;
    info
}

/// Planner-reported missing or stale DPB reference: the picture the frame
/// predicts from is not there, so decoding it would only paint garbage. Does
/// not match [`PlanWarning::TruncatedAu`]: that is concealment the planner
/// already applied; refusing it would turn every clipped AU into a keyframe
/// request.
pub(crate) fn lost_reference(warnings: &[PlanWarning]) -> Option<(u8, u8)> {
    warnings.iter().find_map(|w| match w {
        PlanWarning::MissingReference { slot, ref_index }
        | PlanWarning::StaleReference { slot, ref_index } => Some((*slot, *ref_index)),
        _ => None,
    })
}

/// One planned frame: decoded, or skipped while waiting for a key. The caller
/// counts skips; a unit of only skips is [`VkDecodeError::AwaitingKeyAv1`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameOutcome {
    /// Submitted, or `show_existing_frame` settled without a submit.
    Decoded,
    SkippedAwaitingKey,
}

/// AV1 planner, caps, and stream state of a [`VkAv1Decoder`].
pub struct Av1 {
    planner: Av1Planner,
    /// Caps per profile key. Bit-depth or film-grain change is a new key.
    caps: Option<(Av1ProfileKey, DecodeCaps)>,
    /// Skip until the next decoded key. The planner has no `flush`, so after a
    /// recovery its store still names the emptied slots.
    /// Per-frame skip, per-AU error: [`VkDecodeError::AwaitingKeyAv1`].
    /// `Ok(None)` would reset the demotion streak.
    awaiting_key: bool,
    /// Over-declared-level warning, once per decoder (`ensure_state` runs per AU).
    level_advisory_warned: bool,
}

/// Native Vulkan Video AV1 decoder. A decoded AU is a temporal unit.
pub type VkAv1Decoder = VkDecoder<Av1>;

impl VkCodec for Av1 {
    type Session = VideoSessionAv1;
    type StdRef = hh::StdVideoDecodeAV1ReferenceInfo;
    type Warning = PlanWarning;
    type DpbSlotInfo<'a> = vk::VideoDecodeAV1DpbSlotInfoKHR<'a>;
    const LABEL: &'static str = "av1";

    fn dpb_slot_info(std: &Self::StdRef) -> Self::DpbSlotInfo<'_> {
        vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(std)
    }

    fn decode_au(
        dec: &mut VkDecoder<Self>,
        au: &[u8],
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        dec.decode_inner(au)
    }

    /// Discard pending pictures (teardown / discontinuity). AV1 has no reorder
    /// buffer: pending entries are hidden frames. Arms the key-frame wait — the
    /// planner has no `flush` and still names the emptied slots.
    fn flush(dec: &mut VkDecoder<Self>) {
        match &mut dec.state {
            Some(state) => {
                for (_, entry) in std::mem::take(&mut dec.pending) {
                    state.pool.pictures[entry.image].pending = false;
                }
            }
            None => dec.pending.clear(),
        }
        dec.reset_slot_ledgers();
        dec.codec.awaiting_key = true;
    }

    fn forgive_unclean(&mut self) {
        self.planner.forgive_unclean();
    }

    fn snapshot_flags(&self) -> &'static str {
        if self.awaiting_key {
            " awaiting=key"
        } else {
            ""
        }
    }
}

impl VkDecoder<Av1> {
    /// Wrap the borrowed device. Sessions/pools are built lazily from the first
    /// sequence header (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The [`DeviceHandles`] contract (liveness, extensions, features, truthful
    /// queue families) holds for this decoder's lifetime. `VK_KHR_video_decode_av1`
    /// must be enabled. The family `videoCodecOperations` check is the device's
    /// claim, not proof the client passed the extension to `vkCreateDevice`.
    /// Missing it is UB at session create, so the family check runs first.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        dev.require_codec_op(vk::VideoCodecOperationFlagsKHR::DECODE_AV1, "AV1 decode")?;
        let codec = Av1 {
            planner: Av1Planner::new(),
            caps: None,
            awaiting_key: false,
            level_advisory_warned: false,
        };
        Ok(Self::with_codec(dev, lock, codec))
    }

    /// Caps check before any AU. `film_grain` is part of the AV1 decode profile;
    /// missing it here is a construction failure, not a mid-stream error streak.
    ///
    /// Negotiated facts are a hint (the sequence header is authoritative). Extent,
    /// DPB depth, and a disagreeing header still fail at the first AU. Declared
    /// level is advisory; `ensure_state` only warns.
    pub fn probe_stream_support(
        &self,
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<(), VkDecodeError> {
        let key = Av1ProfileKey::from_negotiated(chroma_format_idc, bit_depth, film_grain)?;
        // SAFETY: the constructor `DeviceHandles` contract holds for this lifetime.
        let raw = unsafe { query_caps(&self.dev, DecodeProfile::Av1(key)) }
            .map_err(|r| caps_query_error(r, key))?;
        let wanted = key
            .output_format()
            .expect("from_negotiated gated the sampling/depth combination");
        derive_caps(&raw, wanted)?;
        Ok(())
    }

    /// One temporal unit: plan every frame, then decode, settle, or skip each.
    /// A `show_existing_frame` naming an empty slot is a warning and displays
    /// nothing.
    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        let plans = match self.codec.planner.plan_au(au) {
            Ok(plans) => plans,
            Err(e) => return Err(VkDecodeError::PlanAv1(e)),
        };
        // Concatenate per AU: one unit's frames share a concealment verdict.
        for plan in &plans {
            for warning in &plan.warnings {
                trace!(?warning, "plan warning");
            }
            self.last_warnings.extend(plan.warnings.iter().cloned());
        }

        let mut skipped = 0usize;
        for plan in &plans {
            // Planner store already holds this picture. Failure below can disagree
            // with the ledgers; latch recovery rather than leave them split.
            match self.decode_planned(plan, au) {
                Ok(FrameOutcome::Decoded) => {}
                Ok(FrameOutcome::SkippedAwaitingKey) => skipped += 1,
                Err(e) => {
                    self.recovery.latch();
                    return Err(e);
                }
            }
        }
        // Count skips; do not return at the first. A key may sit behind a skipped
        // frame in the same unit. Not a latch: recovery already ran.
        if whole_unit_skipped(plans.len(), skipped) {
            return Err(VkDecodeError::AwaitingKeyAv1);
        }
        Ok(self.ready.pop_front())
    }

    fn decode_planned(&mut self, plan: &AuPlan, au: &[u8]) -> Result<FrameOutcome, VkDecodeError> {
        // Only a decoded key clears the wait. `show_existing_frame` of a key
        // resets the planner store (7.20) but decodes nothing — empty ledger vs
        // full store, then the next inter fails and re-arms the wait.
        if self.codec.awaiting_key && clears_awaiting_key(plan) {
            debug!("AV1 key frame reached — decoding resumes");
            self.codec.awaiting_key = false;
        }
        if self.codec.awaiting_key {
            trace!(
                show_existing = plan.dpb.stored.is_none(),
                "frame skipped while awaiting the next AV1 key frame"
            );
            return Ok(FrameOutcome::SkippedAwaitingKey);
        }

        // `show_existing_frame`: settle DPB and output with no GPU submit.
        let Some(setup_id) = plan.dpb.stored else {
            self.settle(&plan.dpb.outputs, &plan.dpb.removed);
            if let Some(state) = &mut self.state {
                for &id in &plan.dpb.removed {
                    state.slots.release(id);
                }
            }
            return Ok(FrameOutcome::Decoded);
        };

        if let Some((slot, ref_index)) = lost_reference(&plan.warnings) {
            return Err(VkDecodeError::MissingReferenceAv1 { slot, ref_index });
        }

        // Stamp decode-order before anything can reorder it.
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;

        self.ensure_state(plan)?;

        // Recreate destroys an existing parameters object; drain first. The
        // session's first parameters create has nothing to destroy.
        {
            let session = &self.state.as_ref().expect("ensure_state built it").session;
            if session.parameters_action(&plan.sequence) == ParamsActionAv1::Recreate
                && session.has_parameters()
            {
                self.drain_gpu()?;
            }
        }
        let state = self.state.as_mut().expect("ensure_state built it");
        // SAFETY: live device; drain above satisfies Recreate; Current touches
        // nothing a submitted decode reads.
        unsafe { state.session.ensure_parameters(&plan.sequence)? };

        // Walk tiles before the DPB ledger: a malformed group must not half-apply.
        let bitstream =
            plan_bitstream(au, &plan.tiles, &plan.header).map_err(VkDecodeError::TilesAv1)?;

        let vk_plan = plan_to_vk_av1(plan, &mut state.slots).map_err(VkDecodeError::ConvertAv1)?;

        // Slots this frame still reads are held through convert/submit so setup
        // assignment cannot recycle them; the release runs either way.
        self.submit_then_release(&vk_plan.release_after_decode, |dec| {
            let op = dec.prepare_op(&vk_plan.refs, vk_plan.setup_slot, vk_plan.setup_ref)?;
            // Tile payloads only; AV1 has no start codes to strip.
            let Some(packed) = pack_av1_tiles(&bitstream.tiles) else {
                return Err(VkDecodeError::Unsupported(
                    "packed tile data exceeds the u32 offsets Vulkan submits".into(),
                ));
            };
            let tiles = submitted_tiles(&packed).map_err(VkDecodeError::TilesAv1)?;
            // SAFETY: the segments are in-bounds plan tile ranges; the recorded
            // tile offsets come from the same `pack_av1_tiles` call.
            let upload = unsafe { dec.upload(au, &packed.segments)? };
            let scope = dec.scope(&op, &vk_plan.refs)?;
            check_reference_names(&vk_plan.refs, &vk_plan.reference_name_slot_indices)?;
            let mut picture_info = av1_picture_info(
                vk_plan.pic.std(),
                vk_plan.reference_name_slot_indices,
                &tiles,
            );
            // SAFETY: `op`, `scope` and `upload` were just taken for this plan
            // on the current generation.
            unsafe { dec.record_and_submit(&op, &scope, &mut picture_info, &upload)? };
            dec.commit_op(&op, &upload, &vk_plan.refs);
            dec.pending.insert(
                vk_plan.setup_id,
                PendingPic {
                    image: op.dst,
                    submission: op.submission,
                    query_slot: op.query_index,
                    timeline_value: op.signal_value,
                    crop: DisplayCrop {
                        x: 0,
                        y: 0,
                        // Render size is a display hint (5.9.6 has no upper bound),
                        // not a window. Unclamped it would crop past the decoded image.
                        width: plan.picture.render_width.min(plan.picture.upscaled_width),
                        height: plan.picture.render_height.min(plan.picture.frame_height),
                    },
                    colour: plan.picture.colour,
                    // No POC. OrderHint wraps; it is not a monotone counter.
                    poc: plan.picture.order_hint as i32,
                    // Key is the only re-anchor (no IDR, no recovery-point SEI).
                    is_idr: plan.picture.is_key,
                    recovery: RecoveryMark::NONE,
                    decode_order,
                    references_clean: plan.picture.references_clean,
                },
            );
            Ok(())
        })?;

        self.settle(&plan.dpb.outputs, &plan.dpb.removed);

        // refresh_frame_flags == 0 never enters the planner store, so it is never
        // reported removed. The ledger still assigned a slot; leave it and nine
        // such frames hit `SlotError::Full`.
        if plan.header.refresh_frame_flags == 0 {
            let state = self.state.as_mut().expect("ensured above");
            state.slots.release(setup_id);
            // Not shown either: nothing can display or reference it.
            if let Some(entry) = self.pending.remove(&setup_id) {
                trace!(
                    id = setup_id,
                    "frame refreshes no slot and is not shown — freeing its image"
                );
                state.pool.pictures[entry.image].pending = false;
            }
        }
        Ok(FrameOutcome::Decoded)
    }

    /// Session/caps match this plan's extent and profile. A declared level above
    /// the device ceiling warns once and proceeds (`seq_level_idx` 31 is not a
    /// level).
    fn ensure_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        let key = profile_key_for(plan)?;
        if self.codec.caps.as_ref().map(|(k, _)| *k) != Some(key) {
            let wanted = key
                .output_format()
                .expect("from_stream gated the sampling/depth combination");
            // SAFETY: live device (constructor contract).
            let raw = unsafe { query_caps(&self.dev, DecodeProfile::Av1(key)) }
                .map_err(|r| caps_query_error(r, key))?;
            self.codec.caps = Some((key, derive_caps(&raw, wanted)?));
        }
        // Applied grain needs an output distinct from the reconstructed reference, DISTINCT
        // reported or not: an in-place device would grain its own references. Let the ladder demote.
        if key.film_grain && self.codec.caps.as_ref().is_some_and(|(_, c)| c.coincide) {
            return Err(VkDecodeError::Unsupported(
                "AV1 film grain needs an output picture apart from the reference, and this \
                 device decodes in place"
                    .into(),
            ));
        }
        // Declared level above maxLevel is not a refusal: extent and DPB depth
        // are the physical facts. `seq_level_idx` 31 is Annex A's "maximum
        // parameters", not a level; sequence headers carry none to the driver.
        let caps_max_level = self
            .codec
            .caps
            .as_ref()
            .expect("queried above")
            .1
            .max_level_idc;
        let stream_level = u32::from(stream_level_idx(plan));
        if stream_level > caps_max_level.code_point() && !self.codec.level_advisory_warned {
            self.codec.level_advisory_warned = true;
            warn!(
                stream_level,
                ceiling = %caps_max_level,
                "stream declares an AV1 level above the device ceiling — the declared \
                 level is advisory (seq_level_idx 31 means \"maximum parameters\", and \
                 encoders over-declare); proceeding, since the level never reaches the \
                 driver"
            );
        }
        let coded = coded_extent(plan);
        match &self.state {
            Some(state) if state.coded_extent == coded && state.session.config.profile == key => {
                Ok(())
            }
            _ => self.rebuild_state(plan),
        }
    }

    /// Retire the current generation ([`VkDecoder::retire_state`]) and build a
    /// fresh session. AV1 never renegotiates DPB depth: [`REQUIRED_SLOTS`].
    fn rebuild_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        self.retire_state()?;
        let (key, caps) = self.codec.caps.as_ref().expect("ensure_state queried caps");
        let key = *key;
        if REQUIRED_SLOTS > caps.max_dpb_slots {
            return Err(VkDecodeError::Unsupported(format!(
                "AV1 needs {REQUIRED_SLOTS} DPB slots, device caps at {}",
                caps.max_dpb_slots
            )));
        }
        let coded = coded_extent(plan);
        let image_extent = session_extent(caps, coded)?;
        let config = SessionConfigAv1 {
            max_coded_extent: image_extent,
            max_dpb_slots: REQUIRED_SLOTS,
            max_active_references: (REQUIRED_SLOTS - 1).min(caps.max_active_references),
            profile: key,
        };
        // SAFETY: live device per the constructor contract; the session is
        // owned by a Drop type the moment it exists.
        let state = unsafe {
            let session = VideoSessionAv1::create(&self.dev, caps, config)?;
            SessionState::create(
                self,
                caps,
                session,
                DecodeProfile::Av1(key),
                REQUIRED_SLOTS,
                coded,
                image_extent,
            )?
        };
        self.state = Some(state);
        Ok(())
    }
}

/// Name a failed caps query. Do not re-query with grain off: that would decode
/// the pictures and silently drop the grain the encoder relied on.
fn caps_query_error(r: vk::Result, key: Av1ProfileKey) -> VkDecodeError {
    if key.film_grain {
        VkDecodeError::Unsupported(format!(
            "AV1 decode capabilities query failed with {r:?}; this stream applies film \
             grain, and a device that cannot host the film-grain AV1 decode profile \
             fails exactly here — decoding it without grain is not offered"
        ))
    } else {
        VkDecodeError::from(r)
    }
}

fn profile_key_for(plan: &AuPlan) -> Result<Av1ProfileKey, VkDecodeError> {
    Av1ProfileKey::from_stream(
        plan.sequence.seq_profile as u8,
        plan.picture.chroma_format_idc,
        plan.picture.bit_depth,
        plan.sequence.film_grain_params_present,
    )
    .map_err(VkDecodeError::ParamsAv1)
}

/// Decoded key frame (references nothing, refreshes all eight slots).
/// `show_existing_frame` of a key resets the planner store (7.20) but decodes
/// nothing — empty ledger vs full store.
fn clears_awaiting_key(plan: &AuPlan) -> bool {
    plan.picture.is_key && plan.dpb.stored.is_some()
}

/// [`VkDecodeError::AwaitingKeyAv1`] when every planned frame was skipped.
/// `planned == 0` is `Ok(None)` (metadata-only AU). `skipped < planned` is not
/// a skip: a key may sit behind a skipped frame in the same unit.
fn whole_unit_skipped(planned: usize, skipped: usize) -> bool {
    planned > 0 && skipped == planned
}

/// Operating point 0 is the full stream (non-scalable default; hosts emit one).
fn stream_level_idx(plan: &AuPlan) -> u8 {
    plan.sequence.operating_points[0].seq_level_idx
}

/// Decode output extent: superres upscales after reconstruction, so pool images
/// hold `upscaled_width` × `frame_height`. One extent per session generation;
/// `ensure_state` rebuilds on change. Mid-sequence size override with scaled
/// refs is outside the envelope: rebuild + the key-frame wait (`Av1::awaiting_key`).
fn coded_extent(plan: &AuPlan) -> vk::Extent2D {
    vk::Extent2D {
        width: plan.picture.upscaled_width,
        height: plan.picture.frame_height,
    }
}

impl ScopeRef for VkRefAv1 {
    type Std = hh::StdVideoDecodeAV1ReferenceInfo;
    fn slot(&self) -> u8 {
        self.slot
    }
    fn std(&self) -> Self::Std {
        self.std
    }
}

/// Every non-negative `referenceNameSlotIndices` entry must equal some
/// `pReferenceSlots` `slotIndex`, i.e. one of `refs`, or the driver reads a
/// slot this op never bound. Runs after [`crate::decoder::core::build_scope`],
/// whose refs pass fails first on an unbound reference.
fn check_reference_names(refs: &[VkRefAv1], names: &[i32]) -> Result<(), VkDecodeError> {
    for name in names {
        if *name == REFERENCE_NAME_UNUSED {
            continue;
        }
        // Negative-but-not-UNUSED is not a slot. `u8::MAX` is past the nine-slot
        // ledger, so the refusal reads as "a name nothing binds".
        let Ok(slot) = u8::try_from(*name) else {
            return Err(VkDecodeError::UnboundReferenceSlot { slot: u8::MAX });
        };
        if !refs.iter().any(|r| r.slot == slot) {
            return Err(VkDecodeError::UnboundReferenceSlot { slot });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ops::Range;

    use ash::vk::Handle as _;
    use cros_codecs::bitstream_utils::IvfIterator;
    use pf_bitstream::av1::PicId;

    use super::*;
    use crate::decoder::core::build_scope;
    use crate::decoder::core::settle_dpb_ids;
    use crate::decoder::core::sync_slot_bindings;
    use crate::slots::SlotMap;

    const AV1_25FPS: &[u8] = pf_bitstream::testing::AV1_25FPS;

    fn std_ref(order_hint: u8, frame_type: u8) -> hh::StdVideoDecodeAV1ReferenceInfo {
        // SAFETY: Std bindgen struct; all-zero is valid for every field.
        let mut std: hh::StdVideoDecodeAV1ReferenceInfo = unsafe { std::mem::zeroed() };
        std.OrderHint = order_hint;
        std.frame_type = frame_type;
        std
    }

    fn vk_ref(slot: u8, order_hint: u8) -> VkRefAv1 {
        VkRefAv1 {
            slot,
            std: std_ref(order_hint, 1),
            id: u64::from(slot) + 100,
        }
    }

    /// Distinct fake view per slot (never dereferenced).
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    fn names(slots: &[u8]) -> [i32; 7] {
        let mut out = [REFERENCE_NAME_UNUSED; 7];
        for (name, slot) in slots.iter().enumerate() {
            out[name] = i32::from(*slot);
        }
        out
    }

    #[test]
    fn a_reference_name_pointing_outside_the_bound_slots_fails_the_whole_op() {
        let refs = vec![vk_ref(4, 10)];
        assert!(
            matches!(
                check_reference_names(&refs, &names(&[4, 7])),
                Err(VkDecodeError::UnboundReferenceSlot { slot: 7 })
            ),
            "name 7 is not one of this frame's bound references"
        );
        assert!(
            matches!(
                check_reference_names(&refs, &[-2, -1, -1, -1, -1, -1, -1]),
                Err(VkDecodeError::UnboundReferenceSlot { slot: u8::MAX })
            ),
            "a negative name other than UNUSED names no slot"
        );

        let refs = vec![vk_ref(4, 10), vk_ref(7, 11)];
        assert!(check_reference_names(&refs, &names(&[4, 7])).is_ok());
        assert!(
            check_reference_names(&refs, &names(&[])).is_ok(),
            "all seven UNUSED is a key frame"
        );
    }

    #[test]
    fn a_key_frames_scope_is_the_activation_entry_alone() {
        let slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>> = vec![None; 9];
        let no_refs: [VkRefAv1; 0] = [];
        let (scope, reference_count) = build_scope(
            &no_refs,
            std::iter::empty(),
            0,
            fake_view(0),
            std_ref(0, 0),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "a key frame references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![-1]
        );
    }

    /// RADV reads 256 entries regardless of `tileCount`; the tail must be zero.
    /// `tileCount` is the real count — ash setters would write 256 from slice len.
    #[test]
    fn the_picture_info_carries_padded_tile_arrays_with_the_real_tile_count() {
        let packed = pack_av1_tiles(&[100..1000, 1000..1600, 1600..2100]).expect("fits u32");
        let tiles = submitted_tiles(&packed).expect("three tiles fit");
        assert_eq!(tiles.count, 3);
        assert_eq!(&tiles.offsets[..3], &[0, 900, 1500]);
        assert_eq!(&tiles.sizes[..3], &[900, 600, 500]);

        // SAFETY: Std bindgen struct; all-zero is valid; no pointer is read.
        let mut std_pic: hh::StdVideoDecodeAV1PictureInfo = unsafe { std::mem::zeroed() };
        std_pic.OrderHint = 42;
        let picture_info = av1_picture_info(&std_pic, names(&[5, 1, 3]), &tiles);

        assert_eq!(
            picture_info.tile_count, 3,
            "tileCount is the real count, not the array length ash's setters would \
             have written"
        );
        assert_eq!(
            picture_info.frame_header_offset, FRAME_HEADER_OFFSET,
            "the buffer holds tile payloads only, so there is no header to point at"
        );
        assert_eq!(
            picture_info.s_type,
            vk::StructureType::VIDEO_DECODE_AV1_PICTURE_INFO_KHR
        );
        assert_eq!(picture_info.reference_name_slot_indices[0], 5);
        assert_eq!(picture_info.reference_name_slot_indices[6], -1);
        // SAFETY: pointers from `tiles`/`std_pic`, alive here; arrays are
        // AV1_MAX_NUM_TILES long, the length this read uses.
        unsafe {
            let offsets =
                std::slice::from_raw_parts(picture_info.p_tile_offsets, AV1_MAX_NUM_TILES);
            let sizes = std::slice::from_raw_parts(picture_info.p_tile_sizes, AV1_MAX_NUM_TILES);
            assert_eq!(&offsets[..3], &[0, 900, 1500]);
            assert_eq!(&sizes[..3], &[900, 600, 500]);
            assert!(
                offsets[3..].iter().all(|o| *o == 0) && sizes[3..].iter().all(|s| *s == 0),
                "the tail a driver reads past tileCount must be zeroed, not \
                 whatever the allocator handed back"
            );
            assert_eq!((*picture_info.p_std_picture_info).OrderHint, 42);
        }

        let std = std_ref(17, 1);
        let dpb_info = vk::VideoDecodeAV1DpbSlotInfoKHR::default().std_reference_info(&std);
        assert_eq!(
            dpb_info.s_type,
            vk::StructureType::VIDEO_DECODE_AV1_DPB_SLOT_INFO_KHR
        );
        // SAFETY: pointer taken from `std`, alive for this scope.
        unsafe {
            assert_eq!((*dpb_info.p_std_reference_info).OrderHint, 17);
        }
    }

    #[test]
    fn packed_tiles_land_end_to_end_and_more_than_the_arrays_hold_is_refused() {
        let packed = pack_av1_tiles(&[100..200, 500..560]).unwrap();
        assert_eq!(packed.offsets, vec![0, 100]);
        let tiles = submitted_tiles(&packed).expect("two tiles fit");
        assert_eq!(tiles.count, 2);
        assert_eq!(&tiles.offsets[..2], &[0, 100]);
        assert_eq!(&tiles.sizes[..2], &[100, 60]);

        // 256 tiles fit; one more is refused, not truncated.
        let ranges: Vec<Range<usize>> = (0..AV1_MAX_NUM_TILES).map(|i| i * 4..i * 4 + 4).collect();
        let full = submitted_tiles(&pack_av1_tiles(&ranges).unwrap()).expect("256 tiles fit");
        assert_eq!(full.count, AV1_MAX_NUM_TILES as u32);
        let ranges: Vec<Range<usize>> = (0..AV1_MAX_NUM_TILES + 1)
            .map(|i| i * 4..i * 4 + 4)
            .collect();
        assert_eq!(
            submitted_tiles(&pack_av1_tiles(&ranges).unwrap()),
            Err(Av1TileError::TooManyTiles {
                tiles: AV1_MAX_NUM_TILES + 1
            })
        );
    }

    /// Slot holds tile payloads only. Offsets alone pass for whole-OBU upload too;
    /// slot length is what distinguishes the layouts.
    #[test]
    fn the_ring_slot_holds_the_tile_payloads_and_nothing_else() {
        let mut planner = Av1Planner::new();
        let (mut checked, mut bytes_saved) = (0u32, 0usize);
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                let bitstream = plan_bitstream(packet, &plan.tiles, &plan.header).expect("splits");
                let packed = pack_av1_tiles(&bitstream.tiles).expect("fits u32");
                let tiles = submitted_tiles(&packed).expect("within the tile limit");

                let mut slot: Vec<u8> = Vec::new();
                for segment in &packed.segments {
                    slot.extend_from_slice(&packet[segment.clone()]);
                }
                let obu_bytes: usize = plan.tiles.iter().map(|t| t.data.len()).sum();
                assert_eq!(
                    slot.len(),
                    bitstream.tiles.iter().map(Range::len).sum::<usize>(),
                    "the slot is the tile payloads exactly"
                );
                assert!(
                    slot.len() < obu_bytes,
                    "the tile payloads must be SHORTER than the OBUs that carried \
                     them, or this frame proves nothing about the layout"
                );
                bytes_saved += obu_bytes - slot.len();

                assert_eq!(tiles.count as usize, bitstream.tiles.len());
                for (i, range) in bitstream.tiles.iter().enumerate() {
                    let start = tiles.offsets[i] as usize;
                    let end = start + tiles.sizes[i] as usize;
                    assert!(end <= slot.len(), "a tile range reaches past the slot");
                    assert_eq!(
                        &slot[start..end],
                        &packet[range.clone()],
                        "the submitted offset does not address this tile's bytes"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 274, "every tile of the vector was addressed");
        eprintln!("bytes not uploaded across the vector: {bytes_saved}");
    }

    #[test]
    fn the_session_shape_comes_off_the_stream_not_a_constant() {
        let mut planner = Av1Planner::new();
        let packet = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plan = planner
            .plan_au(packet)
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");

        let extent = coded_extent(&plan);
        assert_eq!(
            (extent.width, extent.height),
            (plan.picture.upscaled_width, plan.picture.frame_height),
            "the decode output is the POST-superres width"
        );
        assert!(extent.width > 0 && extent.height > 0);

        let key = profile_key_for(&plan).expect("inside the envelope");
        assert_eq!(key.output_format(), Some(crate::caps::NV12));
        assert!(!key.film_grain);

        // Operating point 0; this vector is a real Std level, not the 31 sentinel.
        assert!(stream_level_idx(&plan) <= 23);
    }

    /// `seq_level_idx` 31 is Annex A "maximum parameters", not a level above 7.3.
    /// `StdVideoAV1Level` stops at 23; 31 > 23 is not "too demanding".
    #[test]
    fn the_av1_max_parameters_sentinel_is_not_a_level_above_the_ceiling() {
        let ceiling = crate::caps::MaxLevelIdc::Av1(hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_7_3);
        assert_eq!(ceiling.code_point(), 23, "the Std enum's top code point");

        for idx in 0..=ceiling.code_point() {
            assert!(idx <= ceiling.code_point());
        }
        // 24..30 reserved; 31 is "maximum parameters". Not a capability test.
        for idx in (ceiling.code_point() + 1)..=31 {
            assert!(
                idx > ceiling.code_point(),
                "seq_level_idx {idx} is outside the Std range, not a more demanding level"
            );
        }

        assert!(31 > ceiling.code_point());
        assert_eq!(format!("{ceiling}"), "AV1 Std level 23");
    }

    #[test]
    fn only_a_decoded_key_frame_ends_the_wait_for_one() {
        let mut planner = Av1Planner::new();
        let packet = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let first = planner
            .plan_au(packet)
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        assert!(first.picture.is_key, "the vector opens on a key frame");
        assert!(
            clears_awaiting_key(&first),
            "a decoded key frame is what resumes decoding"
        );

        // `show_existing_frame` of a key must not resume (full store, empty ledger).
        let mut shown = first.clone();
        shown.dpb.stored = None;
        assert!(shown.picture.is_key);
        assert!(!clears_awaiting_key(&shown));

        let inter = planner
            .plan_au(IvfIterator::new(AV1_25FPS).nth(1).expect("a second packet"))
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        assert!(!inter.picture.is_key);
        assert!(!clears_awaiting_key(&inter));
    }

    /// Skip is per frame; error is per AU. `Ok(None)` would reset demotion.
    /// `planned == 0` is metadata, not a wait.
    #[test]
    fn a_unit_reports_the_key_frame_wait_only_when_it_decoded_nothing_at_all() {
        assert!(whole_unit_skipped(1, 1), "a single-frame unit");
        assert!(whole_unit_skipped(2, 2), "and a two-frame one");

        // Key behind a skip in the same unit: an early return would miss it.
        assert!(!whole_unit_skipped(2, 1));
        assert!(!whole_unit_skipped(3, 2));
        assert!(!whole_unit_skipped(2, 0));
        assert!(!whole_unit_skipped(0, 0));
    }

    /// Wait error must not share text with the loss that latched recovery.
    #[test]
    fn the_key_frame_wait_names_itself_in_the_error_text() {
        let waiting = format!("{}", VkDecodeError::AwaitingKeyAv1);
        assert!(waiting.contains("key frame"), "{waiting}");
        assert!(waiting.contains("skipped"), "{waiting}");
        let lost = format!(
            "{}",
            VkDecodeError::MissingReferenceAv1 {
                slot: 3,
                ref_index: 2
            }
        );
        assert_ne!(waiting, lost);
    }

    /// This vector never hits `refresh_frame_flags == 0`.
    #[test]
    fn every_frame_of_the_vector_refreshes_a_slot_so_the_orphan_arm_is_review_only() {
        let mut planner = Av1Planner::new();
        let (mut frames, mut orphans) = (0u32, 0u32);
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                if plan.header.refresh_frame_flags == 0 {
                    orphans += 1;
                }
            }
        }
        assert_eq!(frames, 274);
        assert_eq!(
            orphans, 0,
            "if this ever fires the orphan release IS exercised — turn this into a \
             ledger-occupancy assertion rather than deleting it"
        );
    }

    /// Production [`lost_reference`], not a re-implemented `find_map`.
    #[test]
    fn a_lost_reference_is_the_condition_the_decoder_refuses_on() {
        assert_eq!(
            lost_reference(&[
                PlanWarning::TruncatedAu { offset: 12 },
                PlanWarning::MissingReference {
                    slot: 3,
                    ref_index: 2,
                },
            ]),
            Some((3, 2)),
            "a missing reference must be found even behind another warning"
        );

        // TruncatedAu is concealment the planner already applied.
        assert_eq!(
            lost_reference(&[PlanWarning::TruncatedAu { offset: 12 }]),
            None
        );
        assert_eq!(
            lost_reference(&[PlanWarning::MissingShowExisting { slot: 4 }]),
            None,
            "a show_existing_frame naming an empty slot decodes nothing, so there \
             is no reference set to be wrong about"
        );
        assert_eq!(lost_reference(&[]), None);
    }

    /// Clean vector must not trip [`lost_reference`], or every frame would refuse.
    #[test]
    fn no_frame_of_the_clean_vector_trips_the_refusal() {
        let mut planner = Av1Planner::new();
        let mut frames = 0u32;
        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("plans") {
                frames += 1;
                assert_eq!(
                    lost_reference(&plan.warnings),
                    None,
                    "frame {frames} of a clean conformance vector must not be refused"
                );
            }
        }
        assert_eq!(frames, 274);
    }

    /// Vector through convert + [`sync_slot_bindings`] + [`build_scope`] +
    /// [`check_reference_names`].
    /// A referenced slot must still bind the picture it was decoded into —
    /// bound to the setup picture is also wrong, and silent on the GPU.
    #[test]
    fn slot_recycling_waits_for_the_decode_op() {
        #[derive(Clone, Default)]
        struct SimPicture {
            bound: bool,
            pending: bool,
            held: u32,
        }
        // Distinct view per pool picture (never dereferenced).
        let image_view = |picture: usize| vk::ImageView::from_raw(picture as u64 + 1);

        let mut planner = Av1Planner::new();
        let mut slots = SlotMap::new(NUM_REF_SLOTS);
        let mut slot_image: Vec<Option<usize>> = vec![None; REQUIRED_SLOTS as usize];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeAV1ReferenceInfo>> =
            vec![None; REQUIRED_SLOTS as usize];
        let mut pictures =
            vec![SimPicture::default(); (REQUIRED_SLOTS + crate::images::HOLD_HEADROOM) as usize];
        let mut pending: BTreeMap<PicId, usize> = BTreeMap::new();
        let mut image_of: BTreeMap<PicId, usize> = BTreeMap::new();

        let (mut frames, mut deferring, mut scope_refs) = (0u32, 0u32, 0u32);

        for packet in IvfIterator::new(AV1_25FPS) {
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                let Some(setup_id) = plan.dpb.stored else {
                    let (ready, dropped) =
                        settle_dpb_ids(&mut pending, &plan.dpb.outputs, &plan.dpb.removed);
                    for image in ready.into_iter().chain(dropped) {
                        pictures[image].pending = false;
                    }
                    for &id in &plan.dpb.removed {
                        slots.release(id);
                        image_of.remove(&id);
                    }
                    continue;
                };
                frames += 1;

                let vk = plan_to_vk_av1(&plan, &mut slots).expect("the clean vector converts");
                let setup = usize::from(vk.setup_slot);
                if !vk.release_after_decode.is_empty() {
                    deferring += 1;
                }

                for picture in sync_slot_bindings(&slots, &mut slot_image, vk.setup_slot) {
                    pictures[picture].bound = false;
                }
                let dst = pictures
                    .iter()
                    .position(|p| !p.bound && !p.pending && p.held == 0)
                    .unwrap_or_else(|| panic!("frame {frames}: picture pool exhausted"));

                // Held slots except setup must bind, or the scope drops them.
                for (slot, _id) in slots.held() {
                    if usize::from(slot) == setup {
                        continue;
                    }
                    assert!(
                        slot_image[usize::from(slot)].is_some(),
                        "frame {frames}: held slot {slot} binds no image"
                    );
                }

                let held_slots: Vec<u8> = slots.held().map(|(slot, _id)| slot).collect();
                let (scope, reference_count) = build_scope(
                    &vk.refs,
                    held_slots.iter().copied(),
                    vk.setup_slot,
                    image_view(dst),
                    vk.setup_ref,
                    &slot_refs,
                    |slot| slot_image[usize::from(slot)].map(image_view),
                )
                .and_then(|scope| {
                    check_reference_names(&vk.refs, &vk.reference_name_slot_indices)?;
                    Ok(scope)
                })
                .unwrap_or_else(|e| {
                    panic!(
                        "frame {frames}: {e}\n  setup_slot={setup} setup_id={setup_id}\n  \
                         refs={:?}\n  names={:?}\n  bindings={slot_image:?}",
                        vk.refs.iter().map(|r| (r.slot, r.id)).collect::<Vec<_>>(),
                        vk.reference_name_slot_indices,
                    )
                });

                // Held slots once each, plus setup as `-1`. Setup is held: assigned.
                assert_eq!(reference_count, vk.refs.len());
                let mut bound: Vec<i32> = scope.iter().map(|e| e.slot_index).collect();
                assert_eq!(bound.pop(), Some(-1), "frame {frames}: no activation entry");
                bound.sort_unstable();
                let mut expected: Vec<i32> = held_slots
                    .iter()
                    .filter(|slot| usize::from(**slot) != setup)
                    .map(|slot| i32::from(*slot))
                    .collect();
                expected.sort_unstable();
                assert_eq!(
                    bound, expected,
                    "frame {frames}: the coding scope must bind exactly the held \
                     slots, once each"
                );

                // Bound to the setup picture is still bound — to the wrong image.
                for (entry, r) in scope[..reference_count].iter().zip(&vk.refs) {
                    let decoded_into = image_of[&r.id];
                    assert_eq!(
                        entry.view,
                        image_view(decoded_into),
                        "frame {frames}: reference picture {} (slot {}) binds pool \
                         image {:?}, but it was decoded into image {decoded_into}",
                        r.id,
                        r.slot,
                        slot_image[usize::from(r.slot)]
                    );
                    assert_ne!(
                        decoded_into, dst,
                        "frame {frames}: reference picture {} resolves to the image \
                         this very frame is decoding into",
                        r.id
                    );
                    scope_refs += 1;
                }

                pictures[dst].pending = true;
                pictures[dst].bound = true;
                slot_image[setup] = Some(dst);
                slot_refs[setup] = Some(vk.setup_ref);
                for r in &vk.refs {
                    slot_refs[usize::from(r.slot)] = Some(r.std);
                }
                for &id in &vk.release_after_decode {
                    assert!(slots.release(id), "frame {frames}: deferred release missed");
                }
                pending.insert(setup_id, dst);
                image_of.insert(setup_id, dst);

                let (ready, dropped) =
                    settle_dpb_ids(&mut pending, &plan.dpb.outputs, &plan.dpb.removed);
                for image in ready.into_iter().chain(dropped) {
                    pictures[image].pending = false;
                }
                for &id in &plan.dpb.removed {
                    image_of.remove(&id);
                }
                if plan.header.refresh_frame_flags == 0 {
                    slots.release(setup_id);
                    if let Some(image) = pending.remove(&setup_id) {
                        pictures[image].pending = false;
                    }
                    image_of.remove(&setup_id);
                }
            }
        }

        assert_eq!(frames, 274, "every frame of the vector must decode");
        // 268 of 274 displace a picture they still read. Zero means the asserts
        // above compare empty lists.
        assert_eq!(
            deferring, 268,
            "268 of 274 frames displace a picture they are reading; at zero, \
             `release_after_decode` could be deleted and nothing here would fail"
        );
        assert_eq!(
            scope_refs, 1616,
            "the references actually bound into a coding scope across the vector"
        );
        eprintln!(
            "frames {frames} · scope references {scope_refs} · deferred releases {deferring}"
        );
    }
}
