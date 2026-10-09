//! [`VkH265Decoder`]: native Vulkan Video H.265 decode over pf-bitstream's planner.
//!
//! Per AU: `plan_au` → `plan_to_vk_h265` → slices-only ring upload → record
//! (barriers, `vkCmdBeginVideoCodingKHR` with every bound DPB slot, one-shot
//! session RESET, a caps-gated `RESULT_STATUS_ONLY` query around
//! `vkCmdDecodeVideoKHR`) → submit on the decode queue under the caller's
//! [`crate::QueueLock`] with a per-image timeline signal.
//!
//! Picture format is the stream's ([`H265ProfileKey`]): Main → NV12, Main 10 →
//! P010, RExt 4:4:4 → two-plane 4:4:4. Refuse a combination the device cannot
//! host before a session exists. `RefPicSetStCurr*`/`LtCurr` name DPB slot
//! indices — bind every referenced slot or fail closed ([`crate::pic_h265`]).
//! Slice offsets are rebased onto [`pack_slices`] output (VCL only, three-byte
//! Annex-B prefix). A RASL after an open-GOP CRA is not an error (8.1.3 NOTE).
//!
//! Lifecycle, status and recording are [`VkDecoder`]'s. Codec dispatch is the
//! client's.

use ash::vk;
use ash::vk::native as hh;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::H265Planner;
use pf_bitstream::h265::PlanError;
use pf_bitstream::h265::PlanWarning;
use tracing::debug;
use tracing::trace;

use crate::caps::derive_caps;
use crate::caps::query_caps;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps_h265::H265ProfileKey;
use crate::decoder::core::PendingPic;
use crate::decoder::core::ScopeRef;
use crate::decoder::core::VkCodec;
use crate::decoder::core::VkDecoder;
use crate::decoder::DecodedVkFrame;
use crate::decoder::VkDecodeError;
use crate::device::DecodeDevice;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::params_h265::level_to_std as level_to_std_h265;
use crate::pic_h265::plan_to_vk_h265;
use crate::pic_h265::DecodePlanVkH265;
use crate::pic_h265::PlanToVkH265Error;
use crate::recovery::RecoveryMark;
use crate::recovery::RecoveryWatch;
use crate::ring::pack_slices;
use crate::session_h265::ParamsActionH265;
use crate::session_h265::SessionConfigH265;
use crate::session_h265::VideoSessionH265;
use crate::session_h265::VpsSource;

/// H.265 planner and stream state of a [`VkH265Decoder`].
pub struct H265 {
    planner: H265Planner,
    /// Outstanding recovery-point SEI ([`crate::recovery`]): stream prediction
    /// structure, unrelated to the decoder's DPB-recovery latch.
    recovery_watch: RecoveryWatch,
    /// [`VkH265Decoder::refuse_multi_slice`].
    single_slice: bool,
}

/// Native Vulkan Video H.265 decoder.
pub type VkH265Decoder = VkDecoder<H265>;

impl VkCodec for H265 {
    type Session = VideoSessionH265;
    type StdRef = hh::StdVideoDecodeH265ReferenceInfo;
    type Warning = PlanWarning;
    type Plan = AuPlan;
    /// A Main→Main 10 switch is a different key: it re-queries and rebuilds.
    type ProfileKey = H265ProfileKey;
    type DpbSlotInfo<'a> = vk::VideoDecodeH265DpbSlotInfoKHR<'a>;
    const LABEL: &'static str = "h265";

    fn dpb_slot_info(std: &Self::StdRef) -> Self::DpbSlotInfo<'_> {
        vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(std)
    }

    fn decode_au(
        dec: &mut VkDecoder<Self>,
        au: &[u8],
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        dec.decode_inner(au)
    }

    fn flush(dec: &mut VkDecoder<Self>) {
        let update = dec.codec.planner.flush();
        dec.flush_dpb(&update);
    }

    fn forgive_unclean(&mut self) {
        self.planner.forgive_unclean();
    }

    fn profile_key(plan: &AuPlan) -> Result<H265ProfileKey, VkDecodeError> {
        profile_key_for(plan)
    }

    fn decode_profile(key: H265ProfileKey) -> DecodeProfile {
        DecodeProfile::H265(key)
    }

    /// Picture format is the key's: Main → NV12, Main 10 → P010, 4:4:4 → two-plane.
    unsafe fn query_caps(
        dev: &DecodeDevice,
        key: H265ProfileKey,
    ) -> Result<DecodeCaps, VkDecodeError> {
        let wanted = key
            .output_format()
            .expect("the key's constructor gated the chroma/depth combination");
        // SAFETY: fn contract.
        let raw = unsafe { query_caps(dev, DecodeProfile::H265(key)) }?;
        Ok(derive_caps(&raw, wanted)?)
    }

    fn stream_level(plan: &AuPlan) -> u32 {
        level_to_std_h265(plan.picture.level_idc)
    }

    fn required_slots(plan: &AuPlan) -> u32 {
        plan.picture.max_dpb_frames as u32 + 1
    }

    fn coded_extent(plan: &AuPlan) -> vk::Extent2D {
        vk::Extent2D {
            width: plan.picture.coded_width,
            height: plan.picture.coded_height,
        }
    }

    unsafe fn create_session(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        key: H265ProfileKey,
        slots: u32,
        extent: vk::Extent2D,
    ) -> Result<VideoSessionH265, VkDecodeError> {
        let config = SessionConfigH265 {
            max_coded_extent: extent,
            max_dpb_slots: slots,
            max_active_references: (slots - 1).min(caps.max_active_references),
            profile: key,
            max_level_idc: caps.max_level_idc.code_point(),
        };
        // SAFETY: fn contract.
        Ok(unsafe { VideoSessionH265::create(dev, caps, config)? })
    }
}

