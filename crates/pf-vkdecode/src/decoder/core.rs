//! What the H.264, H.265 and AV1 decoders share: [`VkDecoder`] (lifecycle,
//! status, release, the decode-op recording and its bookkeeping) over a
//! [`VkCodec`], plus the op ring, slot ledgers, recovery latch, and coding
//! scope. A codec plans, converts, and supplies its picture info.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::ops::Range;

use ash::vk;
use pf_bitstream::h264::ColourDescription;
use pf_bitstream::h264::DisplayCrop;
use pf_bitstream::h264::DpbUpdate;
use pf_bitstream::h264::PicId;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use super::DecodeStatus;
use super::DecodedVkFrame;
use super::VkDecodeError;
use super::DECODE_TIMEOUT_NS;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::device::DecodeDevice;
use crate::device::QueueLock;
use crate::device::QueueSubmitGuard;
use crate::images::allow_copy_out;
use crate::images::plan_pools;
use crate::images::DpbPool;
use crate::images::PicturePool;
use crate::ring::BitstreamRing;
use crate::ring::RingLayout;
use crate::ring::UploadedAu;
use crate::ring::INITIAL_SLOT_SIZE;
use crate::ring::RING_SLOTS;
use crate::session::CodecSession;
use crate::slots::SlotMap;

/// One codec [`VkDecoder`] drives: planner and stream state, plus the types and
/// session-shape hooks the shared code needs. Decoding and flushing are the
/// codec's; lifecycle, caps, session shape, status, release and the decode-op
/// recording are shared.
pub trait VkCodec: Sized {
    /// Video session plus the codec's parameters object.
    type Session: CodecSession;
    /// Std reference info cached per DPB slot.
    type StdRef: Copy;
    /// What [`VkDecoder::take_warnings`] returns.
    type Warning;
    /// The planned picture a session is shaped from.
    type Plan;
    /// What caps are cached by and a session is built for; a change rebuilds both.
    type ProfileKey: Copy + Eq;
    /// `VkVideoDecode*DpbSlotInfoKHR`, chained onto every bound slot.
    type DpbSlotInfo<'a>: vk::ExtendsVideoReferenceSlotInfoKHR
    where
        Self: 'a;
    /// [`VkDecoder::debug_snapshot`] prefix.
    const LABEL: &'static str;

    fn dpb_slot_info(std: &Self::StdRef) -> Self::DpbSlotInfo<'_>;
    /// Plan and submit one access unit. [`VkDecoder::decode`] has already run
    /// owed recovery and cleared the warnings. Latch `dec.recovery` on a failure
    /// after planning advanced.
    fn decode_au(
        dec: &mut VkDecoder<Self>,
        au: &[u8],
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError>;
    /// [`VkDecoder::flush`].
    fn flush(dec: &mut VkDecoder<Self>);
    /// [`VkDecoder::forgive_unclean`].
    fn forgive_unclean(&mut self);
    /// Codec state [`VkDecoder::debug_snapshot`] appends, leading space included.
    fn snapshot_flags(&self) -> &'static str {
        ""
    }

    fn profile_key(plan: &Self::Plan) -> Result<Self::ProfileKey, VkDecodeError>;
    fn decode_profile(key: Self::ProfileKey) -> DecodeProfile;
    /// Caps for `key`, derived for the key's output format.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    unsafe fn query_caps(
        dev: &DecodeDevice,
        key: Self::ProfileKey,
    ) -> Result<DecodeCaps, VkDecodeError>;
    /// Refuse caps that derived but that this codec cannot use for `key`.
    fn admit(_key: Self::ProfileKey, _caps: &DecodeCaps) -> Result<(), VkDecodeError> {
        Ok(())
    }
    /// The declared level, in the Std code space of the caps' `max_level_idc`.
    fn stream_level(plan: &Self::Plan) -> u32;
    /// DPB slots a session for `plan` needs, the setup slot included.
    fn required_slots(plan: &Self::Plan) -> u32;
    /// The extent decode writes; pool images round it up to the granularity.
    fn coded_extent(plan: &Self::Plan) -> vk::Extent2D;
    /// A session of `slots` DPB slots at `extent` for `key`.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    unsafe fn create_session(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        key: Self::ProfileKey,
        slots: u32,
        extent: vk::Extent2D,
    ) -> Result<Self::Session, VkDecodeError>;
}

/// One session generation. Extent / DPB / profile renegotiation retires it.
pub(crate) struct SessionState<C: VkCodec> {
    pub(crate) session: C::Session,
    pub(crate) slots: SlotMap,
    /// Distinct-mode reference-only DPB; `None` in coincide (picture pool backs
    /// the DPB).
    dpb: Option<DpbPool>,
    pub(crate) pool: PicturePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Last Std reference info per DPB slot. `vkCmdBeginVideoCodingKHR` wants
    /// codec info for every bound slot, including ones this AU does not
    /// reference. Refreshed from each plan so long-term promotions land.
    slot_refs: Vec<Option<C::StdRef>>,
    /// Coincide: pool picture bound to each DPB slot (rebound at activation).
    slot_image: Vec<Option<usize>>,
    /// Per command-buffer completion tokens (reuse gate).
    cmd_marks: Vec<Option<(vk::Semaphore, u64)>>,
    /// Per query-slot submission ordinals (staleness validation).
    query_marks: Vec<u64>,
    /// Submissions on this session (cmd/query indexing).
    submitted: u64,
    /// Newest submission's completion token (session drain).
    last_submit: Option<(vk::Semaphore, u64)>,
    /// The profile the session was built for (renegotiation comparison).
    key: C::ProfileKey,
    /// Stream coded extent (renegotiation comparison).
    pub(crate) coded_extent: vk::Extent2D,
    /// Granularity-aligned allocation extent (picture resources + frames).
    image_extent: vk::Extent2D,
}

