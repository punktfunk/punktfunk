//! Native Vulkan Video H.264 decoder: planner, session, and decode-queue submit.
//!
//! Each AU is planned, converted, packed into the bitstream ring, recorded
//! (bound DPB slots, one-time session RESET, a `RESULT_STATUS_ONLY` query
//! around `vkCmdDecodeVideoKHR`), then submitted under the caller's
//! [`crate::QueueLock`] with a per-picture timeline signal. Lifecycle, status
//! and recording are [`VkDecoder`]'s; this file plans and converts.
//!
//! Every decode op has a query slot. [`VkDecoder::poll_status`] reads it
//! without waiting; a non-COMPLETE result is the concealment signal. FFmpeg
//! runs `nb_queries = 0` and cannot see driver-reported corruption.
//!
//! Residual: sampling a still-live reference while a decode reads it writes
//! presenter layout metadata; `VK_KHR_unified_image_layouts` drops that trip.

use ash::vk;
use ash::vk::native as hh;
use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::H264Planner;
use pf_bitstream::h264::PlanWarning;
use tracing::debug;
use tracing::trace;

use crate::caps::derive_caps;
use crate::caps::query_caps;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps::NV12;
use crate::decoder::core::PendingPic;
use crate::decoder::core::ScopeRef;
use crate::decoder::core::VkCodec;
use crate::decoder::core::VkDecoder;
use crate::decoder::DecodedVkFrame;
use crate::decoder::VkDecodeError;
use crate::device::DecodeDevice;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::params::level_to_std;
use crate::params::ParamsError;
use crate::pic::plan_to_vk;
use crate::pic::DecodePlanVk;
use crate::pic::PlanToVkError;
use crate::recovery::RecoveryMark;
use crate::recovery::RecoveryWatch;
use crate::ring::pack_slices;
use crate::session::ParamsAction;
use crate::session::SessionConfig;
use crate::session::VideoSession;

/// H.264 planner and stream state of a [`VkH264Decoder`].
pub struct H264 {
    planner: H264Planner,
    /// Outstanding recovery-point SEI ([`crate::recovery`]). Survives session
    /// rebuilds: a fact about the stream, not Vulkan objects. Distinct from the
    /// decoder's DPB-recovery latch.
    recovery_watch: RecoveryWatch,
}

/// Native Vulkan Video H.264 decoder.
pub type VkH264Decoder = VkDecoder<H264>;

impl VkCodec for H264 {
    type Session = VideoSession;
    type StdRef = hh::StdVideoDecodeH264ReferenceInfo;
    type Warning = PlanWarning;
    type Plan = AuPlan;
    /// The Std profile idc.
    type ProfileKey = hh::StdVideoH264ProfileIdc;
    type DpbSlotInfo<'a> = vk::VideoDecodeH264DpbSlotInfoKHR<'a>;
    const LABEL: &'static str = "h264";

    fn dpb_slot_info(std: &Self::StdRef) -> Self::DpbSlotInfo<'_> {
        vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(std)
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

    fn profile_key(plan: &AuPlan) -> Result<Self::ProfileKey, VkDecodeError> {
        std_profile_for(plan)
    }

    fn decode_profile(key: Self::ProfileKey) -> DecodeProfile {
        DecodeProfile::H264(key)
    }

    /// Every H.264 profile asks for NV12.
    unsafe fn query_caps(
        dev: &DecodeDevice,
        key: Self::ProfileKey,
    ) -> Result<DecodeCaps, VkDecodeError> {
        // SAFETY: fn contract.
        let raw = unsafe { query_caps(dev, DecodeProfile::H264(key)) }?;
        Ok(derive_caps(&raw, NV12)?)
    }

    fn stream_level(plan: &AuPlan) -> u32 {
        level_to_std(plan.picture.level_idc)
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
        key: Self::ProfileKey,
        slots: u32,
        extent: vk::Extent2D,
    ) -> Result<VideoSession, VkDecodeError> {
        let config = SessionConfig {
            max_coded_extent: extent,
            max_dpb_slots: slots,
            max_active_references: (slots - 1).min(caps.max_active_references),
            std_profile_idc: key,
            max_level_idc: caps.max_level_idc.code_point(),
        };
        // SAFETY: fn contract.
        Ok(unsafe { VideoSession::create(dev, caps, config)? })
    }
}

