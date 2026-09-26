//! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).

use super::pw_cursor::{composite_cursor, update_cursor_meta, CursorState};
use super::pw_pods::{
    build_cursor_meta_param, build_default_format_obj, build_dmabuf_buffers, build_dmabuf_format,
    build_hdr_dmabuf_format, build_header_meta_param, build_mappable_buffers,
    build_shm_only_buffers, build_sync_timeline_meta_param, offer_framerate_denom, serialize_pod,
    Pacing, HDR_FORMAT_ORDER,
};
use super::sync_timeline::{hand_back, plane_count, SyncDevice, SyncPoints};
use super::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat, ZeroCopyPolicy};
use anyhow::{Context, Result};
use pipewire as pw;
use pw::{properties::properties, spa};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;

fn map_format(f: VideoFormat) -> Option<PixelFormat> {
    Some(match f {
        VideoFormat::BGRx => PixelFormat::Bgrx,
        VideoFormat::RGBx => PixelFormat::Rgbx,
        VideoFormat::BGRA => PixelFormat::Bgra,
        VideoFormat::RGBA => PixelFormat::Rgba,
        VideoFormat::RGB => PixelFormat::Rgb,
        VideoFormat::BGR => PixelFormat::Bgr,
        VideoFormat::NV12 => PixelFormat::Nv12,
        // Only the `want_hdr` offer negotiates these (MANDATORY PQ/BT.2020): packed
        // 2:10:10:10, or gamescope's own P010.
        VideoFormat::xRGB_210LE => PixelFormat::X2Rgb10,
        VideoFormat::xBGR_210LE => PixelFormat::X2Bgr10,
        VideoFormat::P010_10LE => PixelFormat::P010,
        _ => return None,
    })
}

struct UserData {
    info: VideoInfoRaw,
    /// `None` until `param_changed`, or if the SPA format is unsupported.
    format: Option<PixelFormat>,
    /// DRM modifier for dmabuf import; 0 = LINEAR.
    modifier: u64,
    /// One-deep mailbox; write only through [`UserData::publish`].
    slot: super::FrameSlot,
    wake: SyncSender<()>,
    signals: super::CaptureSignals,
    /// Raw dmabuf to the encoder instead of a CUDA import (VAAPI).
    vaapi_passthrough: bool,
    /// Tiled 10-bit was offered because the encoder's raw convert reads it; hold it, never import.
    hdr_tiled_raw: bool,
    /// CUDA import choices; the consumer imports held frames with the same policy.
    import_policy: ImportPolicy,
    /// Arrival-path import memory. The consumer keeps its own.
    import_state: ImportState,
    /// Rate-limit counter for the latest-frame-only diagnostic (see `.process`).
    dbg_log_n: u64,
    /// Which clock feeds wire `pts_ns`. Delivery stamps sit downstream of compositor jitter.
    pts: crate::pts_provenance::PtsProvenance,
    pts_reported: std::time::Instant,
    /// `CLOCK_REALTIME − CLOCK_MONOTONIC`, ns. Re-sampled each 30 s window; clocks drift by µs.
    rt_minus_mono_ns: i64,
    /// `PUNKTFUNK_CAPTURE_HDR_PTS=0` puts the wire back on the delivery stamp unconditionally.
    hdr_pts_enabled: bool,
    /// Producer-fence wait, measured on this loop thread (a block here delays the next recycle).
    fence_wait: FenceWaitStats,
    /// Negotiated pool depth from `add_buffer`/`remove_buffer`. Budget for a deeper encode pipeline.
    pool: PoolCensus,
    /// Raw-passthrough frames that fell through to CPU, by reason. Fresh `UserData` per pipeline.
    passthrough_fallbacks: PassthroughFallbacks,
    cursor: CursorState,
    /// Sacrificial birth-mode size (kwin.rs `create`). `.process` skips until it matches, then clears.
    expect_dims: Option<(u32, u32)>,
    /// Buffers skipped by `expect_dims` (rate-limits its log).
    gate_skips: u64,
    /// When the gate first held a buffer. After [`GATE_DEADLINE`] it disarms: degraded dims beat a retry loop.
    gate_since: Option<std::time::Instant>,
    /// Encode reads the dmabuf after `.process` returns; do not rejoin the pool until [`BufferHold`] drops.
    defer: std::sync::Arc<DeferredRequeue>,
    /// Lazy-driver pacing; `None` when the producer keeps the tick.
    pacer: Option<std::rc::Rc<Pacer>>,
    /// Arrivals dropped because every hold was out (the slot kept an older frame).
    held_drops: u64,
    /// Explicit-sync device; `None` when the lane cannot offer it. Whether a buffer carries
    /// sync points is the producer's call at negotiation.
    sync: Option<std::sync::Arc<SyncDevice>>,
}

impl UserData {
    /// Latest-wins into [`super::FrameSlot`], then a wakeup edge.
    ///
    /// Must not block: this runs inside `.process` and would stall the compositor. A full
    /// wakeup channel already has a pending edge; the slot is the truth.
    fn publish(&self, frame: CapturedFrame) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(frame);
        }
        let _ = self.wake.try_send(());
    }

    /// Withhold this buffer from the producer until the returned hold drops.
    /// `None` requeues at `.process` return — the producer may then rewrite the dmabuf while
    /// encode still reads it, so no lane publishes a raw frame under `None`: a transient
    /// shortage drops the arrival, a pool that can never hold (`holds_possible` false) takes
    /// the lane's own fallback.
    /// Every hold out with an untaken frame in the slot: that frame gives its hold to this one.
    /// A buffer the book already lists was re-sent by the producer: no hold, and the capture is
    /// flagged for a rebuild.
    fn try_defer(
        &mut self,
        pw_buf: *mut pw::sys::pw_buffer,
        stream: *mut pw::sys::pw_stream,
    ) -> Option<pf_frame::FrameHold> {
        if !zerocopy_hold_enabled() {
            return None;
        }
        let buf = pw_buf as usize;
        // A hold the encoder dropped during this `.process` (the fence wait above is where it
        // lands) is still in the book until the loop services the wake — after this callback.
        // Requeue it now, so the arrival spends a budget that is current.
        // SAFETY: `stream` is the stream whose `.process` is running on this loop thread.
        unsafe { self.defer.drain(stream) };
        // PipeWire below 1.6 (no `node.reliable`) lets the producer reclaim a buffer before this
        // side marked it busy, then send it again. Its frame is fresh; the earlier hold's read may
        // be torn. Re-held under a new generation, the stale hold's release is a no-op
        // (`HoldBook::complete`), the pool stays whole, and the loop answers with one IDR.
        if self.defer.book.lock().ok()?.contains(buf)
            && self.signals.resent.fetch_add(1, Ordering::Relaxed) == 0
        {
            tracing::warn!(
                "producer re-sent a buffer this capture still holds — re-holding it; one IDR \
                 covers the frame encoded from the earlier read (PipeWire < 1.6)"
            );
        }
        let pool_live = self.pool.live;
        let mut generation = self.defer.book.lock().ok()?.try_hold(buf, pool_live);
        if generation.is_none() && self.release_unconsumed(stream) {
            generation = self.defer.book.lock().ok()?.try_hold(buf, pool_live);
        }
        let Some(generation) = generation else {
            if !self.defer.logged_shallow.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    pool_depth = pool_live,
                    reserve = HOLD_POOL_RESERVE,
                    holds_possible = holds_possible(true, pool_live),
                    "zero-copy: the producer's buffer pool cannot spare a buffer to hold across \
                     the encode — while holds are possible at all this arrival is dropped and \
                     the slot keeps its frame (held_drops= on the provenance line); a pool that \
                     can never hold takes the CPU copy instead"
                );
            }
            return None;
        };
        if !self.defer.logged_active.swap(true, Ordering::Relaxed) {
            tracing::info!(
                pool_depth = pool_live,
                reserve = HOLD_POOL_RESERVE,
                "zero-copy: withholding each published buffer from the producer until the \
                 encoder releases it (deferred requeue — the producer can no longer rewrite a \
                 frame mid-encode); PUNKTFUNK_ZEROCOPY_HOLD=0 restores the immediate requeue"
            );
        }
        Some(std::sync::Arc::new(BufferHold {
            defer: self.defer.clone(),
            buf,
            generation,
        }))
    }

    /// Requeue the buffer under the slot's untaken held frame now. `publish` would replace that
    /// frame anyway, but its hold returns only when the loop services the release, too late for
    /// the arrival that needs it. `true` ⇒ one hold came back.
    fn release_unconsumed(&self, stream: *mut pw::sys::pw_stream) -> bool {
        // Out of the slot before the requeue, so the consumer can never take it after.
        let (stale, buf, generation) = {
            let Ok(mut slot) = self.slot.lock() else {
                return false;
            };
            let Some(CapturedFrame {
                payload:
                    FramePayload::Dmabuf(DmabufFrame {
                        hold: Some(hold), ..
                    }),
                ..
            }) = &*slot
            else {
                return false;
            };
            let Some(held) = hold.downcast_ref::<BufferHold>() else {
                return false;
            };
            if std::sync::Arc::strong_count(hold) != 1 {
                return false;
            }
            let (buf, generation) = (held.buf, held.generation);
            (slot.take(), buf, generation)
        };
        // SAFETY: `stream` is the stream whose `.process` is running on this loop thread. The
        // frame left the slot above and its hold is unique, so nothing reads the buffer after.
        let requeued = unsafe { self.defer.release(stream, buf, generation) };
        // Its hold's late release finds the book already completed and no-ops.
        drop(stale);
        requeued
    }
}

/// Facts the zero-copy decision needs, sampled at one instant so the decision is a pure
/// function — shared by the PipeWire thread and `spawn_pipewire` (see [`NegotiationPlan`]).
#[derive(Debug, Clone, Copy)]
pub(super) struct NegotiationInputs {
    pub zerocopy: bool,
    /// `PUNKTFUNK_FORCE_SHM` — race-free download path.
    pub force_shm: bool,
    pub want_hdr: bool,
    pub want_444: bool,
    pub backend_is_vaapi: bool,
    pub pyrowave_session: bool,
    pub native_nv12_session: bool,
    /// Scoped raw-passthrough latch.
    pub raw_dmabuf_import_disabled: bool,
    /// Repeated import-worker deaths.
    pub gpu_import_disabled: bool,
    /// Previous EGL→CUDA dmabuf-only offer timed out (compositor accepts none of the modifiers).
    pub gpu_dmabuf_negotiation_failed: bool,
    pub native_nv12_env_on: bool,
    /// Encoder can ingest packed 10-bit PQ CUDA. Only direct-SDK NVENC can.
    pub hdr_cuda_ok: bool,
    /// `PUNKTFUNK_NV12`: the CUDA import emits NV12 (tiled blit or LINEAR compute CSC).
    pub nv12_env_on: bool,
    /// The NVENC encoder converts held dmabufs itself (`ZeroCopyPolicy::nvenc_raw_dmabuf`).
    pub nvenc_raw: bool,
}

/// Format choices the CUDA import makes per frame; the same on both threads.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ImportPolicy {
    /// `PUNKTFUNK_NV12`: emit NV12 for native NVENC YUV. Off leaves packed RGB.
    pub nv12: bool,
    /// Planar YUV444 on tiled EGL. Wins over `nv12` — 4:4:4 must not subsample.
    pub yuv444: bool,
}

/// Per-stream import memory: the LINEAR NV12 latch and the tiled failure streak.
#[derive(Debug, Default)]
pub(super) struct ImportState {
    /// LINEAR NV12 compute CSC failed once: RGB for the rest of this stream.
    pub linear_nv12_failed: bool,
    /// Consecutive tiled-import failures; reset on success. See [`IMPORT_FAIL_POISON`].
    pub fail_streak: u32,
}

impl ImportPolicy {
    /// A 10-bit SDR session keeps packed RGB whatever `PUNKTFUNK_NV12` asks: NVENC widens 8-bit
    /// to 10-bit only from packed RGB and refuses a planar 8-bit surface in a 10-bit session.
    fn for_ten_bit_sdr(mut self, ten_bit_sdr: bool) -> Self {
        if ten_bit_sdr {
            self.nv12 = false;
        }
        self
    }
}

/// [`gpu_import`]'s verdict. `ImporterLost` is a LINEAR failure: the caller retires the
/// importer and the stream continues on the CPU path.
pub(super) enum ImportOutcome {
    Frame(pf_zerocopy::DeviceBuffer, PixelFormat),
    Dropped,
    ImporterLost,
}

/// Zero-copy negotiation, resolved once and consumed by the PipeWire thread and `spawn_pipewire`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NegotiationPlan {
    pub build_importer: bool,
    /// What every CUDA import of this stream does, on either thread.
    pub import_policy: ImportPolicy,
    /// Held frames go to the encoder as dmabufs; the importer stays for the modifier offer and
    /// for a producer that cannot be held.
    pub nvenc_raw: bool,
    pub vaapi_passthrough: bool,
    pub prefer_native_nv12: bool,
    /// The HDR twin of [`prefer_native_nv12`](Self::prefer_native_nv12): gamescope's P010
    /// pod goes first.
    pub prefer_native_p010: bool,
    /// Carried so [`want_dmabuf`](Self::want_dmabuf) needs no second copy.
    pub force_shm: bool,
    /// Would have taken raw passthrough, but its latch is set.
    pub raw_dmabuf_latched: bool,
    /// Would have built the EGL→CUDA importer, but a latch fired.
    pub gpu_import_latched: bool,
}

/// Resolve the negotiation plan. **Pure** — every environment read is already in `i`.
///
/// Invariants (pinned by `negotiation_plan_invariants`):
/// 1. HDR never takes the 8-bit EGL de-tile blit. An EGL/CUDA fallback offers LINEAR;
///    direct raw lanes may offer proved tiled formats, guarded again per frame.
/// 2. 4:4:4 never prefers producer NV12 or P010 (must not subsample).
/// 3. Producer-native planar only on a `native_nv12_session` under active raw passthrough
///    (the VAAPI session takes RGB; the CUDA importer expects packed RGB): NV12 for SDR,
///    P010 for HDR.
/// 4. Raw passthrough is off once its latch has fired.
pub(super) fn negotiation_plan(i: NegotiationInputs) -> NegotiationPlan {
    // Consumer imports raw dmabufs: VAAPI (libva + GPU CSC) or PyroWave (its Vulkan device).
    let raw_passthrough = i.backend_is_vaapi || i.pyrowave_session;
    // Skip under raw passthrough (payloads only NVENC consumes) and both GPU latches
    // (worker-death crash-loop; compositor that rejects our modifiers would re-pay 10 s).
    // HDR through this importer is LINEAR, so it avoids the 8-bit de-tile blit. Exclude
    // it when the encoder cannot take packed 10-bit CUDA (a build without `nvenc`).
    let build_importer = i.zerocopy
        && !raw_passthrough
        && !i.gpu_import_disabled
        && !i.gpu_dmabuf_negotiation_failed
        && (!i.want_hdr || i.hdr_cuda_ok);
    let vaapi_passthrough =
        i.zerocopy && !i.force_shm && raw_passthrough && !i.raw_dmabuf_import_disabled;
    let native_planar = i.native_nv12_env_on
        && i.native_nv12_session
        && i.backend_is_vaapi
        && vaapi_passthrough
        && !i.pyrowave_session
        && !i.want_444;
    let prefer_native_nv12 = native_planar && !i.want_hdr;
    let prefer_native_p010 = native_planar && i.want_hdr;
    NegotiationPlan {
        build_importer,
        import_policy: ImportPolicy {
            nv12: i.nv12_env_on,
            yuv444: i.want_444,
        },
        nvenc_raw: build_importer && i.nvenc_raw && !i.force_shm && !i.raw_dmabuf_import_disabled,
        vaapi_passthrough,
        prefer_native_nv12,
        prefer_native_p010,
        force_shm: i.force_shm,
        raw_dmabuf_latched: i.zerocopy
            && !i.force_shm
            && raw_passthrough
            && i.raw_dmabuf_import_disabled,
        // Every `build_importer` term except the two latches, then either latch.
        gpu_import_latched: i.zerocopy
            && !raw_passthrough
            && (!i.want_hdr || i.hdr_cuda_ok)
            && (i.gpu_import_disabled || i.gpu_dmabuf_negotiation_failed),
    }
}

impl NegotiationPlan {
    /// Request dmabufs only if the importer actually constructed and returned modifiers.
    pub(super) fn want_dmabuf(&self, have_importer: bool, modifiers: &[u64]) -> bool {
        (have_importer || self.vaapi_passthrough) && !modifiers.is_empty() && !self.force_shm
    }
}

/// Which capture arm a negotiated pipeline resolved to.
///
/// Product of a policy, a latch, whether the importer constructed, and the modifier list.
/// [`resolved_capture_arm`] plus the INFO line at pipeline build is the one place this is stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureArm {
    /// Raw dmabufs to the encoder (libva or PyroWave Vulkan). No host pixel touch.
    DmabufPassthrough,
    /// Held dmabufs go to the NVENC encoder, whose worker converts each in one pass; the
    /// importer stays for the offer and for a producer that cannot be held.
    DmabufToEncoder,
    /// dmabufs imported to CUDA by the EGL→CUDA worker, for NVENC.
    CudaImport,
    /// CPU mmap de-pad. A downgrade when the consumer could have taken a dmabuf.
    Cpu,
}

impl CaptureArm {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            CaptureArm::DmabufPassthrough => "dmabuf-passthrough",
            CaptureArm::DmabufToEncoder => "dmabuf-to-encoder",
            CaptureArm::CudaImport => "cuda-import",
            CaptureArm::Cpu => "cpu",
        }
    }
}

/// Resolve the arm this pipeline ended up on. **Pure.**
///
/// `have_importer` and `want_dmabuf` are the two runtime facts `negotiation_plan` cannot know.
pub(super) fn resolved_capture_arm(
    plan: &NegotiationPlan,
    have_importer: bool,
    want_dmabuf: bool,
) -> CaptureArm {
    if !want_dmabuf {
        CaptureArm::Cpu
    } else if plan.vaapi_passthrough {
        CaptureArm::DmabufPassthrough
    } else if have_importer && plan.nvenc_raw {
        CaptureArm::DmabufToEncoder
    } else if have_importer {
        CaptureArm::CudaImport
    } else {
        // `want_dmabuf` requires `have_importer || vaapi_passthrough`. Fallback so a logging
        // helper can never panic the capture thread.
        CaptureArm::Cpu
    }
}

/// Who consumes captured frames — whether a CPU arm is a downgrade, and what to call it.
///
/// From the resolved [`ZeroCopyPolicy`](crate::ZeroCopyPolicy), not the encoder pref:
/// `pyrowave_session` is per-session, so a PyroWave session on an NVENC host is PyroWave here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConsumerKind {
    /// Wavelet encoder's Vulkan device imports dmabufs on any vendor; CPU costs the passthrough.
    PyroWave,
    /// AMD/Intel encoder: libva or Vulkan Video imports the dmabuf.
    AmdIntel,
    Nvenc,
    /// Software encoder — CPU frames are native input, so a CPU arm is not a downgrade.
    Software,
}

impl ConsumerKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ConsumerKind::PyroWave => "pyrowave",
            ConsumerKind::AmdIntel => "amd-intel",
            ConsumerKind::Nvenc => "nvenc",
            ConsumerKind::Software => "software",
        }
    }

    /// True for every GPU consumer; false for software, which wants CPU frames.
    pub(super) fn cpu_is_downgrade(self) -> bool {
        !matches!(self, ConsumerKind::Software)
    }
}

/// Classify the frames' consumer. **Pure.** `pyrowave_session` wins over `backend_is_vaapi`
/// because it is per-session and the pref is host-global (a PyroWave session also sets
/// `backend_is_vaapi` via `linux_zero_copy_is_vaapi`'s `Pyrowave` arm).
pub(super) fn consumer_kind(
    pyrowave_session: bool,
    backend_is_vaapi: bool,
    backend_is_gpu: bool,
) -> ConsumerKind {
    if pyrowave_session {
        ConsumerKind::PyroWave
    } else if !backend_is_gpu {
        ConsumerKind::Software
    } else if backend_is_vaapi {
        ConsumerKind::AmdIntel
    } else {
        ConsumerKind::Nvenc
    }
}

