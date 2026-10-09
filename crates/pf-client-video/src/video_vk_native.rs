//! Native Vulkan Video backend: pf-vkdecode on the presenter's `VkDevice`.
//! Decoded images are sampled in place — no FFmpeg, copies, imports, or a second device.
//! H.264, H.265, and AV1; NV12, P010, or 8/10-bit two-plane 4:4:4.
//!
//! Admission and stream shape are [`crate::video::native_vulkan_gate`] and
//! [`NativeVulkanDecoder::new`]; a refusal falls through the ladder in [`crate::video`].
//! The backend is `Send` and not `Sync`. [`submit_queues_collide`] takes the shared
//! queue lock when decode and graphics share an externally synchronized queue.
//!
//! A delivered [`NativeVkFrame`] pins its pool slot until [`NativeReleaseGuard`] drops
//! after present retires. [`Codec::release_frame`] also waits the status verdict;
//! teardown waits [`TEARDOWN_BUDGET`] before destroying the pools.
//! Failed or unreadable status queries are rung errors. Integrity concealment drops
//! the picture unshown and raises [`NativeVulkanDecoder::take_recovery_request`].
//! `Ok(None)` is buffering or an H.265 RASL skip — never a post-failure keyframe wait.
//! AV1 may decode hidden frames but exposes at most one displayable frame per
//! [`NativeVulkanDecoder::decode`].

use crate::video::{
    ColorDesc, DecodeHealth, NativeReleaseGuard, NativeReleaseToken, NativeVkFrame, NativeVkLayout,
    VulkanDecodeDevice,
};
use crate::video_types::trim_deliverable;
use anyhow::{anyhow, bail, Result};
use pf_vkdecode::ash::vk;
use pf_vkdecode::ash::vk::Handle as _;
use pf_vkdecode::{
    DecodeStatus, DecodedVkFrame, DeviceHandles, VkAv1Decoder, VkDecodeError, VkH264Decoder,
    VkH265Decoder,
};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Queue index 0: the presenter creates one queue per enabled family (`vk/setup.rs`).
const DECODE_QUEUE_INDEX: u32 = 0;

/// 500 ms ≈ one present fence, plus slack for a paused stream. After this the pools die
/// even if a token is still out; residue is a logically-held frame, not in-flight GPU work.
const TEARDOWN_BUDGET: Duration = Duration::from_millis(500);

/// Token back means the presenter's submit already waited decode, so the query must be
/// readable. Three Pending polls after that give the slot back with an unknown verdict.
const MAX_POLLS_AFTER_RELEASE: u32 = 3;

/// Family equality is the collision test: both sides submit on queue index 0 of their family.
fn submit_queues_collide(graphics_qf: u32, decode_qf: u32) -> bool {
    graphics_qf == decode_qf
}

/// Shared with [`hevc_shape_supported`] so the probe and the session pick the same lock.
fn queue_lock_for(vk: &VulkanDecodeDevice) -> Box<dyn pf_vkdecode::QueueLock> {
    if submit_queues_collide(vk.graphics_qf, vk.decode_qf) {
        Box::new(NativeQueueLock::Shared(vk.queue_lock.clone()))
    } else {
        Box::new(NativeQueueLock::Uncontended)
    }
}

/// Presenter's live handles. The probe must ask about the device the session would use.
fn device_handles(vk: &VulkanDecodeDevice) -> DeviceHandles {
    DeviceHandles {
        get_instance_proc_addr: vk.get_instance_proc_addr,
        instance: vk.instance,
        physical_device: vk.physical_device,
        device: vk.device,
        decode_qf: vk.decode_qf,
        decode_queue_index: DECODE_QUEUE_INDEX,
        graphics_qf: vk.graphics_qf,
    }
}

/// Whether this device can hardware-decode HEVC at this picture shape, asked before Hello
/// so the client never advertises a shape it would refuse.
///
/// Same construction as [`NativeVulkanDecoder::new`]'s H.265 arm (`VkH265Decoder::new`
/// then `probe_stream_support`) so advertisement and the rung cannot disagree. Builds
/// and drops a decoder: capability queries only, no session, images, or submits.
/// `false` when the presenter has no Vulkan Video decode — for 4:4:4 that is the
/// answer; see [`crate::video::hevc_444_hardware_decodable`].
pub(crate) fn hevc_shape_supported(
    vk: &VulkanDecodeDevice,
    chroma_format_idc: u8,
    bit_depth_luma_minus8: u8,
) -> bool {
    if !vk.video_decode {
        return false;
    }
    // No picture format in pf-vkdecode means no driver to ask (`probe_stream_support` would re-derive it).
    if pf_vkdecode::output_format_for(chroma_format_idc, bit_depth_luma_minus8).is_none() {
        return false;
    }
    // SAFETY: `DeviceHandles` is the presenter's live instance/device; they outlive this
    // call (presenter owns them for the process; this runs on its thread while building
    // Hello). The decoder is dropped before return, so nothing outlives the borrow.
    let dec = unsafe { pf_vkdecode::VkH265Decoder::new(&device_handles(vk), queue_lock_for(vk)) };
    match dec {
        Ok(d) => d
            .probe_stream_support(chroma_format_idc, bit_depth_luma_minus8)
            .is_ok(),
        Err(_) => false,
    }
}

/// [`pf_vkdecode::QueueLock`] over the shared [`crate::video::QueueLock`], or nothing
/// when the decode queue has no other submitter.
enum NativeQueueLock {
    Shared(std::sync::Arc<crate::video::QueueLock>),
    Uncontended,
}

impl pf_vkdecode::QueueLock for NativeQueueLock {
    fn lock(&self) {
        if let NativeQueueLock::Shared(l) = self {
            l.lock();
        }
    }
    fn unlock(&self) {
        if let NativeQueueLock::Shared(l) = self {
            l.unlock();
        }
    }
}

/// Codecs pf-vkdecode can decode, named ash-free so `video.rs` can pick from the wire
/// codec without this module knowing wire bits.
///
/// Membership here is not admission: `video::native_vulkan_gate` decides whether `auto`
/// may pick a codec. Changing that list does not belong here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeCodec {
    H264,
    H265,
    Av1,
}

/// Decoder chosen once from the negotiated codec. Ledger, tokens, status, and teardown
/// are codec-agnostic: the three [`DecodedVkFrame`] surfaces match. Forwarders stay
/// identical per arm so dispatch cannot change the hardware-verified H.264 path.
// Unboxed against `large_enum_variant`: one of these lives in the session's
// `Box<NativeVulkanDecoder>`. Boxing would add a per-AU indirection for ~1.7 KB.
#[allow(clippy::large_enum_variant)]
enum Codec {
    H264(VkH264Decoder),
    H265(VkH265Decoder),
    Av1(VkAv1Decoder),
}