impl<C: VkCodec> SessionState<C> {
    /// Pools, bitstream ring and op ring around a freshly created `session`,
    /// for `required_slots` DPB slots at `image_extent`, with `dec`'s export
    /// and copy-out choices.
    ///
    /// # Safety
    ///
    /// `dec.dev` wraps live handles ([`crate::DeviceHandles`] contract), and
    /// `session` was created on it for `key`.
    unsafe fn create(
        dec: &VkDecoder<C>,
        caps: &DecodeCaps,
        session: C::Session,
        key: C::ProfileKey,
        required_slots: u32,
        coded_extent: vk::Extent2D,
        image_extent: vk::Extent2D,
    ) -> Result<Self, VkDecodeError> {
        let profile = C::decode_profile(key);
        let mut pool_plan = plan_pools(caps, required_slots);
        // Test-only: `gpu_parity` copies pictures to the host, and
        // `vkCmdCopyImageToBuffer` needs TRANSFER_SRC — a bit production
        // pools omit. Opt-in via env so no production path grows it.
        if std::env::var("PF_VKD_TEST_READBACK").is_ok_and(|v| v == "1") {
            pool_plan.picture_usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let dev = &dec.dev;
        if dec.copy_out {
            // SAFETY: fn contract; a physical-device format query.
            unsafe { allow_copy_out(dev, profile, &mut pool_plan, caps.output_format) };
        }
        // SAFETY: fn contract, for every create in this block; each created half
        // is owned by a Drop type the moment it exists, so a mid-build failure
        // unwinds cleanly.
        let (dpb, pool, ring, ops) = unsafe {
            let dpb = if caps.coincide {
                None
            } else {
                Some(
                    DpbPool::create(dev, caps, &pool_plan, image_extent, profile)
                        .map_err(VkDecodeError::from)?,
                )
            };
            let pool = PicturePool::create(dev, caps, &pool_plan, image_extent, profile)
                .map_err(VkDecodeError::from)?;
            let ring = BitstreamRing::create(
                dev,
                RingLayout::new(
                    INITIAL_SLOT_SIZE,
                    RING_SLOTS,
                    caps.min_bitstream_offset_alignment,
                    caps.min_bitstream_size_alignment,
                ),
                profile,
                dec.export_bitstream,
            )
            .map_err(VkDecodeError::from)?;
            let ops = OpRing::create(dev, profile, pool_plan.picture_count, RING_SLOTS)
                .map_err(VkDecodeError::from)?;
            (dpb, pool, ring, ops)
        };
        Ok(Self {
            session,
            // `SlotMap` adds the setup slot back: capacity is `required_slots`.
            slots: SlotMap::new(required_slots as usize - 1),
            slot_refs: vec![None; required_slots as usize],
            slot_image: vec![None; required_slots as usize],
            cmd_marks: vec![None; RING_SLOTS as usize],
            query_marks: vec![u64::MAX; pool_plan.picture_count as usize],
            submitted: 0,
            last_submit: None,
            key,
            coded_extent,
            image_extent,
            dpb,
            pool,
            ring,
            ops,
        })
    }

    /// Picture resource view for DPB `slot`: bound pool picture layer (coincide)
    /// or DPB array layer (distinct). `None` when a coincide slot has no binding.
    fn slot_view(&self, slot: u8) -> Option<vk::ImageView> {
        match &self.dpb {
            Some(dpb) => Some(dpb.dpb_view(slot)),
            None => self.slot_image[usize::from(slot)].map(|p| self.pool.pictures[p].view),
        }
    }
}

/// Allocation extent for stream extent `coded`: rounded up to the access
/// granularity, and refused unless the device's coded-extent range covers it.
pub(crate) fn session_extent(
    caps: &DecodeCaps,
    coded: vk::Extent2D,
) -> Result<vk::Extent2D, VkDecodeError> {
    // Bounds-check the allocation extent (granularity-rounded): that is
    // what images are created at and what maxCodedExtent must cover.
    let image_extent = caps.aligned_extent(coded);
    if coded.width < caps.min_coded_extent.width
        || coded.height < caps.min_coded_extent.height
        || image_extent.width > caps.max_coded_extent.width
        || image_extent.height > caps.max_coded_extent.height
    {
        return Err(VkDecodeError::Unsupported(format!(
            "coded extent {}x{} (allocated {}x{}) outside device range {}x{}..{}x{}",
            coded.width,
            coded.height,
            image_extent.width,
            image_extent.height,
            caps.min_coded_extent.width,
            caps.min_coded_extent.height,
            caps.max_coded_extent.width,
            caps.max_coded_extent.height
        )));
    }
    Ok(image_extent)
}

/// One decode op's claims, from [`VkDecoder::prepare_op`] to
/// [`VkDecoder::commit_op`]: destination picture, the timeline waits it owes,
/// its ring slots, and the setup slot it activates.
pub(crate) struct Op<S> {
    pub(crate) dst: usize,
    waits: Vec<(vk::Semaphore, u64)>,
    pub(crate) signal_value: u64,
    pub(crate) submission: u64,
    cmd_index: usize,
    pub(crate) query_index: u32,
    setup_slot: u8,
    setup_ref: S,
}

/// Native Vulkan Video decoder for one codec ([`VkCodec`]):
/// [`crate::VkH264Decoder`], [`crate::VkH265Decoder`] or [`crate::VkAv1Decoder`].
///
/// Frames, status, release and teardown are codec-agnostic. Every
/// [`DecodedVkFrame`] from `decode` / `take_ready` comes back through
/// [`Self::release_frame`] exactly once.
pub struct VkDecoder<C: VkCodec> {
    pub(crate) dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    /// Planner and the codec's stream state.
    pub(crate) codec: C,
    /// Caps of the last profile asked for, queried once per profile.
    pub(crate) caps: Option<(C::ProfileKey, DecodeCaps)>,
    /// The over-ceiling level warning has fired: the level is the stream's, so
    /// once per decoder.
    level_advisory_warned: bool,
    pub(crate) state: Option<SessionState<C>>,
    /// Pictures awaiting their planner output verdict, keyed by planner id.
    pub(crate) pending: BTreeMap<PicId, PendingPic>,
    /// Display-ready, not yet handed out. An AU usually fills one; flushes,
    /// reorder bursts and AV1 temporal units can fill more.
    pub(crate) ready: VecDeque<DecodedVkFrame>,
    /// Retired generations' pools with consumer-held images (die on last token).
    graveyard: Vec<RetiredPool>,
    /// Most recent plan warnings ([`Self::take_warnings`]).
    pub(crate) last_warnings: Vec<C::Warning>,
    /// Post-failure DPB recovery owed ([`RecoveryLatch`]).
    pub(crate) recovery: RecoveryLatch,
    /// Pictures planned so far — stamped as [`DecodedVkFrame::decode_order`].
    /// Survives session rebuilds: it describes the stream, not Vulkan objects.
    pub(crate) decoded: u64,
    /// Bumped on every rebuild; stamped into frames.
    generation: u64,
    device_lost: bool,
    /// [`Self::export_bitstream`].
    export_bitstream: bool,
    /// [`Self::copy_out`].
    copy_out: bool,
}

impl<C: VkCodec> VkDecoder<C> {
    /// Decoder over a wrapped device. Sessions and pools are built lazily from
    /// the first AU (their shape is the stream's, not the device's).
    pub(crate) fn with_codec(dev: DecodeDevice, lock: Box<dyn QueueLock>, codec: C) -> Self {
        Self {
            dev,
            lock,
            codec,
            caps: None,
            level_advisory_warned: false,
            state: None,
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
            graveyard: Vec::new(),
            last_warnings: Vec::new(),
            recovery: RecoveryLatch::default(),
            decoded: 0,
            generation: 0,
            device_lost: false,
            export_bitstream: false,
            copy_out: false,
        }
    }

    /// Export the bitstream ring as a dma-buf from the next session on, for an owner that
    /// waits the decode through the kernel ([`Self::bitstream_dmabuf`]).
    pub fn export_bitstream(&mut self) {
        self.export_bitstream = true;
    }

    /// From the next session on, give pictures TRANSFER_SRC where the driver answers for
    /// it, so the owner can copy them out ([`DecodedVkFrame::copyable`]).
    pub fn copy_out(&mut self) {
        self.copy_out = true;
    }

    /// The bitstream ring's dma-buf, while its backing lives; every decode writes fences
    /// onto it. `None` before the first session or without an export.
    #[cfg(unix)]
    pub fn bitstream_dmabuf(&self) -> Option<std::os::fd::RawFd> {
        self.state.as_ref().and_then(|s| s.ring.dmabuf_fd())
    }