impl VkDecoder<H265> {
    /// Wrap the borrowed device. Sessions and pools are built lazily from the
    /// first AU's SPS (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The [`DeviceHandles`] contract (liveness, enabled extensions and
    /// features, truthful queue families) is held for this decoder's lifetime,
    /// not just this call. The device must have been created with
    /// `VK_KHR_video_decode_h265` enabled. The check below reads the decode
    /// queue family's advertised `videoCodecOperations` — the device's claim
    /// about the family, not proof the client enabled the extension at
    /// `vkCreateDevice`. Missing the extension is UB at session creation, so
    /// the family check runs before any query or create.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        // Queue family must advertise DECODE_H265 before any query or create:
        // `query_caps` is a physical-device query and succeeds without the
        // extension; `vkCreateVideoSessionKHR` with DECODE_H265 then is UB.
        dev.require_codec_op(vk::VideoCodecOperationFlagsKHR::DECODE_H265, "H.265 decode")?;
        let codec = H265 {
            planner: H265Planner::new(),
            recovery_watch: RecoveryWatch::new(),
            single_slice: false,
        };
        Ok(Self::with_codec(dev, lock, codec))
    }

    /// Refuse a picture with more than one slice segment before it reaches the driver.
    /// For a driver that faults on one; the refusal is a device fact
    /// ([`VkDecodeError::is_device_fact`]), so the owner's next rung takes the stream.
    pub fn refuse_multi_slice(&mut self) {
        self.codec.single_slice = true;
    }

    /// Ask whether the device can host a stream of this (chroma, bit-depth)
    /// shape, before any AU is fed.
    ///
    /// Picture format is the stream's; advertising H.265 decode does not
    /// advertise every shape (4:4:4 RExt and 10-bit are commonly absent).
    /// Discovering that in `ensure_state` makes the refusal a mid-stream error
    /// streak and demotes past later hardware rungs. Same query as
    /// `ensure_state`; only the timing differs.
    ///
    /// Negotiated facts are a hint — the in-band SPS is authoritative — so this
    /// is not a promise decode will succeed. It does guarantee a shape the
    /// device cannot host never gets a session.
    pub fn probe_stream_support(
        &self,
        chroma_format_idc: u8,
        bit_depth_luma_minus8: u8,
    ) -> Result<(), VkDecodeError> {
        self.probe_key(H265ProfileKey::from_negotiated(
            chroma_format_idc,
            bit_depth_luma_minus8,
        )?)
    }

    /// One AU: plan, fold recovery-point SEI, submit.
    ///
    /// A RASL the planner refuses after an open-GOP join is not an error: it
    /// is undecodable by definition (8.1.3 NOTE — its references precede the
    /// join). Returns `Ok` with whatever is already display-ready and leaves
    /// planner, DPB, and slot ledger untouched. Treating it as an error would
    /// request a keyframe the host has no reason to send.
    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        let plan = match self.codec.planner.plan_au(au) {
            Ok(plan) => plan,
            Err(PlanError::RaslSkipped { poc }) => {
                trace!(poc, "RASL picture after a CRA join — skipped, not failed");
                return Ok(self.ready.pop_front());
            }
            Err(e) => return Err(VkDecodeError::PlanH265(e)),
        };
        for warning in &plan.warnings {
            // Integration reads these via `take_warnings`; never silent.
            trace!(?warning, "plan warning");
        }
        self.last_warnings = plan.warnings.clone();
        // One picture per AU: stamp decode-order before anything reorders it
        // (`DecodedVkFrame::decode_order`).
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;
        // Folded once per planned AU, in decode order — the SEI's POC delta
        // is measured in that order. The mark rides the pending picture into
        // display order ([`crate::recovery`]).
        let recovery = self.codec.recovery_watch.note_h265(
            plan.picture.pic_order_cnt,
            plan.picture.is_irap,
            plan.picture.recovery_point,
        );
        if recovery != RecoveryMark::NONE {
            trace!(
                sei = recovery.sei_here,
                recovery_point = recovery.is_recovery_point,
                poc = plan.picture.pic_order_cnt,
                "recovery point SEI"
            );
        }

        // Planner has advanced: a failure below can leave its DPB and this
        // decoder's slot/image ledgers disagreeing, including failures before
        // `plan_to_vk_h265` (planner-resident, no slot). Latch recovery rather
        // than return into a wedged stream.
        let result = self.decode_planned(&plan, au, recovery, decode_order);
        if result.is_err() {
            self.recovery.latch();
        }
        result
    }

    /// Submission half of one decode, after the planner has advanced. Split
    /// out so [`Self::decode_inner`] can latch recovery on any failure past
    /// that line without a flag on every exit. `au` is the buffer `plan`'s
    /// slice ranges index; `recovery` and `decode_order` are already folded
    /// for this AU — this path is not reached for every planned AU. No release
    /// is deferred: `plan_to_vk_h265` applies RPS removals itself.
    fn decode_planned(
        &mut self,
        plan: &AuPlan,
        au: &[u8],
        recovery: RecoveryMark,
        decode_order: u64,
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        crate::caps::require_segments(self.codec.single_slice, plan.slices.len())?;
        self.ensure_state(plan)?;

        // Stream VPS, or the fallback identity when the join missed the VPS
        // NALU ([`crate::session_h265`]).
        let vps = VpsSource::for_sps(&plan.sps);

        // One rebuild retry on CapacityMismatch — DPB-depth renegotiation
        // ([`crate::pic_h265`]).
        let mut vk_plan: Option<DecodePlanVkH265> = None;
        for attempt in 0..2 {
            // Recreate destroys the old parameters object; an in-flight decode
            // may still be executing against it: drain first.
            if self
                .state
                .as_ref()
                .expect("ensure_state built it")
                .session
                .parameters_action(&vps, &plan.sps, &plan.pps)
                == ParamsActionH265::Recreate
            {
                self.drain_gpu()?;
            }
            let state = self.state.as_mut().expect("ensure_state built it");
            // SAFETY: live device (constructor contract); the drain above
            // satisfies ensure_parameters' Recreate contract, and Current/Add
            // touch nothing a submitted decode reads.
            unsafe {
                state
                    .session
                    .ensure_parameters(&vps, &plan.sps, &plan.pps)?
            };
            match plan_to_vk_h265(plan, &mut state.slots) {
                Ok(converted) => {
                    vk_plan = Some(converted);
                    break;
                }
                Err(PlanToVkH265Error::CapacityMismatch { required, capacity }) if attempt == 0 => {
                    debug!(
                        required,
                        capacity, "DPB depth renegotiated — rebuilding session"
                    );
                    self.rebuild_state(plan)?;
                }
                Err(e) => return Err(VkDecodeError::ConvertH265(e)),
            }
        }
        let vk_plan = vk_plan.expect("the rebuilt session matches its own plan");

        let op = self.prepare_op(&vk_plan.refs, vk_plan.setup_slot, vk_plan.setup_ref)?;
        // Slice-segment NALUs only. Non-VCL (AUD/SEI, IRAP VPS/SPS/PPS) inside
        // the decode range hangs VCN; parameter sets ride the session object.
        // Plan offsets are AU-relative and get rebased into the packed buffer.
        let plan_segments: Vec<std::ops::Range<usize>> =
            plan.slices.iter().map(|s| s.nal.clone()).collect();
        // One rebased offset per slice, in plan order — same walk as
        // `vk_plan.slice_offsets`, so count and order agree by construction.
        // `pack_slices` puts each NAL behind a three-byte start code; a
        // four-byte prefix shifts drivers that skip a fixed `+3 +2` off the header.
        let Some(packed) = pack_slices(&plan_segments) else {
            return Err(VkDecodeError::Unsupported(
                "packed slice data exceeds the u32 offsets Vulkan submits".into(),
            ));
        };
        // SAFETY: each segment is a slice NAL plus the three start-code bytes
        // before it, inside the plan's own in-bounds slice range; the
        // recorded offsets come from the same `pack_slices` call.
        let upload = unsafe { self.upload(au, &packed.segments)? };
        // `refs` order is the contract: RPS arrays index into the decode op's
        // reference array, so a missing entry fails before the buffer is begun.
        let scope = self.scope(&op, &vk_plan.refs)?;
        // Offsets rebased into the packed slices-only buffer, not the plan's
        // AU-absolute offsets — non-slice NALUs were never uploaded.
        let mut picture_info = vk::VideoDecodeH265PictureInfoKHR::default()
            .std_picture_info(&vk_plan.std_pic)
            .slice_segment_offsets(&packed.offsets);
        // SAFETY: `op`, `scope` and `upload` were just taken for this plan on
        // the current generation.
        unsafe { self.record_and_submit(&op, &scope, &mut picture_info, &upload)? };
        self.commit_op(&op, &upload, &vk_plan.refs);
        self.pending.insert(
            vk_plan.setup_id,
            PendingPic {
                image: op.dst,
                submission: op.submission,
                query_slot: op.query_index,
                timeline_value: op.signal_value,
                crop: plan.picture.display_crop,
                colour: plan.picture.colour,
                poc: plan.picture.pic_order_cnt,
                is_idr: plan.picture.is_idr,
                recovery,
                decode_order,
                references_clean: plan.picture.references_clean,
            },
        );

        self.settle(&plan.dpb.outputs, &plan.dpb.removed);
        Ok(self.ready.pop_front())
    }
}