impl VkDecoder<H264> {
    /// Wrap the borrowed device. Sessions and pools are built lazily from the
    /// first AU's SPS (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// Full [`DeviceHandles`] contract (liveness, enabled extensions and
    /// features, truthful queue families) for this decoder's lifetime. The
    /// device must have `VK_KHR_video_decode_h264` enabled; that part is
    /// checked below because a miss is UB at session creation, not an error.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        // Queue family must actually run H.264 decode. Caps would answer for
        // the hardware even if the extension was never enabled (`device.rs`).
        dev.require_codec_op(vk::VideoCodecOperationFlagsKHR::DECODE_H264, "H.264 decode")?;
        // The picture-pool arrangement is a device fact, not the stream's: ask with
        // the profile every host encodes, so an unusable device refuses the rung
        // before its first AU. A stream in another profile re-queries at its SPS.
        // SAFETY: live device (the `wrap` contract above).
        let caps = unsafe { H264::query_caps(&dev, H264_PROFILE_HIGH)? };
        let codec = H264 {
            planner: H264Planner::new(),
            recovery_watch: RecoveryWatch::new(),
        };
        let mut dec = Self::with_codec(dev, lock, codec);
        dec.caps = Some((H264_PROFILE_HIGH, caps));
        Ok(dec)
    }

    /// One AU: plan, fold recovery-point SEI, submit. Zero-reorder: the
    /// returned frame is the AU's own picture.
    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        let plan = self.codec.planner.plan_au(au)?;
        for warning in &plan.warnings {
            // Recovery verdict is the integration layer's; still not silent here.
            trace!(?warning, "plan warning");
        }
        self.last_warnings = plan.warnings.clone();
        // One picture per AU: stamp decode-order before anything can reorder it.
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;
        // Fold recovery-point SEI once per planned AU, in decode order (the
        // SEI's count). The mark rides the pending picture to display order.
        let recovery = self.codec.recovery_watch.note_h264(
            plan.picture.frame_num,
            plan.picture.is_idr,
            plan.picture.recovery_point,
        );
        if recovery != RecoveryMark::NONE {
            trace!(
                sei = recovery.sei_here,
                recovery_point = recovery.is_recovery_point,
                frame_num = plan.picture.frame_num,
                "recovery point SEI"
            );
        }

        // Planner has advanced; its DPB holds this picture. A later failure
        // can disagree with the slot/image ledgers — latch recovery. Wider
        // than SlotMap mutations: `ensure_state` / `NoFreeSlot` strands the
        // picture planner-resident with no slot. One flush cures both.
        let result = self.decode_planned(&plan, au, recovery, decode_order);
        if result.is_err() {
            self.recovery.latch();
        }
        result
    }

    /// Submit one already-planned AU. Split so [`Self::decode_inner`] can latch
    /// recovery on any failure past that line without a flag on every exit.
    /// `au` is the buffer `plan`'s slice ranges index; `recovery` and
    /// `decode_order` are already folded (this path is not every planned AU).
    fn decode_planned(
        &mut self,
        plan: &AuPlan,
        au: &[u8],
        recovery: RecoveryMark,
        decode_order: u64,
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        self.ensure_state(plan)?;
        let sps_id = plan.sps.seq_parameter_set_id;

        // One rebuild retry on CapacityMismatch — DPB-depth renegotiation
        // (`pic.rs`).
        let mut vk_plan: Option<DecodePlanVk> = None;
        for attempt in 0..2 {
            // Recreate destroys the old parameters object; an in-flight decode
            // may still execute against it. Drain first. Rare (encoder
            // reconfiguration); the stall is the trade.
            if self
                .state
                .as_ref()
                .expect("ensure_state built it")
                .session
                .parameters_action(&plan.sps, &plan.pps)
                == ParamsAction::Recreate
            {
                self.drain_gpu()?;
            }
            let state = self.state.as_mut().expect("ensure_state built it");
            // SAFETY: live device (constructor contract); the drain above
            // satisfies ensure_parameters' Recreate contract, and Current/Add
            // touch nothing a submitted decode reads.
            unsafe { state.session.ensure_parameters(&plan.sps, &plan.pps)? };
            match plan_to_vk(plan, &mut state.slots, sps_id) {
                Ok(converted) => {
                    vk_plan = Some(converted);
                    break;
                }
                Err(PlanToVkError::CapacityMismatch { required, capacity }) if attempt == 0 => {
                    debug!(
                        required,
                        capacity, "DPB depth renegotiated — rebuilding session"
                    );
                    self.rebuild_state(plan)?;
                }
                Err(e) => return Err(VkDecodeError::Convert(e)),
            }
        }
        let vk_plan = vk_plan.expect("the rebuilt session matches its own plan");

        // `plan_to_vk` committed the setup assignment and withheld
        // `release_after_decode`; the release runs whether or not this submits.
        self.submit_then_release(&vk_plan.release_after_decode, |dec| {
            let op = dec.prepare_op(&vk_plan.refs, vk_plan.setup_slot, vk_plan.setup_ref)?;
            // Bitstream is slice NALUs only. A real AU opens with AUD/SEI
            // (and SPS/PPS at IDRs); feeding those to VCN inside the decode
            // range hangs it. `pack_slices` rebases offsets and puts each NAL
            // behind a three-byte start code.
            let plan_segments: Vec<std::ops::Range<usize>> =
                plan.slices.iter().map(|s| s.nal.clone()).collect();
            let Some(packed) = pack_slices(&plan_segments) else {
                return Err(VkDecodeError::Unsupported(
                    "packed slice data exceeds the u32 offsets Vulkan submits".into(),
                ));
            };
            // SAFETY: each segment is a slice NAL plus the three start-code bytes
            // before it, inside the plan's own in-bounds slice range; the
            // recorded offsets come from the same `pack_slices` call.
            let upload = unsafe { dec.upload(au, &packed.segments)? };
            let scope = dec.scope(&op, &vk_plan.refs)?;
            // Offsets into the packed slices-only buffer, not the plan's
            // AU-absolute offsets — non-slice NALUs were never uploaded.
            let mut picture_info = vk::VideoDecodeH264PictureInfoKHR::default()
                .std_picture_info(&vk_plan.std_pic)
                .slice_offsets(&packed.offsets);
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
                    crop: plan.picture.display_crop,
                    colour: plan.picture.colour,
                    poc: plan.picture.pic_order_cnt,
                    is_idr: plan.picture.is_idr,
                    recovery,
                    decode_order,
                    references_clean: plan.picture.references_clean,
                },
            );
            Ok(())
        })?;

        self.settle(&plan.dpb.outputs, &plan.dpb.removed);
        Ok(self.ready.pop_front())
    }
}