    /// Decode one access unit and return the next display-ready frame; drain
    /// the rest with [`Self::take_ready`]. An AV1 access unit is a temporal
    /// unit and can carry several frames. An H.265 RASL skipped after an
    /// open-GOP join is `Ok`, not an error: the host has no keyframe to send.
    ///
    /// A failure after planning advanced latches recovery: the next call first
    /// flushes the DPB to the next random-access point. Never panics.
    /// `VkDecodeError::DeviceLost` latches: later calls fail fast until the
    /// owner rebuilds on fresh handles.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        if self.device_lost {
            return Err(VkDecodeError::DeviceLost);
        }
        // Recover before planning: a stranded DPB picture would fail every later
        // reference ([`RecoveryLatch`]).
        if self.recovery.take() {
            self.recover_dpb();
        }
        // "Cleared by the next decode": clear before planning so a failed plan
        // cannot leave the previous AU's warnings to be re-read as damage.
        self.last_warnings.clear();
        let result = C::decode_au(self, au);
        if matches!(result, Err(VkDecodeError::DeviceLost)) {
            self.device_lost = true;
        }
        result
    }

    /// Drain the planner (teardown / discontinuity). Buffered pictures become
    /// display-ready via [`Self::take_ready`]; DPB slots free; never-output
    /// pictures free their images. AV1 has no reorder buffer: it drops pending
    /// hidden frames and waits for the next key frame.
    pub fn flush(&mut self) {
        C::flush(self);
    }

    /// Hand a delivered frame back. `presenter_signaled` is whether the consumer
    /// sampled the image and enqueued `value + 1` per [`DecodedVkFrame`]. The
    /// decoder then waits that write-back before reuse. Every `decode` /
    /// `take_ready` frame must come back once, including stale-generation ones.
    pub fn release_frame(
        &mut self,
        frame: &DecodedVkFrame,
        presenter_signaled: bool,
    ) -> Result<(), VkDecodeError> {
        let pool = if frame.generation == self.generation {
            match &mut self.state {
                Some(state) => &mut state.pool,
                None => {
                    return Err(VkDecodeError::StaleFrame {
                        frame_generation: frame.generation,
                        current_generation: self.generation,
                    })
                }
            }
        } else {
            match self
                .graveyard
                .iter_mut()
                .find(|r| r.generation == frame.generation)
            {
                Some(retired) => &mut retired.pool,
                None => {
                    return Err(VkDecodeError::StaleFrame {
                        frame_generation: frame.generation,
                        current_generation: self.generation,
                    })
                }
            }
        };
        let index = frame.picture as usize;
        if index >= pool.pictures.len() {
            return Err(VkDecodeError::StaleFrame {
                frame_generation: frame.generation,
                current_generation: self.generation,
            });
        }
        let picture = &mut pool.pictures[index];
        match picture.held.checked_sub(1) {
            Some(remaining) => picture.held = remaining,
            None => {
                debug!(index, "frame released more often than delivered");
                return Ok(());
            }
        }
        if presenter_signaled {
            picture.value = picture.value.max(frame.value + 1);
        }
        // Retired pool dies on its last token. Presenter fence-waited before
        // the token; decode work drained at retirement.
        if frame.generation != self.generation {
            self.graveyard
                .retain(|r| r.generation != frame.generation || r.pool.held_total() > 0);
        }
        Ok(())
    }

    /// A display-ready frame beyond the one `decode` returned, if any. Drain
    /// after every decode; leftover frames still occupy pool pictures.
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// Warnings of the most recent planned AU (concealment / want_keyframe),
    /// decode order. Cleared by the next `decode`.
    pub fn take_warnings(&mut self) -> Vec<C::Warning> {
        std::mem::take(&mut self.last_warnings)
    }

    /// Forget the planner's unclean marks after a freeze lift on intra refresh marks
    /// ([`pf_bitstream::clean::CleanLedger::clear`]).
    pub fn forgive_unclean(&mut self) {
        self.codec.forgive_unclean();
    }

    /// Session generation stamped onto newly delivered frames.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Decode-order ordinal of the most recently planned picture. Compare
    /// [`DecodedVkFrame::decode_order`] against this to tell pre-loss from
    /// post-loss: recovery flushes buffered pictures into `ready` at once, so a
    /// pre-loss picture can arrive after the loss. 0 before the first AU plans;
    /// AV1 `show_existing_frame` does not advance it.
    pub fn decode_order(&self) -> u64 {
        self.decoded
    }

    /// One-line state snapshot for failure paths. Not a stable format.
    pub fn debug_snapshot(&self) -> String {
        // Next to a failure: the next decode flushes rather than resuming
        // ([`RecoveryLatch`]).
        let recovery = if self.recovery.is_latched() {
            " recovery=owed"
        } else {
            ""
        };
        let flags = self.codec.snapshot_flags();
        match &self.state {
            None => format!(
                "{} gen={}{recovery}{flags} <no session>",
                C::LABEL,
                self.generation
            ),
            Some(state) => {
                let occupancy: Vec<String> = state
                    .pool
                    .pictures
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        format!(
                            "{i}:{}{}h{}",
                            if p.bound { "B" } else { "-" },
                            if p.pending { "P" } else { "-" },
                            p.held
                        )
                    })
                    .collect();
                format!(
                    "{} gen={}{recovery}{flags} mode={} slots_held={}/{} pool=[{}] \
                     pending={} ready={} graveyard={}",
                    C::LABEL,
                    self.generation,
                    if state.dpb.is_none() {
                        "coincide"
                    } else {
                        "distinct"
                    },
                    state.slots.active(),
                    state.slots.capacity(),
                    occupancy.join(" "),
                    self.pending.len(),
                    self.ready.len(),
                    self.graveyard.len(),
                )
            }
        }
    }

    /// Read `frame`'s decode status without waiting.
    ///
    /// [`DecodeStatus::Failed`] covers driver errors and a query slot re-armed
    /// before it was read (unprovable → same conservative verdict).
    ///
    /// Without `queryResultStatusSupport`, `Ok` means the op completed on the
    /// timeline — the same information FFmpeg has on every driver.
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    /// Whether this decode queue family answers per-op `RESULT_STATUS` queries
    /// (`queryResultStatusSupport`).
    ///
    /// When true, [`DecodeStatus::Failed`] is the driver's verdict. When false
    /// (RADV hangs the VCN if a query is recorded), `Ok` means timeline
    /// completion: unmeasured, not clean.
    pub fn status_queries(&self) -> bool {
        self.dev.result_status_queries()
    }

    /// [`Self::poll_status`], but waits for the op first. The only blocking
    /// status read (GPU smoke assertions; integration polls).
    pub fn wait_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, true)
    }

    fn read_status(&mut self, frame: &DecodedVkFrame, block: bool) -> DecodeStatus {
        if frame.generation != self.generation {
            trace!(
                frame_generation = frame.generation,
                current = self.generation,
                "status asked for a stale-generation frame — Failed, without \
                 touching the new pools"
            );
            return DecodeStatus::Failed;
        }
        let Some(state) = &self.state else {
            return DecodeStatus::Failed;
        };
        let Some(query_pool) = state.ops.query_pool else {
            // No queries on this driver: verdict degrades to timeline completion.
            if block {
                // SAFETY: live device; pool-owned semaphore.
                return match unsafe {
                    wait_timeline(self.dev.ash(), frame.semaphore, frame.value, "status wait")
                } {
                    Ok(()) => DecodeStatus::Ok,
                    Err(VkDecodeError::DeviceLost) => {
                        self.device_lost = true;
                        DecodeStatus::Failed
                    }
                    Err(_) => DecodeStatus::Failed,
                };
            }
            // SAFETY: live device; pool-owned semaphore.
            return match unsafe { self.dev.ash().get_semaphore_counter_value(frame.semaphore) } {
                Ok(current) if current >= frame.value => DecodeStatus::Ok,
                Ok(_) => DecodeStatus::Pending,
                Err(vk::Result::ERROR_DEVICE_LOST) => {
                    self.device_lost = true;
                    DecodeStatus::Failed
                }
                Err(_) => DecodeStatus::Failed,
            };
        };
        let slot = frame.query_slot as usize;
        if slot >= state.query_marks.len() || state.query_marks[slot] != frame.submission {
            trace!(
                slot,
                "status query slot re-armed before it was read — unprovable, reported Failed"
            );
            return DecodeStatus::Failed;
        }
        let flags = if block {
            vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR
        } else {
            vk::QueryResultFlags::WITH_STATUS_KHR
        };
        let mut status = [0i32; 1];
        // SAFETY: live device; the query pool is this session generation's own and
        // `frame.query_slot` indexes within its count (checked above against the
        // marks array it is sized to).
        let result = unsafe {
            self.dev
                .ash()
                .get_query_pool_results(query_pool, frame.query_slot, &mut status, flags)
        };
        match result {
            // VkQueryResultStatusKHR: >0 complete, 0 not ready, <0 error.
            Ok(()) if status[0] > 0 => DecodeStatus::Ok,
            Ok(()) if status[0] == 0 => DecodeStatus::Pending,
            Ok(()) => DecodeStatus::Failed,
            Err(vk::Result::NOT_READY) => DecodeStatus::Pending,
            Err(vk::Result::ERROR_DEVICE_LOST) => {
                self.device_lost = true;
                DecodeStatus::Failed
            }
            Err(r) => {
                debug!(?r, "status query read failed");
                DecodeStatus::Failed
            }
        }
    }

    /// Wait up to `timeout_ns` for [`DecodedVkFrame::semaphore`] to reach
    /// [`DecodedVkFrame::value`]. Measurement only: a timeout degrades the
    /// latency stat; the consumer's GPU wait gates sampling. `frame` must be
    /// unreleased so the semaphore stays alive. Stale-generation declines.
    pub fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        if frame.generation != self.generation {
            return false;
        }
        let semaphores = [frame.semaphore];
        let values = [frame.value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        // SAFETY: live device (constructor contract); the semaphore is a pool
        // semaphore the unreleased frame keeps alive (fn docs); the info arrays
        // are locals outliving the call.
        unsafe { self.dev.ash().wait_semaphores(&info, timeout_ns) }.is_ok()
    }

    /// Clear DPB state a failed AU left so planning resumes at the next
    /// random-access point.
    ///
    /// After a post-planning failure three ledgers disagree: the planner DPB,
    /// [`SlotMap`], and slot→picture bindings. [`Self::flush`] settles the first
    /// (and still delivers pictures that reached output);
    /// [`Self::reset_slot_ledgers`] empties the other two. Images a consumer
    /// holds stay pinned by `held`, as across a rebuild.
    ///
    /// Not a session rebuild: session, pools, and ring are still valid.
    fn recover_dpb(&mut self) {
        debug!(
            snapshot = %self.debug_snapshot(),
            "recovering from a failed AU — flushing to the next random-access point"
        );
        self.flush();
        self.reset_slot_ledgers();
    }

    /// Empty the slot map, the slot→picture bindings and the cached reference
    /// info; the pictures those bindings pinned are free again.
    pub(crate) fn reset_slot_ledgers(&mut self) {
        if let Some(state) = &mut self.state {
            let unbound = reset_slot_bindings(
                &mut state.slots,
                &mut state.slot_image,
                &mut state.slot_refs,
            );
            for picture in unbound {
                state.pool.pictures[picture].bound = false;
            }
        }
    }

    /// Settle a planner flush: buffered pictures become display-ready,
    /// `update`'s removals free their slots, never-output pictures free their
    /// images.
    pub(crate) fn flush_dpb(&mut self, update: &DpbUpdate) {
        let (ready, dropped) = settle_dpb(&mut self.pending, update);
        if let Some(state) = &mut self.state {
            state.slots.apply(update);
            for entry in ready {
                let frame = build_frame(
                    &mut state.pool,
                    state.dpb.is_none(),
                    state.image_extent,
                    &entry,
                    self.generation,
                );
                self.ready.push_back(frame);
            }
            for entry in dropped {
                state.pool.pictures[entry.image].pending = false;
            }
            // A pending picture neither output nor removed should not exist
            // after a flush; free leftovers.
            for (_, entry) in std::mem::take(&mut self.pending) {
                debug!(poc = entry.poc, "pending picture survived a flush — freed");
                state.pool.pictures[entry.image].pending = false;
            }
        } else {
            self.pending.clear();
        }
    }

    /// Outputs become ready frames (pending → held); removed-but-never-output
    /// pictures free their images.
    pub(crate) fn settle(&mut self, outputs: &[PicId], removed: &[PicId]) {
        let (ready, dropped) = settle_dpb_ids(&mut self.pending, outputs, removed);
        let Some(state) = self.state.as_mut() else {
            return;
        };
        for entry in ready {
            let frame = build_frame(
                &mut state.pool,
                state.dpb.is_none(),
                state.image_extent,
                &entry,
                self.generation,
            );
            self.ready.push_back(frame);
        }
        for entry in dropped {
            debug!(
                poc = entry.poc,
                "picture removed without output — freeing its image"
            );
            state.pool.pictures[entry.image].pending = false;
        }
    }

    /// Caps and session match `plan`'s profile and extent, or the session is
    /// rebuilt. A declared level above the device ceiling warns once and
    /// proceeds: encoders over-declare, and no level above the ceiling reaches
    /// the driver. DPB depth and extent are the real limits, checked in
    /// [`Self::rebuild_state`].
    pub(crate) fn ensure_state(&mut self, plan: &C::Plan) -> Result<(), VkDecodeError> {
        let key = C::profile_key(plan)?;
        if self.caps.as_ref().map(|(cached, _)| *cached) != Some(key) {
            // SAFETY: live device (constructor contract).
            let caps = unsafe { C::query_caps(&self.dev, key)? };
            self.caps = Some((key, caps));
        }
        let caps = &self.caps.as_ref().expect("queried above").1;
        C::admit(key, caps)?;
        let ceiling = caps.max_level_idc;
        let stream_level = C::stream_level(plan);
        if stream_level > ceiling.code_point() && !self.level_advisory_warned {
            self.level_advisory_warned = true;
            warn!(
                stream_level,
                %ceiling,
                "stream declares a level above the device ceiling; the declared level is \
                 advisory (encoders over-declare), so decode proceeds and no level above \
                 the ceiling reaches the driver"
            );
        }
        let coded = C::coded_extent(plan);
        match &self.state {
            Some(state) if state.coded_extent == coded && state.key == key => Ok(()),
            _ => self.rebuild_state(plan),
        }
    }

    /// Caps for `key` derive on this device: [`Self::ensure_state`]'s query,
    /// asked before any AU.
    pub(crate) fn probe_key(&self, key: C::ProfileKey) -> Result<(), VkDecodeError> {
        // SAFETY: the constructor's `DeviceHandles` contract holds for this
        // decoder's whole lifetime, so the physical device is live.
        unsafe { C::query_caps(&self.dev, key)? };
        Ok(())
    }

    /// Retire the current generation ([`Self::retire_state`]) and build a fresh
    /// one shaped by `plan`, on the caps [`Self::ensure_state`] cached.
    pub(crate) fn rebuild_state(&mut self, plan: &C::Plan) -> Result<(), VkDecodeError> {
        self.retire_state()?;
        let (key, caps) = self.caps.as_ref().expect("ensure_state queried caps");
        let key = *key;
        let required_slots = C::required_slots(plan);
        if required_slots > caps.max_dpb_slots {
            return Err(VkDecodeError::Unsupported(format!(
                "stream needs {required_slots} DPB slots, device caps at {}",
                caps.max_dpb_slots
            )));
        }
        let coded = C::coded_extent(plan);
        let image_extent = session_extent(caps, coded)?;
        // SAFETY: live device per the constructor contract; the session is
        // owned by a Drop type the moment it exists.
        let state = unsafe {
            let session = C::create_session(&self.dev, caps, key, required_slots, image_extent)?;
            SessionState::create(
                self,
                caps,
                session,
                key,
                required_slots,
                coded,
                image_extent,
            )?
        };
        self.state = Some(state);
        Ok(())
    }

    /// Tear down the current generation: drain decode work, retire the picture
    /// pool to the graveyard if the consumer still holds images, and bump
    /// [`Self::generation`] so old frames route there.
    ///
    /// A pool with holds retires intact until `release_frame` takes its last
    /// token (sent only after the presenter's sampling fence). Tokens carry
    /// generation, so releases cannot alias. Session/ring/ops die here after
    /// [`Self::drain_gpu`]; [`DecodedVkFrame`] borrows pool resources only,
    /// and `poll_status` generation-gates before touching the new query pool.
    pub(crate) fn retire_state(&mut self) -> Result<(), VkDecodeError> {
        self.drain_gpu()?;
        if let Some(state) = self.state.take() {
            debug!(
                codec = C::LABEL,
                "rebuilding decode session (stream renegotiation)"
            );
            // SessionState has no Drop: session/dpb/ring/ops die here (decode
            // drained; presenter never references them). The picture pool may
            // outlive: drop undelivered holds, free pending, graveyard if the
            // consumer still holds delivered images.
            let SessionState { mut pool, .. } = state;
            for frame in self.ready.drain(..) {
                let picture = &mut pool.pictures[frame.picture as usize];
                picture.held = picture.held.saturating_sub(1);
            }
            for (_, entry) in std::mem::take(&mut self.pending) {
                pool.pictures[entry.image].pending = false;
            }
            for picture in &mut pool.pictures {
                picture.bound = false;
            }
            let held = pool.held_total();
            if held > 0 {
                debug!(
                    held,
                    generation = self.generation,
                    "consumer still holds images of the retired generation — graveyarding"
                );
                self.graveyard.push(RetiredPool {
                    generation: self.generation,
                    pool,
                });
            }
        }
        self.generation += 1;
        Ok(())
    }

    /// Wait out every in-flight decode submission of the current session.
    pub(crate) fn drain_gpu(&mut self) -> Result<(), VkDecodeError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        if let Some((sem, value)) = state.last_submit {
            // SAFETY: live device; the token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "session drain")? };
        }
        Ok(())
    }

    /// Claim one decode op before its bitstream uploads: check the reference
    /// count, unbind stale coincide slots, pick a free destination picture and
    /// the timeline waits it owes, and take the op's command/query ring slots.
    pub(crate) fn prepare_op<R: ScopeRef<Std = C::StdRef>>(
        &mut self,
        refs: &[R],
        setup_slot: u8,
        setup_ref: C::StdRef,
    ) -> Result<Op<C::StdRef>, VkDecodeError> {
        let state = self.state.as_mut().expect("ensure_state built it");
        // Session was created with maxActiveReferencePictures; binding more
        // in one op is a silent VUID violation on the drivers that matter.
        let max_active = state.session.max_active_references() as usize;
        if refs.len() > max_active {
            return Err(VkDecodeError::Unsupported(format!(
                "AU references {} pictures, session allows {max_active} active references",
                refs.len()
            )));
        }

        // Coincide: released slots unbind (pictures may still be pending/held).
        // Clear the setup slot's previous binding before it binds fresh.
        if state.dpb.is_none() {
            let unbound = sync_slot_bindings(&state.slots, &mut state.slot_image, setup_slot);
            for picture in unbound {
                state.pool.pictures[picture].bound = false;
            }
        }

        // Free pool picture, never one a consumer holds. Exhaustion means the
        // consumer owes HOLD_HEADROOM releases; no wait frees a picture here.
        let Some(dst) = state.pool.free_index() else {
            debug!(
                held = state.pool.held_total(),
                "picture pool exhausted — release_frame owed"
            );
            return Err(VkDecodeError::NoFreeSlot);
        };

        // Cross-queue waits: dst's last timeline (presenter write-back after
        // release), plus — coincide — every referenced image, so reference
        // reads order after a reported layout restore.
        let mut waits: Vec<(vk::Semaphore, u64)> = Vec::new();
        {
            let dst_pic = &state.pool.pictures[dst];
            if dst_pic.value > 0 {
                waits.push((dst_pic.semaphore, dst_pic.value));
            }
        }
        if state.dpb.is_none() {
            for r in refs {
                if let Some(picture) = state.slot_image[usize::from(r.slot())] {
                    let pic = &state.pool.pictures[picture];
                    if pic.value > 0 && !waits.iter().any(|(sem, _)| *sem == pic.semaphore) {
                        waits.push((pic.semaphore, pic.value));
                    }
                }
            }
        }
        let signal_value = state.pool.pictures[dst].value + 1;

        let submission = state.submitted;
        let cmd_index = (submission % state.ops.cmds.len() as u64) as usize;
        if let Some((sem, value)) = state.cmd_marks[cmd_index] {
            // SAFETY: live device; the token is a pool picture's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "command buffer reuse")? };
        }
        let query_index = (submission % u64::from(state.ops.query_count)) as u32;
        Ok(Op {
            dst,
            waits,
            signal_value,
            submission,
            cmd_index,
            query_index,
            setup_slot,
            setup_ref,
        })
    }

    /// Copy `segments` of `au` into the next bitstream ring slot, first waiting
    /// out a slot whose previous decode still reads it.
    ///
    /// # Safety
    ///
    /// `segments` are in-bounds ranges of `au`, and the offsets the op records
    /// come from the same packer call.
    pub(crate) unsafe fn upload(
        &mut self,
        au: &[u8],
        segments: &[Range<usize>],
    ) -> Result<UploadedAu, VkDecodeError> {
        let device = self.dev.ash().clone();
        let mut poll = |token: &(vk::Semaphore, u64)| -> Result<bool, VkDecodeError> {
            // SAFETY: live device; the token's semaphore is a pool semaphore.
            let current = unsafe { device.get_semaphore_counter_value(token.0) }
                .map_err(VkDecodeError::from)?;
            Ok(current >= token.1)
        };
        let device2 = self.dev.ash().clone();
        let mut wait = |token: &(vk::Semaphore, u64)| -> Result<(), VkDecodeError> {
            // SAFETY: as above.
            unsafe { wait_timeline(&device2, token.0, token.1, "bitstream slot drain") }
        };
        let state = self
            .state
            .as_mut()
            .expect("prepare_op ran on this generation");
        // SAFETY: live device; fn contract for `segments`; every pending token is
        // the completion signal of the submission that consumed the slot.
        unsafe {
            state
                .ring
                .upload(&self.dev, au, segments, &mut poll, &mut wait)
        }
    }

    /// Coding scope for `op`: this AU's references, the other held slots, then
    /// the setup activation ([`build_scope`]). Fails closed on an unbound
    /// reference, before any command is recorded.
    pub(crate) fn scope<R: ScopeRef<Std = C::StdRef>>(
        &self,
        op: &Op<C::StdRef>,
        refs: &[R],
    ) -> Result<Scope<C::StdRef>, VkDecodeError> {
        let state = self
            .state
            .as_ref()
            .expect("prepare_op ran on this generation");
        // Setup/dst: fresh pool picture layer (coincide) or DPB layer (distinct).
        let setup_view = match &state.dpb {
            None => state.pool.pictures[op.dst].view,
            Some(dpb) => dpb.dpb_view(op.setup_slot),
        };
        let held: Vec<u8> = state.slots.held().map(|(slot, _id)| slot).collect();
        build_scope(
            refs,
            held.into_iter(),
            op.setup_slot,
            setup_view,
            op.setup_ref,
            &state.slot_refs,
            |slot| state.slot_view(slot),
        )
    }

    /// Record one decode op and submit it under the queue lock: image waits per
    /// the pool contract, dst timeline signal at `op.signal_value`. The codec
    /// supplies only its picture info, chained onto the decode info.
    ///
    /// # Safety
    ///
    /// `op`, `scope` and `upload` came from [`Self::prepare_op`],
    /// [`Self::scope`] and [`Self::upload`] for one plan on this generation:
    /// the command buffer's previous submission completed, `op.dst` is a free
    /// pool picture, and the bitstream sits in `upload`'s ring slot.
    pub(crate) unsafe fn record_and_submit<P: vk::ExtendsVideoDecodeInfoKHR>(
        &mut self,
        op: &Op<C::StdRef>,
        scope: &Scope<C::StdRef>,
        picture_info: &mut P,
        upload: &UploadedAu,
    ) -> Result<(), VkDecodeError> {
        let dev = &self.dev;
        let state = self
            .state
            .as_mut()
            .expect("prepare_op ran on this generation");
        let (scope, reference_count) = (&scope.0, scope.1);
        let device = dev.ash();
        let cmd = state.ops.cmds[op.cmd_index];
        let coded_extent = state.coded_extent;
        let coincide = state.dpb.is_none();

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: the buffer's previous submission completed (fn contract) and its
        // pool allows per-buffer reset, so begin implicitly resets it.
        unsafe {
            device
                .begin_command_buffer(cmd, &begin_info)
                .map_err(VkDecodeError::from)?
        };

        // Prior reconstructions must be visible to this op's reference reads.
        let memory_barriers = [vk::MemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            )];
        // Decode targets are fully overwritten: discard via UNDEFINED with an
        // execution+memory dependency on earlier ops that touched them.
        let decode_layer_barrier = |image: vk::Image, layer: u32, new_layout: vk::ImageLayout| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                .src_access_mask(
                    vk::AccessFlags2::VIDEO_DECODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
                )
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
                .dst_access_mask(
                    vk::AccessFlags2::VIDEO_DECODE_READ_KHR
                        | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
                )
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(new_layout)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: layer,
                    layer_count: 1,
                })
        };
        let dst_picture = &state.pool.pictures[op.dst];
        let dst_image = dst_picture.image;
        let mut image_barriers = Vec::new();
        if coincide {
            // Coincide: dst pool layer is the setup DPB picture.
            image_barriers.push(decode_layer_barrier(
                dst_image,
                dst_picture.layer,
                vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
            ));
        } else {
            let dpb = state.dpb.as_ref().expect("distinct mode");
            let (setup_image, setup_layer) = dpb.dpb_target(op.setup_slot);
            image_barriers.push(decode_layer_barrier(
                setup_image,
                setup_layer,
                vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
            ));
            image_barriers.push(decode_layer_barrier(
                dst_image,
                dst_picture.layer,
                vk::ImageLayout::VIDEO_DECODE_DST_KHR,
            ));
        }
        let dependency = vk::DependencyInfo::default()
            .memory_barriers(&memory_barriers)
            .image_memory_barriers(&image_barriers);
        // SAFETY: recording into the begun buffer; synchronization2 is enabled per
        // the DeviceHandles feature contract.
        unsafe { device.cmd_pipeline_barrier2(cmd, &dependency) };

        // Reset the status query before the coding scope. None without
        // queryResultStatusSupport (RADV hangs the VCN if a query is recorded).
        if let Some(query_pool) = state.ops.query_pool {
            // SAFETY: recording; `query_index` is within the pool's count (fn contract).
            unsafe { device.cmd_reset_query_pool(cmd, query_pool, op.query_index, 1) };
        }

        // resources → std infos → codec slot infos → slot infos. Each vector is
        // finished before the next borrows it, so nothing reallocates under a pointer.
        let resources: Vec<vk::VideoPictureResourceInfoKHR<'_>> = scope
            .iter()
            .map(|e| {
                vk::VideoPictureResourceInfoKHR::default()
                    .coded_extent(coded_extent)
                    .base_array_layer(0)
                    .image_view_binding(e.view)
            })
            .collect();
        let std_refs: Vec<C::StdRef> = scope.iter().map(|e| e.std).collect();
        let mut dpb_infos: Vec<C::DpbSlotInfo<'_>> =
            std_refs.iter().map(C::dpb_slot_info).collect();
        let mut begin_slots: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
            Vec::with_capacity(scope.len());
        for (index, entry) in scope.iter().enumerate() {
            begin_slots.push(
                vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(entry.slot_index)
                    .picture_resource(&resources[index]),
            );
        }
        for (slot_info, dpb_info) in begin_slots.iter_mut().zip(dpb_infos.iter_mut()) {
            *slot_info = (*slot_info).push_next(dpb_info);
        }
        // Decode-op references: exactly this AU's refs (the first `reference_count`
        // scope entries, carrying real slot indices). H.265 RPS and AV1 names
        // index this prefix.
        let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
            begin_slots[..reference_count].to_vec();

        // Setup slot as the decode op sees it: real index (the begin list's twin
        // carries -1), same resource, own codec info chain.
        let setup_std = op.setup_ref;
        let mut setup_dpb = C::dpb_slot_info(&setup_std);
        let setup_resource = resources[scope.len() - 1];
        let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
            .slot_index(i32::from(op.setup_slot))
            .picture_resource(&setup_resource)
            .push_next(&mut setup_dpb);

        // Decode destination: the setup picture layer (coincide) or pool picture.
        let dst_resource = if coincide {
            setup_resource
        } else {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded_extent)
                .base_array_layer(0)
                .image_view_binding(state.pool.pictures[op.dst].view)
        };

        let mut decode_info = vk::VideoDecodeInfoKHR::default()
            .src_buffer(upload.buffer)
            .src_buffer_offset(0)
            .src_buffer_range(upload.range)
            .dst_picture_resource(dst_resource)
            .setup_reference_slot(&setup_slot_info)
            .push_next(picture_info);
        if reference_count > 0 {
            decode_info = decode_info.reference_slots(&decode_refs);
        }

        let parameters = state.session.parameters();
        let raw = state.session.raw_mut();
        let begin_coding = vk::VideoBeginCodingInfoKHR::default()
            .video_session(raw.session())
            .video_session_parameters(parameters)
            .reference_slots(&begin_slots);
        // One-shot session RESET, consumed here but re-armed on every error path
        // below. A RESET recorded into a buffer that never reaches the queue
        // initialized nothing; the next successful recording must carry it.
        let did_reset = raw.take_needs_reset();
        // SAFETY: recording into the begun buffer, through end_command_buffer; every
        // pointed-to struct above is a local (or session-state field) that outlives
        // the calls; the session/parameters handles are this generation's own.
        let recorded: Result<(), vk::Result> = unsafe {
            (dev.video_queue().fp().cmd_begin_video_coding_khr)(cmd, &begin_coding);
            if did_reset {
                let control = vk::VideoCodingControlInfoKHR::default()
                    .flags(vk::VideoCodingControlFlagsKHR::RESET);
                (dev.video_queue().fp().cmd_control_video_coding_khr)(cmd, &control);
            }
            if let Some(query_pool) = state.ops.query_pool {
                device.cmd_begin_query(
                    cmd,
                    query_pool,
                    op.query_index,
                    vk::QueryControlFlags::empty(),
                );
            }
            (dev.video_decode_queue().fp().cmd_decode_video_khr)(cmd, &decode_info);
            if let Some(query_pool) = state.ops.query_pool {
                device.cmd_end_query(cmd, query_pool, op.query_index);
            }
            (dev.video_queue().fp().cmd_end_video_coding_khr)(
                cmd,
                &vk::VideoEndCodingInfoKHR::default(),
            );
            device.end_command_buffer(cmd)
        };
        if let Err(e) = recorded {
            if did_reset {
                state.session.raw_mut().re_arm_reset();
            }
            return Err(VkDecodeError::from(e));
        }

        let cmd_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
        let wait_infos: Vec<vk::SemaphoreSubmitInfo<'_>> = op
            .waits
            .iter()
            .map(|&(semaphore, value)| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(semaphore)
                    .value(value)
                    .stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            })
            .collect();
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(state.pool.pictures[op.dst].semaphore)
            .value(op.signal_value)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let submits = [vk::SubmitInfo2::default()
            .command_buffer_infos(&cmd_infos)
            .wait_semaphore_infos(&wait_infos)
            .signal_semaphore_infos(&signals)];
        let guard = QueueSubmitGuard::acquire(&*self.lock);
        // SAFETY: the decode queue is the device's own (DeviceHandles contract) and
        // externally synchronized by the guard; the submit arrays are locals.
        let result =
            unsafe { device.queue_submit2(dev.decode_queue(), &submits, vk::Fence::null()) };
        drop(guard);
        if let Err(e) = result {
            // Recorded RESET never executed: the next recording must redo it.
            if did_reset {
                state.session.raw_mut().re_arm_reset();
            }
            return Err(VkDecodeError::from(e));
        }
        Ok(())
    }

    /// Book a submitted op: dst turns pending (coincide: bound to the setup
    /// slot), ring and query marks advance, and each slot's cached reference
    /// info refreshes from this plan.
    pub(crate) fn commit_op<R: ScopeRef<Std = C::StdRef>>(
        &mut self,
        op: &Op<C::StdRef>,
        upload: &UploadedAu,
        refs: &[R],
    ) {
        let state = self
            .state
            .as_mut()
            .expect("prepare_op ran on this generation");
        let setup = usize::from(op.setup_slot);
        let dst_sem = state.pool.pictures[op.dst].semaphore;
        state.pool.pictures[op.dst].value = op.signal_value;
        state.pool.pictures[op.dst].pending = true;
        if state.dpb.is_none() {
            state.pool.pictures[op.dst].bound = true;
            state.slot_image[setup] = Some(op.dst);
        }
        state.cmd_marks[op.cmd_index] = Some((dst_sem, op.signal_value));
        state.query_marks[op.query_index as usize] = op.submission;
        state.submitted += 1;
        state.last_submit = Some((dst_sem, op.signal_value));
        state
            .ring
            .pending
            .set_pending(upload.slot, (dst_sem, op.signal_value));

        state.slot_refs[setup] = Some(op.setup_ref);
        for r in refs {
            state.slot_refs[usize::from(r.slot())] = Some(r.std());
        }
    }

    /// Run `submit`, then free the slots the planner retired while this op
    /// still bound them (`release_after_decode`). The release runs on failure
    /// too: the planner already dropped them, and a skipped release leaks a slot
    /// per failed AU.
    pub(crate) fn submit_then_release(
        &mut self,
        release_after_decode: &[PicId],
        submit: impl FnOnce(&mut Self) -> Result<(), VkDecodeError>,
    ) -> Result<(), VkDecodeError> {
        let submitted = submit(self);
        if let Some(state) = self.state.as_mut() {
            for &id in release_after_decode {
                if !state.slots.release(id) {
                    trace!(id, "deferred release of an id the slot map no longer holds");
                }
            }
        }
        submitted
    }
}