/// Why a raw-dmabuf passthrough frame fell through to the CPU de-pad path.
///
/// Each variant is a different diagnosis. Without this, the `if` fell out silently and a
/// zero-copy session could pay CPU on every frame while logging a healthy open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PassthroughFallback {
    /// No format yet — transient around renegotiation.
    NoFormat,
    /// Producer delivered SHM/MemFd, not a dmabuf.
    NotDmabuf,
    /// Negotiated format has no DRM fourcc, so the encoder cannot describe it.
    NoFourcc,
    /// `F_DUPFD_CLOEXEC` failed (fd-limit, not graphics).
    DupFailed,
    /// A linear pitch off 64 bytes: iHD imports it at a rounded pitch and the picture shears.
    UnalignedPitch,
    /// This pool can never spare a deferred-requeue hold (depth ≤ reserve, or
    /// `PUNKTFUNK_ZEROCOPY_HOLD=0`), so no raw frame is safe to publish. A transient shortage
    /// on a pool that can hold never gets here: `.process` drops that arrival (`held_drops`).
    NoHold,
}

impl PassthroughFallback {
    fn bit(self) -> u8 {
        match self {
            PassthroughFallback::NoFormat => 1 << 0,
            PassthroughFallback::NotDmabuf => 1 << 1,
            PassthroughFallback::NoFourcc => 1 << 2,
            PassthroughFallback::DupFailed => 1 << 3,
            PassthroughFallback::UnalignedPitch => 1 << 4,
            PassthroughFallback::NoHold => 1 << 5,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            PassthroughFallback::NoFormat => "no format negotiated yet",
            PassthroughFallback::NotDmabuf => "the producer delivered an SHM/MemFd buffer",
            PassthroughFallback::NoFourcc => "the negotiated format has no DRM fourcc",
            PassthroughFallback::DupFailed => "F_DUPFD_CLOEXEC failed on the dmabuf fd",
            PassthroughFallback::UnalignedPitch => {
                "the dmabuf's pitch is not a multiple of 64 bytes"
            }
            PassthroughFallback::NoHold => {
                "this producer pool can never spare a deferred-requeue hold"
            }
        }
    }

    /// `NoFormat` drops the frame (CPU path needs `ud.format` too); the rest downgrade to CPU.
    pub(super) fn falls_back_to_cpu(self) -> bool {
        !matches!(self, PassthroughFallback::NoFormat)
    }

    pub(super) fn hint(self) -> &'static str {
        match self {
            PassthroughFallback::NoFormat => {
                "harmless if it stops: the first buffers can arrive before param_changed"
            }
            PassthroughFallback::NotDmabuf => {
                "the compositor accepted the dmabuf offer and is serving memory anyway — check \
                 PUNKTFUNK_FORCE_SHM and the compositor's allocator"
            }
            PassthroughFallback::NoFourcc => {
                "a capture format the encoder path cannot describe — file it, the negotiation \
                 should not have accepted it"
            }
            PassthroughFallback::DupFailed => "out of file descriptors — raise the host's NOFILE",
            PassthroughFallback::UnalignedPitch => {
                "the compositor pads linear buffers only for scanout, and iHD reads an odd pitch \
                 rounded — this width streams through the CPU copy instead of the raw import"
            }
            PassthroughFallback::NoHold => {
                "the pool is at or below the reserve, or PUNKTFUNK_ZEROCOPY_HOLD=0 — every frame \
                 takes the CPU copy rather than letting the producer rewrite a DMA-BUF the \
                 encoder still reads"
            }
        }
    }
}

/// Upper bounds (µs) of the fence-wait histogram; last bucket is overflow.
///
/// ≤100 µs is noise; >1 ms is a stall on a 60 Hz 16.6 ms budget. Coarse on purpose: the
/// question is whether the tail is ~0 or milliseconds.
const FENCE_WAIT_BUCKETS_US: [u64; 6] = [100, 500, 1_000, 2_000, 5_000, 10_000];

/// Producer implicit-fence wait, measured on the PipeWire loop thread.
///
/// That thread is the compositor's consumer, so a block here delays recycling for the next
/// frame. A `NoFence` majority means the wait is structurally free, not merely short.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct FenceWaitStats {
    samples: u64,
    total_us: u64,
    max_us: u64,
    /// Overflow bucket: one past the last bound.
    buckets: [u64; FENCE_WAIT_BUCKETS_US.len() + 1],
    signaled: u64,
    no_fence: u64,
    timed_out: u64,
    failed: u64,
}

impl FenceWaitStats {
    pub(super) fn record(&mut self, us: u64) {
        self.samples += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        let idx = FENCE_WAIT_BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(FENCE_WAIT_BUCKETS_US.len());
        self.buckets[idx] += 1;
    }

    /// Bucket upper bound for the `q`-quantile, µs; inner `None` is overflow.
    /// Counts up to the quantile rather than interpolating.
    pub(super) fn quantile_bucket_us(&self, q: f64) -> Option<Option<u64>> {
        if self.samples == 0 {
            return None;
        }
        // 0-based index of the sample at `q`; q=1.0 is the last sample.
        let target = ((self.samples as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                return Some(FENCE_WAIT_BUCKETS_US.get(i).copied());
            }
        }
        Some(None)
    }

    pub(super) fn mean_us(&self) -> u64 {
        self.total_us.checked_div(self.samples).unwrap_or(0)
    }

    /// 100 frames ≈ 1.7 s at 60 fps — past the point one outlier dominates p99.
    pub(super) fn is_meaningful(&self) -> bool {
        self.samples >= 100
    }
}

/// How many buffers the producer allocated for this stream.
///
/// `live` comes from `add_buffer`/`remove_buffer` on the loop thread. There is no "pool
/// complete" event, so the count is published from `.process` (first dequeue ⇒ allocation
/// finished). Depth is the budget [`HoldBook::try_hold`] spends; a pool of ≤ [`HOLD_POOL_RESERVE`]
/// cannot defer and the producer may rewrite a buffer mid-encode.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct PoolCensus {
    live: u32,
    /// Deepest `live` this session. A depth decision must not follow a renegotiation down to zero.
    high_water: u32,
    /// Last logged `live`, so a stable pool logs once per distinct depth.
    logged: Option<u32>,
}

impl PoolCensus {
    fn add(&mut self) {
        self.live += 1;
        self.high_water = self.high_water.max(self.live);
    }

    fn remove(&mut self) {
        self.live = self.live.saturating_sub(1);
    }

    /// `Some(live)` the first time each distinct depth is seen.
    fn note_frame(&mut self) -> Option<u32> {
        (self.logged != Some(self.live)).then(|| {
            self.logged = Some(self.live);
            self.live
        })
    }
}

/// Per-session tally of raw-passthrough fall-throughs, one log line per distinct reason.
///
/// `.process` runs per frame; per-reason so a transient `NoFormat` at open does not spend
/// the budget a persistent `NotDmabuf` needs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PassthroughFallbacks {
    frames: u64,
    logged: u8,
}

impl PassthroughFallbacks {
    /// `Some(frames_so_far)` the first time this reason is seen this session.
    pub(super) fn note(&mut self, reason: PassthroughFallback) -> Option<u64> {
        self.frames += 1;
        let bit = reason.bit();
        (self.logged & bit == 0).then(|| {
            self.logged |= bit;
            self.frames
        })
    }
}

/// What a broken raw-passthrough frame does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassthroughFallbackAction {
    /// Nothing streams — the CPU path cannot serve this frame either.
    Drop,
    /// De-pad through the CPU mmap path.
    Cpu,
    /// The tiled modifier failed: refuse it for this identity and rebuild on LINEAR.
    DropTiledAndRebuild,
}

/// The action for a passthrough break. A nonzero modifier is never de-padded: a
/// tiled buffer read as linear is a scrambled picture, so any failure on it
/// retires the tiled offer itself. On LINEAR, keep today's split.
fn passthrough_fallback_action(
    reason: PassthroughFallback,
    modifier: u64,
) -> PassthroughFallbackAction {
    if modifier != 0 {
        PassthroughFallbackAction::DropTiledAndRebuild
    } else if reason.falls_back_to_cpu() {
        PassthroughFallbackAction::Cpu
    } else {
        PassthroughFallbackAction::Drop
    }
}

/// The encoder-proved list for an exact drm fourcc out of
/// [`ZeroCopyPolicy::encoder_modifiers`]: cloned, LINEAR stripped, order kept.
fn encoder_modifiers_for(policy: &ZeroCopyPolicy, fourcc: u32) -> Vec<u64> {
    let Some(list) = policy
        .encoder_modifiers
        .iter()
        .find(|(f, _)| *f == fourcc)
        .map(|(_, m)| m)
    else {
        return Vec::new();
    };
    let mut out: Vec<u64> = Vec::with_capacity(list.len());
    for &m in list {
        if m != 0 && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

/// Run the fallback for a broken raw-passthrough frame: pick the action for
/// `(reason, ud.modifier)`, emit its once-per-reason line, and act on it.
/// `true` = the caller falls through to the mmap de-pad; `false` = the frame
/// ends here — dropped, or the tiled offer refused and the capture flagged to
/// rebuild on LINEAR.
fn handle_passthrough_fallback(ud: &mut UserData, reason: PassthroughFallback) -> bool {
    let action = passthrough_fallback_action(reason, ud.modifier);
    // Once per distinct reason (`.process` is per-frame). The running count separates a
    // persistent downgrade from a one-frame hiccup at renegotiation.
    if let Some(frames) = ud.passthrough_fallbacks.note(reason) {
        tracing::warn!(
            frames,
            "zero-copy raw-dmabuf passthrough did not take this frame: {} — {} ({})",
            reason.as_str(),
            match action {
                PassthroughFallbackAction::Cpu => {
                    "it falls back to the CPU capture path, costing a full-resolution mmap \
                     de-pad plus the encoder's own upload on every such frame"
                }
                PassthroughFallbackAction::Drop => {
                    "the frame is DROPPED — the CPU de-pad path needs the negotiated format \
                     too, so nothing streams while this persists"
                }
                PassthroughFallbackAction::DropTiledAndRebuild => {
                    "the tiled offer is refused for this capture identity and it rebuilds on \
                     LINEAR"
                }
            },
            reason.hint()
        );
    }
    match action {
        PassthroughFallbackAction::Cpu => true,
        PassthroughFallbackAction::Drop => false,
        PassthroughFallbackAction::DropTiledAndRebuild => {
            if ud.signals.health.refuse_passthrough_tiled() {
                tracing::warn!(
                    "tiled raw-dmabuf passthrough could not continue ({}) — capture rebuilds \
                     on LINEAR",
                    reason.as_str()
                );
            }
            ud.signals.broken.store(true, Ordering::Relaxed);
            false
        }
    }
}

/// Tiled-import failures (worker alive) before the stream is poisoned for rebuild.
/// Never fall through to CPU mmap: de-padding tiled bytes as linear is a scrambled image.
const IMPORT_FAIL_POISON: u32 = 3;

/// Holds exist for this pool: two stay with the producer, the rest may sit in the slot or under
/// the consumer's import. False means the arrival path imports itself.
fn holds_possible(hold_enabled: bool, pool_live: u32) -> bool {
    hold_enabled && pool_live > HOLD_POOL_RESERVE
}

/// One dmabuf → CUDA import: tiled through EGL, LINEAR through the Vulkan bridge, NV12 or
/// YUV444 where the policy asks. Failures are graded here so both threads act alike: a tiled
/// failure drops the frame and poisons the stream after [`IMPORT_FAIL_POISON`] (or at once
/// when the worker died); a LINEAR failure retires the importer.
#[allow(clippy::too_many_arguments)]
pub(super) fn gpu_import(
    importer: &mut pf_zerocopy::Importer,
    policy: ImportPolicy,
    state: &mut ImportState,
    signals: &super::CaptureSignals,
    fmt: PixelFormat,
    w: u32,
    h: u32,
    plane: pf_zerocopy::DmabufPlane,
    modifier: u64,
) -> ImportOutcome {
    let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
        return ImportOutcome::Dropped; // format has no DRM fourcc mapping
    };
    let modifier = (modifier != 0).then_some(modifier);
    let ten_bit = fmt.is_hdr_rgb10();
    // The raw lane let go of a tiled HDR stream. Rebuild it on the LINEAR offer.
    if ten_bit && modifier.is_some() {
        if signals.health.refuse_hdr_tiled() {
            tracing::warn!(
                "tiled 10-bit dmabuf reached the CUDA import — capture rebuilds on LINEAR"
            );
        }
        signals.broken.store(true, Ordering::Relaxed);
        return ImportOutcome::Dropped;
    }
    let yuv444 = policy.yuv444 && modifier.is_some() && !ten_bit;
    let mut nv12 = policy.nv12 && !policy.yuv444 && !ten_bit;
    let imported = if let Some(m) = modifier {
        if yuv444 {
            importer.import_yuv444(&plane, w, h, fourcc, Some(m))
        } else if nv12 {
            importer.import_nv12(&plane, w, h, fourcc, Some(m))
        } else {
            importer.import(&plane, w, h, fourcc, Some(m))
        }
    } else if nv12 && !state.linear_nv12_failed {
        match importer.import_linear_nv12(&plane, w, h) {
            Ok(buf) => Ok(buf),
            Err(e) => {
                state.linear_nv12_failed = true;
                nv12 = false;
                tracing::warn!(error = %format!("{e:#}"),
                    "LINEAR NV12 compute CSC failed — RGB for the rest of this \
                     stream (NVENC does the CSC internally)");
                importer.import_linear(&plane, w, h)
            }
        }
    } else {
        nv12 = false;
        importer.import_linear(&plane, w, h)
    };
    match imported {
        Ok(devbuf) => {
            state.fail_streak = 0;
            signals.health.note_gpu_import_ok();
            static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
            if ONCE.swap(false, Ordering::Relaxed) {
                tracing::info!(
                    w,
                    h,
                    modifier = modifier.unwrap_or(0),
                    nv12,
                    yuv444,
                    "zero-copy: dmabuf imported to CUDA (no CPU copy)"
                );
            }
            let out = if yuv444 {
                PixelFormat::Yuv444
            } else if nv12 {
                PixelFormat::Nv12
            } else {
                fmt
            };
            ImportOutcome::Frame(devbuf, out)
        }
        Err(e) => {
            let dead = importer.dead();
            if dead {
                signals.health.note_gpu_import_death();
            }
            if modifier.is_none() {
                tracing::warn!(error = %format!("{e:#}"),
                    "LINEAR dmabuf GPU import failed — falling back to the CPU copy path");
                return ImportOutcome::ImporterLost;
            }
            state.fail_streak += 1;
            if dead || state.fail_streak >= IMPORT_FAIL_POISON {
                tracing::error!(error = %format!("{e:#}"), dead,
                    "tiled GPU import lost — failing this capture for rebuild");
                signals.broken.store(true, Ordering::Relaxed);
            } else {
                tracing::warn!(error = %format!("{e:#}"),
                    streak = state.fail_streak,
                    "tiled dmabuf GPU import failed — frame dropped");
            }
            ImportOutcome::Dropped
        }
    }
}

/// Buffers left in the producer's pool: one it is rendering, one in transit. The remaining
/// pool is the consumer hold budget (host frame, capture slot, and pipelined encoder sources).
const HOLD_POOL_RESERVE: u32 = 2;

/// Pool the raw lane asks for: four encoder holds, the host frame, the capture slot, and two
/// buffers reserved for the producer. A producer capped below this spends only the holds it
/// serves; [`UserData::release_unconsumed`] frees the slot's for the next arrival.
const RAW_LANE_POOL_MIN: i32 = 8;

/// Least pool depth this stream asks for: the producer's minimum, deepened to
/// [`RAW_LANE_POOL_MIN`] on the raw lane but never past `pool_max`. A minimum above what the
/// producer serves fails negotiation outright.
fn pool_ask(pool_min: i32, pool_max: Option<i32>, raw_lane: bool) -> i32 {
    if !raw_lane {
        return pool_min;
    }
    let deep = pool_min.max(RAW_LANE_POOL_MIN);
    pool_max.map_or(deep, |max| deep.min(max).max(pool_min))
}

/// `PUNKTFUNK_ZEROCOPY_HOLD=0` restores immediate requeue (racy). On the raw passthrough it
/// never publishes an unheld dmabuf: `try_defer` returns `None` there and the frame takes the
/// safe CPU fallback instead. Use `env_on`; a bare `== "0"` is the trap `PUNKTFUNK_FORCE_SHM`
/// already hit.
fn zerocopy_hold_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_ZEROCOPY_HOLD").unwrap_or(true))
}

/// Which buffers are withheld, each under a per-hold generation.
///
/// A pointer-value reuse across pool renegotiation must not satisfy a stale hold (`complete`).
/// Insert/remove run only on the loop thread; a dropping [`BufferHold`] only sends. Between
/// hold creation and the loop servicing release, `contains` is stable — what `.process` uses
/// to choose "requeue now" vs "the hold owns the requeue".
#[derive(Default)]
struct HoldBook {
    /// `*mut pw_buffer` as usize → generation that owns it.
    out: std::collections::HashMap<usize, u64>,
    /// Last issued hold generation (monotonic per stream).
    last_gen: u64,
}

impl HoldBook {
    /// Withhold `buf` if the pool can spare it (`pool_live - HOLD_POOL_RESERVE` out at once).
    /// A buffer already out was re-sent by the producer: the new generation takes over its
    /// requeue, and the stale hold's [`complete`](Self::complete) no longer matches.
    fn try_hold(&mut self, buf: usize, pool_live: u32) -> Option<u64> {
        let cap = pool_live.saturating_sub(HOLD_POOL_RESERVE) as usize;
        if !self.out.contains_key(&buf) && self.out.len() >= cap {
            return None;
        }
        self.last_gen += 1;
        self.out.insert(buf, self.last_gen);
        Some(self.last_gen)
    }

    /// Release `buf` iff this generation still owns it. `true` ⇒ caller must requeue;
    /// `false` ⇒ purged (pool renegotiated — pointer may be a new buffer) so do not touch it.
    fn complete(&mut self, buf: usize, generation: u64) -> bool {
        match self.out.get(&buf) {
            Some(&g) if g == generation => {
                self.out.remove(&buf);
                true
            }
            _ => false,
        }
    }

    /// `remove_buffer`: the buffer is being freed under us. Later release of its hold is a no-op.
    fn purge(&mut self, buf: usize) {
        self.out.remove(&buf);
    }

    fn contains(&self, buf: usize) -> bool {
        self.out.contains_key(&buf)
    }
}

/// Shared by the loop thread ([`HoldBook`] ops) and [`BufferHold`] guards on the encode thread.
struct DeferredRequeue {
    book: std::sync::Mutex<HoldBook>,
    /// Releases dropped on any thread, `(buffer, generation)`, until the loop thread requeues
    /// them: the wake callback does, and so does `try_defer` before it gives an arrival up.
    /// The book alone would count a hold the encoder already let go until the loop got round
    /// to the wake — on a pool of 4 that is the second and last hold, and the arrival that
    /// finds it still out pays the full-frame CPU copy.
    pending: std::sync::Mutex<Vec<(usize, u64)>>,
    /// Wakes the loop to drain `pending`. Send failure = the loop is gone.
    wake: pw::channel::Sender<()>,
    logged_active: std::sync::atomic::AtomicBool,
    logged_shallow: std::sync::atomic::AtomicBool,
    /// Signals a buffer's release point as it rejoins; `None` without explicit sync.
    sync: Option<std::sync::Arc<SyncDevice>>,
}

impl DeferredRequeue {
    /// Return `buf` to the producer iff `generation` still owns it. `true` ⇒ requeued.
    /// [`HoldBook::complete`] no-ops a renegotiated-away (or reused) address, so a stale hold
    /// can never queue somebody else's buffer.
    ///
    /// # Safety
    /// Loop thread only, and `stream` is the live stream whose buffers this book tracks.
    unsafe fn release(&self, stream: *mut pw::sys::pw_stream, buf: usize, generation: u64) -> bool {
        let requeue = self
            .book
            .lock()
            .map(|mut b| b.complete(buf, generation))
            .unwrap_or(false);
        if requeue {
            // SAFETY: `complete` returned true ⇒ this buffer was withheld by exactly this hold
            // and no `remove_buffer` has freed it since (that purges the book), so the pointer
            // is a live buffer of `stream` that we own (dequeued, never requeued). The caller
            // guarantees `stream` is live and that we are on its loop thread.
            unsafe { hand_back(self.sync.as_deref(), stream, buf as *mut pw::sys::pw_buffer) };
        }
        requeue
    }

