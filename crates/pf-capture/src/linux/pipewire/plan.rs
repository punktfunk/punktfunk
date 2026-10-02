//! The zero-copy negotiation, resolved once per pipeline, and the CUDA import both
//! threads grade alike. Pure except [`gpu_import`].

use crate::PixelFormat;
use pf_zerocopy::ImportKind;
use std::sync::atomic::Ordering;

/// Facts the zero-copy decision needs, sampled at one instant so the decision is a pure
/// function — shared by the PipeWire thread and `spawn_pipewire` (see [`NegotiationPlan`]).
#[derive(Debug, Clone, Copy)]
pub(in crate::linux) struct NegotiationInputs {
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
pub(in crate::linux) struct ImportPolicy {
    /// `PUNKTFUNK_NV12`: emit NV12 for native NVENC YUV. Off leaves packed RGB.
    pub nv12: bool,
    /// Planar YUV444 on tiled EGL. Wins over `nv12` — 4:4:4 must not subsample.
    pub yuv444: bool,
}

/// Per-stream import memory: the LINEAR NV12 latch and the tiled failure streak.
#[derive(Debug, Default)]
pub(in crate::linux) struct ImportState {
    /// LINEAR NV12 compute CSC failed once: RGB for the rest of this stream.
    pub linear_nv12_failed: bool,
    /// Consecutive tiled-import failures; reset on success. See [`IMPORT_FAIL_POISON`].
    pub fail_streak: u32,
}

impl ImportPolicy {
    /// A 10-bit SDR session keeps packed RGB whatever `PUNKTFUNK_NV12` asks: NVENC widens 8-bit
    /// to 10-bit only from packed RGB and refuses a planar 8-bit surface in a 10-bit session.
    pub(super) fn for_ten_bit_sdr(mut self, ten_bit_sdr: bool) -> Self {
        if ten_bit_sdr {
            self.nv12 = false;
        }
        self
    }
}

/// [`gpu_import`]'s verdict. `ImporterLost` is a LINEAR failure: the caller retires the
/// importer and the stream continues on the CPU path.
pub(in crate::linux) enum ImportOutcome {
    Frame(pf_zerocopy::DeviceBuffer, PixelFormat),
    Dropped,
    ImporterLost,
}

/// Zero-copy negotiation, resolved once and consumed by the PipeWire thread and `spawn_pipewire`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::linux) struct NegotiationPlan {
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
/// 3. Producer-native planar only on a `native_nv12_session` under a raw lane (VAAPI's
///    passthrough or NVENC's): NV12 for SDR, P010 for HDR. The CUDA importer expects packed
///    RGB, so a tripped raw latch withdraws the planar offer.
/// 4. Raw passthrough is off once its latch has fired.
pub(in crate::linux) fn negotiation_plan(i: NegotiationInputs) -> NegotiationPlan {
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
    let nvenc_raw = build_importer && i.nvenc_raw && !i.force_shm && !i.raw_dmabuf_import_disabled;
    // NVENC's raw lane copies a producer NV12 or P010 into its slot.
    let planar_lane = (i.backend_is_vaapi && vaapi_passthrough) || nvenc_raw;
    let native_planar = i.native_nv12_env_on
        && i.native_nv12_session
        && planar_lane
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
        nvenc_raw,
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

/// Per-session tally of raw-passthrough fall-throughs, one log line per distinct reason.
///
/// `.process` runs per frame; per-reason so a transient `NoFormat` at open does not spend
/// the budget a persistent `NotDmabuf` needs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PassthroughFallbacks {
    pub(super) frames: u64,
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
pub(super) enum PassthroughFallbackAction {
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
pub(super) fn passthrough_fallback_action(
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

/// Tiled-import failures (worker alive) before the stream is poisoned for rebuild.
/// Never fall through to CPU mmap: de-padding tiled bytes as linear is a scrambled image.
const IMPORT_FAIL_POISON: u32 = 3;

/// The import a frame takes. 4:4:4 needs the tiled EGL convert and wins over NV12; 10-bit
/// keeps packed RGB; a LINEAR NV12 CSC that failed once stays RGB for the stream.
fn import_kind(
    policy: ImportPolicy,
    tiled: bool,
    ten_bit: bool,
    linear_nv12_failed: bool,
) -> ImportKind {
    let yuv444 = policy.yuv444 && !ten_bit;
    let nv12 = policy.nv12 && !policy.yuv444 && !ten_bit;
    match (tiled, yuv444, nv12) {
        (true, true, _) => ImportKind::Tiled444,
        (true, false, true) => ImportKind::TiledNv12,
        (true, false, false) => ImportKind::Tiled,
        (false, _, true) if !linear_nv12_failed => ImportKind::LinearNv12,
        (false, _, _) => ImportKind::Linear,
    }
}

/// One dmabuf → CUDA import: tiled through EGL, LINEAR through the Vulkan bridge, NV12 or
/// YUV444 where the policy asks. Failures are graded here so both threads act alike: a tiled
/// failure drops the frame and poisons the stream after [`IMPORT_FAIL_POISON`] (or at once
/// when the worker died); a LINEAR failure retires the importer.
#[allow(clippy::too_many_arguments)]
pub(in crate::linux) fn gpu_import(
    importer: &mut pf_zerocopy::Importer,
    policy: ImportPolicy,
    state: &mut ImportState,
    signals: &crate::linux::CaptureSignals,
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
    let mut kind = import_kind(
        policy,
        modifier.is_some(),
        ten_bit,
        state.linear_nv12_failed,
    );
    let mut imported = importer.import(kind, &plane, w, h, fourcc, modifier);
    if let (ImportKind::LinearNv12, Err(e)) = (kind, &imported) {
        state.linear_nv12_failed = true;
        tracing::warn!(error = %format!("{e:#}"),
            "LINEAR NV12 compute CSC failed — RGB for the rest of this \
             stream (NVENC does the CSC internally)");
        kind = ImportKind::Linear;
        imported = importer.import(kind, &plane, w, h, fourcc, modifier);
    }
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
                    ?kind,
                    "zero-copy: dmabuf imported to CUDA (no CPU copy)"
                );
            }
            let out = match kind.layout() {
                pf_zerocopy::cuda::PlaneLayout::Yuv444 => PixelFormat::Yuv444,
                pf_zerocopy::cuda::PlaneLayout::Nv12 => PixelFormat::Nv12,
                pf_zerocopy::cuda::PlaneLayout::Packed32 => fmt,
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

#[cfg(test)]
mod tests {
    use super::super::hold::{holds_possible, SHALLOW_POOL};
    use super::{
        consumer_kind, negotiation_plan, passthrough_fallback_action, resolved_capture_arm,
        CaptureArm, ConsumerKind, ImportPolicy, NegotiationInputs, PassthroughFallback,
        PassthroughFallbackAction, PassthroughFallbacks,
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

    /// 4:4:4 needs a tiled source and beats NV12; 10-bit keeps packed RGB; a failed LINEAR
    /// NV12 CSC stays RGB.
    #[test]
    fn the_import_kind_follows_the_policy_and_the_source() {
        use pf_zerocopy::ImportKind as K;
        let rgb = ImportPolicy::default();
        let nv12 = ImportPolicy {
            nv12: true,
            yuv444: false,
        };
        let yuv444 = ImportPolicy {
            nv12: true,
            yuv444: true,
        };
        assert_eq!(super::import_kind(rgb, true, false, false), K::Tiled);
        assert_eq!(super::import_kind(rgb, false, false, false), K::Linear);
        assert_eq!(super::import_kind(nv12, true, false, false), K::TiledNv12);
        assert_eq!(super::import_kind(nv12, false, false, false), K::LinearNv12);
        assert_eq!(super::import_kind(nv12, false, false, true), K::Linear);
        assert_eq!(super::import_kind(nv12, true, true, false), K::Tiled);
        assert_eq!(super::import_kind(yuv444, true, false, false), K::Tiled444);
        assert_eq!(super::import_kind(yuv444, false, false, false), K::Linear);
        assert_eq!(super::import_kind(yuv444, true, true, false), K::Tiled);
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
        assert!(holds_possible(true, SHALLOW_POOL + 1));
        assert!(!holds_possible(true, SHALLOW_POOL));
        assert!(!holds_possible(false, 8));
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
        // NVENC's raw lane takes a producer NV12 or P010 (copied into its slot), and a tripped
        // raw latch or a lane-less session withdraws it: the importer reads RGB only.
        let nvenc_native = NegotiationInputs {
            nvenc_raw: true,
            native_nv12_session: true,
            ..nvenc()
        };
        assert!(negotiation_plan(nvenc_native).prefer_native_nv12);
        let hdr = negotiation_plan(NegotiationInputs {
            want_hdr: true,
            ..nvenc_native
        });
        assert!(hdr.prefer_native_p010 && !hdr.prefer_native_nv12);
        for (why, inputs) in [
            (
                "HDR raw latch",
                NegotiationInputs {
                    want_hdr: true,
                    raw_dmabuf_import_disabled: true,
                    ..nvenc_native
                },
            ),
            (
                "4:4:4",
                NegotiationInputs {
                    want_444: true,
                    ..nvenc_native
                },
            ),
            (
                "raw latch",
                NegotiationInputs {
                    raw_dmabuf_import_disabled: true,
                    ..nvenc_native
                },
            ),
            (
                "no raw lane",
                NegotiationInputs {
                    nvenc_raw: false,
                    ..nvenc_native
                },
            ),
            (
                "SHM",
                NegotiationInputs {
                    force_shm: true,
                    ..nvenc_native
                },
            ),
        ] {
            let p = negotiation_plan(inputs);
            assert!(!p.prefer_native_nv12 && !p.prefer_native_p010, "{why}");
        }
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
}