impl<C: VkCodec> Drop for VkDecoder<C> {
    fn drop(&mut self) {
        // Drain so pool Drop never destroys in-flight decode work; a wedged
        // driver falls through after the bounded timeout. Presenter sampling of
        // held images is the caller's teardown: wait every release token before
        // drop, or remaining graveyard pools are a warned forfeit.
        if let Err(e) = self.drain_gpu() {
            debug!(error = %e, "drain on drop failed; tearing down anyway");
        }
        if !self.graveyard.is_empty() {
            debug!(
                pools = self.graveyard.len(),
                "graveyard not fully token-drained at decoder drop — destroying anyway \
                 (upstream teardown forfeited its bounded wait)"
            );
        }
    }
}

/// Query and command pools. Query slots cycle per submission (checked against
/// [`DecodedVkFrame::submission`]); command buffers cycle within the bitstream
/// ring's in-flight bound. This type owns and destroys the Vulkan objects.
///
/// `query_pool` is `None` without `queryResultStatusSupport`: recording a
/// RESULT_STATUS query is invalid there (RADV hangs the VCN ring). Verdicts
/// then fall back to timeline completion.
pub(crate) struct OpRing {
    device: ash::Device,
    pub(crate) query_pool: Option<vk::QueryPool>,
    pub(crate) query_count: u32,
    cmd_pool: vk::CommandPool,
    pub(crate) cmds: Vec<vk::CommandBuffer>,
}