    /// Requeue every release parked since the last drain. Returns how many buffers rejoined.
    ///
    /// # Safety
    /// As [`release`](Self::release).
    unsafe fn drain(&self, stream: *mut pw::sys::pw_stream) -> usize {
        self.drain_with(|buf| {
            // SAFETY: `drain_with` hands over only buffers the book still listed under the
            // dropping hold's generation (see `release`); the caller's contract is `release`'s.
            unsafe { hand_back(self.sync.as_deref(), stream, buf as *mut pw::sys::pw_buffer) };
        })
    }

    /// Complete every parked release in the book; `requeue` gets each buffer that was still
    /// withheld under its hold's generation (a purged or re-held one is skipped).
    fn drain_with(&self, mut requeue: impl FnMut(usize)) -> usize {
        let pending = self
            .pending
            .lock()
            .map(|mut p| std::mem::take(&mut *p))
            .unwrap_or_default();
        let mut rejoined = 0;
        for (buf, generation) in pending {
            let owned = self
                .book
                .lock()
                .map(|mut b| b.complete(buf, generation))
                .unwrap_or(false);
            if owned {
                requeue(buf);
                rejoined += 1;
            }
        }
        rejoined
    }
}

/// Releases its buffer to the producer when the last clone drops. Park-and-wake only from the
/// dropping thread; `pw_stream_queue_buffer` runs on the loop thread ([`DeferredRequeue::drain`]).
struct BufferHold {
    defer: std::sync::Arc<DeferredRequeue>,
    buf: usize,
    generation: u64,
}

impl Drop for BufferHold {
    fn drop(&mut self) {
        if let Ok(mut p) = self.defer.pending.lock() {
            p.push((self.buf, self.generation));
        }
        let _ = self.defer.wake.send(());
    }
}

/// Log a frame-drop reason once per process (`.process` runs per frame).
fn warn_once(msg: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&msg) {
        seen.push(msg);
        tracing::warn!("{msg}");
    }
}

/// Read-only mmap of a dmabuf fd, unmapped on drop. Used when MAP_BUFFERS left the buffer unmapped
/// (producers do not always flag dmabufs mappable — gamescope Vulkan exports).
struct DmabufMap {
    ptr: *mut std::ffi::c_void,
    len: usize,
}

impl DmabufMap {
    fn new(fd: i32, len: usize) -> Option<DmabufMap> {
        // SAFETY: a null `addr` lets the kernel choose the mapping address; `fd` is a caller-owned
        // dmabuf/MemFd fd, valid for the duration of this call, and `len` is the requested map length.
        // `mmap` reads no Rust memory — it installs a fresh PROT_READ/MAP_SHARED page mapping and
        // returns its base (or MAP_FAILED, checked below before `DmabufMap` adopts it). The returned
        // region is a brand-new VMA, so it aliases no live Rust object, and it keeps the underlying
        // object mapped independently of `fd` (which may be closed after this returns).
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        (ptr != libc::MAP_FAILED).then_some(DmabufMap { ptr, len })
    }
}

impl Drop for DmabufMap {
    fn drop(&mut self) {
        // SAFETY: `self.ptr`/`self.len` are exactly the base+length of a successful `mmap` in
        // `DmabufMap::new` (constructed only when `ptr != MAP_FAILED`). This `DmabufMap` uniquely owns
        // that mapping and `drop` runs once, so `munmap` releases a live mapping exactly once — no
        // double-unmap. Every `&[u8]` derived from the mapping is bounded by this `DmabufMap`'s
        // lifetime, so no borrow outlives the unmap.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

fn supported_data_plane_count(count: u32) -> Option<usize> {
    (1..=2).contains(&count).then_some(count as usize)
}

/// How often the wire-pts provenance line is emitted. Matches the audio plane's stats cadence.
const PTS_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// `CLOCK_REALTIME − CLOCK_MONOTONIC`, ns.
///
/// PipeWire stamps `spa_meta_header.pts` in `CLOCK_MONOTONIC`; the wire speaks realtime-since-epoch.
/// A failed read reports 0, which puts every rebased stamp outside the 50 ms plausibility window
/// and falls the stream back to delivery stamps — the safe direction.
pub(super) fn realtime_minus_monotonic_ns() -> i64 {
    let rt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes one `timespec` through the pointer and touches nothing else;
    // `ts` is a live, properly aligned local. A non-zero return leaves it untouched.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    rt - (ts.tv_sec * 1_000_000_000 + ts.tv_nsec)
}

/// The dmabuf's allocation as `fstat` reports it; 0 once the fd is gone.
fn dmabuf_len(fd: i32) -> u64 {
    // SAFETY: `stat` is plain data that `fstat` only writes.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an integer; a closed fd fails the call and nothing is dereferenced.
    if unsafe { libc::fstat(fd, &mut st) } == 0 {
        st.st_size as u64
    } else {
        0
    }
}

/// Whether the selected GPU's driver rounds a linear import pitch: iHD does; an unknown
/// selection is treated as one, a CPU copy being the cheaper mistake.
fn linear_pitch_rounds() -> bool {
    static RULE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RULE.get_or_init(|| {
        pf_gpu::selected_gpu().is_none_or(|g| g.info.vendor_id == pf_gpu::VENDOR_INTEL)
    })
}

fn packed_frame_geometry(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    reported_stride: usize,
) -> Option<(usize, usize, usize, usize)> {
    if height == 0 {
        return None;
    }
    let row = width.checked_mul(bytes_per_pixel)?;
    let stride = if reported_stride == 0 {
        row
    } else {
        reported_stride
    };
    if stride < row {
        return None;
    }
    let needed = stride.checked_mul(height - 1)?.checked_add(row)?;
    let tight = row.checked_mul(height)?;
    Some((row, stride, needed, tight))
}

/// De-pad / import one PipeWire buffer and push it to the encoder.
///
/// Called from `.process` with the newest drained buffer. `datas` uses the same transparent
/// cast as libspa's `Buffer::datas_mut`, so the safe `Data` accessors keep working. `pw_buf`
/// is identity for [`UserData::try_defer`] only — never dereferenced here. `stream` is the
/// stream running this `.process`; `try_defer` requeues a stale held buffer on it.
/// A broken raw-passthrough frame routes through [`handle_passthrough_fallback`]:
/// CPU de-pad, drop, or tiled-offer refusal + rebuild.
fn consume_frame(
    ud: &mut UserData,
    spa_buf: *mut spa::sys::spa_buffer,
    pw_buf: *mut pw::sys::pw_buffer,
    stream: *mut pw::sys::pw_stream,
    hdr_pts_ns: Option<i64>,
) {
    // Inactive: skip the de-pad (expensive at 5K).
    if !ud.signals.active.load(Ordering::Relaxed) {
        return;
    }
    // GPU import lost: skip per-frame work until the rebuild tears this stream down.
    if ud.signals.broken.load(Ordering::Relaxed) {
        return;
    }
    // SAFETY: the dequeued buffer stays held for this callback. We reject counts outside the
    // one/two-plane formats this function supports before using PipeWire's array pointer;
    // the sync datas behind the planes never enter the slice.
    let datas: &mut [pw::spa::buffer::Data] = unsafe {
        if spa_buf.is_null() || (*spa_buf).datas.is_null() {
            &mut []
        } else if let Some(len) = supported_data_plane_count(plane_count(spa_buf)) {
            std::slice::from_raw_parts_mut((*spa_buf).datas as *mut pw::spa::buffer::Data, len)
        } else {
            &mut []
        }
    };
    if datas.is_empty() {
        return;
    }
    let sz = ud.info.size();
    let (w, h) = (sz.width as usize, sz.height as usize);
    if w == 0 || h == 0 {
        return; // format not negotiated yet
    }

    // One stamp for every publish path, taken before de-pad/import. Sampling at publish put
    // CPU work inside the timestamp and let the three paths drift apart.
    let delivery_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let hdr_pts_ns = hdr_pts_ns.filter(|&p| p > 0);
    // Observe even if we ship the delivery stamp: which clock is cleaner on this host is the
    // question; a diagnostic that runs only after you trust the answer cannot inform it.
    ud.pts.observe(hdr_pts_ns, delivery_ns);
    let stamp = crate::pts_provenance::wire_pts(
        ud.hdr_pts_enabled.then_some(hdr_pts_ns).flatten(),
        delivery_ns,
        ud.rt_minus_mono_ns,
    );
    let pts_ns = stamp.pts_ns;
    if ud.hdr_pts_enabled && hdr_pts_ns.is_some() && !stamp.from_header {
        ud.pts.implausible += 1;
    }
    if ud.pts_reported.elapsed() >= PTS_REPORT_EVERY {
        if let Some(r) = ud.pts.report() {
            tracing::info!(
                frames = r.frames,
                with_hdr = r.with_hdr,
                samples = r.samples,
                period_us = r.period_us,
                // `frames + skipped` ≈ the window's expected count when the
                // producer skipped ticks; a producer running slow moves
                // `period_us` instead and leaves this at 0.
                skipped = r.skipped,
                // Tighter hdr_mad than delivery_mad ⇒ compositor stamp is worth shipping;
                // equally ragged ⇒ the producer composes irregularly and no stamp fixes it.
                hdr_mad_us = r.hdr_mad_us,
                delivery_mad_us = r.delivery_mad_us,
                offset_p50_ms = r.offset_p50_ms,
                implausible = r.implausible,
                hdr_pts_used = ud.hdr_pts_enabled,
                held_drops = ud.held_drops,
                // Session total of raw-passthrough frames that took the CPU copy instead.
                cpu_fallbacks = ud.passthrough_fallbacks.frames,
                // Buffers the producer sent again while held (PipeWire < 1.6); each re-held.
                resent = ud.signals.resent.load(Ordering::Relaxed),
                pool_depth = ud.pool.live,
                "capture wire-pts provenance"
            );
        }
        ud.pts.reset_window();
        ud.pts_reported = std::time::Instant::now();
        // Clocks drift by µs over a window; re-pair here so a multi-hour session stays honest.
        ud.rt_minus_mono_ns = realtime_minus_monotonic_ns();
    }

    // The render is fenced at the acquire point when the stream negotiated explicit sync, else
    // by the dmabuf's implicit fence (none on NVIDIA: a stale frame can be read). 100 ms is a
    // guard: past it the producer is wedged, not slow. A CPU wait on the loop thread; a GPU
    // semaphore import would free it, and the perf line below says whether that is owed.
    if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
        let t0 = std::time::Instant::now();
        // SAFETY: `spa_buf` is the buffer this callback holds.
        let explicit = ud.sync.as_ref().zip(unsafe { SyncPoints::of(spa_buf) });
        let waited = match &explicit {
            Some((dev, p)) => dev.wait(
                p.acquire_fd,
                p.acquire_point,
                std::time::Duration::from_millis(100),
            ),
            None => pf_zerocopy::dmabuf_fence::wait_read_ready(datas[0].fd(), 100),
        };
        ud.fence_wait.record(t0.elapsed().as_micros() as u64);
        match waited {
            Ok(outcome) => {
                use pf_zerocopy::dmabuf_fence::WaitOutcome;
                match outcome {
                    WaitOutcome::Signaled => ud.fence_wait.signaled += 1,
                    WaitOutcome::NoFence => ud.fence_wait.no_fence += 1,
                    WaitOutcome::TimedOut => ud.fence_wait.timed_out += 1,
                }
                static F0: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
                if explicit.is_some() && F0.swap(false, Ordering::Relaxed) {
                    tracing::info!(
                        ?outcome,
                        "dmabuf explicit sync active (SyncTimeline): the producer's acquire \
                         point is waited here and its release point signalled on hand-back — \
                         it no longer finishes the GPU for this stream"
                    );
                }
                static F1: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
                if explicit.is_none() && F1.swap(false, Ordering::Relaxed) {
                    tracing::info!(
                        ?outcome,
                        "dmabuf implicit-fence sync active (Signaled → driver fences the \
                         render, race closed; NoFence → no implicit fence, zero-copy may \
                         still show stale frames; TimedOut → fence pending past 100ms, \
                         proceeded anyway)"
                    );
                }
            }
            Err(e) => {
                ud.fence_wait.failed += 1;
                static F2: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
                if F2.swap(false, Ordering::Relaxed) {
                    tracing::warn!(
                        error = %e,
                        "dmabuf EXPORT_SYNC_FILE failed — no implicit-fence sync; NVIDIA \
                         zero-copy may show stale frames (no producer explicit sync)"
                    );
                }
            }
        }
        // One line per ~5 s at 60 fps, under PUNKTFUNK_PERF only — same gate as encode submit splits.
        if pf_host_config::config().perf
            && ud.fence_wait.is_meaningful()
            && ud.fence_wait.samples % 300 == 0
        {
            let q = |p: f64| match ud.fence_wait.quantile_bucket_us(p) {
                Some(Some(us)) => format!("<={us}us"),
                Some(None) => format!(
                    ">{}us",
                    FENCE_WAIT_BUCKETS_US[FENCE_WAIT_BUCKETS_US.len() - 1]
                ),
                None => "n/a".to_string(),
            };
            tracing::info!(
                samples = ud.fence_wait.samples,
                mean_us = ud.fence_wait.mean_us(),
                max_us = ud.fence_wait.max_us,
                p50 = %q(0.50),
                p99 = %q(0.99),
                signaled = ud.fence_wait.signaled,
                no_fence = ud.fence_wait.no_fence,
                timed_out = ud.fence_wait.timed_out,
                failed = ud.fence_wait.failed,
                "dmabuf implicit-fence wait on the PipeWire loop thread (PW4: a p99 in the first \
                 bucket means this wait is already free and moving it off-thread buys nothing)"
            );
        }
    }

    // Raw DMA-BUF passthrough: packed RGB for GPU CSC, or producer NV12 without another convert.
    // Publishes and returns, or breaks with a named reason. Silent fall-through CPU-touches
    // every frame on a session that had negotiated zero-copy.
    if ud.vaapi_passthrough {
        let reason = 'passthrough: {
            let Some(fmt) = ud.format else {
                break 'passthrough PassthroughFallback::NoFormat;
            };
            if datas[0].type_() != pw::spa::buffer::DataType::DmaBuf {
                break 'passthrough PassthroughFallback::NotDmabuf;
            }
            let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
                break 'passthrough PassthroughFallback::NoFourcc;
            };
            let chunk = datas[0].chunk();
            let offset = chunk.offset();
            let stride = chunk.stride().max(0) as u32;
            // NV12/P010 are usually two SPA planes on one BO; plane 1's chunk has the real UV
            // offset/stride. BO identity is inode, not fd number. A two-BO frame cannot
            // travel the single-fd import — drop it rather than stream garbage chroma.
            let planar = matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010);
            let plane1 = if planar && datas.len() >= 2 && datas[1].fd() > 0 {
                // SAFETY: zeroed `libc::stat` is a valid POD initializer; both fds are
                // owned by the live PipeWire buffer for this callback, and `fstat`
                // only writes the out-param structs, whose fields are read only after
                // the `== 0` success checks.
                let same_bo = unsafe {
                    let mut s0: libc::stat = std::mem::zeroed();
                    let mut s1: libc::stat = std::mem::zeroed();
                    libc::fstat(datas[0].fd() as i32, &mut s0) == 0
                        && libc::fstat(datas[1].fd() as i32, &mut s1) == 0
                        && (s0.st_dev, s0.st_ino) == (s1.st_dev, s1.st_ino)
                };
                if !same_bo {
                    warn_once(
                        "the planes live in different buffer objects — frames \
                                 dropped (single-fd import only)",
                    );
                    // Dropped, not downgraded: de-padding as linear would scramble chroma.
                    return;
                }
                let c1 = datas[1].chunk();
                Some((c1.offset(), c1.stride().max(0) as u32))
            } else {
                None
            };
            // iHD reads a linear import at a pitch rounded to 64 bytes, so an odd pitch shears
            // the picture. Mutter's RENDERING-only GBM buffers pad only for SCANOUT. The
            // de-pad copy below uploads through a driver-allocated surface instead.
            if ud.modifier == 0
                && linear_pitch_rounds()
                && (stride % 64 != 0 || plane1.is_some_and(|(_, s)| s % 64 != 0))
            {
                break 'passthrough PassthroughFallback::UnalignedPitch;
            }
            // Dup so the fd outlives SPA recycle. Content stability is `try_defer`: a raw
            // frame is published only under a hold, so the producer can never rewrite a
            // DMA-BUF the encoder still reads. No hold — shallow pool or
            // PUNKTFUNK_ZEROCOPY_HOLD=0 — is a safe CPU fallback, never an unsafe publish.