/// `STD_VIDEO_H264_PROFILE_IDC_HIGH`: the profile every punktfunk host encodes.
const H264_PROFILE_HIGH: hh::StdVideoH264ProfileIdc = 100;

/// Map `profile_idc` to the Std code point. Identity for the four
/// Vulkan-representable profiles; reject otherwise.
fn std_profile_for(plan: &AuPlan) -> Result<hh::StdVideoH264ProfileIdc, VkDecodeError> {
    match u32::from(plan.picture.profile_idc) {
        p @ (66 | 77 | 100 | 244) => Ok(p),
        _ => Err(VkDecodeError::Params(ParamsError::UnmappableProfileIdc(
            plan.picture.profile_idc,
        ))),
    }
}

impl ScopeRef for crate::pic::VkRef {
    type Std = hh::StdVideoDecodeH264ReferenceInfo;
    fn slot(&self) -> u8 {
        self.slot
    }
    fn std(&self) -> Self::Std {
        self.std
    }
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;

    use super::*;
    use crate::decoder::core::build_scope;

    /// Fake, never-dereferenced view handle keyed by slot. Lets a scope's
    /// bindings be checked without a device.
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    fn h264_std_ref(frame_num: u16) -> hh::StdVideoDecodeH264ReferenceInfo {
        // SAFETY: StdVideoDecodeH264ReferenceInfo is a plain-C bindgen struct of a
        // bitfield word and integers; all-zero is valid for every field.
        let mut std: hh::StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
        std.FrameNum = frame_num;
        std
    }

    fn h264_ref(slot: u8, frame_num: u16) -> crate::pic::VkRef {
        crate::pic::VkRef {
            slot,
            std: h264_std_ref(frame_num),
            id: u64::from(slot),
        }
    }

    /// Fail closed: an unbound reference slot is `UnboundReferenceSlot`, not a
    /// skipped entry. Hardware would still decode against the missing picture.
    #[test]
    fn an_h264_reference_slot_without_a_bound_image_fails_the_whole_op() {
        let refs = vec![h264_ref(1, 10), h264_ref(3, 20)];
        let slot_refs = vec![Some(h264_std_ref(0)); 8];
        let err = build_scope(
            &refs,
            [1u8, 3].into_iter(),
            0,
            fake_view(0),
            h264_std_ref(30),
            &slot_refs,
            |slot| (slot != 3).then(|| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 3 }),
            "{err}"
        );
    }

    /// `reference_count` is this AU's references only. Counting after the
    /// held-slot pass would let the decode op's reference array run into
    /// unrelated held slots — a picture predicted from something never named.
    #[test]
    fn the_h264_reference_count_covers_the_references_and_never_a_held_slot() {
        // Two references (slots 1, 3); slots 5 and 6 are held but not referenced.
        let refs = vec![h264_ref(1, 10), h264_ref(3, 20)];
        let slot_refs = vec![Some(h264_std_ref(77)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3, 5, 6].into_iter(),
            0,
            fake_view(0),
            h264_std_ref(30),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();

        assert_eq!(reference_count, 2, "exactly this AU's references");
        assert_eq!(
            scope[..reference_count]
                .iter()
                .map(|e| e.slot_index)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "the decode op's reference prefix is the references, in order"
        );
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![1, 3, 5, 6, -1]
        );
    }

    #[test]
    fn std_level_code_points_ascend_so_the_max_level_gate_compares_numerically() {
        use pf_bitstream::h264::Level;
        // Gate is `level_to_std(stream) > caps.max_level_idc.code_point()`.
        // Sound only if Std code points ascend within one codec. Pin the
        // ordering (and the 1b fold onto 1.1).
        let ascending = [
            Level::L1,
            Level::L1_1,
            Level::L2_0,
            Level::L3_1,
            Level::L4,
            Level::L4_2,
            Level::L5_2,
            Level::L6_2,
        ];
        for pair in ascending.windows(2) {
            assert!(
                level_to_std(pair[0]) < level_to_std(pair[1]),
                "{:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(level_to_std(Level::L1B), level_to_std(Level::L1_1));

        let max = level_to_std(Level::L4_1);
        assert!(
            level_to_std(Level::L4) <= max,
            "within the ceiling: allowed"
        );
        assert!(
            level_to_std(Level::L4_2) > max,
            "above the ceiling: Unsupported"
        );
    }
}