impl OpRing {
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        decode_profile: DecodeProfile,
        query_count: u32,
        cmd_count: u32,
    ) -> Result<Self, vk::Result> {
        let query_pool = if dev.result_status_queries() {
            let mut chain = decode_profile.chain();
            // SAFETY: fn contract. `chain` outlives the call, and the helper's
            // SIGNATURE — not a comment — is what keeps it immobile across it.
            Some(unsafe { Self::create_status_query_pool(dev, chain.wire(), query_count)? })
        } else {
            debug!(
                "decode family lacks queryResultStatusSupport — no per-op status \
                 queries on this driver (verdicts fall back to timeline completion)"
            );
            None
        };

        let destroy_query = |pool: Option<vk::QueryPool>| {
            if let Some(pool) = pool {
                // SAFETY: destroying the just-created query pool (unwind path).
                unsafe { dev.ash().destroy_query_pool(pool, None) };
            }
        };
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(dev.decode_qf())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: live device; unwind destroys the query pool on failure.
        let cmd_pool = match unsafe { dev.ash().create_command_pool(&pool_ci, None) } {
            Ok(p) => p,
            Err(e) => {
                destroy_query(query_pool);
                return Err(e);
            }
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(cmd_count);
        // SAFETY: live device + the pool created above; unwind destroys both pools
        // (destroying the command pool frees any allocated buffers).
        let cmds = match unsafe { dev.ash().allocate_command_buffers(&alloc) } {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: destroying the command pool created above.
                unsafe { dev.ash().destroy_command_pool(cmd_pool, None) };
                destroy_query(query_pool);
                return Err(e);
            }
        };
        Ok(Self {
            device: dev.ash().clone(),
            query_pool,
            query_count,
            cmd_pool,
            cmds,
        })
    }

    /// RESULT_STATUS query pool against `profile`.
    ///
    /// Split out so the profile borrow outlives `vkCreateQueryPool`.
    /// `push_next` would clobber the profile's own `p_next`; the chain is a raw
    /// `*const`, which ends the borrow the moment it is taken. A `&` parameter
    /// holds it for the whole call.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    unsafe fn create_status_query_pool(
        dev: &DecodeDevice,
        profile: &vk::VideoProfileInfoKHR<'_>,
        query_count: u32,
    ) -> Result<vk::QueryPool, vk::Result> {
        let mut query_ci = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::RESULT_STATUS_ONLY_KHR)
            .query_count(query_count);
        // Manual chain: `push_next` would clobber the profile's own `p_next`.
        query_ci.p_next = std::ptr::from_ref(profile).cast();
        // SAFETY: fn contract; `query_ci` roots the wired chain for the call, and
        // `profile` is borrowed for the whole of this body so the chain cannot move
        // out from under that pointer. The video profile chained in satisfies the
        // "same profile as the session" rule for queries used inside a coding scope.
        unsafe { dev.ash().create_query_pool(&query_ci, None) }
    }
}