impl Codec {
    /// One access unit. `Ok(None)` is "no display-ready picture" — including an H.265
    /// RASL skip after an open-GOP join, never an error.
    ///
    /// Post-failure re-anchor waits are `Err` (`PlanError::AwaitingIdr` /
    /// `VkDecodeError::AwaitingKeyAv1`). `Ok(None)` would clear the demotion streak
    /// once per frame and strand a silent rung.
    ///
    /// AV1's AU is a temporal unit: first displayable picture here, the rest via
    /// [`Self::take_ready`].
    fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        match self {
            Codec::H264(d) => d.decode(au),
            Codec::H265(d) => d.decode(au),
            Codec::Av1(d) => d.decode(au),
        }
    }

    /// Typed per codec: the three warning enums are not subsets of each other, and
    /// only some mean damage ([`PlanWarnings::integrity`]). Flattening to strings
    /// would make that a `Debug` substring match.
    fn take_warnings(&mut self) -> PlanWarnings {
        match self {
            Codec::H264(d) => PlanWarnings::H264(d.take_warnings()),
            Codec::H265(d) => PlanWarnings::H265(d.take_warnings()),
            // Whole temporal unit, decode order — one unit, one concealment verdict.
            Codec::Av1(d) => PlanWarnings::Av1(d.take_warnings()),
        }
    }

    /// Burst leftovers from the last AU (H.265 reorder; AV1 only if non-conformant).
    /// Drain after every decode so a burst cannot strand inside the decoder.
    fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        match self {
            Codec::H264(d) => d.take_ready(),
            Codec::H265(d) => d.take_ready(),
            Codec::Av1(d) => d.take_ready(),
        }
    }

    fn forgive_unclean(&mut self) {
        match self {
            Codec::H264(d) => d.forgive_unclean(),
            Codec::H265(d) => d.forgive_unclean(),
            Codec::Av1(d) => d.forgive_unclean(),
        }
    }

    /// Return a delivered frame to its pool. `presented` is whether `value + 1` was enqueued.
    fn release_frame(
        &mut self,
        frame: &DecodedVkFrame,
        presented: bool,
    ) -> Result<(), VkDecodeError> {
        match self {
            Codec::H264(d) => d.release_frame(frame, presented),
            Codec::H265(d) => d.release_frame(frame, presented),
            Codec::Av1(d) => d.release_frame(frame, presented),
        }
    }

    /// Session generation. A frame from an older one has an unknowable status verdict.
    fn generation(&self) -> u64 {
        match self {
            Codec::H264(d) => d.generation(),
            Codec::H265(d) => d.generation(),
            Codec::Av1(d) => d.generation(),
        }
    }

    fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        match self {
            Codec::H264(d) => d.poll_status(frame),
            Codec::H265(d) => d.poll_status(frame),
            Codec::Av1(d) => d.poll_status(frame),
        }
    }

    /// Bounded host wait on the decode-complete timeline (pump's sampled decode-latency).
    fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        match self {
            Codec::H264(d) => d.wait_decoded(frame, timeout_ns),
            Codec::H265(d) => d.wait_decoded(frame, timeout_ns),
            Codec::Av1(d) => d.wait_decoded(frame, timeout_ns),
        }
    }

    /// Per-op decode-status queries. A device fact, forwarded because the decoder owns `DecodeDevice`.
    fn status_queries(&self) -> bool {
        match self {
            Codec::H264(d) => d.status_queries(),
            Codec::H265(d) => d.status_queries(),
            Codec::Av1(d) => d.status_queries(),
        }
    }

    /// Newest planned picture's decode-order ordinal ([`NativeVkFrame::decode_order`]).
    /// AV1 `show_existing_frame` does not advance it — it decodes nothing.
    fn decode_order(&self) -> u64 {
        match self {
            Codec::H264(d) => d.decode_order(),
            Codec::H265(d) => d.decode_order(),
            Codec::Av1(d) => d.decode_order(),
        }
    }

    /// The bitstream ring's dma-buf, once exported and a session exists.
    #[cfg(target_os = "linux")]
    fn bitstream_dmabuf(&self) -> Option<std::os::fd::RawFd> {
        match self {
            Codec::H264(d) => d.bitstream_dmabuf(),
            Codec::H265(d) => d.bitstream_dmabuf(),
            Codec::Av1(d) => d.bitstream_dmabuf(),
        }
    }
}

/// Status of previously shipped frames from one [`NativeVulkanDecoder::settle_statuses`].
///
/// The same `DecodeStatus::Failed` is two facts: with `RESULT_STATUS` queries it is the
/// driver's verdict (`DecodeHealth::failed`); without, `poll_status` reads the decode
/// timeline and `Failed` is a lost generation, a lost device, or an unreadable semaphore.
/// Counting the latter as driver-failed yields `driver-failed 1 · no driver status`.
/// Both drop the picture, surface as an error, and extend the concealed run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StatusVerdicts {
    /// Driver-reported corrupt. Non-zero only when the device answers status queries.
    driver_failed: u32,
    /// Status unreadable on a device with no query support (degraded timeline path).
    unreadable: u32,
}

impl StatusVerdicts {
    fn total(&self) -> u32 {
        self.driver_failed + self.unreadable
    }
}

/// Plan warnings for one AU, still in the codec's own enum.
///
/// Integrity (missing reference, `frame_num` gap, truncated AU) means the plan used a
/// substitute: drop the picture unshown and request a re-anchor. Spec-legal envelope
/// signals (`H265PlanWarning::NonZeroReorder`, `PlanWarning::Mmco5Rebase`) are fully
/// planned; `NonZeroReorder` fires on SPS activation (opening IDR and every ABR IDR),
/// so treating it as concealment hitches every renegotiation. Only integrity drops.
///
/// AV1 has no reorder envelope and no MMCO; every warning is damage.
/// [`pf_vkdecode::Av1PlanWarning::is_integrity`] is exhaustive so a new warning cannot
/// default to clean. The spec-legal branch is the landing site for a future one.
enum PlanWarnings {
    H264(Vec<pf_vkdecode::PlanWarning>),
    H265(Vec<pf_vkdecode::H265PlanWarning>),
    Av1(Vec<pf_vkdecode::Av1PlanWarning>),
}

impl PlanWarnings {
    fn is_empty(&self) -> bool {
        match self {
            PlanWarnings::H264(w) => w.is_empty(),
            PlanWarnings::H265(w) => w.is_empty(),
            PlanWarnings::Av1(w) => w.is_empty(),
        }
    }