            // SAFETY: `datas[0].fd()` is the dmabuf fd owned by the live PipeWire buffer (valid
            // for this callback). `fcntl(fd, F_DUPFD_CLOEXEC, 0)` reads only the integer fd,
            // touches no Rust memory, and returns a fresh independent CLOEXEC duplicate (or -1).
            // The original stays owned by PipeWire; the dup is a new fd we own (checked >= 0).
            let dup = unsafe { libc::fcntl(datas[0].fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                break 'passthrough PassthroughFallback::DupFailed;
            }
            let Some(hold) = ud.try_defer(pw_buf, stream) else {
                // SAFETY: `dup` is ours and was not published.
                unsafe { libc::close(dup) };
                // A shortage, not a broken frame: drop it as the import lane does — the slot
                // keeps its frame, the next arrival takes the hold that comes back. The CPU
                // copy on this thread starves the requeues that would end the shortage; a
                // tiled rebuild asks KWin for a new output each time (#1443 never settled).
                // Only a pool that can never hold falls through, or nothing would stream.
                if holds_possible(zerocopy_hold_enabled(), ud.pool.live) {
                    ud.held_drops += 1;
                    return;
                }
                break 'passthrough PassthroughFallback::NoHold;
            };
            ud.publish(CapturedFrame {
                provenance: Default::default(),
                width: w as u32,
                height: h as u32,
                pts_ns,
                format: fmt,
                payload: FramePayload::Dmabuf(DmabufFrame {
                    // SAFETY: `dup` is the fresh fd `fcntl(F_DUPFD_CLOEXEC)` just returned
                    // (checked `dup >= 0`); nothing else owns it, so `OwnedFd` takes sole
                    // ownership and closes it exactly once on drop — no alias, no
                    // double-close.
                    fd: unsafe { OwnedFd::from_raw_fd(dup) },
                    fourcc,
                    modifier: ud.modifier,
                    offset,
                    stride,
                    plane1,
                    hold: Some(hold),
                    health: ud.signals.health.clone(),
                    rebuild: ud.signals.broken.clone(),
                }),
                // RGB→NV12 backends blend cursor-as-metadata. Gamescope burns the pointer in;
                // native NV12/P010 has none.
                cursor: ud.cursor.overlay(),
            });
            // Once per geometry, not once: a resize renegotiates the pool, and a stale
            // stride against a new size is a sheared picture.
            static LAST: std::sync::Mutex<(usize, usize, u32, u32)> =
                std::sync::Mutex::new((0, 0, 0, 0));
            let geometry = (w, h, offset, stride);
            let changed = std::mem::replace(
                &mut *LAST.lock().unwrap_or_else(|e| e.into_inner()),
                geometry,
            ) != geometry;
            if changed {
                tracing::info!(
                    w,
                    h,
                    offset,
                    stride,
                    fd_size = dmabuf_len(dup),
                    modifier = ud.modifier,
                    fourcc = format_args!("{:#010x}", fourcc),
                    source = match fmt {
                        PixelFormat::Nv12 => "producer-native NV12",
                        PixelFormat::P010 => "producer-native P010",
                        _ => "packed RGB (encoder GPU CSC)",
                    },
                    "zero-copy: handing the raw DMA-BUF to the encoder"
                );
            }
            return;
        };
        if !handle_passthrough_fallback(ud, reason) {
            return;
        }
    }

    // dmabuf + importer → CUDA, no CPU touch. Else fall through to the shm de-pad copy.
    // dmabuf + importer: hand the held buffer to the consumer, which imports at its own tick
    // (`import_held`). Arrivals above the wire rate then cost nothing here. A buffer that
    // cannot be held is dropped while holds are possible at all — the slot already has a held
    // frame — and imported here only when this pool can never hold.
    let mut gpu_import_broken = false;
    if ud.signals.has_importer.load(Ordering::Relaxed) {
        if let Some(fmt) = ud.format {
            let hdr_tiled = fmt.is_hdr_rgb10() && ud.modifier != 0;
            if hdr_tiled && !ud.hdr_tiled_raw {
                warn_once(
                    "HDR frame arrived with a tiled modifier — the GPU de-tile blit is 8-bit, so \
                     this stream falls back to the CPU path (the producer ignored our LINEAR-only \
                     HDR offer)",
                );
            }
            if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf
                && (!hdr_tiled || ud.hdr_tiled_raw)
            {
                let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
                    return; // format has no DRM fourcc mapping — skip the frame
                };
                let plane = pf_zerocopy::DmabufPlane {
                    fd: datas[0].fd(),
                    offset: datas[0].chunk().offset(),
                    stride: datas[0].chunk().stride().max(0) as u32,
                };
                // SAFETY: `fd` is the producer's open dmabuf for this buffer; F_DUPFD_CLOEXEC
                // only creates a second descriptor.
                let dup = unsafe { libc::fcntl(datas[0].fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
                if dup >= 0 {
                    if let Some(hold) = ud.try_defer(pw_buf, stream) {
                        ud.publish(CapturedFrame {
                            provenance: Default::default(),
                            width: w as u32,
                            height: h as u32,
                            pts_ns,
                            format: fmt,
                            payload: FramePayload::Dmabuf(DmabufFrame {
                                // SAFETY: `dup` is a fresh descriptor this frame owns.
                                fd: unsafe { OwnedFd::from_raw_fd(dup) },
                                fourcc,
                                modifier: ud.modifier,
                                offset: plane.offset,
                                stride: plane.stride,
                                plane1: None,
                                hold: Some(hold),
                                health: ud.signals.health.clone(),
                                rebuild: ud.signals.broken.clone(),
                            }),
                            cursor: ud.cursor.overlay(),
                        });
                        return;
                    }
                    // SAFETY: `dup` is ours and nothing else saw it.
                    unsafe { libc::close(dup) };
                    if holds_possible(zerocopy_hold_enabled(), ud.pool.live) {
                        ud.held_drops += 1;
                        return;
                    }
                }
                let cell = ud.signals.importer.clone();
                let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(importer) = guard.as_mut() {
                    match gpu_import(
                        importer,
                        ud.import_policy,
                        &mut ud.import_state,
                        &ud.signals,
                        fmt,
                        w as u32,
                        h as u32,
                        plane,
                        ud.modifier,
                    ) {
                        ImportOutcome::Frame(devbuf, out_fmt) => {
                            ud.publish(CapturedFrame {
                                provenance: Default::default(),
                                width: w as u32,
                                height: h as u32,
                                pts_ns,
                                format: out_fmt,
                                payload: FramePayload::Cuda(devbuf),
                                cursor: ud.cursor.overlay(),
                            });
                            return;
                        }
                        ImportOutcome::Dropped => return,
                        ImportOutcome::ImporterLost => gpu_import_broken = true,
                    }
                }
                if gpu_import_broken {
                    *guard = None;
                }
            }
        }
    }
    if gpu_import_broken {
        ud.signals.has_importer.store(false, Ordering::Relaxed);
    }

    let d = &mut datas[0];
    // LINEAR dmabufs also land here (gamescope). Capture the fd before `data()` borrows `d`.
    let data_type = d.type_();
    let raw_fd = d.fd();
    // `mapoffset` is this spa_data's start in the fd — non-zero when one fd is pooled.
    // PipeWire's MAP_BUFFERS slice already starts there; our self-mmap maps from 0, so
    // add it (`region_off`). Skip it and we index the wrong buffer; `needed > avail` cannot
    // catch that because `avail` is the whole-fd mapping.
    let map_off = d.as_raw().mapoffset as usize;
    let (size, chunk_off, stride) = {
        let c = d.chunk();
        (
            c.size() as usize,
            c.offset() as usize,
            c.stride().max(0) as usize,
        )
    };
    let Some(fmt) = ud.format else { return }; // unsupported/not negotiated
                                               // This de-pad assumes one packed plane. `bytes_per_pixel` reports 4 for NV12, so a
                                               // native NV12 buffer (`stride ≈ w`, two planes) always trips `stride < row` and blames
                                               // the producer. The second plane is not in `datas[0]`; arriving here is a host bug
                                               // (NV12 offer without the raw-dmabuf passthrough that consumes it).
    if matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010) {
        warn_once(
            "negotiated a producer-native planar format but this capture fell back to the CPU \
             de-pad path, which handles single-plane packed formats only — frames dropped (the \
             planar offer is only valid under the raw-dmabuf passthrough that imports it)",
        );
        return;
    }
    let bpp = fmt.bytes_per_pixel();
    let Some((row, stride, needed, tight_len)) = packed_frame_geometry(w, h, bpp, stride) else {
        warn_once("invalid or overflowing packed-frame geometry — frames dropped");
        return;
    };
    // dmabuf chunks commonly report size 0; fall back to the computed span.
    let size = if size == 0 { needed } else { size };
    // mmap the fd ourselves at fstat length. xdg-desktop-portal-wlr MemFd reports
    // `data.maxsize` past the mapped bytes — reading to maxsize segfaults. Also covers
    // MAP_BUFFERS skipping Vulkan dmabufs. MemPtr (no fd) is same-process: trust `d.data()`.
    let fd_len = if raw_fd > 0 {
        // SAFETY: `libc::stat` is a C plain-old-data struct for which all-zero is a valid value, so
        // `mem::zeroed()` is a sound initializer. `raw_fd` is the buffer's fd (`> 0` checked here) and
        // valid for this callback; `fstat` writes metadata into `&mut st`, a live, aligned,
        // correctly-sized stack `stat` that outlives the synchronous call. `st.st_size` is read only
        // after the return value is confirmed `== 0`. `st` is a fresh local, so nothing aliases it.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            (libc::fstat(raw_fd as i32, &mut st) == 0 && st.st_size > 0)
                .then_some(st.st_size as usize)
        }
    } else {
        None
    };
    let _mapping; // keeps a manual mmap alive for the copy below
                  // Prefer our fstat-sized mmap; else PipeWire's MAP_BUFFERS slice. `fd_len` is required:
                  // falling back to `offset + needed` maps a producer-invented length and can SIGBUS past
                  // the object. Without a real length, decline to self-map.
    let self_mapped: Option<&[u8]> = if raw_fd > 0 {
        match fd_len.and_then(|map_len| DmabufMap::new(raw_fd as i32, map_len)) {
            Some(m) => {
                _mapping = m;
                // SAFETY: `_mapping` is the `DmabufMap` just stored; its `ptr`/`len` come from a
                // successful `mmap` of `map_len` PROT_READ bytes, so `ptr` is non-null, page-aligned,
                // and the VMA is one allocated object of `len` bytes valid for reads. In the common
                // path `map_len == fd_len` (the fd's real size from `fstat`), so the mapping spans the
                // whole object; the de-pad copy below is further bounded by the `offset <= buf.len()`
                // and `needed > avail` guards. The `&[u8]` borrows `_mapping`, which lives to the end
                // of `consume_frame`, so the slice never outlives the mapping, and the memory is only
                // read here, so there is no aliasing/mutation.
                Some(unsafe { std::slice::from_raw_parts(_mapping.ptr as *const u8, _mapping.len) })
            }
            None => None,
        }
    } else {
        None
    };
    // Self-mmap starts at fd offset 0, so this spa_data begins at `mapoffset`; MAP_BUFFERS
    // already begins there. Checked add — both halves are producer-controlled.
    let (buf, region_off): (&[u8], usize) = if let Some(b) = self_mapped {
        match map_off.checked_add(chunk_off) {
            Some(off) => (b, off),
            None => {
                warn_once("mapoffset + chunk offset overflows — frames dropped");
                return;
            }
        }
    } else if let Some(data) = d.data() {
        (data, chunk_off)
    } else {
        warn_once("buffer has no mappable data — frames dropped");
        return;
    };
    // Need stride*(h-1)+row valid bytes within [region_off, region_off+size).
    if region_off > buf.len() {
        return;
    }
    let avail = buf.len() - region_off;
    {
        // First-frame geometry — a compositor/GPU layout mismatch is otherwise silent here.
        use std::sync::atomic::{AtomicBool, Ordering};
        static ONCE: AtomicBool = AtomicBool::new(true);
        if ONCE.swap(false, Ordering::Relaxed) {
            tracing::info!(
                stride, size, chunk_off, map_off, region_off, buf_len = buf.len(), needed,
                data_type = ?data_type, fd_len = ?fd_len, self_mapped = self_mapped.is_some(),
                "capture CPU de-pad geometry (first frame)"
            );
        }
    }
    if needed > avail || needed > size {
        warn_once("buffer smaller than frame span — frames dropped");
        return;
    }
    let region = &buf[region_off..region_off + size.min(avail)];
    let mut tight = vec![0u8; tight_len];
    for y in 0..h {
        tight[y * row..y * row + row].copy_from_slice(&region[y * stride..y * stride + row]);
    }
    // Blit the latched pointer (no-op when hidden or not packed RGB) unless the host places it:
    // a baked copy would double the forwarded or blended one. The producer's hardware cursor
    // plane stays out of the captured buffer.
    if !ud.signals.host_places_cursor.load(Ordering::Relaxed) {
        composite_cursor(&mut tight, w, h, fmt, &ud.cursor);
    }
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: w as u32,
        height: h as u32,
        pts_ns,
        format: fmt,
        payload: FramePayload::Cpu(tight),
        // Already composited into `tight` — nothing for the encoder to blend.
        cursor: None,
    };
    ud.publish(frame);
}