impl Drop for OpRing {
    fn drop(&mut self) {
        // SAFETY: own handles on the contract-live device; the owning decoder
        // drains GPU work before dropping state. Destroying the command pool frees
        // its buffers; both destroys ignore NULL.
        unsafe {
            self.device.destroy_command_pool(self.cmd_pool, None);
            if let Some(pool) = self.query_pool {
                self.device.destroy_query_pool(pool, None);
            }
        }
    }
}

/// Decoded picture waiting for its output verdict, plus the fields its
/// [`DecodedVkFrame`] needs.
pub(crate) struct PendingPic {
    pub(crate) image: usize,
    pub(crate) submission: u64,
    pub(crate) query_slot: u32,
    pub(crate) timeline_value: u64,
    pub(crate) crop: DisplayCrop,
    pub(crate) colour: ColourDescription,
    pub(crate) poc: i32,
    pub(crate) is_idr: bool,
    /// Folded at plan time (the codec's counting unit is known only there).
    /// Display order is not decode order; see [`DecodedVkFrame::recovery`].
    pub(crate) recovery: crate::recovery::RecoveryMark,
    /// See [`DecodedVkFrame::decode_order`].
    pub(crate) decode_order: u64,
    /// From the plan at decode time; display order is not decode order. See
    /// [`DecodedVkFrame::references_clean`].
    pub(crate) references_clean: bool,
}

/// Retired generation's picture pool. Lives until release tokens return, then
/// the pool dies.
pub(crate) struct RetiredPool {
    pub(crate) generation: u64,
    pub(crate) pool: PicturePool,
}

/// Set when an AU fails after planning has already advanced. The next `decode`
/// consumes it and flushes to the next random-access point before planning
/// anything new.
///
/// Fail closed (H.265 shown): `RefPicSetStCurr*`/`LtCurr` are indices into this op's
/// reference array, so dropping a missing binding re-points every later index
/// at the wrong picture. By the time that error returns, `plan_to_vk_h265` has
/// already mutated [`SlotMap`] and coincide sync has cleared the setup image,
/// so planner and slots both claim picture N is resident with no image holding
/// it — every later AU then fails [`build_scope`] with `UnboundReferenceSlot`.
///
/// Recovery is a flush to the next IRAP/IDR/key (planner flush plus
/// [`reset_slot_bindings`]), not reference substitution. Own type so the
/// latch/consume cycle is testable without a device ([`crate::session::ResetArm`]).
#[derive(Debug, Default)]
pub(crate) struct RecoveryLatch(bool);

impl RecoveryLatch {
    /// Record that recovery is owed. Idempotent: two failures in a row still owe
    /// exactly one flush.
    pub(crate) fn latch(&mut self) {
        self.0 = true;
    }

    /// Whether recovery is owed, clearing the latch — once per failure run, not
    /// on every later decode.
    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    /// Whether recovery is owed, without consuming it (state snapshots).
    pub(crate) fn is_latched(&self) -> bool {
        self.0
    }
}