    /// Integrity (concealment) subset. Allocates only off the clean path.
    ///
    /// Predicate lives on pf-bitstream's warning enums so the fault-injection
    /// harness asserts against the same list production conceals on.
    fn integrity(&self) -> PlanWarnings {
        match self {
            PlanWarnings::H264(w) => {
                PlanWarnings::H264(w.iter().filter(|x| x.is_integrity()).cloned().collect())
            }
            PlanWarnings::H265(w) => {
                PlanWarnings::H265(w.iter().filter(|x| x.is_integrity()).cloned().collect())
            }
            PlanWarnings::Av1(w) => {
                PlanWarnings::Av1(w.iter().filter(|x| x.is_integrity()).cloned().collect())
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            PlanWarnings::H264(w) => w.len(),
            PlanWarnings::H265(w) => w.len(),
            PlanWarnings::Av1(w) => w.len(),
        }
    }

    /// Concealment log, per arm, so tracing prints the codec's own enum (not a `Vec<String>`
    /// or a wrapper prefix). `concealed` is the integrity count; the list is every warning.
    fn warn_concealment(&self, concealed: usize) {
        // Per arm so the message stays a static tracing string, not a formatted one.
        match self {
            PlanWarnings::H264(w) => tracing::warn!(
                concealed,
                warnings = ?w,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            ),
            PlanWarnings::H265(w) => tracing::warn!(
                concealed,
                warnings = ?w,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            ),
            PlanWarnings::Av1(w) => tracing::warn!(
                concealed,
                warnings = ?w,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            ),
        }
    }

    /// Spec-legal envelope: planned in full, frame shown. `warn` not `debug` because
    /// it is rare (SPS activation, MMCO 5). Spelled on AV1 so a future spec-legal
    /// warning does not take the concealment branch.
    fn warn_planned_in_full(&self) {
        match self {
            PlanWarnings::H264(w) => tracing::warn!(
                warnings = ?w,
                "native decode: spec-legal envelope signal — the AU was planned in \
                 full and the frame is kept"
            ),
            PlanWarnings::H265(w) => tracing::warn!(
                warnings = ?w,
                "native decode: spec-legal envelope signal — the AU was planned in \
                 full and the frame is kept"
            ),
            PlanWarnings::Av1(w) => tracing::warn!(
                warnings = ?w,
                "native decode: spec-legal envelope signal — the AU was planned in \
                 full and the frame is kept"
            ),
        }
    }
}

/// Shipped to the presenter, not yet settled: token back (GPU reads done) and status read.
struct Shipped {
    seq: u64,
    frame: DecodedVkFrame,
    released: bool,
    /// Sampling submit enqueued `value + 1`; forwarded so the decoder waits write-back.
    presented: bool,
    resolved: bool,
    polls_after_release: u32,
}

/// Mark the named shipped entry released. `false` is a late token after a demotion drain.
fn note_token(outstanding: &mut [Shipped], token: NativeReleaseToken) -> bool {
    match outstanding.iter_mut().find(|s| s.seq == token.seq) {
        Some(s) => {
            debug_assert_eq!(
                s.frame.generation, token.generation,
                "a token's generation always matches the frame it rode on"
            );
            s.released = true;
            s.presented = token.presented;
            true
        }
        None => false,
    }
}

/// Lift a [`DecodedVkFrame`] into the ash-free [`NativeVkFrame`] the presenter consumes.
///
/// [`NativeVkFrame::vk_format`] is the stream's format, not the codec's. H.265 can
/// switch NV12 / P010 / 4:4:4 mid-stream; inferring 8-bit 4:2:0 from the codec
/// applies the wrong CSC. Carry it; never infer.
fn project_frame(frame: &DecodedVkFrame, guard: NativeReleaseGuard) -> NativeVkFrame {
    NativeVkFrame {
        image: frame.image.as_raw(),
        vk_format: crate::video::RawVkFormat(frame.format.as_raw()),
        plane_views: [frame.plane_views[0].as_raw(), frame.plane_views[1].as_raw()],
        layer: frame.layer,
        layout: if frame.layout == vk::ImageLayout::VIDEO_DECODE_DPB_KHR {
            NativeVkLayout::DecodeDpb
        } else {
            NativeVkLayout::DecodeDst
        },
        semaphore: frame.semaphore.as_raw(),
        semaphore_value: frame.value,
        generation: frame.generation,
        width: frame.crop.width,
        height: frame.crop.height,
        coded_width: frame.coded_width,
        coded_height: frame.coded_height,
        crop_x: frame.crop.x,
        crop_y: frame.crop.y,
        // Per frame, never latched: HDR can switch PQ in-band after an SDR Welcome.
        color: ColorDesc::from(&frame.colour),
        keyframe: frame.is_idr,
        poc: frame.poc,
        // Recovery-point SEI for this picture (`RecoveryWatch` at plan time). Separate
        // types on purpose: pf-vkdecode must not depend on punktfunk-core for a
        // bitstream fact, and the reverse for accepting one.
        recovery: punktfunk_core::reanchor::LocalRecovery {
            sei_here: frame.recovery.sei_here,
            is_recovery_point: frame.recovery.is_recovery_point,
        },
        // This picture's references decoded cleanly — the corroboration against a host
        // `USER_FLAG_RECOVERY_ANCHOR`. Only the planner resolved this AU's lists.
        references_clean: frame.references_clean,
        // Which side of a loss this picture was decoded on. A post-failure DPB flush
        // can deliver pre-loss pictures whose recovery marks describe a finished wave.
        decode_order: frame.decode_order,
        copyable: frame.copyable,
        guard,
    }
}

/// Picture format for a negotiated stream shape, or a named refusal. `codec` is the
/// refusal label only; the map is the crate's (sampling, depth) → format table.
///
/// Device-independent half of [`NativeVulkanDecoder::new`]: 4:2:2, monochrome, and
/// 12-bit have no plumbing here, so no driver is asked. Device-dependent refusal is
/// `probe_stream_support`. One function for both codecs: AV1's profile builder admits
/// exactly the pairs [`pf_vkdecode::output_format_for`] maps.
fn picture_format(codec: &str, stream: crate::video::StreamFormat) -> Result<vk::Format> {
    let depth = stream.bit_depth_minus8().ok_or_else(|| {
        anyhow!(
            "negotiated {codec} bit depth {} is outside the 8/10-bit decode envelope",
            stream.bit_depth
        )
    })?;
    pf_vkdecode::output_format_for(stream.chroma_format_idc, depth).ok_or_else(|| {
        anyhow!(
            "no native picture format for the negotiated {codec} stream shape \
             (chroma_format_idc={}, {}-bit)",
            stream.chroma_format_idc,
            stream.bit_depth
        )
    })
}

/// Film-grain flag the AV1 construction-time probe asks. Grain is part of the decode
/// profile and negotiation carries no grain bit, so this is an assumption.
///
/// `false`: punktfunk hosts encode desktop capture (grain off), and grain is an added
/// capability, never a replacement — probing `true` would refuse every grain-less
/// device. A grain stream still fails at the first AU via `ensure_state` (sequence
/// header, never softened), same backstop as a level above `maxLevelIdc`.
const AV1_PROBE_FILM_GRAIN: bool = false;

/// Worst-case delivered-but-unreleased frames the client pipeline holds at once.
/// [`pf_vkdecode::HOLD_HEADROOM`] enumerates 4–7; the bound uses the maximum.
const PIPELINE_HOLD: usize = 7;

/// Display-ready frames this backend may hold for later AUs ([`trim_deliverable`]).
///
/// Derived: [`pf_vkdecode::HOLD_HEADROOM`] is total delivered-but-unreleased pool
/// budget (`picture_count = required_slots + HOLD_HEADROOM`). A queued frame counts
/// like a shipped one from `build_frame` until [`Codec::release_frame`]. Deeper than
/// `HOLD_HEADROOM - PIPELINE_HOLD` still hits `NoFreeSlot` (queue + in-flight).
///
/// The status-query ring is `picture_count` deep and re-armed per submit (up to two
/// per temporal unit). A frame waiting `MAX_DELIVERABLE` AUs burns
/// `2 * (MAX_DELIVERABLE + 1)` slots unread; overrun reads as `Failed` and
/// [`NativeVulkanDecoder::settle_statuses`] attributes it to `driver_failed`.
/// At this depth the wait is ~4 of 17.
///
/// H.265 bumping can produce a burst; AV1 spec admits one shown frame per temporal
/// unit, so this is defence against a non-conformant stream. Oldest-first drop: the
/// alternative (queue the burst) spends the whole headroom and `NoFreeSlot`s next AU.
const MAX_DELIVERABLE: usize = pf_vkdecode::HOLD_HEADROOM as usize - PIPELINE_HOLD;

// Must leave room for the one frame a two-output AU strands. `PIPELINE_HOLD >=
// HOLD_HEADROOM` would drop every burst — or underflow this const.
const _: () = assert!(
    MAX_DELIVERABLE >= 1,
    "the deliverable queue must be able to carry at least one frame between AUs"
);

pub(crate) struct NativeVulkanDecoder {
    dec: Codec,
    /// Cloned into every shipped guard. `Option` so teardown can drop this sender;
    /// then `release_rx` reports Disconnected once the last guard is gone.
    release_tx: Option<mpsc::Sender<NativeReleaseToken>>,
    release_rx: mpsc::Receiver<NativeReleaseToken>,
    /// Display-ready frames not yet handed to the pump (H.265 burst / extra AV1).
    /// Oldest first, bounded by [`MAX_DELIVERABLE`]; each holds a pool image.
    deliverable: std::collections::VecDeque<DecodedVkFrame>,
    outstanding: Vec<Shipped>,
    next_seq: u64,
    health: DecodeHealth,
    /// Stream damage; host should re-anchor. Drained into the shared `want_keyframe`.
    want_recovery: bool,
    /// `PUNKTFUNK_AU_FAULT`. Armed here so a faulted AU is byte-identical to a lossy
    /// network delivery; other backends cannot see a typo'd variable.
    fault: Option<pf_vkdecode::AuFault>,
    /// Intel on i915: the GEM wait that keeps the media engine clocked (`wait_timeline`).
    #[cfg(target_os = "linux")]
    boost: Option<pf_dmabuf::i915_boost::I915Boost>,
}

// SAFETY: used strictly serially through `&mut self` from the session pump that owns
// the enclosing `Decoder`. `Send` only moves that ownership. Planner `Rc`s never
// escape, so they move together; every queue submit runs under the collision-aware
// lock; the mpsc endpoints are `Send`. Not `Sync`.
unsafe impl Send for NativeVulkanDecoder {}

impl NativeVulkanDecoder {
    /// Build over the presenter's device for the negotiated `codec`, already admitted
    /// by `video::native_vulkan_gate`. The decoder re-checks the family's codec op:
    /// a video session for an operation the family cannot run is UB, not an error.
    ///
    /// Sessions and pools are lazy from the first AU, so stream shape is checked here
    /// against Welcome. A mid-stream format miss is an error streak that demotes past
    /// this rung, often to software. A construction failure walks the ladder instead.
    ///
    /// Level above `maxLevelIdc`, SPS/sequence disagreeing with Welcome, and AV1 film
    /// grain ([`AV1_PROBE_FILM_GRAIN`]) still fail at first decode.
    /// H.264 is not probed: envelope is fixed 8-bit 4:2:0; the never-delivered arm
    /// is the backstop on the hardware-verified path.
    pub(crate) fn new(
        vk: &VulkanDecodeDevice,
        codec: NativeCodec,
        stream: crate::video::StreamFormat,
    ) -> Result<NativeVulkanDecoder> {
        if !vk.video_decode {
            bail!("presenter device lacks Vulkan Video decode");
        }
        let lock = queue_lock_for(vk);
        let handles = device_handles(vk);
        // Presenter's live instance/device; they outlive the pump (torn down first).
        // `video_decode` means the presenter enabled the decode extension stack and
        // per-codec decode extensions. Decoders re-check the family's
        // `videoCodecOperations` — same fact `native_vulkan_gate` and `vk/setup.rs` use.

        // Intel's media engine idles at 100 MHz and i915 raises it only for its own GEM
        // wait, so this rung waits every decode through one, on the exported ring.
        #[cfg(target_os = "linux")]
        let boost = (vk.vendor_id == crate::video::VENDOR_INTEL && vk.dmabuf_import)
            .then(pf_dmabuf::i915_boost::I915Boost::open)
            .flatten();
        #[cfg(not(target_os = "linux"))]
        let boost: Option<()> = None;
        // The native Wayland lane copies pictures out; only a presenter that can export asks.
        let copy_out = crate::video::native_scanout_wanted() && vk.dmabuf_import;
        let dec = match codec {
            NativeCodec::H264 => {
                // SAFETY: the handle contract stated directly above.
                let mut d = unsafe { VkH264Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkH264Decoder init: {e}"))?;
                if boost.is_some() {
                    d.export_bitstream();
                }
                if copy_out {
                    d.copy_out();
                }
                Codec::H264(d)
            }
            NativeCodec::H265 => {
                // Shape with no pf-vkdecode picture format (4:2:2, 12-bit) needs no driver.
                let wanted = picture_format("HEVC", stream)?;
                // SAFETY: the handle contract stated directly above.
                let mut d = unsafe { VkH265Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkH265Decoder init: {e}"))?;
                // A host may cut a second slice whatever the caps asked for.
                if !crate::video::multi_slice_decodable(Some(vk.vendor_id)) {
                    d.refuse_multi_slice();
                }
                if boost.is_some() {
                    d.export_bitstream();
                }
                if copy_out {
                    d.copy_out();
                }
                // Does this driver advertise that format for this profile? Same query
                // `ensure_state` would run at the first AU — only the timing differs.
                let depth = stream
                    .bit_depth_minus8()
                    .expect("picture_format accepted the depth");
                d.probe_stream_support(stream.chroma_format_idc, depth)
                    .map_err(|e| {
                        anyhow!(
                            "device cannot decode the negotiated HEVC stream shape \
                             (chroma_format_idc={}, {}-bit, needs {wanted:?}): {e}",
                            stream.chroma_format_idc,
                            stream.bit_depth
                        )
                    })?;
                Codec::H265(d)
            }
            NativeCodec::Av1 => {
                // AV1 profile is (seq_profile, sampling, depth, film grain). Advertising
                // the AV1 decode op does not offer every profile. Refuse here so the
                // ladder walks; discovered at first AU the only exit is an error streak.
                let wanted = picture_format("AV1", stream)?;
                // SAFETY: the handle contract stated directly above.
                let mut d = unsafe { VkAv1Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkAv1Decoder init: {e}"))?;
                if boost.is_some() {
                    d.export_bitstream();
                }
                if copy_out {
                    d.copy_out();
                }
                d.probe_stream_support(
                    stream.chroma_format_idc,
                    // AV1 profile key takes absolute bit depth (8/10), not H.265's
                    // `bit_depth_luma_minus8`. `picture_format` already proved the envelope.
                    stream.bit_depth,
                    AV1_PROBE_FILM_GRAIN,
                )
                .map_err(|e| {
                    anyhow!(
                        "device cannot decode the negotiated AV1 stream shape \
                         (chroma_format_idc={}, {}-bit, needs {wanted:?}): {e}",
                        stream.chroma_format_idc,
                        stream.bit_depth
                    )
                })?;
                Codec::Av1(d)
            }
        };
        let (release_tx, release_rx) = mpsc::channel();
        let status_queries = dec.status_queries();
        if !status_queries {
            // Once at construction: a clean integrity report here means "nothing was
            // detectable", not "nothing was wrong" — no stats window after the fact.
            tracing::warn!(
                "native decode: this device's decode queue family does not support \
                 RESULT_STATUS queries — driver-reported corruption is not \
                 observable on this session (decode status degrades to timeline \
                 completion — the only signal libavcodec's rungs ever had)"
            );
        }
        // `PUNKTFUNK_AU_FAULT=<mode>[:<period>]`. Unset is normal; a spec that does
        // not parse leaves the injector disarmed rather than half-arming.
        let fault = std::env::var("PUNKTFUNK_AU_FAULT").ok().and_then(|spec| {
            match pf_vkdecode::AuFault::from_spec(&spec) {
                Some(f) => {
                    tracing::warn!(
                        mode = ?f.mode(),
                        period = f.period(),
                        "PUNKTFUNK_AU_FAULT: deliberately corrupting decoder input"
                    );
                    Some(f)
                }
                None => {
                    tracing::warn!(
                        value = %spec,
                        "PUNKTFUNK_AU_FAULT not understood (want drop|truncate|flip[:period]) \
                         — ignored"
                    );
                    None
                }
            }
        });
        Ok(NativeVulkanDecoder {
            dec,
            release_tx: Some(release_tx),
            release_rx,
            deliverable: std::collections::VecDeque::new(),
            outstanding: Vec::new(),
            next_seq: 0,
            health: DecodeHealth {
                status_queries,
                ..DecodeHealth::default()
            },
            want_recovery: false,
            fault,
            #[cfg(target_os = "linux")]
            boost,
        })
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    /// Newest planned picture's decode-order ordinal ([`NativeVkFrame::decode_order`]).
    pub(crate) fn decode_order(&self) -> u64 {
        self.dec.decode_order()
    }

    /// Drain the re-anchor ask. Separate from `Err`: concealment is a stream fact
    /// and must not tick the decoder-demotion streak.
    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.want_recovery)
    }

    /// The gate lifted on intra refresh marks: the planner's damaged-chain marks are stale
    /// (`CleanLedger::clear`).
    pub(crate) fn forgive_unclean(&mut self) {
        self.dec.forgive_unclean();
    }

    /// One complete access unit; at most one displayable frame out.
    ///
    /// AV1 may decode several frames (hidden ones never reach this ledger). Extra
    /// displayable pictures wait in [`Self::deliverable`], bounded by [`MAX_DELIVERABLE`].
    ///
    /// `Ok(Some)` is display-ready. `Ok(None)` is buffer, H.265 RASL skip, or
    /// concealment (output released unshown, [`Self::take_recovery_request`] raised).
    /// `Err` is decoder trouble (Vulkan/session, unresolved AV1 ref, keyframe wait,
    /// or a prior-frame `RESULT_STATUS` Failed) — streak/demotion may act.
    ///
    /// Decode this AU first so planner state advances even if a prior frame's status
    /// is Failed; otherwise a recovery IDR lands on a skipped AU and a phantom gap.
    /// A RASL skip is not trouble: never a `VkDecodeError`, no concealment, no re-anchor.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<NativeVkFrame>> {
        self.drain_releases();

        // Last moment before the decoder: a faulted AU is byte-for-byte a lossy
        // delivery. Inert unless armed.
        let faulted;
        let au = match self.fault.as_mut().map(|f| f.apply(au)) {
            None | Some(pf_vkdecode::FaultAction::Pass) => au,
            Some(pf_vkdecode::FaultAction::Drop) => {
                tracing::warn!(len = au.len(), "PUNKTFUNK_AU_FAULT: dropping this AU");
                // Never fed — same observable as a lost AU. Still settle prior
                // verdicts so query slots do not sit unread. Do not fold a clean
                // `note`: this AU never reached the decoder, so it is not evidence
                // of health (that would reset the concealed run on a lost AU).
                let verdicts = self.settle_statuses();
                if verdicts.total() > 0 {
                    self.health.note(false, false, verdicts.total());
                    return Err(self.status_error(verdicts));
                }
                return Ok(None);
            }
            Some(pf_vkdecode::FaultAction::Corrupt(bytes)) => {
                tracing::warn!(
                    len = au.len(),
                    corrupted_len = bytes.len(),
                    "PUNKTFUNK_AU_FAULT: corrupting this AU"
                );
                faulted = bytes;
                &faulted[..]
            }
        };

        // Fold a refusal into the health ledger before returning: there is no
        // after `?`. Prior-frame verdicts settle first (they still owe a read).
        // A rung refusing every AU must not report `damaged 0 · failed 0 · run 0`.
        let delivered = match self.dec.decode(au) {
            Ok(delivered) => delivered,
            Err(e) => {
                // Nothing from a refused AU reaches the screen. AV1 can leave frame 1
                // in `ready` when frame 2 fails; `take_ready` on the next AU would ship
                // it with an empty warning ledger. H.26x `recover_dpb` flushes pre-loss
                // pictures into `AwaitingIdr` here; release returns the pool slot sooner.
                while let Some(frame) = self.dec.take_ready() {
                    if let Err(e) = self.dec.release_frame(&frame, false) {
                        tracing::debug!(error = %e, "releasing a stranded frame failed");
                    }
                }
                // Warnings describe a plan whose frames were released unshown;
                // carrying them would make the next AU look freshly damaged.
                let _ = self.dec.take_warnings();
                let verdicts = self.settle_statuses();
                // Not fed: idle until the IDR, not a refusal. The IDR request rides
                // the concealment path so the host resends parameter sets.
                if e.awaits_idr() {
                    if verdicts.total() > 0 {
                        self.health.note(false, false, verdicts.total());
                    }
                    self.want_recovery = true;
                    tracing::debug!(error = %e, "native decode idle until the next IDR");
                    return Ok(None);
                }
                self.health.note(false, true, verdicts.total());
                tracing::warn!(
                    error = %e,
                    driver_failed = verdicts.driver_failed,
                    "native decode refused the access unit"
                );
                // Typed, so the ladder can read a device fact off the chain.
                return Err(anyhow::Error::new(e).context("decode"));
            }
        };
        let warnings = self.dec.take_warnings();
        // Oldest first. Drain `take_ready` so a burst cannot strand inside the decoder.
        let mut fresh: Vec<DecodedVkFrame> = Vec::new();
        if let Some(frame) = delivered {
            fresh.push(frame);
        }
        while let Some(frame) = self.dec.take_ready() {
            fresh.push(frame);
        }

        let verdicts = self.settle_statuses();
        // Only integrity is concealment ([`PlanWarnings`]). Spec-legal envelope
        // (`NonZeroReorder` on SPS activation, `Mmco5Rebase`) was planned in full.
        let integrity = warnings.integrity();
        let concealed = !integrity.is_empty();
        // One fold per AU: a clean AU is what ends a run; a damage-only counter
        // cannot tell a lossy link from a stream that never came back.
        self.health.note(concealed, false, verdicts.total());
        if concealed || verdicts.total() > 0 {
            // This AU's plan needed concealment, or a prior frame's status is bad:
            // output is released unshown either way.
            for frame in fresh {
                if let Err(e) = self.dec.release_frame(&frame, false) {
                    tracing::debug!(error = %e, "releasing an unshown frame failed");
                }
            }
            if verdicts.total() > 0 {
                // Decoder, not stream: streak-eligible, same volume as libavcodec ref-miss.
                return Err(self.status_error(verdicts));
            }
            warnings.warn_concealment(integrity.len());
            self.want_recovery = true;
            return Ok(None);
        }
        if !warnings.is_empty() {
            warnings.warn_planned_in_full();
        }

        self.deliverable.extend(fresh);
        // This AU's frame off the front first: the bound is carry-over
        // ([`trim_deliverable`]), so two outputs ship the first and hold the second.
        let shipped = self.deliverable.pop_front().map(|frame| self.ship(frame));
        // One frame per AU to the caller; anything that cannot drain holds a pool image.
        let queued = self.deliverable.len();
        for frame in trim_deliverable(&mut self.deliverable, MAX_DELIVERABLE) {
            // `queued` is pre-trim depth: after the trim it would be the constant
            // `MAX_DELIVERABLE` every time.
            if self.health.note_dropped() {
                tracing::warn!(
                    queued,
                    dropped_total = self.health.dropped,
                    poc = frame.poc,
                    "native decode: more display-ready frames than the pump can take — \
                     dropping the oldest so its pool image is not held forever"
                );
            }
            if let Err(e) = self.dec.release_frame(&frame, false) {
                tracing::debug!(error = %e, "releasing an over-queued frame failed");
            }
        }
        Ok(shipped)
    }

    /// Wrap for the presenter and enter the shipped ledger (release/poll need the original).
    fn ship(&mut self, frame: DecodedVkFrame) -> NativeVkFrame {
        let seq = self.next_seq;
        self.next_seq += 1;
        let token = NativeReleaseToken {
            seq,
            generation: frame.generation,
            presented: false,
        };
        let native = project_frame(
            &frame,
            NativeReleaseGuard::new(
                self.release_tx
                    .as_ref()
                    .expect("release_tx lives until Drop")
                    .clone(),
                token,
            ),
        );
        self.outstanding.push(Shipped {
            seq,
            frame,
            released: false,
            presented: false,
            resolved: false,
            polls_after_release: 0,
        });
        native
    }

    /// Intel on i915: the decode wait is the media clock boost, and its GEM wait covers
    /// the whole bitstream ring, so the pump waits each AU at once rather than one behind.
    pub(crate) fn boosted(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.boost.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Bounded wait for a shipped frame's decode-complete signal (pump decode-latency).
    /// Lookup is the liveness proof: an unreleased frame pins its pool; a pair matching
    /// nothing (already settled, or stray) declines the sample instead of unknown handles.
    pub(crate) fn wait_timeline(&mut self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        // The GEM wait is the one i915 boosts for; the semaphore wait then returns at once.
        #[cfg(target_os = "linux")]
        if let (Some(boost), Some(fd)) = (self.boost.as_mut(), self.dec.bitstream_dmabuf()) {
            if boost.track(fd) {
                boost.wait(timeout_ns.min(i64::MAX as u64) as i64);
            }
        }
        self.outstanding
            .iter()
            .find(|s| s.frame.semaphore.as_raw() == sem && s.frame.value == value)
            .is_some_and(|s| self.dec.wait_decoded(&s.frame, timeout_ns))
    }

    /// Whether a shipped frame's decode is complete now, without waiting. A pair the
    /// ledger no longer holds was released by the presenter, whose own GPU wait covered
    /// the decode: done.
    pub(crate) fn timeline_done(&mut self, sem: u64, value: u64) -> bool {
        self.outstanding
            .iter()
            .find(|s| s.frame.semaphore.as_raw() == sem && s.frame.value == value)
            .is_none_or(|s| self.dec.wait_decoded(&s.frame, 0))
    }

    /// Drain the release channel. Actual pool release waits for the status read.
    fn drain_releases(&mut self) {
        while let Ok(token) = self.release_rx.try_recv() {
            if !note_token(&mut self.outstanding, token) {
                tracing::debug!(
                    seq = token.seq,
                    generation = token.generation,
                    "release token without an outstanding frame"
                );
            }
        }
    }

    /// Worded for the device: do not blame a driver that cannot report corruption.
    fn status_error(&self, verdicts: StatusVerdicts) -> anyhow::Error {
        if verdicts.driver_failed > 0 {
            anyhow!(
                "driver reported decode corruption on {} prior frame(s) \
                 (RESULT_STATUS_ONLY query) — re-anchor needed",
                verdicts.driver_failed
            )
        } else {
            anyhow!(
                "decode status unreadable on {} prior frame(s) (this device answers \
                 no RESULT_STATUS queries — the verdict degraded to the decode \
                 timeline) — re-anchor needed",
                verdicts.unreadable
            )
        }
    }

    /// Non-blocking poll of every unresolved shipped frame; release those both
    /// status-settled and token-returned. Returns newly-`Failed` frames, split by
    /// whether this device can produce a driver verdict ([`StatusVerdicts`]).
    ///
    /// Polling an unreleased frame is sound: its slot is pinned until `release_frame`,
    /// so the query slot cannot have been recycled (the false-`Failed` that would read).
    fn settle_statuses(&mut self) -> StatusVerdicts {
        let mut verdicts = StatusVerdicts::default();
        // Decides which kind of `Failed` this is ([`StatusVerdicts`]), never whether to drop.
        let status_queries = self.dec.status_queries();
        let Self {
            dec, outstanding, ..
        } = self;
        for s in outstanding.iter_mut() {
            if s.resolved {
                continue;
            }
            // Rebuild retired this frame's session objects (query pool included).
            // `poll_status` would report Failed, which is not driver corruption.
            // The rebuild rode an IDR, so the stream already has its re-anchor.
            if s.frame.generation != dec.generation() {
                tracing::debug!(
                    poc = s.frame.poc,
                    frame_generation = s.frame.generation,
                    "outstanding frame outlived its session generation — status unknowable"
                );
                s.resolved = true;
                continue;
            }
            match dec.poll_status(&s.frame) {
                DecodeStatus::Ok => s.resolved = true,
                DecodeStatus::Failed => {
                    s.resolved = true;
                    if status_queries {
                        verdicts.driver_failed += 1;
                        tracing::warn!(
                            poc = s.frame.poc,
                            slot = s.frame.query_slot,
                            "decode status query: Failed (driver-reported corruption)"
                        );
                    } else {
                        // No query pool: this is not the driver's opinion of the decode.
                        // Drop the picture the same way; do not invent attribution.
                        verdicts.unreadable += 1;
                        tracing::warn!(
                            poc = s.frame.poc,
                            "decode status unreadable — this device answers no \
                             RESULT_STATUS queries, so this is a timeline failure, \
                             not a driver verdict"
                        );
                    }
                }
                DecodeStatus::Pending => {
                    if s.released {
                        // Token back ⇒ decode completed before sampling ⇒ query should be readable.
                        s.polls_after_release += 1;
                        if s.polls_after_release >= MAX_POLLS_AFTER_RELEASE {
                            tracing::debug!(
                                poc = s.frame.poc,
                                "status query still pending after release — giving \
                                 the slot back with an unknown verdict"
                            );
                            s.resolved = true;
                        }
                    }
                }
            }
        }
        outstanding.retain(|s| {
            if !(s.released && s.resolved) {
                return true;
            }
            match dec.release_frame(&s.frame, s.presented) {
                Ok(()) => {}
                // Stale-generation frames release into the graveyard (rebuild retires
                // a still-held pool intact). `Err` is a double-release ghost, not a
                // dangling image.
                Err(e) => tracing::debug!(error = %e, "release_frame: {e}"),
            }
            false
        });
        verdicts
    }
}

impl Drop for NativeVulkanDecoder {
    fn drop(&mut self) {
        // Run loop drops the presenter's retired frame before joining this pump, so
        // tokens are in-channel or imminent. Bound below is that hand-off, not GPU work.
        // Frames never handed to the pump release unsampled.
        for frame in std::mem::take(&mut self.deliverable) {
            if let Err(e) = self.dec.release_frame(&frame, false) {
                tracing::debug!(error = %e, "releasing an undelivered frame failed");
            }
        }
        // Drop our sender first: once every shipped guard is gone, Disconnected
        // short-circuits the wait instead of burning the budget on a gone presenter.
        drop(self.release_tx.take());
        // Wait for every shipped token before pool destroy: a token proves the
        // sampling fence was waited. Graveyarded pools use the same contract —
        // stale-generation releases route there, so those pools die after last fence.
        let deadline = Instant::now() + TEARDOWN_BUDGET;
        loop {
            self.drain_releases();
            let Self {
                dec, outstanding, ..
            } = self;
            outstanding.retain(|s| {
                if !s.released {
                    return true;
                }
                if let Err(e) = dec.release_frame(&s.frame, s.presented) {
                    tracing::debug!(error = %e, "teardown release_frame: {e}");
                }
                false
            });
            if self.outstanding.is_empty() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!(
                    outstanding = self.outstanding.len(),
                    "native decode teardown: presenter still holds frames past the \
                     budget — destroying the pools anyway"
                );
                break;
            }
            match self
                .release_rx
                .recv_timeout((deadline - now).min(Duration::from_millis(50)))
            {
                Ok(token) => {
                    note_token(&mut self.outstanding, token);
                }
                // Every sender gone: no more tokens can arrive. Outstanding is a
                // bookkeeping ghost, not a held frame.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !self.outstanding.is_empty() {
                        tracing::debug!(
                            outstanding = self.outstanding.len(),
                            "release channel disconnected with entries outstanding — \
                             no tokens can arrive; proceeding with teardown"
                        );
                    }
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }
        // `self.dec` drops after this body: drains decode-side GPU work and destroys
        // remaining graveyard pools (a forfeit means the presenter held past budget).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture whose every field is a distinct non-zero. [`project_frame`] bugs are
    /// field swaps and drops; zeros hide them.
    fn decoded(format: vk::Format, layout: vk::ImageLayout, generation: u64) -> DecodedVkFrame {
        DecodedVkFrame {
            image: vk::Image::from_raw(0x1001),
            format,
            view: vk::ImageView::from_raw(0x2001),
            plane_views: [
                vk::ImageView::from_raw(0x2002),
                vk::ImageView::from_raw(0x2003),
            ],
            layer: 3,
            layout,
            coded_width: 1920,
            coded_height: 1088,
            // Non-origin crop: x != y so an x/y swap in the projection is visible.
            crop: pf_vkdecode::DisplayCrop {
                x: 8,
                y: 4,
                width: 1904,
                height: 1072,
            },
            // Distinct CICP code points so no two colour fields share a value.
            colour: pf_vkdecode::ColourDescription {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 10,
                video_full_range: true,
            },
            semaphore: vk::Semaphore::from_raw(0x3001),
            value: 7,
            poc: 5,
            is_idr: true,
            // Both set: dropping the recovery mark would reinstate the 500 ms freeze
            // on every intra-refresh session, and `false` would hide it.
            recovery: pf_vkdecode::RecoveryMark {
                sei_here: true,
                is_recovery_point: true,
            },
            // True per this fixture's no-false-boolean rule. Default `false` would
            // make the gate refuse every host recovery anchor on a healthy stream.
            references_clean: true,
            // Distinct from every other number: drop would make every frame look
            // pre-loss (0) and disable local recovery.
            decode_order: 17,
            query_slot: 2,
            submission: 11,
            picture: 6,
            generation,
            // True per the no-false-boolean rule: a dropped field would read `false`.
            copyable: true,
        }
    }

    /// Ledger entry with inert handles — the bookkeeping under test is pure.
    fn shipped(seq: u64, generation: u64) -> Shipped {
        Shipped {
            seq,
            // Ledger under test never reads picture format; NV12 is what H.264 delivers.
            frame: decoded(
                pf_vkdecode::NV12,
                vk::ImageLayout::VIDEO_DECODE_DST_KHR,
                generation,
            ),
            released: false,
            presented: false,
            resolved: false,
            polls_after_release: 0,
        }
    }

    fn project(frame: &DecodedVkFrame) -> NativeVkFrame {
        let (tx, _rx) = mpsc::channel();
        project_frame(
            frame,
            NativeReleaseGuard::new(
                tx,
                NativeReleaseToken {
                    seq: 0,
                    generation: frame.generation,
                    presented: false,
                },
            ),
        )
    }

    /// Picture format is the stream's and must reach the presenter. Dropping it
    /// would CSC a Main 10 picture with 8-bit math — decoded right, shown wrong.
    #[test]
    fn the_projection_carries_the_pictures_own_format_whatever_the_codec() {
        for format in [
            pf_vkdecode::NV12,
            pf_vkdecode::P010,
            pf_vkdecode::YUV444_8,
            pf_vkdecode::YUV444_10,
        ] {
            let frame = decoded(format, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 1);
            assert_eq!(
                project(&frame).vk_format,
                crate::video::RawVkFormat(format.as_raw()),
                "the presenter reads the format off the frame, never off the codec"
            );
        }
    }

    /// Every projected field, against distinct values (see [`decoded`]). A new
    /// [`NativeVkFrame`] field should fail to compile here before it can go unchecked.
    #[test]
    fn the_projection_carries_every_field_the_presenter_can_no_longer_look_up() {
        let frame = decoded(pf_vkdecode::P010, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 4);
        let p = project(&frame);
        // Destructure, not field access: a new NativeVkFrame field breaks this pattern.
        let NativeVkFrame {
            image,
            vk_format,
            plane_views,
            layer,
            layout,
            semaphore,
            semaphore_value,
            generation,
            width,
            height,
            coded_width,
            coded_height,
            crop_x,
            crop_y,
            color,
            keyframe,
            poc,
            recovery,
            references_clean,
            decode_order,
            copyable,
            guard: _,
        } = p;
        assert!(
            copyable,
            "the pool's TRANSFER_SRC answer reaches the presenter"
        );
        assert_eq!(image, 0x1001);
        assert_eq!(
            vk_format,
            crate::video::RawVkFormat(pf_vkdecode::P010.as_raw())
        );
        assert_eq!(
            plane_views,
            [0x2002, 0x2003],
            "the plane views, in order — NOT the whole-image view (0x2001)"
        );
        assert_eq!(layer, 3, "the picture's array layer, not slot 0");
        assert_eq!(layout, NativeVkLayout::DecodeDst);
        assert_eq!(semaphore, 0x3001);
        assert_eq!(
            semaphore_value, 7,
            "the frame's timeline value — not its POC (5)"
        );
        assert_eq!(generation, 4);
        assert_eq!((width, height), (1904, 1072), "the display crop's SIZE");
        assert_eq!(
            (coded_width, coded_height),
            (1920, 1088),
            "the allocated surface — the UV-scale denominator"
        );
        assert_eq!((crop_x, crop_y), (8, 4), "the crop ORIGIN, x then y");
        assert_eq!(color.primaries, 9);
        assert_eq!(color.transfer, 16);
        assert_eq!(color.matrix, 10);
        assert!(color.full_range);
        assert!(
            keyframe,
            "is_idr rides through as the pump's re-anchor signal"
        );
        assert_eq!(poc, 5);
        assert_eq!(
            recovery,
            punktfunk_core::reanchor::LocalRecovery {
                sei_here: true,
                is_recovery_point: true,
            },
            "the recovery point SEI's verdict reaches the gate — it is the ONLY \
             clean point an intra-refresh session has"
        );
        assert!(
            references_clean,
            "the reference-cleanliness verdict rides along — without it the gate \
             cannot refute a host recovery anchor that names a picture this decoder \
             had to conceal, which is the grey-with-motion field report"
        );
        assert_eq!(
            decode_order, 17,
            "the decode ordinal rides along — without it the pump cannot tell a \
             frame decoded before a loss from one decoded after it"
        );

        // Coincide mode: the picture is a DPB slot; presenter must restore DPB layout.
        let dpb = project(&decoded(
            pf_vkdecode::NV12,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
            4,
        ));
        assert_eq!(dpb.layout, NativeVkLayout::DecodeDpb);
    }

    /// Device-independent shape refusal. A shape with no picture format must fail
    /// where `Decoder::new` can still walk the ladder, not at the first AU.
    #[test]
    fn a_stream_shape_with_no_native_picture_format_is_refused_at_construction() {
        use crate::video::StreamFormat;
        let f = |chroma, bit_depth| {
            picture_format(
                "HEVC",
                StreamFormat {
                    chroma_format_idc: chroma,
                    bit_depth,
                },
            )
        };
        assert_eq!(f(1, 8).unwrap(), pf_vkdecode::NV12);
        assert_eq!(f(1, 10).unwrap(), pf_vkdecode::P010);
        assert_eq!(f(3, 8).unwrap(), pf_vkdecode::YUV444_8);
        assert_eq!(f(3, 10).unwrap(), pf_vkdecode::YUV444_10);
        assert_eq!(
            picture_format("HEVC", StreamFormat::SDR_420_8).unwrap(),
            pf_vkdecode::NV12,
            "the default/older-host shape is the ordinary one"
        );
        assert!(f(2, 8).is_err(), "4:2:2");
        assert!(f(0, 8).is_err(), "monochrome");
        // 12-bit has no format; a depth below 8 must not wrap into `bit_depth_luma_minus8`.
        assert!(f(1, 12).is_err(), "12-bit");
        assert!(
            f(1, 0).is_err(),
            "an absurd depth refuses, never underflows"
        );
        assert!(f(3, 6).is_err());
    }

    /// AV1 construction-time gate is the same envelope: [`picture_format`] refuses
    /// first, then `Av1ProfileKey` refuses the same set. Disagreement would either
    /// refuse a decodable shape or admit one that fails mid-stream (error streak).
    /// The label is checked because it is what distinguishes an AV1 refusal from HEVC.
    #[test]
    fn the_av1_shape_gate_admits_exactly_what_pf_vkdecodes_av1_profile_key_admits() {
        use crate::video::StreamFormat;
        let f = |chroma, bit_depth| {
            picture_format(
                "AV1",
                StreamFormat {
                    chroma_format_idc: chroma,
                    bit_depth,
                },
            )
        };
        assert_eq!(f(1, 8).unwrap(), pf_vkdecode::NV12);
        assert_eq!(f(1, 10).unwrap(), pf_vkdecode::P010);
        assert_eq!(f(3, 8).unwrap(), pf_vkdecode::YUV444_8);
        assert_eq!(f(3, 10).unwrap(), pf_vkdecode::YUV444_10);
        assert!(f(0, 8).is_err(), "monochrome");
        assert!(f(2, 8).is_err(), "4:2:2");
        assert!(
            f(4, 8).is_err(),
            "the planner's 4:4:0 sentinel is not 4:4:4"
        );
        assert!(f(1, 12).is_err(), "12-bit");
        assert!(
            f(1, 0).is_err(),
            "an absurd depth refuses, never underflows"
        );

        // The refusal names the codec — the only reason this function takes one.
        let err = format!("{:#}", f(2, 8).unwrap_err());
        assert!(err.contains("AV1"), "{err}");
        let hevc = format!(
            "{:#}",
            picture_format(
                "HEVC",
                StreamFormat {
                    chroma_format_idc: 2,
                    bit_depth: 8
                }
            )
            .unwrap_err()
        );
        assert!(hevc.contains("HEVC"), "{hevc}");

        // The probe's own gate, same questions: the agreement above, asserted.
        for (chroma, depth) in [(1u8, 8u8), (1, 10), (3, 8), (3, 10)] {
            assert!(
                pf_vkdecode::Av1ProfileKey::from_negotiated(chroma, depth, AV1_PROBE_FILM_GRAIN)
                    .is_ok(),
                "{chroma}/{depth} passes here, so it must pass the probe's key too"
            );
        }
        for (chroma, depth) in [(0u8, 8u8), (2, 8), (4, 8), (1, 12), (1, 0)] {
            assert!(
                pf_vkdecode::Av1ProfileKey::from_negotiated(chroma, depth, AV1_PROBE_FILM_GRAIN)
                    .is_err(),
                "{chroma}/{depth} refuses here, so the probe's key must refuse it too"
            );
        }
    }

    /// The deliverable queue hands one frame per AU and every waiter pins a pool
    /// image. Unbounded growth hits `NoFreeSlot` and demotes with no log of the queue.
    ///
    /// Drops from the front: the oldest is several AUs stale and the next stage is
    /// newest-wins. This test is [`trim_deliverable`] alone — not that the caller
    /// trims after taking this AU's frame, nor that drops go to `release_frame`.
    #[test]
    fn the_deliverable_queue_drops_its_oldest_rather_than_pinning_pool_images_forever() {
        let mut q: std::collections::VecDeque<DecodedVkFrame> = (0..5)
            .map(|i| {
                let mut f = decoded(pf_vkdecode::NV12, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 1);
                // Distinct per frame so which were dropped is decidable, not merely counted.
                f.poc = i;
                f
            })
            .collect();

        let dropped = trim_deliverable(&mut q, 3);
        assert_eq!(
            dropped.iter().map(|f| f.poc).collect::<Vec<_>>(),
            vec![0, 1],
            "the OLDEST two come back for release — not the newest"
        );
        assert_eq!(
            q.iter().map(|f| f.poc).collect::<Vec<_>>(),
            vec![2, 3, 4],
            "…and what survives stays in display order"
        );

        assert!(trim_deliverable(&mut q, 3).is_empty());
        assert_eq!(q.len(), 3);

        // Queue at bound plus pipeline hold must fit in HOLD_HEADROOM; otherwise
        // the bound caps memory without preventing `NoFreeSlot`. Pinned to
        // pf-vkdecode's constant so a hardcoded depth fails the build.
        assert!(
            MAX_DELIVERABLE + PIPELINE_HOLD <= pf_vkdecode::HOLD_HEADROOM as usize,
            "a queue of {MAX_DELIVERABLE} on top of the pipeline's {PIPELINE_HOLD} \
             exceeds the {} frames the picture pool is sized for",
            pf_vkdecode::HOLD_HEADROOM
        );

        // Production bound: one AU's surplus is held (the burst this queue exists
        // for); a second AU's is not. Against `MAX_DELIVERABLE` so a `PIPELINE_HOLD`
        // change lands here.
        let mut q: std::collections::VecDeque<DecodedVkFrame> = (0..MAX_DELIVERABLE)
            .map(|i| {
                let mut f = decoded(pf_vkdecode::NV12, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 1);
                f.poc = i as i32;
                f
            })
            .collect();
        assert!(
            trim_deliverable(&mut q, MAX_DELIVERABLE).is_empty(),
            "a queue AT the bound is exactly what a two-output AU leaves behind"
        );
        assert_eq!(q.len(), MAX_DELIVERABLE);

        // A zero bound drains rather than looping or panicking. Teardown does not
        // come through here (`Drop` uses `mem::take`); this pins termination.
        let drained = q.len();
        assert_eq!(trim_deliverable(&mut q, 0).len(), drained);
        assert!(q.is_empty());
        assert!(trim_deliverable(&mut q, 0).is_empty(), "and it terminates");
    }

    #[test]
    fn release_tokens_mark_their_frame_and_tolerate_strays() {
        let mut outstanding = vec![shipped(0, 1), shipped(1, 1)];
        assert!(note_token(
            &mut outstanding,
            NativeReleaseToken {
                seq: 1,
                generation: 1,
                presented: true,
            }
        ));
        assert!(!outstanding[0].released);
        assert!(outstanding[1].released);
        assert!(
            outstanding[1].presented,
            "the token's presented flag rides into the ledger (the decoder waits \
             the presenter's value+1 write-back only when it was really enqueued)"
        );
        // Stray token (already settled, e.g. post-demotion drain) matches nothing.
        assert!(!note_token(
            &mut outstanding,
            NativeReleaseToken {
                seq: 7,
                generation: 1,
                presented: false,
            }
        ));
        assert!(!outstanding[0].released);
    }

    #[test]
    fn the_guard_sends_its_token_exactly_once_on_drop() {
        let (tx, rx) = mpsc::channel();
        let token = NativeReleaseToken {
            seq: 42,
            generation: 3,
            presented: false,
        };
        let guard = NativeReleaseGuard::new(tx, token);
        assert!(
            rx.try_recv().is_err(),
            "nothing is sent while the frame lives"
        );
        drop(guard);
        assert_eq!(rx.try_recv().ok(), Some(token), "drop sends the token");
        assert!(rx.try_recv().is_err(), "exactly once");
    }

    #[test]
    fn a_dropped_unpresented_frame_still_releases_through_the_same_guard() {
        // Newest-wins displacement: the frame never presents, but drop must return its slot.
        let (tx, rx) = mpsc::channel();
        let frame = NativeVkFrame {
            image: 0,
            vk_format: crate::video::RawVkFormat(pf_vkdecode::NV12.as_raw()),
            plane_views: [0; 2],
            layer: 0,
            layout: NativeVkLayout::DecodeDst,
            semaphore: 0,
            semaphore_value: 0,
            generation: 5,
            width: 1920,
            height: 1080,
            coded_width: 1920,
            coded_height: 1088,
            crop_x: 0,
            crop_y: 0,
            color: ColorDesc {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: false,
            },
            keyframe: true,
            poc: 0,
            recovery: punktfunk_core::reanchor::LocalRecovery::NONE,
            references_clean: true,
            decode_order: 1,
            copyable: false,
            guard: NativeReleaseGuard::new(
                tx,
                NativeReleaseToken {
                    seq: 9,
                    generation: 5,
                    presented: false,
                },
            ),
        };
        drop(frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(NativeReleaseToken {
                seq: 9,
                generation: 5,
                presented: false,
            }),
            "an unpresented drop reports presented=false — the decoder must not \
             wait a value+1 write-back that was never enqueued"
        );
    }

    #[test]
    fn a_dead_channel_is_ignored_not_fatal() {
        // Demotion mid-stream: Receiver is gone while the presenter still holds a frame.
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let guard = NativeReleaseGuard::new(
            tx,
            NativeReleaseToken {
                seq: 1,
                generation: 1,
                presented: false,
            },
        );
        drop(guard);
    }

    /// Concealment is integrity warnings, not any warning.
    ///
    /// `NonZeroReorder` fires on SPS activation (opening IDR and every ABR IDR).
    /// Treating it as concealment releases that IDR unshown at every renegotiation.
    #[test]
    fn a_spec_legal_envelope_warning_is_not_concealment() {
        use pf_vkdecode::H265PlanWarning as H265;
        use pf_vkdecode::PlanWarning as H264;

        let reorder = PlanWarnings::H265(vec![H265::NonZeroReorder {
            max_num_reorder_pics: 1,
        }]);
        assert!(!reorder.is_empty(), "it IS a warning and IS logged");
        assert!(
            reorder.integrity().is_empty(),
            "…but it is not concealment: the frame must be shown, not dropped"
        );

        // MMCO 5 was planned in full too (plan carries pre-rebase 8.2.1 values).
        let mmco5 = PlanWarnings::H264(vec![H264::Mmco5Rebase]);
        assert!(!mmco5.is_empty());
        assert!(mmco5.integrity().is_empty());

        // Missing reference or truncated AU is still concealment.
        for w in [
            H264::FrameNumGap {
                expected: 4,
                got: 7,
            },
            H264::MissingReference {
                context: "list0",
                detail: "poc 12".into(),
            },
            H264::TruncatedAu { offset: 900 },
        ] {
            let warnings = PlanWarnings::H264(vec![w]);
            assert_eq!(warnings.integrity().len(), 1, "damage is concealment");
        }
        for w in [
            H265::MissingReference {
                context: "StCurrBefore",
                detail: "poc 12".into(),
            },
            H265::TruncatedAu { offset: 900 },
        ] {
            let warnings = PlanWarnings::H265(vec![w]);
            assert_eq!(warnings.integrity().len(), 1);
        }

        // Mixed AU: damage decides; spec-legal companion rides in the log only.
        let mixed = PlanWarnings::H265(vec![
            H265::NonZeroReorder {
                max_num_reorder_pics: 2,
            },
            H265::TruncatedAu { offset: 12 },
        ]);
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed.integrity().len(), 1);
    }

    /// AV1 has no spec-legal companion to `NonZeroReorder`/`Mmco5Rebase`, so every
    /// warning conceals. An arm wired to the wrong predicate would show a damaged
    /// picture and ask for no re-anchor. `MissingShowExisting` is the one most
    /// likely to be read as harmless (decoded nothing, displayed nothing).
    #[test]
    fn every_av1_warning_conceals_because_av1_has_no_spec_legal_signal() {
        use pf_vkdecode::Av1PlanWarning as Av1;

        for w in [
            Av1::MissingReference {
                slot: 3,
                ref_index: 1,
            },
            Av1::StaleReference {
                slot: 3,
                ref_index: 1,
            },
            Av1::MissingShowExisting { slot: 5 },
            Av1::TruncatedAu { offset: 900 },
        ] {
            let warnings = PlanWarnings::Av1(vec![w.clone()]);
            assert!(!warnings.is_empty());
            assert_eq!(
                warnings.integrity().len(),
                1,
                "{w:?} means the picture is not fit to present"
            );
        }

        // Concealment count is the damage count; here that is the full list.
        let all = PlanWarnings::Av1(vec![
            Av1::MissingReference {
                slot: 0,
                ref_index: 0,
            },
            Av1::MissingShowExisting { slot: 1 },
            Av1::TruncatedAu { offset: 4 },
        ]);
        assert_eq!((all.len(), all.integrity().len()), (3, 3));

        // Empty ledger is clean — do not manufacture concealment on a healthy AU.
        assert!(PlanWarnings::Av1(Vec::new()).is_empty());
        assert!(PlanWarnings::Av1(Vec::new()).integrity().is_empty());
    }

    /// `run` climbs only while damage is consecutive; `worst_run` survives the
    /// recovery that clears `run`. A total alone cannot tell a lossy recovering
    /// link from a stream that stayed down.
    #[test]
    fn the_concealed_run_separates_a_lossy_link_from_a_stream_that_never_came_back() {
        let mut h = DecodeHealth::default();
        // Single damaged AUs with clean stretches between.
        for _ in 0..3 {
            h.note(true, false, 0);
            h.note(false, false, 0);
            h.note(false, false, 0);
        }
        assert_eq!(h.damaged, 3);
        assert_eq!(h.run, 0, "the last AU was clean");
        assert_eq!(h.worst_run, 1, "…and no two damaged AUs were adjacent");

        let mut h = DecodeHealth::default();
        for _ in 0..7 {
            h.note(true, false, 0);
        }
        assert_eq!((h.damaged, h.run, h.worst_run), (7, 7, 7));
        h.note(false, false, 0);
        assert_eq!((h.damaged, h.run, h.worst_run), (7, 0, 7));
    }

    /// A refused AU (`Err`, not concealment) must reach the ledger and stay
    /// separate: concealment means the decoder coped; refusal means it could not run.
    #[test]
    fn a_rung_refusing_every_au_cannot_report_a_clean_bill_of_health() {
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        for _ in 0..5 {
            h.note(false, true, 0);
        }
        assert_eq!(h.refused, 5, "every refusal is counted");
        assert_eq!(h.damaged, 0, "and none of them is concealment");
        assert_eq!(h.failed, 0, "nor a driver verdict — the driver never ran");
        assert_eq!(
            (h.run, h.worst_run),
            (5, 5),
            "a refused AU is as absent from the screen as a concealed one"
        );
        h.note(false, false, 0);
        assert_eq!((h.refused, h.run, h.worst_run), (5, 0, 5));
    }

    /// Three counts, one run: causes differ; "did the picture come back" does not.
    #[test]
    fn concealment_refusal_and_driver_failure_are_three_separate_counts() {
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        h.note(true, false, 0);
        h.note(false, true, 0);
        h.note(false, false, 2);
        assert_eq!((h.damaged, h.refused, h.failed), (1, 1, 2));
        assert_eq!((h.run, h.worst_run), (3, 3), "one unbroken run of three");
    }

    /// Driver `Failed` counts apart from concealment and extends the same run.
    /// Collapsing them is how "the stream is fine, it's your GPU" starts.
    #[test]
    fn driver_failures_count_separately_but_share_the_run() {
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        h.note(false, false, 2);
        assert_eq!((h.damaged, h.failed, h.run), (0, 2, 1));
        h.note(true, false, 1);
        assert_eq!((h.damaged, h.failed, h.run), (1, 3, 2));
        h.note(false, false, 0);
        assert_eq!((h.run, h.worst_run), (0, 2));
    }

    /// `status_queries` is set once from the device; a fold must not turn "cannot
    /// report" into "reported none". With queries off, `failed` stays 0 even when
    /// `note` is fed a real failure — otherwise `driver-failed 1 · no driver status`.
    #[test]
    fn the_status_query_capability_survives_every_fold() {
        let mut h = DecodeHealth {
            status_queries: false,
            ..DecodeHealth::default()
        };
        h.note(true, false, 0);
        h.note(false, false, 0);
        assert!(!h.status_queries);
        h.note(false, false, 1);
        assert_eq!(
            h.failed, 0,
            "a device that answers no status queries can produce no driver \
             verdict — `failed` must stay 0 whatever `read_status` returned"
        );
        assert_eq!(
            h.run, 1,
            "…but the frame was still dropped, so the run still counts it: the \
             ATTRIBUTION is what must not be invented, not the damage"
        );
        assert_eq!(h.worst_run, 1);

        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        h.note(false, false, 1);
        assert_eq!((h.failed, h.run), (1, 1));
    }

    #[test]
    fn the_queue_lock_is_shared_only_when_the_families_collide() {
        // Same family ⇒ same VkQueue (both use index 0) ⇒ shared lock.
        assert!(submit_queues_collide(0, 0));
        assert!(submit_queues_collide(2, 2));
        // A separate decode family has exactly one submitter — no lock.
        assert!(!submit_queues_collide(0, 3));
    }
}