/// The PipeWire loop thread for one capture session: connects, builds the
/// modifier offers ([`packed_modifier_offers`], [`hdr_modifier_offers`]),
/// negotiates, and runs `.process` until `quit_rx`, `broken`, or disconnect.
#[allow(clippy::too_many_arguments)]
pub fn pipewire_thread(
    fd: Option<OwnedFd>,
    node_id: u32,
    // One-deep mailbox: publish overwrites, so a stalled consumer loses intermediates, never the latest.
    slot: super::FrameSlot,
    wake: SyncSender<()>,
    signals: super::CaptureSignals,
    // Zero-copy decision, resolved once by `spawn_pipewire` — never re-derived here.
    plan: NegotiationPlan,
    // `want_444`/`want_hdr` pick the pod family; `expect_exact_dims` arms the birth-mode gate.
    opts: super::CaptureOpts,
    preferred: Option<(u32, u32, u32)>,
    quit_rx: pw::channel::Receiver<()>,
    // Encode-backend facts from the facade — never re-derived here.
    policy: ZeroCopyPolicy,
) -> Result<()> {
    let super::CaptureOpts {
        want_444,
        want_hdr,
        expect_exact_dims,
        cursor_id0_hides,
        producer_is_gamescope,
        pool_min,
        pool_max,
        unpaced,
        lazy,
        ..
    } = opts;
    // Node ids and remote fds do not identify a compositor: Mutter and gamescope
    // can both use the default daemon. Keep the producer contract explicit.
    let pool_min = pool_ask(pool_min, pool_max, plan.nvenc_raw || plan.vaapi_passthrough);
    let offer_cursor_meta = !producer_is_gamescope;
    crate::pwinit::ensure_init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    // Capturer `Drop` lands here on the loop thread and stops `run()` so the thread unwinds
    // instead of blocking to process exit. Hold the attachment for the loop's life. The
    // registry probe below also runs the loop; `quit_seen` keeps a quit during it terminal.
    let quit_seen = std::rc::Rc::new(std::cell::Cell::new(false));
    let quit_loop = mainloop.clone();
    let _quit_attach = quit_rx.attach(mainloop.loop_(), {
        let quit_seen = quit_seen.clone();
        move |()| {
            tracing::debug!("pipewire: quit signal received — stopping capture loop");
            quit_seen.set(true);
            quit_loop.quit();
        }
    });
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    // Portal source: fd to a sandboxed PipeWire remote. KWin virtual-output: no fd, default daemon.
    let core = match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .context("pw connect_fd (portal remote)")?,
        None => context
            .connect_rc(None)
            .context("pw connect (default daemon)")?,
    };
    // Lazy driver (PipeWire ≥ 1.2.7 "headless server" scheduling): the producer paints only
    // in a graph cycle this stream starts, so the encode loop owns the tick and no second
    // clock beats against it. Only a producer that emits RequestProcess (Mutter ≥ 49 virtual
    // monitors) may be driven this way; any other stays the driver as before.
    let probe = probe_producer(&core, &mainloop, node_id);
    let lazy = lazy && probe.supports_request;
    if quit_seen.get() {
        return Ok(());
    }

    let backend_is_vaapi = policy.backend_is_vaapi;
    let force_shm = plan.force_shm;
    let vaapi_passthrough = plan.vaapi_passthrough;
    let prefer_native_nv12 = plan.prefer_native_nv12;
    let prefer_native_p010 = plan.prefer_native_p010;
    // Isolated worker (design/zerocopy-worker-isolation.md): a driver fault kills the worker,
    // not this host. Construction failure → CPU path (no dmabuf request). `plan.build_importer`
    // already encodes when to try.
    if plan.gpu_import_latched {
        tracing::warn!(
            "zero-copy GPU import disabled for this capture identity (repeated import-worker \
             deaths or a previous dmabuf negotiation timeout) — using CPU path"
        );
    }
    let mut importer = if plan.build_importer {
        match pf_zerocopy::Importer::new_for_capture() {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "zero-copy import unavailable — using CPU path");
                None
            }
        }
    } else {
        None
    };
    if prefer_native_nv12 || prefer_native_p010 {
        tracing::info!(
            container = if prefer_native_p010 { "P010" } else { "NV12" },
            "zero-copy: preferring gamescope's producer-side planar LINEAR DMA-BUF (no host \
             RGB CSC; PUNKTFUNK_PIPEWIRE_NV12=0 restores the packed-RGB negotiation)"
        );
    }
    // Per-fourcc offers: importer lists plus the encoder-proved gamescope seed
    // and the PyroWave Vulkan list, finalized by `dmabuf_modifiers_for_producer`.
    let (modifiers, modifiers_bgra, extend_pyrowave) = packed_modifier_offers(
        &policy,
        &signals.health,
        importer.as_mut(),
        vaapi_passthrough,
        producer_is_gamescope,
    );
    let hdr_modifiers = hdr_modifier_offers(
        &policy,
        &signals.health,
        importer.as_mut(),
        want_hdr,
        vaapi_passthrough,
        producer_is_gamescope,
        plan.nvenc_raw,
    );
    if extend_pyrowave {
        tracing::info!(
            count = modifiers.len(),
            "zero-copy: advertising the PyroWave device's Vulkan-importable dmabuf modifiers"
        );
    }
    let want_dmabuf = plan.want_dmabuf(importer.is_some(), &modifiers);
    // Latch must fire only for an offer actually made — `plan.build_importer` cannot know
    // the importer constructed.
    signals.gpu_dmabuf_offer.store(
        want_dmabuf && !vaapi_passthrough && !want_hdr,
        Ordering::Relaxed,
    );
    // One line for the resolved arm and its consumer. Detail lines below explain an arm;
    // they do not state which one this session took.
    let consumer = consumer_kind(
        policy.pyrowave_session,
        backend_is_vaapi,
        policy.backend_is_gpu,
    );
    let arm = resolved_capture_arm(&plan, importer.is_some(), want_dmabuf);
    tracing::info!(
        capture_arm = arm.as_str(),
        consumer = consumer.as_str(),
        modifier_count = if want_hdr {
            hdr_modifiers
                .iter()
                .map(|(_, m)| m.len())
                .max()
                .unwrap_or(0)
        } else {
            modifiers.len()
        },
        // Latch state belongs on the same line as the arm: `cpu` is either "never dmabuf"
        // or "a prior failure we are still living with" — only the second is a bug.
        raw_dmabuf_latch = signals.health.raw_state(),
        "capture pipeline resolved: {} → {}",
        arm.as_str(),
        consumer.as_str()
    );
    if force_shm {
        tracing::info!(
            "capture: PUNKTFUNK_FORCE_SHM — race-free SHM download path (no dmabuf, no zero-copy)"
        );
    } else if plan.raw_dmabuf_latched {
        tracing::warn!(
            "zero-copy raw-dmabuf passthrough disabled for this capture identity (repeated \
             encoder import failures or a negotiation timeout) — capturing CPU frames instead"
        );
    } else if !want_dmabuf && (plan.build_importer || plan.vaapi_passthrough) {
        tracing::warn!("zero-copy: no importable dmabuf modifiers — using CPU path");
    } else if vaapi_passthrough {
        // PyroWave remains raw passthrough when its tiled lists are empty: LINEAR is valid.
        tracing::info!(
            native_nv12_preferred = prefer_native_nv12,
            native_p010_preferred = prefer_native_p010,
            modifier_count = modifiers.len(),
            pyrowave_extended = extend_pyrowave,
            "zero-copy: advertising DMA-BUF modifiers for direct encoder import (LINEAR \
             always; native NV12 first when enabled, packed RGB fallback)"
        );
    } else if want_dmabuf {
        tracing::info!(
            bgrx_count = modifiers.len(),
            bgra_count = modifiers_bgra.len(),
            // Sample is truncated to 6, LINEAR pushed last — reading the sample as the whole
            // list makes a good offer look tiled-only.
            linear_offered = modifiers.contains(&0),
            sample = ?&modifiers[..modifiers.len().min(6)],
            "zero-copy: advertising EGL-importable dmabuf modifiers (BGRx + BGRA pods)"
        );
    } else if consumer.cpu_is_downgrade() {
        // No dmabuf advertised: this is the CPU path. `raw_dmabuf_latched` already caught a
        // latched downgrade. Warn for every GPU consumer; software wants CPU frames.
        // `consumer_kind` is per-session so a PyroWave session on an NVIDIA host still warns
        // (the host-global encoder pref would have called it NVENC and logged nothing).
        tracing::warn!(
            consumer = consumer.as_str(),
            "{} encode with the CPU capture path (per-frame de-pad + CSC + upload) — \
             zero-copy is off for this capture ({}); set PUNKTFUNK_ZEROCOPY=1 to restore the \
             dmabuf default",
            consumer.as_str(),
            if std::env::var_os("PUNKTFUNK_ZEROCOPY").is_some() {
                "PUNKTFUNK_ZEROCOPY is set falsy"
            } else if want_hdr && !policy.hdr_cuda_ok {
                // `build_importer` drops HDR when the encoder cannot take packed 10-bit
                // CUDA. Naming the output format would send the reader to the wrong knob.
                "this HDR session's encoder cannot ingest a 10-bit CUDA payload, so the capture \
                 stays on CPU frames"
            } else {
                "this session's output format asked for CPU frames"
            }
        );
    }
    if want_dmabuf && !vaapi_passthrough && want_444 {
        tracing::info!(
            "4:4:4 zero-copy: tiled dmabufs convert to planar YUV444 (BT.709) on the GPU — \
             NVENC fed native full-chroma YUV, no CPU pixel path"
        );
    } else if want_dmabuf && !vaapi_passthrough && pf_zerocopy::nv12_enabled() {
        tracing::info!(
            "PUNKTFUNK_NV12: tiled dmabufs convert to NV12 (BT.709 limited) on the GPU — NVENC \
             fed native YUV (no internal RGB→YUV CSC)"
        );
    }

    // Holds on published frames park their release from whichever thread drops last and wake
    // this channel; a withheld buffer rejoins only on the loop thread — the receiver (attached
    // after the stream exists) or `try_defer` drains the parked releases.
    let (requeue_tx, requeue_rx) = pw::channel::channel::<()>();
    // Explicit sync needs a dmabuf lane and a DRM node that serves syncobjs; whether a buffer
    // then carries sync points is the producer's call at negotiation.
    let sync = (crate::explicit_sync() && (want_hdr || want_dmabuf))
        .then(SyncDevice::open)
        .flatten()
        .map(std::sync::Arc::new);
    let defer = std::sync::Arc::new(DeferredRequeue {
        book: std::sync::Mutex::new(HoldBook::default()),
        pending: std::sync::Mutex::new(Vec::new()),
        wake: requeue_tx,
        logged_active: std::sync::atomic::AtomicBool::new(false),
        logged_shallow: std::sync::atomic::AtomicBool::new(false),
        sync: sync.clone(),
    });

    // The heartbeat timer reads `driving` after `signals` moves into the listener's state.
    let signals_hb = signals.clone();
    // A driven producer paints only in cycles this stream starts; the pacer starts one per
    // request, no sooner than a wire interval after the last. Every entry point runs on this
    // thread. The stream pointer lands once the stream exists.
    let pacer = lazy.then(|| Pacer::new(wire_interval(preferred)));
    // Shared with the consumer, which imports held frames at its own tick.
    signals
        .has_importer
        .store(importer.is_some(), Ordering::Relaxed);
    *signals.importer.lock().unwrap_or_else(|e| e.into_inner()) = importer;
    let signals_exit = signals.clone();
    let hdr_tiled_raw =
        want_hdr && policy.gamescope_tiled && plan.nvenc_raw && !signals.health.hdr_tiled_refused();
    let data = UserData {
        info: VideoInfoRaw::default(),
        format: None,
        modifier: 0,
        slot,
        wake,
        signals,
        vaapi_passthrough,
        // Same predicate `hdr_modifier_offers` used for the NVENC raw lane.
        hdr_tiled_raw,
        import_policy: plan.import_policy.for_ten_bit_sdr(opts.ten_bit_sdr),
        import_state: ImportState::default(),
        dbg_log_n: 0,
        pts: crate::pts_provenance::PtsProvenance::new(),
        pts_reported: std::time::Instant::now(),
        rt_minus_mono_ns: realtime_minus_monotonic_ns(),
        hdr_pts_enabled: std::env::var("PUNKTFUNK_CAPTURE_HDR_PTS").as_deref() != Ok("0"),
        fence_wait: FenceWaitStats::default(),
        pool: PoolCensus::default(),
        passthrough_fallbacks: PassthroughFallbacks::default(),
        cursor: CursorState::new(cursor_id0_hides),
        expect_dims: if expect_exact_dims {
            preferred.map(|(w, h, _)| (w, h))
        } else {
            None
        },
        gate_skips: 0,
        gate_since: None,
        defer: defer.clone(),
        pacer: pacer.clone(),
        held_drops: 0,
        sync: sync.clone(),
    };

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE     => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Screen",
        // Do not let the session manager re-target this stream: an orphaned auto-link to
        // a fresh Video/Source wedges that node and head-blocks the daemon work queue,
        // stalling all new link negotiation system-wide.
        "node.dont-reconnect"     => "true",
    };
    if lazy {
        // "2" outranks the producer's supports-request, so PipeWire picks this node as the
        // driver and the producer becomes a requesting follower.
        props.insert("node.supports-lazy", "2");
    }
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-screencast", props).context("pw Stream")?;
    if let Some(p) = &pacer {
        p.stream.set(stream.as_raw_ptr());
    }

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|stream, ud, old, new| {
            let streaming = matches!(new, pw::stream::StreamState::Streaming);
            // Valid only while Streaming. True = the pacer's triggers start every cycle;
            // false = the producer kept the tick.
            let driving = streaming && stream.is_driving();
            tracing::info!(?old, ?new, driving, "pipewire stream state");
            // `Streaming` with no buffers is a static desktop. Anything else means the source
            // went away; `try_latest` turns a sustained non-Streaming state into capture-loss
            // so the encode loop rebuilds instead of freezing on the last frame.
            ud.signals.streaming.store(streaming, Ordering::Relaxed);
            ud.signals.driving.store(driving, Ordering::Relaxed);
            if matches!(new, pw::stream::StreamState::Error(_)) {
                ud.signals.errored.store(true, Ordering::Relaxed);
            }
            if let Some(p) = &ud.pacer {
                p.on_streaming(driving);
            }
        })
        .param_changed(|_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) =
                pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            // Parse once (`parse` takes `&mut self`) and report failure. On `Err`, `negotiated`
            // stays false so the timeout looks like "no accepted format" — a malformed pod we
            // accepted, not a format mismatch.
            let parsed = ud.info.parse(param);
            if let Err(e) = &parsed {
                tracing::error!(
                    error = %e,
                    "pipewire: the negotiated Format pod does not parse — capture will time out \
                     with no usable format"
                );
            }
            if parsed.is_ok() {
                ud.signals.negotiated.store(true, Ordering::Relaxed);
                // Renegotiation replaces the pool: cached per-buffer imports key on buffers
                // that no longer exist, and a recycled fd/inode must not resolve to a stale import.
                if let Some(imp) = ud
                    .signals
                    .importer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    imp.clear_cache();
                }
                let sz = ud.info.size();
                // Gamescope cursor source scales root→frame (`xfixes_cursor::scale_to_frame`).
                ud.signals.frame_size.store(
                    (u64::from(sz.width) << 32) | u64::from(sz.height),
                    Ordering::Relaxed,
                );
                ud.format = map_format(ud.info.format());
                ud.modifier = ud.info.modifier();
                // 10-bit PQ is only offered with MANDATORY BT.2020/PQ, so a 10-bit negotiation
                // is HDR — still log the producer's fixated transfer/primaries.
                let hdr = ud.format.is_some_and(|f| f.is_hdr());
                ud.signals.hdr_negotiated.store(hdr, Ordering::Relaxed);
                tracing::info!(
                    width = sz.width,
                    height = sz.height,
                    spa_format = ?ud.info.format(),
                    mapped = ?ud.format,
                    modifier = ud.modifier,
                    hdr,
                    transfer_function = ud.info.transfer_function(),
                    color_primaries = ud.info.color_primaries(),
                    "pipewire format negotiated"
                );
                if ud.format.is_none() {
                    tracing::error!(
                        spa_format = ?ud.info.format(),
                        "negotiated a pixel format the encoder cannot consume — frames will be skipped"
                    );
                }
            }
        })
        // Pool census. `remove_buffer` also purges the deferred-requeue book: the buffer is
        // being freed under any hold, so that hold's later release must be a no-op (generation
        // in `HoldBook::complete` also covers the address being reused by a new pool).
        .add_buffer(|_stream, ud, _buf| ud.pool.add())
        .remove_buffer(|_stream, ud, buf| {
            ud.pool.remove();
            if let Ok(mut book) = ud.defer.book.lock() {
                book.purge(buf as usize);
            }
        })
        .process(|stream, ud| {
            // Latest-frame-only: Mutter bursts, older queued buffers are stale. Drain, read the
            // older ones' cursor meta, requeue them, keep newest. Dequeue/requeue stay outside
            // `catch_unwind` — a panic inside would strand `newest` and shrink the fixed pool.

            // SAFETY: `stream` is the live stream PipeWire passes into this `.process` callback on the
            // loop thread; `dequeue_raw_buffer` returns a stream-owned `*mut pw_buffer` or null
            // (null-checked), single-threaded so no concurrent access.
            let mut newest = unsafe { stream.dequeue_raw_buffer() };
            if newest.is_null() {
                return;
            }
            let mut drained = 1u32;
            loop {
                // SAFETY: same stream/loop-thread contract; returns the next stream-owned buffer or null.
                let next = unsafe { stream.dequeue_raw_buffer() };
                if next.is_null() {
                    break;
                }
                // A new cursor bitmap rides only the buffer of the shape change; read it before
                // the stale pixels go back. Not while gated: that meta is in the doomed size.
                if ud.expect_dims.is_none() {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // SAFETY: `newest` is dequeued and not yet requeued, as below.
                        update_cursor_meta(&mut ud.cursor, unsafe { (*newest).buffer });
                    }));
                }
                // SAFETY: `newest` was dequeued from this stream and not yet requeued; we immediately
                // overwrite it, so the requeued pointer is never touched again.
                unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
                newest = next;
                drained += 1;
            }
            // Producer's actual pool depth, once per distinct value. `build_dmabuf_buffers`
            // asks for a range; the producer picks. Depth is the deferred-requeue budget:
            // ≤ HOLD_POOL_RESERVE cannot defer, and a requeued buffer may be rewritten mid-encode.
            if let Some(depth) = ud.pool.note_frame() {
                tracing::info!(
                    pool_depth = depth,
                    high_water = ud.pool.high_water,
                    drained,
                    "pipewire buffer pool negotiated — the producer's ACTUAL count \
                     (add_buffer/remove_buffer): the deferred-requeue budget, and the rewrite \
                     window for any frame published without a hold"
                );
            }
            // Sacrificial birth mode (kwin.rs `create`): frame and cursor meta are in the doomed
            // size until renegotiation. Self-disarms on match, or after `GATE_DEADLINE` — degraded
            // dims beat a first-frame-timeout retry loop if the promised renegotiation never comes.
            if let Some((ew, eh)) = ud.expect_dims {
                /// Renegotiation normally lands within a frame or two; past this, stop starving
                /// the pipeline (the real mode never applied).
                const GATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);
                let sz = ud.info.size();
                if sz.width == ew && sz.height == eh {
                    tracing::info!(
                        skipped = ud.gate_skips,
                        width = ew,
                        height = eh,
                        "producer renegotiated to the expected mode — frames flow"
                    );
                    ud.expect_dims = None;
                } else if ud
                    .gate_since
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed()
                    > GATE_DEADLINE
                {
                    tracing::warn!(
                        negotiated_w = sz.width,
                        negotiated_h = sz.height,
                        expected_w = ew,
                        expected_h = eh,
                        skipped = ud.gate_skips,
                        "producer never renegotiated to the expected mode — accepting its \
                         dims (session runs degraded rather than wedged)"
                    );
                    ud.expect_dims = None;
                } else {
                    ud.gate_skips += 1;
                    if ud.gate_skips == 1 || ud.gate_skips.is_power_of_two() {
                        tracing::info!(
                            negotiated_w = sz.width,
                            negotiated_h = sz.height,
                            expected_w = ew,
                            expected_h = eh,
                            n = ud.gate_skips,
                            "holding frames until the producer renegotiates to the expected mode"
                        );
                    }
                    // SAFETY: `newest` was dequeued from this stream and not yet requeued;
                    // requeued exactly once here, then never touched (mirrors the null path).
                    unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
                    return;
                }
            }
            // PipeWire dispatches from a C trampoline with no catch_unwind; a panic across that
            // FFI aborts the host. Contain inspect/consume — the only Rust here that can panic.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: `newest` is the non-null buffer we still own (dequeued, not requeued);
                // `.buffer` is a `*mut spa_buffer` field libpipewire populated. This is a single field
                // load through a valid pointer — no mutation or aliasing.
                let spa_buf = unsafe { (*newest).buffer };

                // Cursor meta before the stale-frame skip: Mutter pointer-only moves arrive as
                // metadata-only CORRUPTED buffers we drop for pixels, but the cursor is fresh.
                update_cursor_meta(&mut ud.cursor, spa_buf);
                // Publish the live overlay so pointer-only motion on a static desktop still
                // moves. Skip when `overlay()` is `None`: gamescope has no `SPA_META_Cursor`,
                // and writing `None` at frame rate would clobber the XFixes `Some` in this
                // same slot (pointer strobes). Hidden is still `Some(visible:false)`.
                if let Some(overlay) = ud.cursor.overlay() {
                    if let Ok(mut slot) = ud.signals.cursor_live.lock() {
                        *slot = Some(overlay);
                    }
                }

                // Header + first chunk for the CORRUPTED skip. SPA_META_Header is optional.

                // SAFETY: `spa_buf` is the `*mut spa_buffer` of the buffer we still hold.
                // `spa_buffer_find_meta_data` scans that buffer's metadata array for a `SPA_META_Header`
                // of at least `size_of::<spa_meta_header>()` bytes and returns a pointer into the held
                // buffer's metadata (or null). The size argument matches the struct the result is cast
                // to, and the pointer stays valid as long as the buffer is held (until requeue). Null is
                // handled below.
                let hdr = unsafe {
                    spa::sys::spa_buffer_find_meta_data(
                        spa_buf,
                        spa::sys::SPA_META_Header,
                        std::mem::size_of::<spa::sys::spa_meta_header>(),
                    ) as *const spa::sys::spa_meta_header
                };
                let hdr_flags = if hdr.is_null() {
                    0u32
                } else {
                    // SAFETY: reached only when `hdr` is non-null; it points to a `spa_meta_header`
                    // inside the live buffer's metadata (returned for a size >=
                    // `size_of::<spa_meta_header>()`, so `.flags` is in bounds). A single field read
                    // while the buffer is still held.
                    unsafe { (*hdr).flags }
                };
                // Compositor stamp, upstream of delivery jitter `SystemTime::now()` cannot see.
                // Whether it is worth shipping is what the provenance line measures.
                let hdr_pts = if hdr.is_null() {
                    None
                } else {
                    // SAFETY: as for `.flags` — non-null, from a lookup that demanded at least
                    // `size_of::<spa_meta_header>()` bytes (so `.pts` is in bounds), read while
                    // the buffer is still held.
                    Some(unsafe { (*hdr).pts })
                };
                // Size + flags for the CORRUPTED skip. dmabuf legitimately reports chunk size
                // 0, so the size-0 stale skip is SHM-only.

                // SAFETY: every dereference is guarded in order before any field read — `spa_buf`
                // non-null, `n_datas > 0`, the `datas` (`*mut spa_data`) array non-null, and the first
                // element's `chunk` (`*mut spa_chunk`) non-null. `d0` is that first `spa_data` and `c`
                // its chunk; reading `(*d0).type_`, `(*c).size`, `(*c).flags` are in-bounds field loads
                // of libspa structs inside the buffer we still hold. Single-threaded loop, no mutation.
                let (chunk_size, chunk_flags, is_dmabuf) = unsafe {
                    if !spa_buf.is_null()
                        && (*spa_buf).n_datas > 0
                        && !(*spa_buf).datas.is_null()
                        && !(*(*spa_buf).datas).chunk.is_null()
                    {
                        let d0 = (*spa_buf).datas;
                        let c = (*d0).chunk;
                        let is_dmabuf =
                            (*d0).type_ == spa::sys::SPA_DATA_DmaBuf;
                        ((*c).size, (*c).flags, is_dmabuf)
                    } else {
                        (0u32, 0i32, false)
                    }
                };

                let corrupted = (hdr_flags & spa::sys::SPA_META_HEADER_FLAG_CORRUPTED) != 0
                    || (chunk_flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32) != 0;

                // Skip Mutter CORRUPTED / size-0 cursor-update buffers. Pointer motion sends
                // metadata-only buffers flagged CORRUPTED (chunk size 0) that still reference
                // a recycled old frame — encoding that is the flash. Size-0 skip is SHM-only.
                if corrupted || (chunk_size == 0 && !is_dmabuf) {
                    ud.dbg_log_n += 1;
                    if ud.dbg_log_n.is_power_of_two() {
                        tracing::debug!(
                            skipped = ud.dbg_log_n,
                            drained,
                            "capture: skipped a stale CORRUPTED/cursor buffer (GNOME)"
                        );
                    }
                    return;
                }

                if let Some(p) = &ud.pacer {
                    p.on_paint();
                }
                consume_frame(ud, spa_buf, newest, stream.as_raw_ptr(), hdr_pts);
            }));
            // Requeue `newest` exactly once on every path unless `try_defer` withheld it —
            // then `BufferHold` owns the requeue; doing both hands the producer the buffer
            // twice. `newest`'s entry is stable here: only this thread removes entries, never
            // `newest`'s inside `.process`. A panic after publish still leaves the hold live.
            let withheld = ud
                .defer
                .book
                .lock()
                .map(|b| b.contains(newest as usize))
                .unwrap_or(false);
            if !withheld {
                // SAFETY: all reads of `spa_buf`/`newest` (update_cursor_meta, consume_frame)
                // completed inside the closure above; `newest` was dequeued from this stream,
                // not yet requeued, and — per the `withheld` check — carries no hold that would
                // requeue it a second time.
                unsafe { hand_back(ud.sync.as_deref(), stream.as_raw_ptr(), newest) };
            }
            if outcome.is_err() {
                // `.process` is per-frame; a deterministic panic would flood. Power-of-two throttle.
                static PANICS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let n = PANICS.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::error!(count = n, "panic in pipewire process callback — frame dropped");
                }
            }
        })
        .register()
        .context("register stream listener")?;

    // A `BufferHold` dropping on any thread only parks and wakes; this loop-thread callback
    // (or `try_defer`, whichever runs first) is where a withheld buffer rejoins.
    let defer_cb = defer.clone();
    let stream_ptr = stream.as_raw_ptr() as usize;
    let _requeue_attach = requeue_rx.attach(mainloop.loop_(), move |()| {
        // SAFETY: the loop thread dispatches this. The stream outlives this attached receiver
        // (declared after it, dropped before it), and the loop stops dispatching once `run()`
        // returns.
        unsafe { defer_cb.drain(stream_ptr as *mut pw::sys::pw_stream) };
    });

    // `PUNKTFUNK_PW_FIXED_POD="WxH"`: one fixed format, to bisect against a producer's EnumFormat.
    let fixed_pod: Option<(u32, u32)> = std::env::var("PUNKTFUNK_PW_FIXED_POD")
        .ok()
        .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)));

    let obj = if let Some((fw, fh)) = fixed_pod {
        tracing::info!(
            fw,
            fh,
            "pipewire: offering a fixed BGRx format pod (PUNKTFUNK_PW_FIXED_POD)"
        );
        pw::spa::pod::object!(
            pw::spa::utils::SpaTypes::ObjectParamFormat,
            pw::spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaType,
                Id,
                pw::spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::MediaSubtype,
                Id,
                pw::spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFormat,
                Id,
                VideoFormat::BGRx
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoSize,
                Rectangle,
                pw::spa::utils::Rectangle {
                    width: fw,
                    height: fh
                }
            ),
            pw::spa::pod::property!(
                pw::spa::param::format::FormatProperties::VideoFramerate,
                Fraction,
                pw::spa::utils::Fraction { num: 0, denom: 1 }
            ),
        )
    } else {
        build_default_format_obj(preferred, Pacing::Producer)
    };

    // gamescope paints the Steam overlay into this node only when negotiated
    // `gamescope_focus_appid` is 0 (the default). Do not advertise a non-zero focus-appid —
    // that is the Remote-Play branch, which drops the overlay.

    if want_hdr {
        tracing::info!(
            "HDR capture: offering xBGR_210LE/xRGB_210LE DMA-BUF modifiers (LINEAR always) \
             with MANDATORY BT.2020 + SMPTE-2084 (PQ) colorimetry"
        );
    }
    // Zero-copy: offer only BGRx dmabuf with our EGL-importable modifiers (offering shm
    // makes the compositor pick shm). Modifiers go out as MANDATORY `ChoiceEnum::Enum`;
    // this is not the two-step DONT_FIXATE handshake (`ChoiceFlags` cannot express it).
    let build_pods = |unpaced: bool| -> Result<Vec<Vec<u8>>> {
        let pacing = offer_pacing(
            unpaced,
            probe.framerate_mhz,
            producer_is_gamescope,
            preferred,
        );
        if want_hdr {
            // Offering SDR alongside lets the producer pick it, and a timeout latches SDR
            // downgrade. Order is the fix — see the NVIDIA note on `HDR_FORMAT_ORDER`. First
            // compatible pod wins, so gamescope's P010 pass leads when the encoder takes it.
            let mut pods = Vec::with_capacity(HDR_FORMAT_ORDER.len() + 1);
            if prefer_native_p010 {
                pods.push(build_hdr_dmabuf_format(
                    VideoFormat::P010_10LE,
                    &[0],
                    preferred,
                    pacing,
                )?);
            }
            for (fmt, list) in &hdr_modifiers {
                pods.push(build_hdr_dmabuf_format(*fmt, list, preferred, pacing)?);
            }
            return Ok(pods);
        }
        if !want_dmabuf {
            // The fixed bisect pod stays exactly what the operator typed.
            let o = if unpaced && fixed_pod.is_none() {
                build_default_format_obj(preferred, Pacing::Unpaced)
            } else {
                obj.clone()
            };
            return Ok(vec![serialize_pod(o)?]);
        }
        let mut pods = Vec::with_capacity(if prefer_native_nv12 { 3 } else { 2 });
        if prefer_native_nv12 {
            // First compatible consumer pod wins. Pinning BT.709 limited selects gamescope's
            // RGB→NV12 shader with our bitstream colorimetry.
            pods.push(build_dmabuf_format(
                VideoFormat::NV12,
                &[0],
                preferred,
                pacing,
            )?);
        }
        if !modifiers.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRx,
                &modifiers,
                preferred,
                pacing,
            )?);
        }
        // xdph (Hyprland/sway) lists only BGRA on its dmabuf EnumFormat (BGRA+BGRx on SHM).
        // A BGRx-only dmabuf offer intersects nothing and the link fails as if modifiers
        // mismatched. Same 32-bit layout; listed after BGRx so a producer offering both
        // still takes the existing path (first compatible consumer pod wins).
        if !modifiers_bgra.is_empty() {
            pods.push(build_dmabuf_format(
                VideoFormat::BGRA,
                &modifiers_bgra,
                preferred,
                pacing,
            )?);
        }
        Ok(pods)
    };
    // Unpaced pods first, the plain set behind them. A KWin before 6.7 floors `maxFramerate`
    // at 1/1, so a fixed 0/1 fails every intersection and the plain set is what fixates.
    let mut format_pods = build_pods(unpaced)?;
    if unpaced {
        format_pods.extend(build_pods(false)?);
    }
    let buffers_values = if want_hdr || want_dmabuf {
        // Dmabuf-only. HDR: Mutter's SHM path paints 8-bit ARGB32 regardless of format, so a
        // MemFd buffer under a 10-bit format would carry mislabeled bytes.
        Some(build_dmabuf_buffers(pool_min, false)?)
    } else if force_shm {
        // Exclude DmaBuf so Mutter must download (glReadPixels orders against render).
        Some(build_shm_only_buffers()?)
    } else {
        // CPU path still accepts mappable dmabufs (gamescope offers only those once its
        // modifier-bearing format pod wins).
        Some(build_mappable_buffers()?)
    };

    let cursor_meta = if offer_cursor_meta {
        Some(build_cursor_meta_param()?)
    } else {
        None
    };
    // Explicit sync: a Buffers twin that demands the meta, ahead of the plain one, and the
    // meta itself. Both sides listing the meta is what puts the two syncobj datas on a buffer.
    let sync_buffers = match &sync {
        Some(_) => Some(build_dmabuf_buffers(pool_min, true)?),
        None => None,
    };
    let sync_meta = match &sync {
        Some(_) => Some(build_sync_timeline_meta_param()?),
        None => None,
    };
    // Any meta listed here narrows the producer's set to the intersection, so the header
    // rides along with the first one; a producer left unlisted keeps its whole set.
    let header_meta = if cursor_meta.is_some() || sync_meta.is_some() {
        Some(build_header_meta_param()?)
    } else {
        None
    };
    let mut byte_slices: Vec<&[u8]> = Vec::new();
    for pod in &format_pods {
        byte_slices.push(pod);
    }
    if let Some(b) = &sync_buffers {
        byte_slices.push(b);
    }
    if let Some(b) = &buffers_values {
        byte_slices.push(b);
    }
    if let Some(m) = &cursor_meta {
        byte_slices.push(m);
    }
    if let Some(m) = &sync_meta {
        byte_slices.push(m);
    }
    if let Some(m) = &header_meta {
        byte_slices.push(m);
    }
    let mut params: Vec<&Pod> = byte_slices
        .iter()
        .map(|&b| Pod::from_bytes(b).context("pod from bytes"))
        .collect::<Result<_>>()?;

    let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    if lazy {
        flags |= pw::stream::StreamFlags::DRIVER;
    }
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            flags,
            &mut params,
        )
        .context("pw stream connect")?;

    let cap_timer = pacer.as_ref().map(|p| {
        let p = p.clone();
        mainloop.loop_().add_timer(move |_| p.schedule())
    });
    if let (Some(p), Some(t)) = (&pacer, &cap_timer) {
        use pw::loop_::IsSource;
        p.timer.set(Some(RawTimer {
            utils: mainloop.loop_().as_raw().utils,
            source: t.as_ptr(),
        }));
    }
    let _requests = pacer
        .as_ref()
        .map(|p| RequestListener::attach(&stream, p.clone()));
    let heartbeat = pacer.as_ref().map(|p| {
        let (p, signals) = (p.clone(), signals_hb.clone());
        mainloop.loop_().add_timer(move |_| {
            // Re-read the role: PipeWire may assign the driver after the Streaming edge.
            // SAFETY: the stream outlives this timer source (declared after it).
            let driving = signals.streaming.load(Ordering::Relaxed)
                && unsafe { pw::sys::pw_stream_is_driving(p.stream.get()) };
            if driving != signals.driving.swap(driving, Ordering::Relaxed) {
                p.on_streaming(driving);
            }
            if driving {
                p.heartbeat();
            }
        })
    });
    if let Some(t) = &heartbeat {
        let _ = t.update_timer(Some(HEARTBEAT), Some(HEARTBEAT));
    }

    // Blocks until capturer `Drop` fires the quit channel. The importer goes here, not with
    // the last `CaptureSignals` clone: the next pipeline must find the EGL/CUDA state gone.
    mainloop.run();
    signals_exit.has_importer.store(false, Ordering::Relaxed);
    *signals_exit
        .importer
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    Ok(())
}