/// Build the delivered frame for one settled pending picture (pending → held).
///
/// [`DecodedVkFrame::format`] comes off the pool, which stamped it from the
/// `caps.output_format` its images were created with.
pub(crate) fn build_frame(
    pool: &mut PicturePool,
    coincide: bool,
    image_extent: vk::Extent2D,
    entry: &PendingPic,
    generation: u64,
) -> DecodedVkFrame {
    let format = pool.format;
    let copyable = pool.copyable;
    let picture = &mut pool.pictures[entry.image];
    picture.pending = false;
    picture.held += 1;
    DecodedVkFrame {
        image: picture.image,
        format,
        view: picture.view,
        plane_views: picture.plane_views,
        layer: picture.layer,
        layout: if coincide {
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR
        } else {
            vk::ImageLayout::VIDEO_DECODE_DST_KHR
        },
        coded_width: image_extent.width,
        coded_height: image_extent.height,
        crop: entry.crop,
        colour: entry.colour,
        semaphore: picture.semaphore,
        value: entry.timeline_value,
        poc: entry.poc,
        is_idr: entry.is_idr,
        recovery: entry.recovery,
        decode_order: entry.decode_order,
        references_clean: entry.references_clean,
        query_slot: entry.query_slot,
        submission: entry.submission,
        picture: entry.image as u32,
        generation,
        copyable,
    }
}

/// Split one [`DpbUpdate`]: `outputs` (bump order) become deliverable;
/// `removed` ids that never reached output are returned so their images are
/// freed. H.265 shares H.264's [`DpbUpdate`].
pub(crate) fn settle_dpb<F>(pending: &mut BTreeMap<PicId, F>, dpb: &DpbUpdate) -> (Vec<F>, Vec<F>) {
    settle_dpb_ids(pending, &dpb.outputs, &dpb.removed)
}

/// [`settle_dpb`] over the two id lists directly.
///
/// AV1 declares its own [`pf_bitstream::av1::DpbUpdate`] — structurally the
/// same, a distinct type. Settling at the id lists lets all three codecs share
/// one implementation.
pub(crate) fn settle_dpb_ids<F>(
    pending: &mut BTreeMap<PicId, F>,
    outputs: &[PicId],
    removed: &[PicId],
) -> (Vec<F>, Vec<F>) {
    let mut ready = Vec::new();
    for id in outputs {
        match pending.remove(id) {
            Some(entry) => ready.push(entry),
            // Ids planned before this decoder existed, or dropped across a
            // rebuild: display-order gaps, not errors.
            None => trace!(id, "output id without a pending picture"),
        }
    }
    let dropped = removed.iter().filter_map(|id| pending.remove(id)).collect();
    (ready, dropped)
}

/// Bounded timeline wait (no-op for the never-signalled value 0).
///
/// # Safety
///
/// `device` is live and `semaphore` is a live timeline semaphore on it.
pub(crate) unsafe fn wait_timeline(
    device: &ash::Device,
    semaphore: vk::Semaphore,
    value: u64,
    what: &'static str,
) -> Result<(), VkDecodeError> {
    if value == 0 {
        return Ok(());
    }
    let semaphores = [semaphore];
    let values = [value];
    let info = vk::SemaphoreWaitInfo::default()
        .semaphores(&semaphores)
        .values(&values);
    // SAFETY: fn contract; the info arrays are locals outliving the call.
    match unsafe { device.wait_semaphores(&info, DECODE_TIMEOUT_NS) } {
        Ok(()) => Ok(()),
        Err(vk::Result::TIMEOUT) => Err(VkDecodeError::Timeout(what)),
        Err(e) => Err(VkDecodeError::from(e)),
    }
}

/// Empty the three per-slot ledgers a recovery resets: DPB residency,
/// slot→picture bindings, and cached per-slot reference info. Returns the pool
/// picture indices the cleared bindings were pinning, for the caller to unbind.
/// Pure over the ledgers so recovery is testable without a device.
///
/// All three empty together: leftover reference info would let [`build_scope`]
/// bind a slot the planner no longer knows. Generic over the cached Std type.
pub(crate) fn reset_slot_bindings<S>(
    slots: &mut SlotMap,
    slot_image: &mut [Option<usize>],
    slot_refs: &mut [Option<S>],
) -> Vec<usize> {
    // `release` is the only way a slot is freed ([`SlotMap`]); collect because
    // `held` borrows the map the releases mutate.
    for (_slot, id) in slots.held().collect::<Vec<_>>() {
        slots.release(id);
    }
    let unbound = slot_image.iter_mut().filter_map(Option::take).collect();
    for cached in slot_refs.iter_mut() {
        *cached = None;
    }
    unbound
}

/// Coincide: unbind slots the ledger no longer holds, and the setup slot's
/// previous image, before it binds fresh. Pictures stay pending/held on those
/// flags ([`crate::images`]). A referenced slot must still bind after this.
pub(crate) fn sync_slot_bindings(
    slots: &SlotMap,
    slot_image: &mut [Option<usize>],
    setup_slot: u8,
) -> Vec<usize> {
    let mut held = vec![false; slot_image.len()];
    for (slot, _id) in slots.held() {
        held[usize::from(slot)] = true;
    }
    let setup = usize::from(setup_slot);
    let mut unbound = Vec::new();
    for (slot, binding) in slot_image.iter_mut().enumerate() {
        if binding.is_some() && (!held[slot] || slot == setup) {
            unbound.extend(binding.take());
        }
    }
    unbound
}

/// One bound-slot list entry: DPB slot index (`-1` for the setup activation),
/// picture resource view, and that slot's codec reference info.
/// No derived equality: the Std bindgen struct has none; tests compare fields.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScopeEntry<S> {
    pub(crate) slot_index: i32,
    pub(crate) view: vk::ImageView,
    pub(crate) std: S,
}

/// One of this AU's references as [`build_scope`] sees it: a DPB slot and the
/// codec reference info to bind with it. All three codecs share the builder so
/// "refuse to guess" has one implementation; AV1 checks its reference names
/// against the result afterwards.
pub(crate) trait ScopeRef {
    type Std: Copy;
    fn slot(&self) -> u8;
    fn std(&self) -> Self::Std;
}

/// [`build_scope`]'s answer: bound-slot list, and how many leading entries are
/// this AU's own references (the prefix the decode op takes as its reference
/// array — ordering note in `build_scope`).
pub(crate) type Scope<S> = (Vec<ScopeEntry<S>>, usize);

/// Build the coding scope's bound-slot list and how many leading entries are
/// this AU's references.
///
/// Layout: (1) every `refs` entry in order — the decode op takes this prefix;
/// (2) every other still-held slot, so its association survives the scope;
/// (3) the setup slot as the activation entry, slot index `-1`.
///
/// A reference whose slot binds no image is a hard error, never a skip.
/// H.265 `RefPicSetStCurr*`/`LtCurr` name DPB slots, and every named slot is
/// one of `refs` ([`crate::pic_h265`]); dropping an entry leaves hardware a
/// named slot this op never bound.
///
/// `reference_count` is captured the instant the `refs` loop ends, before the
/// held-slot pass appends. The decode op takes `scope[..reference_count]` as
/// its reference list; a count after the second pass could hand it a
/// still-held slot this AU does not reference.
pub(crate) fn build_scope<R: ScopeRef>(
    refs: &[R],
    held_slots: impl Iterator<Item = u8>,
    setup_slot: u8,
    setup_view: vk::ImageView,
    setup_ref: R::Std,
    slot_refs: &[Option<R::Std>],
    view_of: impl Fn(u8) -> Option<vk::ImageView>,
) -> Result<Scope<R::Std>, VkDecodeError> {
    let mut scope: Vec<ScopeEntry<R::Std>> = Vec::with_capacity(refs.len() + slot_refs.len() + 1);
    for r in refs {
        match view_of(r.slot()) {
            Some(view) => scope.push(ScopeEntry {
                slot_index: i32::from(r.slot()),
                view,
                std: r.std(),
            }),
            None => return Err(VkDecodeError::UnboundReferenceSlot { slot: r.slot() }),
        }
    }
    let reference_count = scope.len();
    for slot in held_slots {
        if slot == setup_slot || refs.iter().any(|r| r.slot() == slot) {
            continue;
        }
        match (
            slot_refs.get(usize::from(slot)).copied().flatten(),
            view_of(slot),
        ) {
            (Some(std), Some(view)) => scope.push(ScopeEntry {
                slot_index: i32::from(slot),
                view,
                std,
            }),
            // Every held slot was a setup slot once; leave unbound rather than fake.
            _ => trace!(
                slot,
                "held slot without reference info/binding — left unbound"
            ),
        }
    }
    scope.push(ScopeEntry {
        slot_index: -1,
        view: setup_view,
        std: setup_ref,
    });
    Ok((scope, reference_count))
}

#[cfg(test)]
mod tests {
    use ash::vk::native as hh;
    use ash::vk::Handle as _;

    use super::*;
    use crate::pic_h265::VkRefH265;

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

    fn vk_ref(slot: u8, poc: i32, long_term: bool) -> VkRefH265 {
        VkRefH265 {
            slot,
            std: std_ref(poc, long_term),
            id: u64::from(slot) + 100,
        }
    }