/// Vulkan profile this AU needs. `separate_colour_plane_flag` comes off the
/// active SPS, not the picture plan — the planner has no use for it, the
/// profile gate does.
fn profile_key_for(plan: &AuPlan) -> Result<H265ProfileKey, VkDecodeError> {
    H265ProfileKey::from_stream(
        plan.picture.general_profile_idc,
        plan.picture.chroma_format_idc,
        plan.sps.separate_colour_plane_flag,
        plan.picture.bit_depth_luma_minus8,
        plan.picture.bit_depth_chroma_minus8,
    )
    .map_err(VkDecodeError::ParamsH265)
}

impl ScopeRef for crate::pic_h265::VkRefH265 {
    type Std = hh::StdVideoDecodeH265ReferenceInfo;
    fn slot(&self) -> u8 {
        self.slot
    }
    fn std(&self) -> Self::Std {
        self.std
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference-info carrying the two fields the assertions read.
    fn std_ref(poc: i32, long_term: bool) -> hh::StdVideoDecodeH265ReferenceInfo {
        // SAFETY: StdVideoDecodeH265ReferenceInfo is a plain-C bindgen struct of a
        // bitfield word and one integer; all-zero is valid for every field.
        let mut std: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
        std.PicOrderCntVal = poc;
        std.flags
            .set_used_for_long_term_reference(u32::from(long_term));
        std
    }

    #[test]
    fn the_picture_info_carries_the_rebased_offsets_and_the_h265_std_struct() {
        // Without a device: `pSliceSegmentOffsets` must be the rebased array
        // (one entry per slice, counted by ash from the slice length), and the
        // picture info must point at the plan's own Std struct.
        let offsets = pack_slices(&[43..900, 903..1500, 1503..2000])
            .unwrap()
            .offsets;
        assert_eq!(offsets, vec![0, 860, 1460]);

        // SAFETY: StdVideoDecodeH265PictureInfo is a plain-C bindgen struct of a
        // bitfield word, integers and byte arrays; all-zero is valid.
        let mut std_pic: hh::StdVideoDecodeH265PictureInfo = unsafe { std::mem::zeroed() };
        std_pic.PicOrderCntVal = 42;
        let picture_info = vk::VideoDecodeH265PictureInfoKHR::default()
            .std_picture_info(&std_pic)
            .slice_segment_offsets(&offsets);
        assert_eq!(picture_info.slice_segment_count, 3);
        assert_eq!(
            picture_info.s_type,
            vk::StructureType::VIDEO_DECODE_H265_PICTURE_INFO_KHR
        );
        // SAFETY: the two pointers were just taken from `offsets` and `std_pic`,
        // both alive for this scope.
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(picture_info.p_slice_segment_offsets, 3),
                &offsets[..]
            );
            assert_eq!((*picture_info.p_std_picture_info).PicOrderCntVal, 42);
        }

        let std = std_ref(17, true);
        let dpb_info = vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(&std);
        assert_eq!(
            dpb_info.s_type,
            vk::StructureType::VIDEO_DECODE_H265_DPB_SLOT_INFO_KHR
        );
        // SAFETY: the pointer was just taken from `std`, alive for this scope.
        unsafe {
            assert_eq!((*dpb_info.p_std_reference_info).PicOrderCntVal, 17);
            assert_eq!(
                (*dpb_info.p_std_reference_info)
                    .flags
                    .used_for_long_term_reference(),
                1
            );
        }
    }
}