/// Heartbeat period: a cycle out this long is lost and a driver that never started one
/// gets its first (Mutter's first paint follows a request only once a cycle has run).
const HEARTBEAT: std::time::Duration = std::time::Duration::from_millis(250);

/// The least spacing between two paints: one wire interval. The multiplier is undone —
/// a driven monitor never ticks on its own, so a multiplied refresh only inflates the
/// mode its clients see.
fn wire_interval(preferred: Option<(u32, u32, u32)>) -> std::time::Duration {
    let hz = preferred.map(|(_, _, hz)| hz).unwrap_or(60).max(1);
    let hz = (hz / pf_host_config::config().vdisplay_hz_mult.max(1)).max(1);
    std::time::Duration::from_nanos(1_000_000_000 / u64::from(hz))
}

/// One-shot loop timer the pacer re-arms from any of its entry points. Both pointers belong
/// to the loop thread and outlive the pacer's listeners.
#[derive(Clone, Copy)]
struct RawTimer {
    utils: *mut spa::sys::spa_loop_utils,
    source: *mut spa::sys::spa_source,
}

impl RawTimer {
    fn arm(&self, after: std::time::Duration) {
        let value = spa::sys::timespec {
            tv_sec: after.as_secs() as _,
            tv_nsec: after.subsec_nanos() as _,
        };
        let interval = spa::sys::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `utils` is the loop's utils interface and `source` a live timer source of
        // that loop; `update_timer` reads the two timespecs for the duration of the call.
        unsafe {
            // The macro names the sys crate by this alias.
            use spa::sys as spa_sys;
            let mut iface = (*self.utils).iface;
            spa::spa_interface_call_method!(
                &mut iface as *mut spa::sys::spa_interface,
                spa::sys::spa_loop_utils_methods,
                update_timer,
                self.source,
                &value as *const _ as *mut _,
                &interval as *const _ as *mut _,
                false
            );
        }
    }
}

/// Request-driven paint pacing for a lazy driver.
///
/// The producer's frame clock is passive: it paints only inside a graph cycle this stream
/// starts, and asks for one (RequestProcess) when a client committed, a frame callback is
/// owed, or the pointer moved. Each request is answered with a cycle at once, or on the cap
/// timer when the last *paint* is less than a wire interval old. The cap counts paints, not
/// cycles: a cycle that only serves a frame callback or the cursor delivers no frame, and a
/// commit right behind it must not wait an interval for it. The producer's node is sync in
/// the graph, so its paint runs inside the cycle and this stream wakes with the frame the
/// same cycle. Every field is a `Cell`: `trigger_done` can land inside `schedule` when a
/// trigger has to close a cycle the producer never finished.
struct Pacer {
    stream: std::cell::Cell<*mut pw::sys::pw_stream>,
    interval: std::time::Duration,
    /// Streaming and driving: the only state a trigger starts a cycle in.
    live: std::cell::Cell<bool>,
    timer: std::cell::Cell<Option<RawTimer>>,
    /// A request came since the last trigger.
    pending: std::cell::Cell<bool>,
    /// The cycle out now, if any: its trigger time.
    in_flight: std::cell::Cell<Option<std::time::Instant>>,
    last_trigger: std::cell::Cell<Option<std::time::Instant>>,
    /// Trigger time of the last cycle that delivered a frame: the cap's anchor.
    last_paint: std::cell::Cell<Option<std::time::Instant>>,
    requests: std::cell::Cell<u64>,
    triggers: std::cell::Cell<u64>,
    /// Requests that waited on the cap timer.
    deferred: std::cell::Cell<u64>,
    reported: std::cell::Cell<Option<std::time::Instant>>,
}

/// A cycle out longer than this is lost (the producer stalled or the stream re-linked).
const LOST_CYCLE: std::time::Duration = std::time::Duration::from_millis(100);

impl Pacer {
    fn new(interval: std::time::Duration) -> std::rc::Rc<Pacer> {
        std::rc::Rc::new(Pacer {
            stream: std::cell::Cell::new(std::ptr::null_mut()),
            interval,
            live: std::cell::Cell::new(false),
            timer: std::cell::Cell::new(None),
            pending: std::cell::Cell::new(false),
            in_flight: std::cell::Cell::new(None),
            last_trigger: std::cell::Cell::new(None),
            last_paint: std::cell::Cell::new(None),
            requests: std::cell::Cell::new(0),
            triggers: std::cell::Cell::new(0),
            deferred: std::cell::Cell::new(0),
            reported: std::cell::Cell::new(None),
        })
    }

    fn on_request(&self) {
        self.requests.set(self.requests.get() + 1);
        self.pending.set(true);
        self.schedule();
    }

    /// Every edge into Streaming-and-driving starts a cycle at once: the producer's request
    /// gate stays latched across a renegotiation, and a trigger sent while paused is lost.
    fn on_streaming(&self, live: bool) {
        self.live.set(live);
        self.in_flight.set(None);
        if live {
            self.pending.set(true);
            self.last_paint.set(None);
            self.schedule();
        }
    }

    fn on_done(&self) {
        self.in_flight.set(None);
        self.schedule();
    }

    /// A cycle delivered a frame (called from `.process`, before it is consumed). The
    /// anchor is that cycle's trigger, not the arrival: the next trigger then lands one
    /// interval after it and the paint period is the interval, not interval plus cycle.
    fn on_paint(&self) {
        self.last_paint.set(Some(
            self.in_flight.get().unwrap_or_else(std::time::Instant::now),
        ));
    }

    /// Start a cycle if one is wanted and allowed; else arm the cap timer for the moment
    /// it is. Never two cycles out at once.
    fn schedule(&self) {
        if !self.live.get() || !self.pending.get() || self.in_flight.get().is_some() {
            return;
        }
        let now = std::time::Instant::now();
        if let Some(next) = self.last_paint.get().map(|t| t + self.interval) {
            if now < next {
                if let Some(timer) = self.timer.get() {
                    timer.arm(next - now);
                }
                self.deferred.set(self.deferred.get() + 1);
                return;
            }
        }
        self.pending.set(false);
        self.trigger(now);
    }

    fn trigger(&self, now: std::time::Instant) {
        // SAFETY: `stream` is this thread's live stream; the listeners that reach the pacer
        // are removed before it drops.
        let res = unsafe { pw::sys::pw_stream_trigger_process(self.stream.get()) };
        if res < 0 {
            // Not started (paused, unlinked): the request stays pending for the next edge.
            self.pending.set(true);
            return;
        }
        self.in_flight.set(Some(now));
        self.last_trigger.set(Some(now));
        self.triggers.set(self.triggers.get() + 1);
    }

    /// Every [`HEARTBEAT`]: retry a lost cycle (its request was never served), serve a
    /// request whose cap timer was missed, and report the tally.
    fn heartbeat(&self) {
        let now = std::time::Instant::now();
        if self.in_flight.get().is_some_and(|t| now - t > LOST_CYCLE) {
            self.in_flight.set(None);
            self.pending.set(true);
        }
        self.schedule();
        if self
            .reported
            .get()
            .is_none_or(|t| now - t >= PTS_REPORT_EVERY)
        {
            if self.reported.get().is_some() {
                tracing::info!(
                    requests = self.requests.get(),
                    triggers = self.triggers.get(),
                    deferred = self.deferred.get(),
                    interval_us = self.interval.as_micros() as u64,
                    "lazy capture pacer: producer requests answered with cycles (deferred = held \
                     to the wire interval)"
                );
            }
            self.reported.set(Some(now));
        }
    }
}

/// The stream events pipewire-rs 0.9 has no builder for: RequestProcess and `trigger_done`.
/// Both callbacks run on the loop thread. Dropping removes the hook, before the pacer.
struct RequestListener {
    hook: Box<spa::sys::spa_hook>,
    _events: Box<pw::sys::pw_stream_events>,
    _pacer: std::rc::Rc<Pacer>,
}

impl RequestListener {
    fn attach(stream: &pw::stream::Stream, pacer: std::rc::Rc<Pacer>) -> RequestListener {
        unsafe extern "C" fn on_command(
            data: *mut std::ffi::c_void,
            command: *const spa::sys::spa_command,
        ) {
            // SAFETY: `data` is the `Rc<Pacer>` this listener holds; `command` is the live pod
            // PipeWire passes for the callback.
            unsafe {
                let body = (*command).body.body;
                if body.type_ == spa::sys::SPA_TYPE_COMMAND_Node
                    && body.id == spa::sys::SPA_NODE_COMMAND_RequestProcess
                {
                    (*(data as *const Pacer)).on_request();
                }
            }
        }
        unsafe extern "C" fn on_trigger_done(data: *mut std::ffi::c_void) {
            // SAFETY: as above.
            unsafe { (*(data as *const Pacer)).on_done() }
        }
        // SAFETY: zeroed is the C initialiser for both structs; every unset event stays
        // `None` and the hook's list link is filled in by `pw_stream_add_listener`.
        let (mut events, mut hook): (Box<pw::sys::pw_stream_events>, Box<spa::sys::spa_hook>) =
            unsafe { (Box::new(std::mem::zeroed()), Box::new(std::mem::zeroed())) };
        events.version = pw::sys::PW_VERSION_STREAM_EVENTS;
        events.command = Some(on_command);
        events.trigger_done = Some(on_trigger_done);
        // SAFETY: the hook and events boxes live as long as this listener, which is removed
        // in `Drop` before either is freed; `data` stays valid while `_pacer` is held.
        unsafe {
            pw::sys::pw_stream_add_listener(
                stream.as_raw_ptr(),
                &mut *hook,
                &*events,
                std::rc::Rc::as_ptr(&pacer) as *mut std::ffi::c_void,
            );
        }
        RequestListener {
            hook,
            _events: events,
            _pacer: pacer,
        }
    }
}

impl Drop for RequestListener {
    fn drop(&mut self) {
        // SAFETY: the hook was added by `attach` and not removed since.
        unsafe { spa::sys::spa_hook_remove(&mut *self.hook) }
    }
}

/// What the producer node says about itself before the stream connects. The registry
/// announce carries only a subset of a node's props, so the node is bound and read.
#[derive(Debug, Clone, Copy, Default)]
struct ProducerProbe {
    /// It emits RequestProcess (`node.supports-request` > 0). False on any doubt — a wrong
    /// true makes a non-lazy producer a follower of a driver that never triggers.
    supports_request: bool,
    /// Its `EnumFormat` states `maxFramerate` in millihertz: KWin 6.8+, which paces the
    /// cast at the ceiling that fixates. Older KWin offers whole hertz and throttles.
    framerate_mhz: bool,
}