    /// Distinguishable fake view per slot. Never dereferenced; the scope only
    /// carries handles.
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    #[test]
    fn the_scopes_leading_entries_are_the_refs_in_plan_order() {
        // Plan refs are in RPS set order (StCurrBefore, StCurrAfter, LtCurr),
        // not slot order. Std index arrays point at positions in that order, so
        // the scope must not sort or dedup them.
        let refs = vec![
            vk_ref(5, 40, false),
            vk_ref(1, 60, false),
            vk_ref(3, 8, true),
        ];
        let slot_refs = vec![Some(std_ref(0, false)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3, 5, 7].into_iter(),
            2,
            fake_view(2),
            std_ref(50, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();

        assert_eq!(reference_count, 3, "exactly this AU's references lead");
        assert_eq!(
            scope[..reference_count]
                .iter()
                .map(|e| e.slot_index)
                .collect::<Vec<_>>(),
            vec![5, 1, 3],
            "plan order, not slot order — the RPS index arrays depend on it"
        );
        for (entry, r) in scope.iter().zip(&refs) {
            assert_eq!(entry.view, fake_view(r.slot));
            assert_eq!(entry.std.PicOrderCntVal, r.std.PicOrderCntVal);
            assert_eq!(
                entry.std.flags.used_for_long_term_reference(),
                r.std.flags.used_for_long_term_reference(),
                "the long-term marking rides with the binding"
            );
        }

        assert_eq!(scope[3].slot_index, 7);
        let last = scope.last().unwrap();
        assert_eq!(
            last.slot_index, -1,
            "the setup slot binds its resource without a current association"
        );
        assert_eq!(last.view, fake_view(2));
        assert_eq!(last.std.PicOrderCntVal, 50);
        assert_eq!(
            scope.len(),
            5,
            "3 refs + 1 other held slot + the activation"
        );
    }

    #[test]
    fn a_reference_slot_without_a_bound_image_fails_the_whole_op() {
        // Compacting past it would shift every later RefPicSetStCurr* index
        // onto the wrong picture. Fail closed.
        let refs = vec![vk_ref(4, 10, false), vk_ref(6, 20, false)];
        let slot_refs = vec![Some(std_ref(0, false)); 8];
        let err = build_scope(
            &refs,
            [4u8, 6].into_iter(),
            0,
            fake_view(0),
            std_ref(30, false),
            &slot_refs,
            |slot| (slot != 6).then(|| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 6 }),
            "{err}"
        );
    }

    #[test]
    fn held_slots_are_bound_once_and_the_setup_slot_never_twice() {
        // Slot 3 is both a reference and still held; slot 2 is setup and also
        // held. Neither may appear twice: a duplicate slot index in one coding
        // scope is invalid, and a second entry for a reference would also
        // break the index arrays.
        let refs = vec![vk_ref(3, 12, false)];
        let slot_refs = vec![Some(std_ref(99, false)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 2, 3].into_iter(),
            2,
            fake_view(2),
            std_ref(24, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 1);
        let indices: Vec<i32> = scope.iter().map(|e| e.slot_index).collect();
        assert_eq!(indices, vec![3, 1, -1]);
        assert_eq!(
            indices.iter().filter(|&&i| i == 3).count(),
            1,
            "a referenced slot is bound exactly once"
        );
        assert!(
            !indices.contains(&2),
            "the setup slot is bound only as the -1 activation entry"
        );
    }

    #[test]
    fn a_held_slot_with_no_cached_reference_info_is_left_unbound_not_faked() {
        // Only reachable if a slot was never a setup slot on this session.
        // Drop it rather than bind zeroed reference info (POC 0, short-term).
        let refs: Vec<VkRefH265> = Vec::new();
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>> = vec![None; 4];
        slot_refs[1] = Some(std_ref(7, false));
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3].into_iter(),
            0,
            fake_view(0),
            std_ref(9, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "an IRAP references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![1, -1],
            "slot 3 had no cached info and is simply not bound"
        );
    }

    #[test]
    fn a_post_mutation_failure_wedges_every_later_au_until_the_ledgers_are_reset() {
        // AU planned, slot assigned, setup image unbound, then decode failed.
        // Planner and SlotMap still claim the picture is resident.
        let mut slots = SlotMap::new(3);
        slots.assign(100).unwrap(); // slot 0, bound
        slots.assign(200).unwrap(); // slot 1, bound
        slots.assign(300).unwrap(); // slot 2, this AU's setup — binding cleared
        let mut slot_image: Vec<Option<usize>> = vec![Some(7), Some(8), None, None];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>> =
            vec![Some(std_ref(10, false)); 4];

        // Later AUs that reference slot 2 fail closed (RPS index arrays).
        let err = build_scope(
            &[vk_ref(2, 30, false)],
            [0u8, 1, 2].into_iter(),
            0,
            fake_view(0),
            std_ref(40, false),
            &slot_refs,
            |slot| slot_image[usize::from(slot)].map(|_| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 2 }),
            "{err}"
        );

        let unbound = reset_slot_bindings(&mut slots, &mut slot_image, &mut slot_refs);
        assert_eq!(
            unbound,
            vec![7, 8],
            "the pool pictures the stale bindings pinned go back on the free list"
        );
        assert_eq!(slots.active(), 0, "no picture is DPB-resident any more");
        assert_eq!(
            slots.capacity(),
            4,
            "capacity survives — no session rebuild"
        );
        assert!(slot_image.iter().all(Option::is_none));
        assert!(
            slot_refs.iter().all(Option::is_none),
            "cached reference info goes too, or build_scope could bind a slot the \
             planner no longer knows about"
        );

        let setup_slot = slots.assign(400).unwrap();
        assert_eq!(setup_slot, 0, "the freed slots are assignable again");
        slot_image[usize::from(setup_slot)] = Some(9);
        // Empty slice needs its element type named: `build_scope` is generic
        // over the codecs' reference types.
        let no_refs: [VkRefH265; 0] = [];
        let (scope, reference_count) = build_scope(
            &no_refs,
            slots.held().map(|(slot, _id)| slot),
            setup_slot,
            fake_view(setup_slot),
            std_ref(0, false),
            &slot_refs,
            |slot| slot_image[usize::from(slot)].map(|_| fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "an IRAP references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![-1],
            "only the setup activation entry — the stream is decoding again"
        );
    }

    #[test]
    fn the_recovery_latch_is_owed_once_and_consumed_by_exactly_one_decode() {
        // Two failures in a row still owe one flush; the decode that performs
        // it clears the debt. Otherwise every later decode would re-flush and
        // the stream could never build a DPB again.
        let mut latch = RecoveryLatch::default();
        assert!(!latch.is_latched(), "a fresh decoder owes nothing");
        assert!(!latch.take());

        latch.latch();
        latch.latch();
        assert!(
            latch.is_latched(),
            "visible in debug_snapshot before it runs"
        );
        assert!(latch.take(), "the next decode recovers");
        assert!(!latch.is_latched());
        assert!(!latch.take(), "and the one after that just decodes");
    }

    /// Every codec's rebuild runs this one gate: images are allocated at the
    /// granularity-rounded extent, so that is what `maxCodedExtent` must cover.
    #[test]
    fn the_session_extent_is_the_rounded_size_and_must_fit_the_device_range() {
        let caps = DecodeCaps {
            coincide: true,
            layered_dpb: false,
            min_bitstream_offset_alignment: 1,
            min_bitstream_size_alignment: 1,
            picture_access_granularity: vk::Extent2D {
                width: 16,
                height: 16,
            },
            min_coded_extent: vk::Extent2D {
                width: 64,
                height: 64,
            },
            max_coded_extent: vk::Extent2D {
                width: 1920,
                height: 1088,
            },
            max_dpb_slots: 17,
            max_active_references: 16,
            max_level_idc: crate::caps::MaxLevelIdc::H264(0),
            dpb_format: crate::caps::NV12,
            output_format: crate::caps::NV12,
            plane_view_formats: [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM],
            std_header_version: vk::ExtensionProperties::default(),
        };
        let extent = |width, height| vk::Extent2D { width, height };
        let fitted = session_extent(&caps, extent(1920, 1080)).unwrap();
        assert_eq!((fitted.width, fitted.height), (1920, 1088));
        assert!(
            matches!(
                session_extent(&caps, extent(1920, 1090)),
                Err(VkDecodeError::Unsupported(_))
            ),
            "1090 rounds to 1104, past the device's 1088"
        );
        assert!(matches!(
            session_extent(&caps, extent(32, 64)),
            Err(VkDecodeError::Unsupported(_))
        ));
    }

    #[test]
    fn settle_dpb_readies_outputs_in_order_and_returns_never_output_removals() {
        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(1, 100);
        pending.insert(2, 200);
        pending.insert(3, 300);

        // 1 outputs and is removed (normal bump). 2 is removed without output
        // (`no_output_of_prior_pics`): free its image, do not leak in the map.
        let update = DpbUpdate {
            stored: Some(3),
            outputs: vec![1],
            removed: vec![1, 2],
        };
        let (ready, dropped) = settle_dpb(&mut pending, &update);
        assert_eq!(ready, vec![100]);
        assert_eq!(dropped, vec![200]);
        assert_eq!(
            pending.keys().copied().collect::<Vec<_>>(),
            vec![3],
            "the still-buffered picture stays pending"
        );

        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(5, 500);
        pending.insert(4, 400);
        let update = DpbUpdate {
            stored: None,
            outputs: vec![5, 99, 4],
            removed: vec![],
        };
        let (ready, dropped) = settle_dpb(&mut pending, &update);
        assert_eq!(ready, vec![500, 400], "bump order, not id order");
        assert!(dropped.is_empty());
    }
}