fn probe_producer(
    core: &pw::core::CoreRc,
    mainloop: &pw::main_loop::MainLoopRc,
    node_id: u32,
) -> ProducerProbe {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    let Ok(registry) = core.get_registry_rc() else {
        return ProducerProbe::default();
    };
    let found: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
    let mhz: Rc<Cell<Option<bool>>> = Rc::new(Cell::new(None));
    // The bound proxy and its listener must outlive the round trips that deliver `info`.
    let bound: Rc<RefCell<Option<(pw::node::Node, pw::node::NodeListener)>>> = Rc::default();
    let _reg = registry
        .add_listener_local()
        .global({
            let (registry, found, mhz, bound) =
                (registry.clone(), found.clone(), mhz.clone(), bound.clone());
            move |g| {
                if g.id != node_id || g.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Ok(node) = registry.bind::<pw::node::Node, _>(g) else {
                    found.set(Some(false));
                    return;
                };
                let listener = node
                    .add_listener_local()
                    .info({
                        let found = found.clone();
                        move |info| {
                            let v = info
                                .props()
                                .and_then(|p| p.get("node.supports-request"))
                                .and_then(|v| v.trim().parse::<u32>().ok())
                                .unwrap_or(0);
                            found.set(Some(v > 0));
                        }
                    })
                    .param({
                        let mhz = mhz.clone();
                        move |_, id, _, _, pod| {
                            if id != pw::spa::param::ParamType::EnumFormat {
                                return;
                            }
                            if let Some(denom) =
                                pod.and_then(|p| offer_framerate_denom(p.as_bytes()))
                            {
                                mhz.set(Some(denom == 1000));
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(pw::spa::param::ParamType::EnumFormat), 0, u32::MAX);
                *bound.borrow_mut() = Some((node, listener));
            }
        })
        .register();
    // Round 1 replays the globals and binds; round 2 lands the bind's `info` and formats.
    // The timer bounds a daemon that never answers.
    let awaited: Rc<Cell<Option<pw::spa::utils::result::AsyncSeq>>> = Rc::new(Cell::new(None));
    let _core_l = core
        .add_listener_local()
        .done({
            let (ml, awaited) = (mainloop.clone(), awaited.clone());
            move |_, seq| {
                if awaited.get() == Some(seq) {
                    ml.quit();
                }
            }
        })
        .register();
    let guard = mainloop.loop_().add_timer({
        let ml = mainloop.clone();
        move |_| ml.quit()
    });
    let _ = guard.update_timer(Some(std::time::Duration::from_secs(2)), None);
    for _ in 0..3 {
        let Ok(seq) = core.sync(0) else {
            return ProducerProbe::default();
        };
        awaited.set(Some(seq));
        mainloop.run();
        if found.get().is_some() && mhz.get().is_some() {
            break;
        }
    }
    let (supports, framerate_mhz) = (found.get(), mhz.get());
    tracing::info!(
        node_id,
        supports_request = ?supports,
        framerate_mhz = ?framerate_mhz,
        "capture producer probed"
    );
    ProducerProbe {
        supports_request: supports.unwrap_or(false),
        framerate_mhz: framerate_mhz.unwrap_or(false),
    }
}

/// The offer's `maxFramerate`. KWin up to 6.7 asks for its own damage signal: it takes no
/// ceiling above its refresh, and that one it throttles with a whole-millisecond timer that
/// slips a frame every few. KWin 6.8 (millihertz offer) paces the cast at the ceiling, so
/// it gets the stream rate. gamescope paints on every commit, so the wire rate caps its
/// pushes; anyone else keeps its rate.
fn offer_pacing(
    unpaced: bool,
    producer_mhz: bool,
    gamescope: bool,
    preferred: Option<(u32, u32, u32)>,
) -> Pacing {
    let hz = preferred.map(|(_, _, hz)| hz).filter(|hz| *hz > 0);
    if unpaced {
        if producer_mhz {
            hz.map_or(Pacing::Unpaced, Pacing::Cap)
        } else {
            Pacing::Unpaced
        }
    } else if gamescope {
        hz.map_or(Pacing::Producer, Pacing::Cap)
    } else {
        Pacing::Producer
    }
}

/// BGRx/BGRA dmabuf offers. Importer lists per format; the direct-import lane's
/// tiled seed is `ZeroCopyPolicy::encoder_modifiers`, offered only to a
/// tiled-opted gamescope producer on VA passthrough while the refusal latch is
/// clear; PyroWave merges its Vulkan-importable list on non-gamescope
/// passthrough. `dmabuf_modifiers_for_producer` finalizes each list. Returns
/// `(bgrx, bgra, extend_pyrowave)` — the flag feeds the session-start log line.
fn packed_modifier_offers(
    policy: &ZeroCopyPolicy,
    health: &pf_zerocopy::ZeroCopyHealth,
    importer: Option<&mut pf_zerocopy::Importer>,
    vaapi_passthrough: bool,
    producer_is_gamescope: bool,
) -> (Vec<u64>, Vec<u64>, bool) {
    // EGL importer answers per format; the encoder seed does too. LINEAR is appended for
    // every advertised list and remains the only gamescope choice without a proved tiled seed.
    let advertise = importer.is_some() || vaapi_passthrough;
    let mut modifiers = Vec::new();
    let mut modifiers_bgra = Vec::new();
    if let Some(i) = importer {
        modifiers = i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgrx).unwrap());
        modifiers_bgra = i.supported_modifiers(pf_frame::drm_fourcc(PixelFormat::Bgra).unwrap());
    }
    // PyroWave imports through Vulkan, not libva. Its per-fourcc lists come from the
    // facade so capture never calls `encode`; gamescope has its separately gated seed.
    let extend_pyrowave = vaapi_passthrough && policy.pyrowave_session && !producer_is_gamescope;
    // The direct-import lane's tiled offer comes from what the session encoder
    // proved (`ZeroCopyPolicy::encoder_modifiers`), per fourcc. A refused tiled
    // offer or a non-gamescope producer keeps the importer's list alone.
    let tiled_refused = health.passthrough_tiled_refused();
    let seed_encoder_mods =
        vaapi_passthrough && producer_is_gamescope && policy.gamescope_tiled && !tiled_refused;
    for (fourcc, mods) in &policy.encoder_modifiers {
        let nonzero: Vec<u64> = mods.iter().copied().filter(|m| *m != 0).collect();
        if !nonzero.is_empty() {
            tracing::info!(
                fourcc = format!("{fourcc:#010x}"),
                modifiers = ?nonzero,
                "zero-copy: encoder-proved tiled dmabuf modifiers for capture"
            );
        }
    }
    for (list, fmt) in [
        (&mut modifiers, PixelFormat::Bgrx),
        (&mut modifiers_bgra, PixelFormat::Bgra),
    ] {
        if seed_encoder_mods || extend_pyrowave {
            if let Some(fourcc) = pf_frame::drm_fourcc(fmt) {
                for m in encoder_modifiers_for(policy, fourcc) {
                    if !list.contains(&m) {
                        list.push(m);
                    }
                }
            }
        }
        *list = dmabuf_modifiers_for_producer(
            list,
            advertise,
            producer_is_gamescope
                && (!policy.gamescope_tiled || (vaapi_passthrough && tiled_refused)),
        );
    }
    (modifiers, modifiers_bgra, extend_pyrowave)
}

/// Packed 10-bit offers. Tiled has two readers: NVENC's raw convert, and the VA
/// encoder's own import — the latter only for `encoder_modifiers`-proved
/// modifiers. Every other arm de-tiles into 8 bits, so tiled is offered only
/// while one lane holds the stream. LINEAR is always appended once.
fn hdr_modifier_offers(
    policy: &ZeroCopyPolicy,
    health: &pf_zerocopy::ZeroCopyHealth,
    importer: Option<&mut pf_zerocopy::Importer>,
    want_hdr: bool,
    vaapi_passthrough: bool,
    producer_is_gamescope: bool,
    nvenc_raw: bool,
) -> Vec<(VideoFormat, Vec<u64>)> {
    let mut importer = importer;
    let hdr_tiled_raw =
        want_hdr && policy.gamescope_tiled && nvenc_raw && !health.hdr_tiled_refused();
    let hdr_tiled_direct = want_hdr
        && vaapi_passthrough
        && producer_is_gamescope
        && policy.gamescope_tiled
        && !health.passthrough_tiled_refused();
    let mut hdr_modifiers: Vec<(VideoFormat, Vec<u64>)> = Vec::new();
    for fmt in HDR_FORMAT_ORDER {
        let mut list = Vec::new();
        if hdr_tiled_direct {
            if let Some(fourcc) = map_format(fmt).and_then(pf_frame::drm_fourcc) {
                list = encoder_modifiers_for(policy, fourcc);
            }
        } else if hdr_tiled_raw {
            if let (Some(i), Some(fourcc)) = (
                importer.as_deref_mut(),
                map_format(fmt).and_then(pf_frame::drm_fourcc),
            ) {
                list = i.supported_modifiers(fourcc);
            }
        }
        list.retain(|&m| m != 0);
        list.push(0);
        hdr_modifiers.push((fmt, list));
    }
    hdr_modifiers
}

/// A `linear_only` gamescope node offers LINEAR as `{0,0}`. spa_pod_filter without
/// DONT_FIXATE fixates our default, so a tiled NVIDIA default fails the link. Empty `egl`
/// with `advertise` still yields LINEAR — the importer exists, EGL listed none.
fn dmabuf_modifiers_for_producer(egl: &[u64], advertise: bool, linear_only: bool) -> Vec<u64> {
    if !advertise {
        return egl.to_vec();
    }
    if linear_only {
        return vec![0];
    }
    let mut m = egl.to_vec();
    if !m.contains(&0) {
        m.push(0);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::{
        dmabuf_modifiers_for_producer, holds_possible, negotiation_plan, offer_pacing,
        packed_frame_geometry, supported_data_plane_count, ImportPolicy, NegotiationInputs, Pacing,
    };

    /// A 10-bit SDR session drops NV12 for packed RGB (NVENC widens 8-bit to 10-bit only from
    /// packed RGB); an 8-bit session keeps whatever `PUNKTFUNK_NV12` configured.
    #[test]
    fn ten_bit_sdr_keeps_packed_rgb() {
        let p = ImportPolicy {
            nv12: true,
            yuv444: false,
        };
        assert!(!p.for_ten_bit_sdr(true).nv12);
        assert!(p.for_ten_bit_sdr(false).nv12);
    }

    /// The raw lane needs the importer (its modifier offer) and a live raw-dmabuf latch.
    #[test]
    fn nvenc_raw_rides_the_importer_and_the_latch() {
        let raw = NegotiationInputs {
            nvenc_raw: true,
            ..nvenc()
        };
        assert!(negotiation_plan(raw).nvenc_raw);
        assert!(negotiation_plan(nvenc()).build_importer && !negotiation_plan(nvenc()).nvenc_raw);
        assert!(
            !negotiation_plan(NegotiationInputs {
                raw_dmabuf_import_disabled: true,
                ..raw
            })
            .nvenc_raw,
            "a tripped latch keeps the import path"
        );
        assert!(
            !negotiation_plan(NegotiationInputs {
                force_shm: true,
                ..raw
            })
            .nvenc_raw,
            "SHM builds no importer, so no raw lane"
        );
    }

    /// The consumer imports with the offer's policy: NV12 unless the session is 4:4:4. Holds
    /// need a pool deeper than the producer's reserve.
    #[test]
    fn import_policy_follows_the_session_and_holds_need_a_deep_pool() {
        let p = negotiation_plan(nvenc());
        assert!(p.import_policy.nv12 && !p.import_policy.yuv444);
        let p = negotiation_plan(NegotiationInputs {
            want_444: true,
            ..nvenc()
        });
        assert!(p.import_policy.yuv444, "4:4:4 must not subsample");
        assert!(holds_possible(true, HOLD_POOL_RESERVE + 1));
        assert!(!holds_possible(true, HOLD_POOL_RESERVE));
        assert!(!holds_possible(false, 8));
    }

    /// A re-sent buffer is re-held: the stale generation no longer requeues, the new one does,
    /// and the pool stays whole.
    #[test]
    fn a_resent_buffer_is_reheld_under_a_new_generation() {
        let mut book = HoldBook::default();
        let old = book.try_hold(0x10, HOLD_POOL_RESERVE + 2).unwrap();
        let new = book.try_hold(0x10, HOLD_POOL_RESERVE + 2).unwrap();
        assert!(new > old);
        assert_eq!(book.out.len(), 1);
        assert!(!book.complete(0x10, old));
        assert!(book.contains(0x10));
        assert!(book.complete(0x10, new));
        assert!(!book.contains(0x10));
    }

    /// gamescope and KWin 6.8 are capped at the wire rate, older KWin never; a missing
    /// rate caps nothing.
    #[test]
    fn pacing_caps_follow_the_wire_rate() {
        assert_eq!(
            offer_pacing(true, false, false, Some((1, 1, 90))),
            Pacing::Unpaced
        );
        assert_eq!(
            offer_pacing(true, true, false, Some((1, 1, 90))),
            Pacing::Cap(90)
        );
        assert_eq!(
            offer_pacing(true, true, false, Some((1, 1, 0))),
            Pacing::Unpaced
        );
        assert_eq!(offer_pacing(true, true, false, None), Pacing::Unpaced);
        assert_eq!(
            offer_pacing(false, false, true, Some((1, 1, 90))),
            Pacing::Cap(90)
        );
        assert_eq!(
            offer_pacing(false, false, true, Some((1, 1, 0))),
            Pacing::Producer
        );
        assert_eq!(
            offer_pacing(false, false, false, Some((1, 1, 90))),
            Pacing::Producer
        );
    }

    /// NVIDIA block-linear, the EGL default on this host. gamescope does not offer it.
    const NVIDIA_TILED: u64 = 216172782120099856;

    #[test]
    fn gamescope_dmabuf_offer_fixates_linear() {
        let egl = [NVIDIA_TILED, NVIDIA_TILED + 4];
        assert_eq!(
            dmabuf_modifiers_for_producer(&egl, true, true),
            vec![0],
            "a forced LINEAR offer must discard tiled defaults"
        );
        assert_eq!(
            dmabuf_modifiers_for_producer(&[], true, true),
            vec![0],
            "a live importer with no EGL list still advertises LINEAR"
        );
        let kwin = dmabuf_modifiers_for_producer(&egl, true, false);
        assert_eq!(
            kwin[0], NVIDIA_TILED,
            "KWin lists the tiled mods; keep them first"
        );
        assert!(kwin.contains(&0));
        assert!(dmabuf_modifiers_for_producer(&[], false, true).is_empty());
    }

    #[test]
    fn only_supported_pipewire_plane_counts_become_slice_lengths() {
        assert_eq!(supported_data_plane_count(0), None);
        assert_eq!(supported_data_plane_count(1), Some(1));
        assert_eq!(supported_data_plane_count(2), Some(2));
        assert_eq!(supported_data_plane_count(3), None);
        assert_eq!(supported_data_plane_count(u32::MAX), None);
    }

    /// A healthy NVENC session: zero-copy on, no latches, SDR 4:2:0, non-VAAPI backend.
    fn nvenc() -> NegotiationInputs {
        NegotiationInputs {
            zerocopy: true,
            force_shm: false,
            want_hdr: false,
            want_444: false,
            backend_is_vaapi: false,
            pyrowave_session: false,
            native_nv12_session: false,
            raw_dmabuf_import_disabled: false,
            gpu_import_disabled: false,
            gpu_dmabuf_negotiation_failed: false,
            native_nv12_env_on: true,
            hdr_cuda_ok: true,
            nv12_env_on: true,
            nvenc_raw: false,
        }
    }

    /// A gamescope-style VAAPI session that CAN take producer-native NV12.
    fn vaapi_native_nv12() -> NegotiationInputs {
        NegotiationInputs {
            backend_is_vaapi: true,
            native_nv12_session: true,
            ..nvenc()
        }
    }

    #[test]
    fn packed_frame_geometry_rejects_overflow_and_short_stride() {
        assert_eq!(packed_frame_geometry(4, 3, 4, 0), Some((16, 16, 48, 48)));
        assert_eq!(packed_frame_geometry(4, 3, 4, 15), None);
        assert_eq!(packed_frame_geometry(1, 0, 4, 4), None);
        assert_eq!(packed_frame_geometry(usize::MAX, 2, 4, 0), None);
        assert_eq!(
            packed_frame_geometry(usize::MAX / 2, 3, 1, usize::MAX / 2),
            None
        );
    }

    /// Pins the four invariants documented on [`negotiation_plan`].
    #[test]
    fn negotiation_plan_invariants() {
        // HDR on the NVENC importer is LINEAR and takes the Vulkan bridge, never the
        // 8-bit de-tile blit. Direct raw lanes gate tiled formats separately.
        for want_444 in [false, true] {
            let p = negotiation_plan(NegotiationInputs {
                want_hdr: true,
                want_444,
                ..nvenc()
            });
            assert!(p.build_importer, "HDR on NVENC keeps zero-copy");
        }
        // …but never under a raw passthrough (VAAPI/PyroWave import the dmabuf themselves).
        assert!(
            !negotiation_plan(NegotiationInputs {
                want_hdr: true,
                ..vaapi_native_nv12()
            })
            .build_importer
        );
        // Never when the encoder cannot take packed 10-bit CUDA.
        // SDR is unaffected — the term is HDR-only.
        assert!(
            !negotiation_plan(NegotiationInputs {
                want_hdr: true,
                hdr_cuda_ok: false,
                ..nvenc()
            })
            .build_importer,
            "HDR must stay on the CPU path where the encoder can't ingest 10-bit CUDA"
        );
        assert!(
            negotiation_plan(NegotiationInputs {
                hdr_cuda_ok: false,
                ..nvenc()
            })
            .build_importer,
            "the HDR-only guard must not touch an SDR session"
        );

        // 2. 4:4:4 never prefers producer NV12 or P010 (a 4:4:4 session must not be subsampled).
        for want_hdr in [false, true] {
            let p = negotiation_plan(NegotiationInputs {
                want_444: true,
                want_hdr,
                ..vaapi_native_nv12()
            });
            assert!(!p.prefer_native_nv12, "4:4:4 must not take NV12");
            assert!(!p.prefer_native_p010, "4:4:4 must not take P010");
        }
        // HDR takes the producer's P010 where SDR takes its NV12: one gate, the depth
        // picks the container.
        let p = negotiation_plan(NegotiationInputs {
            want_hdr: true,
            ..vaapi_native_nv12()
        });
        assert!(
            !p.prefer_native_nv12,
            "an HDR session must not take 8-bit NV12"
        );
        assert!(
            p.prefer_native_p010,
            "an HDR session takes the producer's P010"
        );
        assert!(!negotiation_plan(vaapi_native_nv12()).prefer_native_p010);

        // Producer-native NV12 needs a `native_nv12_session` and an active raw passthrough:
        // the VAAPI session takes RGB, and so does the CUDA importer.
        assert!(negotiation_plan(vaapi_native_nv12()).prefer_native_nv12);
        assert!(
            !negotiation_plan(NegotiationInputs {
                native_nv12_session: false,
                ..vaapi_native_nv12()
            })
            .prefer_native_nv12,
            "a session whose encoder can't ingest NV12 must never be offered it"
        );
        assert!(
            !negotiation_plan(NegotiationInputs {
                force_shm: true,
                ..vaapi_native_nv12()
            })
            .prefer_native_nv12,
            "no passthrough (force_shm) ⇒ no native NV12"
        );
        // A PyroWave session takes the passthrough but its CSC ingests packed RGB only.
        for want_hdr in [false, true] {
            let p = negotiation_plan(NegotiationInputs {
                pyrowave_session: true,
                want_hdr,
                ..vaapi_native_nv12()
            });
            assert!(!p.prefer_native_nv12 && !p.prefer_native_p010);
        }

        // Passthrough (and the pyrowave-modifier extension) is off once the raw-dmabuf latch fires.
        let p = negotiation_plan(NegotiationInputs {
            raw_dmabuf_import_disabled: true,
            ..vaapi_native_nv12()
        });
        assert!(!p.vaapi_passthrough, "latched ⇒ no raw passthrough");
        assert!(!p.prefer_native_nv12);
        assert!(p.raw_dmabuf_latched, "…and the operator gets told why");
    }

    /// The latch must move `vaapi_passthrough`. One resolver is shared by the thread and
    /// `spawn_pipewire`; a timeout must not latch a downgrade for an offer nobody made.
    #[test]
    fn the_raw_dmabuf_latch_moves_the_passthrough_decision() {
        for pyrowave in [false, true] {
            let base = NegotiationInputs {
                backend_is_vaapi: !pyrowave,
                pyrowave_session: pyrowave,
                ..nvenc()
            };
            assert!(negotiation_plan(base).vaapi_passthrough);
            assert!(
                !negotiation_plan(NegotiationInputs {
                    raw_dmabuf_import_disabled: true,
                    ..base
                })
                .vaapi_passthrough
            );
        }
    }

    /// EGL→CUDA negotiation-timeout latch gates `build_importer` only, so a compositor that
    /// accepts none of the importer's modifiers is not re-asked (10 s) every session.
    #[test]
    fn gpu_dmabuf_negotiation_latch_gates_only_the_importer() {
        let p = negotiation_plan(NegotiationInputs {
            gpu_dmabuf_negotiation_failed: true,
            ..nvenc()
        });
        assert!(!p.build_importer, "latched offer must not be re-made");
        assert!(p.gpu_import_latched, "the downgrade must be diagnosable");
        let p = negotiation_plan(NegotiationInputs {
            gpu_dmabuf_negotiation_failed: true,
            ..vaapi_native_nv12()
        });
        assert!(
            p.vaapi_passthrough,
            "the raw passthrough has its own latch — this one must not touch it"
        );
        assert!(!p.gpu_import_latched, "no importer was ever wanted here");
    }

    /// PyroWave takes raw passthrough (its Vulkan device imports on any vendor) and must not
    /// also build the EGL→CUDA importer — those payloads only NVENC can consume.
    #[test]
    fn a_pyrowave_session_passes_through_without_a_cuda_importer() {
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert!(p.vaapi_passthrough);
        assert!(!p.build_importer);
    }

    /// `force_shm` is the race-free download path: no passthrough, and `want_dmabuf` stays false
    /// even with an importer and a full modifier list.
    #[test]
    fn force_shm_wins_over_every_dmabuf_path() {
        let p = negotiation_plan(NegotiationInputs {
            force_shm: true,
            ..vaapi_native_nv12()
        });
        assert!(!p.vaapi_passthrough);
        assert!(!p.want_dmabuf(true, &[0, 1, 2]));
        // SHM-forced NVENC may still build the importer (it will not be fed dmabufs), so
        // `want_dmabuf` — not `build_importer` — is the gate.
        let p = negotiation_plan(NegotiationInputs {
            force_shm: true,
            ..nvenc()
        });
        assert!(p.build_importer);
        assert!(!p.want_dmabuf(true, &[0]));
    }

    /// `want_dmabuf` needs a real modifier list: an importer that constructed but advertised
    /// nothing importable falls back to the CPU path.
    #[test]
    fn want_dmabuf_needs_both_a_consumer_and_a_modifier() {
        let p = negotiation_plan(nvenc());
        assert!(p.want_dmabuf(true, &[0]));
        assert!(!p.want_dmabuf(true, &[]), "no modifiers ⇒ CPU path");
        assert!(
            !p.want_dmabuf(false, &[0]),
            "importer failed to construct and no passthrough ⇒ CPU path"
        );
        // The passthrough needs no importer at all.
        let p = negotiation_plan(vaapi_native_nv12());
        assert!(p.want_dmabuf(false, &[0]));
    }

    #[test]
    fn the_gpu_import_death_latch_skips_the_importer() {
        let p = negotiation_plan(NegotiationInputs {
            gpu_import_disabled: true,
            ..nvenc()
        });
        assert!(!p.build_importer);
        assert!(p.gpu_import_latched);
        // HDR takes the same importer (LINEAR/Vulkan-bridge), so the latch costs it zero-copy.
        assert!(
            negotiation_plan(NegotiationInputs {
                gpu_import_disabled: true,
                want_hdr: true,
                ..nvenc()
            })
            .gpu_import_latched
        );
        // Not reported for a raw passthrough that would never have built an importer.
        assert!(
            !negotiation_plan(NegotiationInputs {
                gpu_import_disabled: true,
                ..vaapi_native_nv12()
            })
            .gpu_import_latched
        );
    }

    #[test]
    fn zerocopy_off_disables_every_branch() {
        for i in [
            NegotiationInputs {
                zerocopy: false,
                ..nvenc()
            },
            NegotiationInputs {
                zerocopy: false,
                ..vaapi_native_nv12()
            },
        ] {
            let p = negotiation_plan(i);
            assert!(!p.build_importer);
            assert!(!p.vaapi_passthrough);
            assert!(!p.prefer_native_nv12);
            assert!(!p.prefer_native_p010);
            assert!(!p.want_dmabuf(false, &[0]));
        }
    }

    // Env-var reads race under a shared test process, so these assert against the pure
    // functions the logging sites call.

    use super::{
        consumer_kind, encoder_modifiers_for, passthrough_fallback_action, resolved_capture_arm,
        CaptureArm, ConsumerKind, FenceWaitStats, PassthroughFallback, PassthroughFallbackAction,
        PassthroughFallbacks, PoolCensus, ZeroCopyPolicy, FENCE_WAIT_BUCKETS_US,
    };

    /// PyroWave wins even when it also flips `backend_is_vaapi` on (`linux_zero_copy_is_vaapi`
    /// `Pyrowave` arm). The other order reports the session as somebody else's backend.
    #[test]
    fn pyrowave_outranks_the_host_global_backend_pref() {
        assert_eq!(consumer_kind(true, true, true), ConsumerKind::PyroWave);
        // NVIDIA/auto host (`backend_is_vaapi` false) is still PyroWave — that case logged nothing.
        assert_eq!(consumer_kind(true, false, true), ConsumerKind::PyroWave);
    }

    #[test]
    fn consumer_kinds_and_which_ones_a_cpu_arm_degrades() {
        assert_eq!(consumer_kind(false, true, true), ConsumerKind::AmdIntel);
        assert_eq!(consumer_kind(false, false, true), ConsumerKind::Nvenc);
        // No GPU backend ⇒ the software encoder, whose native input IS CPU frames.
        assert_eq!(consumer_kind(false, false, false), ConsumerKind::Software);
        assert!(ConsumerKind::PyroWave.cpu_is_downgrade());
        assert!(ConsumerKind::AmdIntel.cpu_is_downgrade());
        assert!(ConsumerKind::Nvenc.cpu_is_downgrade());
        assert!(!ConsumerKind::Software.cpu_is_downgrade());
    }

    /// The arm is a function of the plan plus the two runtime facts. Pinned against every plan the
    /// resolver can produce, so the headline line can never claim an arm the session did not take.
    #[test]
    fn resolved_arm_matches_the_plan_that_produced_it() {
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert!(p.vaapi_passthrough);
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[0])),
            CaptureArm::DmabufPassthrough
        );
        let p = negotiation_plan(nvenc());
        assert!(p.build_importer);
        assert_eq!(
            resolved_capture_arm(&p, true, p.want_dmabuf(true, &[0])),
            CaptureArm::CudaImport
        );
        // The importer was meant to be built but did not construct (no driver): CPU, not a
        // cuda-import the session never got.
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[0])),
            CaptureArm::Cpu
        );
        // An empty modifier list is a CPU arm even under a live passthrough plan.
        let p = negotiation_plan(NegotiationInputs {
            pyrowave_session: true,
            ..nvenc()
        });
        assert_eq!(
            resolved_capture_arm(&p, false, p.want_dmabuf(false, &[])),
            CaptureArm::Cpu
        );
        // Forced SHM: CPU regardless of everything else.
        let p = negotiation_plan(NegotiationInputs {
            force_shm: true,
            ..nvenc()
        });
        assert_eq!(
            resolved_capture_arm(&p, true, p.want_dmabuf(true, &[0])),
            CaptureArm::Cpu
        );
    }

    /// The rate limiter: ONE line per distinct reason per session, counting every fall-through.
    /// `.process` runs per frame, so an off-by-one here is a log flood at the capture rate.
    #[test]
    fn fallback_log_budget_is_one_line_per_reason() {
        let mut f = PassthroughFallbacks::default();
        assert_eq!(f.note(PassthroughFallback::NotDmabuf), Some(1));
        for _ in 0..1_000 {
            assert_eq!(f.note(PassthroughFallback::NotDmabuf), None);
        }
        // A different reason is a different diagnosis and gets its own line, carrying the
        // running total — which distinguishes a persistent downgrade from a hiccup.
        assert_eq!(f.note(PassthroughFallback::DupFailed), Some(1002));
        assert_eq!(f.note(PassthroughFallback::DupFailed), None);
        assert_eq!(f.note(PassthroughFallback::NoFormat), Some(1004));
        assert_eq!(f.note(PassthroughFallback::NoFourcc), Some(1005));
        assert_eq!(f.note(PassthroughFallback::UnalignedPitch), Some(1006));
        for r in [
            PassthroughFallback::NoFormat,
            PassthroughFallback::NotDmabuf,
            PassthroughFallback::NoFourcc,
            PassthroughFallback::DupFailed,
            PassthroughFallback::UnalignedPitch,
        ] {
            assert_eq!(f.note(r), None);
        }
    }

    /// Every reason is distinguishable (a shared bit would silence one of them) and carries an
    /// actionable hint — a reason with no fix is a line the reader cannot use.
    #[test]
    fn every_fallback_reason_is_distinct_and_actionable() {
        let all = [
            PassthroughFallback::NoFormat,
            PassthroughFallback::NotDmabuf,
            PassthroughFallback::NoFourcc,
            PassthroughFallback::DupFailed,
            PassthroughFallback::UnalignedPitch,
            PassthroughFallback::NoHold,
        ];
        let mut f = PassthroughFallbacks::default();
        for r in all {
            assert!(
                f.note(r).is_some(),
                "{r:?} shares a bit with an earlier reason"
            );
            assert!(!r.as_str().is_empty());
            assert!(!r.hint().is_empty());
        }
        // Only `NoFormat` drops the frame; the other five downgrade it.
        assert!(!PassthroughFallback::NoFormat.falls_back_to_cpu());
        assert!(PassthroughFallback::NotDmabuf.falls_back_to_cpu());
        assert!(PassthroughFallback::NoFourcc.falls_back_to_cpu());
        assert!(PassthroughFallback::DupFailed.falls_back_to_cpu());
        assert!(PassthroughFallback::UnalignedPitch.falls_back_to_cpu());
        assert!(PassthroughFallback::NoHold.falls_back_to_cpu());
    }

    /// A tiled buffer can never take the CPU de-pad — any failure on a nonzero
    /// modifier retires the tiled offer and rebuilds the capture on LINEAR.
    /// LINEAR keeps the per-reason split.
    #[test]
    fn tiled_passthrough_failures_rebuild_on_linear() {
        let all = [
            PassthroughFallback::NoFormat,
            PassthroughFallback::NotDmabuf,
            PassthroughFallback::NoFourcc,
            PassthroughFallback::DupFailed,
            PassthroughFallback::UnalignedPitch,
            PassthroughFallback::NoHold,
        ];
        for reason in all {
            for modifier in [1u64, 0x100000000000001, 0x200000000000a04] {
                assert_eq!(
                    passthrough_fallback_action(reason, modifier),
                    PassthroughFallbackAction::DropTiledAndRebuild,
                    "{reason:?} on modifier {modifier:#x} must refuse the tiled offer"
                );
            }
            let linear = passthrough_fallback_action(reason, 0);
            let want = match reason {
                PassthroughFallback::NoFormat => PassthroughFallbackAction::Drop,
                _ => PassthroughFallbackAction::Cpu,
            };
            assert_eq!(linear, want, "{reason:?} on LINEAR");
        }
    }

    /// The lookup clones only the exact fourcc's list, drops LINEAR entries and
    /// duplicates, and answers empty for a fourcc the encoder never proved.
    #[test]
    fn encoder_modifiers_lookup_is_exact_and_linear_stripped() {
        const XR24: u32 = 0x34325258;
        const AR24: u32 = 0x34325241;
        let policy = ZeroCopyPolicy {
            encoder_modifiers: vec![(
                XR24,
                vec![0x100000000000001, 0, 0x100000000000001, 0x100000000000002],
            )],
            ..Default::default()
        };
        assert_eq!(
            encoder_modifiers_for(&policy, XR24),
            vec![0x100000000000001, 0x100000000000002]
        );
        assert!(encoder_modifiers_for(&policy, AR24).is_empty());
        assert!(encoder_modifiers_for(&policy, 0xdeadbeef).is_empty());
    }

    // A p99 one bucket low would call the wait free; one bucket high would justify moving
    // load-bearing sync off the loop thread for nothing.

    /// An empty histogram must say "no answer", not "zero" — the second looks like p99 ≈ 0.
    #[test]
    fn an_empty_histogram_has_no_quantile() {
        let s = FenceWaitStats::default();
        assert_eq!(s.quantile_bucket_us(0.99), None);
        assert_eq!(s.mean_us(), 0);
        assert!(!s.is_meaningful(), "it must not be trusted yet either");
    }

    /// Every sample in the first bucket must put p99 there too.
    #[test]
    fn an_all_fast_distribution_puts_p99_in_the_first_bucket() {
        let mut s = FenceWaitStats::default();
        for _ in 0..1000 {
            s.record(3);
        }
        assert_eq!(
            s.quantile_bucket_us(0.50),
            Some(Some(FENCE_WAIT_BUCKETS_US[0]))
        );
        assert_eq!(
            s.quantile_bucket_us(0.99),
            Some(Some(FENCE_WAIT_BUCKETS_US[0]))
        );
        assert_eq!(s.mean_us(), 3);
    }

    /// Fast median, heavy tail: p50 stays low and p99 finds the tail. A smear cannot tell them apart.
    #[test]
    fn a_heavy_tail_moves_p99_without_moving_p50() {
        let mut s = FenceWaitStats::default();
        for _ in 0..980 {
            s.record(10); // fast majority
        }
        for _ in 0..20 {
            s.record(6_000); // 2 % of frames stall milliseconds
        }
        assert_eq!(
            s.quantile_bucket_us(0.50),
            Some(Some(FENCE_WAIT_BUCKETS_US[0])),
            "the median is still free"
        );
        assert_eq!(
            s.quantile_bucket_us(0.99),
            Some(Some(10_000)),
            "...but the p99 must land in the 5-10ms bucket, not with the median"
        );
        assert_eq!(s.max_us, 6_000);
    }

    /// Anything past the last edge reports as overflow rather than being clamped into the last
    /// bucket — "worse than 10 ms" is a distinct finding and must not read as "10 ms".
    #[test]
    fn waits_past_the_last_edge_report_as_overflow() {
        let mut s = FenceWaitStats::default();
        s.record(99_000);
        assert_eq!(s.quantile_bucket_us(0.99), Some(None));
    }

    /// Bucket edges are inclusive upper bounds, so a sample exactly ON an edge belongs to that
    /// bucket and not the next one up.
    #[test]
    fn bucket_edges_are_inclusive() {
        for (i, &edge) in FENCE_WAIT_BUCKETS_US.iter().enumerate() {
            let mut s = FenceWaitStats::default();
            s.record(edge);
            assert_eq!(
                s.quantile_bucket_us(1.0),
                Some(Some(edge)),
                "a sample of exactly {edge}us belongs in bucket {i}"
            );
        }
    }

    /// A stable pool logs one line, not one per frame. `.process` runs at capture rate.
    #[test]
    fn a_stable_pool_is_logged_once() {
        let mut p = PoolCensus::default();
        for _ in 0..8 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(8));
        for _ in 0..100 {
            assert_eq!(p.note_frame(), None, "the same depth must not re-log");
        }
    }

    /// A renegotiation frees the pool and re-allocates it. The LIVE count therefore dips (and the
    /// new depth is worth a second line), but `high_water` — the number a pipeline-depth decision
    /// keys on — must not follow the dip down.
    #[test]
    fn a_renegotiated_pool_relogs_but_the_high_water_holds() {
        let mut p = PoolCensus::default();
        for _ in 0..8 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(8));
        for _ in 0..8 {
            p.remove();
        }
        for _ in 0..4 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(4), "a changed depth is worth a line");
        assert_eq!(p.high_water, 8, "the deepest pool seen this session");
    }

    /// `remove_buffer` without a matching `add_buffer` must not wrap the count to `u32::MAX` —
    /// a depth gate reading that would happily pipeline against a pool of zero.
    #[test]
    fn unmatched_removes_saturate_at_zero() {
        let mut p = PoolCensus::default();
        p.remove();
        p.remove();
        assert_eq!(p.note_frame(), Some(0));
        assert_eq!(p.high_water, 0);
    }

    use super::{HoldBook, HOLD_POOL_RESERVE};

    /// The book must always leave [`HOLD_POOL_RESERVE`] buffers with the producer: an 8-pool
    /// spares 6, and the pools at or below the reserve spare NOTHING — those sessions must fall
    /// back to the immediate requeue rather than starve the compositor of render targets.
    #[test]
    fn hold_book_spends_at_most_pool_minus_reserve() {
        let mut b = HoldBook::default();
        for i in 0..6 {
            assert!(
                b.try_hold(0x1000 + i, 8).is_some(),
                "hold {i} within budget"
            );
        }
        assert!(
            b.try_hold(0x2000, 8).is_none(),
            "7th of 8 exceeds the budget"
        );
        assert!(
            HoldBook::default()
                .try_hold(0x1000, HOLD_POOL_RESERVE)
                .is_none(),
            "a pool of exactly the reserve cannot spare a buffer"
        );
        assert!(
            HoldBook::default()
                .try_hold(0x1000, HOLD_POOL_RESERVE + 1)
                .is_some(),
            "one past the reserve spares exactly one"
        );
    }

    /// KWin's pool of 4 spares two holds: the host's frame and the slot's. The arrival fits only
    /// once the slot's untaken frame is released in `.process`, and the late release it sends
    /// afterwards must not requeue the buffer a second time.
    #[test]
    fn a_released_slot_hold_admits_the_arrival_on_a_kwin_pool() {
        let pool = crate::KWIN_POOL_MAX as u32;
        let mut b = HoldBook::default();
        assert!(b.try_hold(0x1000, pool).is_some(), "the host's frame");
        let slot = b.try_hold(0x2000, pool).expect("the slot's frame");
        assert!(b.try_hold(0x3000, pool).is_none(), "both holds are out");
        assert!(
            b.complete(0x2000, slot),
            "the untaken frame gives its hold back"
        );
        assert!(b.try_hold(0x3000, pool).is_some(), "the arrival now holds");
        assert!(!b.complete(0x2000, slot), "the late release no-ops");
    }

    /// The raw lane deepens the ask to 6 unless the producer caps lower; KWin fails negotiation
    /// above its cap, so there the ask stays 4.
    #[test]
    fn the_raw_lane_pool_ask_stops_at_the_producers_cap() {
        use super::{pool_ask, RAW_LANE_POOL_MIN};
        assert_eq!(pool_ask(crate::POOL_MIN, None, false), crate::POOL_MIN);
        assert_eq!(pool_ask(crate::POOL_MIN, None, true), RAW_LANE_POOL_MIN);
        assert_eq!(
            pool_ask(crate::KWIN_POOL_MIN, Some(crate::KWIN_POOL_MAX), true),
            crate::KWIN_POOL_MAX
        );
        assert_eq!(
            pool_ask(crate::KWIN_POOL_MIN, Some(crate::KWIN_POOL_MAX), false),
            crate::KWIN_POOL_MIN
        );
        assert_eq!(
            pool_ask(4, Some(3), true),
            4,
            "never below the producer's own minimum"
        );
    }

    /// One hold ⇒ one requeue: the first `complete` releases, a duplicate release (a bug shape,
    /// but also the benign stale-message case) must NOT requeue a second time — handing the
    /// producer the same buffer twice corrupts its pool.
    #[test]
    fn hold_book_releases_exactly_once() {
        let mut b = HoldBook::default();
        let g = b.try_hold(0x1000, 8).unwrap();
        assert!(b.complete(0x1000, g), "first release requeues");
        assert!(!b.complete(0x1000, g), "second release is a no-op");
        assert!(!b.contains(0x1000));
    }

    /// Pool replaced (`remove_buffer` purges), a new buffer lands on the same address, then the
    /// old hold's release arrives. Matching by pointer alone would requeue the new tenant while
    /// its own hold is still out.
    #[test]
    fn hold_book_generation_outlives_an_address_reuse() {
        let mut b = HoldBook::default();
        let old = b.try_hold(0x1000, 8).unwrap();
        b.purge(0x1000); // `remove_buffer`: pool renegotiated away under the hold
        assert!(!b.complete(0x1000, old), "purged hold releases nothing");
        let new = b.try_hold(0x1000, 8).unwrap(); // new pool, same address
        assert!(
            !b.complete(0x1000, old),
            "the OLD hold cannot release the NEW tenant"
        );
        assert!(b.contains(0x1000), "new tenant still withheld");
        assert!(b.complete(0x1000, new), "its own hold releases it");
    }

    /// A buffer already out keeps one requeue duty: the re-hold moves it to the new
    /// generation instead of adding a second entry, and it does not spend the cap.
    #[test]
    fn hold_book_rehold_spends_no_second_slot() {
        let mut b = HoldBook::default();
        b.try_hold(0x1000, HOLD_POOL_RESERVE + 1).unwrap();
        assert!(
            b.try_hold(0x2000, HOLD_POOL_RESERVE + 1).is_none(),
            "cap is one"
        );
        assert!(
            b.try_hold(0x1000, HOLD_POOL_RESERVE + 1).is_some(),
            "re-hold within cap"
        );
        assert_eq!(b.out.len(), 1);
    }

    /// KWin's pool of 4: the host's frame and the one it just replaced are both out, the
    /// replaced one's hold already dropped on the encode thread. Before the loop services that
    /// wake the book still counts it, and the arrival would take the CPU copy. `try_defer`
    /// drains the parked release first, so the arrival holds — and the wake callback that runs
    /// later finds nothing left to requeue.
    #[test]
    fn an_arrival_drains_a_release_the_loop_has_not_serviced() {
        use super::{BufferHold, DeferredRequeue};
        let pool = crate::KWIN_POOL_MAX as u32;
        let (wake, _rx) = pipewire::channel::channel::<()>();
        let defer = std::sync::Arc::new(DeferredRequeue {
            book: std::sync::Mutex::new(HoldBook::default()),
            pending: std::sync::Mutex::new(Vec::new()),
            wake,
            logged_active: std::sync::atomic::AtomicBool::new(false),
            logged_shallow: std::sync::atomic::AtomicBool::new(false),
            sync: None,
        });
        let hold = |buf: usize| {
            let generation = defer.book.lock().unwrap().try_hold(buf, pool).unwrap();
            BufferHold {
                defer: defer.clone(),
                buf,
                generation,
            }
        };
        let replaced = hold(0x1000);
        let _current = hold(0x2000);
        drop(replaced); // the encode thread let it go; the loop has not run its wake yet
        assert!(
            defer.book.lock().unwrap().try_hold(0x3000, pool).is_none(),
            "the book still counts the dropped hold"
        );
        let mut requeued = Vec::new();
        assert_eq!(defer.drain_with(|buf| requeued.push(buf)), 1);
        assert_eq!(requeued, vec![0x1000], "the dropped hold's buffer rejoins");
        assert!(
            defer.book.lock().unwrap().try_hold(0x3000, pool).is_some(),
            "the arrival now holds"
        );
        assert_eq!(
            defer.drain_with(|_| panic!("nothing left to requeue")),
            0,
            "the late wake is a no-op"
        );
    }
}
