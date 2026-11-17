//! The zero-copy negotiation, resolved once per pipeline, and the CUDA import both
//! threads grade alike. Pure except [`gpu_import`].

use crate::linux::delivery::import_once;
pub(in crate::linux) use crate::linux::delivery::{ImportOutcome, PassthroughFallback};
use crate::PixelFormat;
use pf_zerocopy::{ImportKind, Lane, Refusal};
use std::sync::atomic::Ordering;

/// Facts the zero-copy decision needs, sampled at one instant so the decision is a pure
/// function — shared by the PipeWire thread and `spawn_pipewire` (see [`NegotiationPlan`]).
#[derive(Debug, Clone, Copy)]
pub(in crate::linux) struct NegotiationInputs {
    pub zerocopy: bool,
    /// `PUNKTFUNK_FORCE_SHM` — race-free download path.
    pub force_shm: bool,
    pub want_hdr: bool,
    /// The producer offers 10-bit SDR: a 10-bit SDR session takes its P010 where HDR does.
    pub sdr10_native: bool,
    pub want_444: bool,
    pub backend_is_vaapi: bool,
    pub pyrowave_session: bool,
    pub native_nv12_session: bool,
    /// What this capture identity has learned: lanes off, offers refused.
    pub ladder: pf_zerocopy::Ladder,
    pub native_nv12_knob: bool,
    /// Encoder can ingest packed 10-bit PQ CUDA. Only direct-SDK NVENC can.
    pub hdr_cuda_ok: bool,
    /// `PUNKTFUNK_NV12`: the CUDA import emits NV12 (tiled blit or LINEAR compute CSC).
    pub nv12_knob: bool,
    /// The NVENC encoder converts held dmabufs itself and its convert samples a tiled layout
    /// (`raw_lane_ok`).
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
    /// The 10-bit twin of [`prefer_native_nv12`](Self::prefer_native_nv12): gamescope's P010
    /// pod goes first, PQ for HDR or BT.709 for its own 10-bit SDR.
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
///    passthrough or NVENC's): NV12 at 8 bits, P010 for HDR and for a producer's own 10-bit
///    SDR. The CUDA importer expects packed RGB, so a raw lane gone off withdraws the planar
///    offer, and so does a planar frame the passthrough declined ([`Refusal::Planar`]).
/// 4. Raw passthrough is off once the ladder turns the raw lane off.
pub(in crate::linux) fn negotiation_plan(i: NegotiationInputs) -> NegotiationPlan {
    // Consumer imports raw dmabufs: VAAPI (libva + GPU CSC) or PyroWave (its Vulkan device).
    let raw_passthrough = i.backend_is_vaapi || i.pyrowave_session;
    let raw_off = i.ladder.off(Lane::Raw);
    let gpu_off = i.ladder.off(Lane::Gpu);
    // Skip under raw passthrough (payloads only NVENC consumes) and once the GPU lane is off
    // (an import that keeps failing loops; compositor that rejects our modifiers re-pays 10 s).
    // HDR through this importer is LINEAR, so it avoids the 8-bit de-tile blit. Exclude
    // it when the encoder cannot take packed 10-bit CUDA (a build without `nvenc`).
    let build_importer =
        i.zerocopy && !raw_passthrough && !gpu_off && (!i.want_hdr || i.hdr_cuda_ok);
    let vaapi_passthrough = i.zerocopy && !i.force_shm && raw_passthrough && !raw_off;
    let nvenc_raw = build_importer && i.nvenc_raw && !i.force_shm && i.ladder.nvenc_raw_ok();
    // NVENC's raw lane copies a producer NV12 or P010 into its slot.
    let planar_lane = (i.backend_is_vaapi && vaapi_passthrough) || nvenc_raw;
    let native_planar = i.native_nv12_knob
        && !i.ladder.refused(Refusal::Planar)
        && i.native_nv12_session
        && planar_lane
        && !i.pyrowave_session
        && !i.want_444;
    let ten_bit = i.want_hdr || i.sdr10_native;
    let prefer_native_nv12 = native_planar && !ten_bit;
    let prefer_native_p010 = native_planar && ten_bit;
    NegotiationPlan {
        build_importer,
        import_policy: ImportPolicy {
            nv12: i.nv12_knob,
            yuv444: i.want_444,
        },
        nvenc_raw,
        vaapi_passthrough,
        prefer_native_nv12,
        prefer_native_p010,
        force_shm: i.force_shm,
        raw_dmabuf_latched: i.zerocopy && !i.force_shm && raw_passthrough && raw_off,
        // Every `build_importer` term except the GPU lane, then that lane off.
        gpu_import_latched: i.zerocopy
            && !raw_passthrough
            && (!i.want_hdr || i.hdr_cuda_ok)
            && gpu_off,
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
/// because it is per-session and `backend_is_vaapi` follows the host-global backend
/// (`Resolved::zero_copy_is_vaapi`), which an AMD/Intel host sets for a PyroWave session too.
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
    /// A LINEAR NV12/P010 frame failed: refuse the planar offer for this identity and
    /// rebuild on packed RGB.
    DropPlanarAndRebuild,
}

/// The action for a passthrough break. A nonzero modifier is never de-padded: a
/// tiled buffer read as linear is a scrambled picture, so any failure on it
/// retires the tiled offer itself. The CPU de-pad reads one packed plane, so a
/// planar frame bound for it retires the planar offer instead.
pub(super) fn passthrough_fallback_action(
    reason: PassthroughFallback,
    modifier: u64,
    planar: bool,
) -> PassthroughFallbackAction {
    if modifier != 0 {
        PassthroughFallbackAction::DropTiledAndRebuild
    } else if !reason.falls_back_to_cpu() {
        PassthroughFallbackAction::Drop
    } else if planar {
        PassthroughFallbackAction::DropPlanarAndRebuild
    } else {
        PassthroughFallbackAction::Cpu
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

/// The PipeWire stream's dmabuf → CUDA import: tiled through EGL, LINEAR through the Vulkan
/// bridge, NV12 or YUV444 where the policy asks, each attempt through [`import_once`].
/// [`import_failure`] grades a failure, so the arrival and the consumer act alike.
#[allow(clippy::too_many_arguments)]
pub(in crate::linux) fn gpu_import(
    importer: &mut pf_zerocopy::Importer,
    policy: ImportPolicy,
    state: &mut ImportState,
    shared: &crate::linux::StreamShared,
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
    let ten_bit = fmt.is_rgb10();
    // The raw lane let go of a tiled 10-bit stream. Rebuild it on the LINEAR offer.
    if ten_bit && modifier.is_some() {
        if shared.health.refuse(Refusal::HdrTiled) {
            tracing::warn!(
                "tiled 10-bit dmabuf reached the CUDA import — capture rebuilds on LINEAR"
            );
        }
        shared.mark_broken();
        return ImportOutcome::Dropped;
    }
    let mut kind = import_kind(
        policy,
        modifier.is_some(),
        ten_bit,
        state.linear_nv12_failed,
    );
    let mut imported = import_once(importer, kind, &plane, w, h, fourcc, modifier, shared);
    if let (ImportKind::LinearNv12, Err(e)) = (kind, &imported) {
        state.linear_nv12_failed = true;
        tracing::warn!(error = %format!("{e:#}"),
            "LINEAR NV12 compute CSC failed — RGB for the rest of this \
             stream (NVENC does the CSC internally)");
        kind = ImportKind::Linear;
        imported = import_once(importer, kind, &plane, w, h, fourcc, modifier, shared);
    }
    match imported {
        Ok(devbuf) => {
            state.fail_streak = 0;
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
            if modifier.is_some() {
                state.fail_streak += 1;
            }
            let (next, strike) = import_failure(modifier.is_some(), dead, state.fail_streak);
            if strike {
                shared.health.note_gpu_import_lost();
            }
            match next {
                ImportFailure::RetireImporter => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "LINEAR dmabuf GPU import failed — falling back to the CPU copy path");
                    ImportOutcome::ImporterLost
                }
                ImportFailure::LoseStream => {
                    tracing::error!(error = %format!("{e:#}"), dead,
                        "tiled GPU import lost — failing this capture for rebuild");
                    shared.mark_broken();
                    ImportOutcome::Dropped
                }
                ImportFailure::DropFrame => {
                    tracing::warn!(error = %format!("{e:#}"),
                        streak = state.fail_streak,
                        "tiled dmabuf GPU import failed — frame dropped");
                    ImportOutcome::Dropped
                }
            }
        }
    }
}

/// What a failed CUDA import does to its stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportFailure {
    /// LINEAR: the importer retires and the stream continues on the CPU path.
    RetireImporter,
    /// A tiled frame is dropped; the next one may import.
    DropFrame,
    /// The worker died, or tiled imports keep failing: the stream rebuilds.
    LoseStream,
}

/// A failed CUDA import's verdict, and whether the GPU lane takes a strike. `streak` counts
/// the stream's consecutive tiled failures, this one included. A tiled stream is lost after
/// [`IMPORT_FAIL_POISON`], or at once when the worker died. A dead worker strikes on each
/// failure, failing tiled imports once per stream: the rebuild offers the same modifiers.
fn import_failure(tiled: bool, dead: bool, streak: u32) -> (ImportFailure, bool) {
    let next = if !tiled {
        ImportFailure::RetireImporter
    } else if dead || streak >= IMPORT_FAIL_POISON {
        ImportFailure::LoseStream
    } else {
        ImportFailure::DropFrame
    };
    (next, dead || (tiled && streak == IMPORT_FAIL_POISON))
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{holds_possible, SHALLOW_POOL};
    use super::{
        consumer_kind, negotiation_plan, passthrough_fallback_action, resolved_capture_arm,
        CaptureArm, ConsumerKind, ImportPolicy, NegotiationInputs, PassthroughFallback,
        PassthroughFallbackAction, PassthroughFallbacks,
    };
    use pf_zerocopy::{step, Evidence, Ladder, Lane, Refusal};

    /// More of `e` than any budget takes: that cause, spent.
    fn spent(e: Evidence) -> Ladder {
        [e; 8].iter().fold(Ladder::default(), |l, e| step(l, *e))
    }

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
                ladder: spent(Evidence::ImportFailed(Lane::Raw)),
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
    pub(super) fn nvenc() -> NegotiationInputs {
        NegotiationInputs {
            zerocopy: true,
            force_shm: false,
            want_hdr: false,
            sdr10_native: false,
            want_444: false,
            backend_is_vaapi: false,
            pyrowave_session: false,
            native_nv12_session: false,
            ladder: Ladder::default(),
            native_nv12_knob: true,
            hdr_cuda_ok: true,
            nv12_knob: true,
            nvenc_raw: false,
        }
    }

    /// A gamescope-style VAAPI session that CAN take producer-native NV12.
    pub(super) fn vaapi_native_nv12() -> NegotiationInputs {
        NegotiationInputs {
            backend_is_vaapi: true,
            native_nv12_session: true,
            ..nvenc()
        }
    }

    /// A producer's own 10-bit SDR takes P010 where HDR does, on both planar lanes; 4:4:4
    /// still refuses it.
    #[test]
    fn producer_sdr10_takes_p010() {
        let nvenc_native = NegotiationInputs {
            nvenc_raw: true,
            native_nv12_session: true,
            ..nvenc()
        };
        for base in [vaapi_native_nv12(), nvenc_native] {
            let p = negotiation_plan(NegotiationInputs {
                sdr10_native: true,
                ..base
            });
            assert!(p.prefer_native_p010 && !p.prefer_native_nv12);
        }
        assert!(
            !negotiation_plan(NegotiationInputs {
                sdr10_native: true,
                want_444: true,
                ..vaapi_native_nv12()
            })
            .prefer_native_p010
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
        for want_hdr in [false, true] {
            let p = negotiation_plan(NegotiationInputs {
                ladder: spent(Evidence::Refused(Refusal::Planar)),
                want_hdr,
                ..vaapi_native_nv12()
            });
            assert!(!p.prefer_native_nv12 && !p.prefer_native_p010);
            assert!(
                p.vaapi_passthrough,
                "a planar refusal keeps packed passthrough"
            );
        }
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
                    ladder: spent(Evidence::ImportFailed(Lane::Raw)),
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
                    ladder: spent(Evidence::ImportFailed(Lane::Raw)),
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
            ladder: spent(Evidence::ImportFailed(Lane::Raw)),
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
                    ladder: spent(Evidence::ImportFailed(Lane::Raw)),
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
            ladder: spent(Evidence::OfferTimedOut(Lane::Gpu)),
            ..nvenc()
        });
        assert!(!p.build_importer, "latched offer must not be re-made");
        assert!(p.gpu_import_latched, "the downgrade must be diagnosable");
        let p = negotiation_plan(NegotiationInputs {
            ladder: spent(Evidence::OfferTimedOut(Lane::Gpu)),
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
            ladder: spent(Evidence::ImportFailed(Lane::Gpu)),
            ..nvenc()
        });
        assert!(!p.build_importer);
        assert!(p.gpu_import_latched);
        // HDR takes the same importer (LINEAR/Vulkan-bridge), so the latch costs it zero-copy.
        assert!(
            negotiation_plan(NegotiationInputs {
                ladder: spent(Evidence::ImportFailed(Lane::Gpu)),
                want_hdr: true,
                ..nvenc()
            })
            .gpu_import_latched
        );
        // Not reported for a raw passthrough that would never have built an importer.
        assert!(
            !negotiation_plan(NegotiationInputs {
                ladder: spent(Evidence::ImportFailed(Lane::Gpu)),
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

    /// PyroWave wins even when the host's backend sets `backend_is_vaapi` (AMD/Intel). The
    /// other order reports the session as somebody else's backend.
    #[test]
    fn pyrowave_outranks_the_host_global_backend_pref() {
        assert_eq!(consumer_kind(true, true, true), ConsumerKind::PyroWave);
        // NVIDIA/auto host (`backend_is_vaapi` false) is still PyroWave.
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

    /// Neither a tiled nor a planar buffer can take the CPU de-pad: a failure on a nonzero
    /// modifier rebuilds on LINEAR, and a planar LINEAR one bound for the CPU rebuilds on
    /// packed RGB. Packed LINEAR keeps the per-reason split.
    #[test]
    fn tiled_and_planar_passthrough_failures_rebuild() {
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
                for planar in [false, true] {
                    assert_eq!(
                        passthrough_fallback_action(reason, modifier, planar),
                        PassthroughFallbackAction::DropTiledAndRebuild,
                        "{reason:?} on modifier {modifier:#x} must refuse the tiled offer"
                    );
                }
            }
            let linear = passthrough_fallback_action(reason, 0, false);
            let planar = passthrough_fallback_action(reason, 0, true);
            let (want, want_planar) = match reason {
                PassthroughFallback::NoFormat => (
                    PassthroughFallbackAction::Drop,
                    PassthroughFallbackAction::Drop,
                ),
                _ => (
                    PassthroughFallbackAction::Cpu,
                    PassthroughFallbackAction::DropPlanarAndRebuild,
                ),
            };
            assert_eq!(linear, want, "{reason:?} on LINEAR");
            assert_eq!(planar, want_planar, "{reason:?} on planar LINEAR");
        }
    }

    /// The table's rows reached through the lanes: a frame the passthrough declines takes the
    /// CPU copy only where the table says `Cpu`.
    #[test]
    fn the_fallback_table_decides_the_lane_after_the_passthrough() {
        use crate::linux::delivery::tests::{buffer, memfd, Fake};
        use crate::linux::delivery::{deliver, Lanes, Memory};
        let lanes = Lanes {
            passthrough: true,
            gpu: None,
            cpu: true,
        };
        let fd = memfd(&[0; 64]);
        type Row = (
            Memory,
            bool,
            u64,
            bool,
            PassthroughFallback,
            &'static [&'static str],
        );
        #[rustfmt::skip]
        let rows: [Row; 5] = [
            (Memory::MemFd,  true,  0, true,  PassthroughFallback::NotDmabuf, &["cpu"]),
            (Memory::MemFd,  true,  1, true,  PassthroughFallback::NotDmabuf, &[]),
            (Memory::DmaBuf, false, 0, true,  PassthroughFallback::NoFormat,  &[]),
            (Memory::DmaBuf, true,  0, false, PassthroughFallback::NoHold,    &["cpu"]),
            (Memory::DmaBuf, true,  1, false, PassthroughFallback::NoHold,    &[]),
        ];
        for (memory, negotiated, modifier, holds, reason, published) in rows {
            let mut buf = buffer(&fd, memory, (4, 4), 16);
            buf.modifier = modifier;
            buf.format = buf.format.filter(|_| negotiated);
            let mut fake = Fake {
                holds,
                next_lane: Box::new(move |r| {
                    let action = passthrough_fallback_action(r, modifier, false);
                    action == PassthroughFallbackAction::Cpu
                }),
                ..Fake::new()
            };
            deliver(lanes, buf, &mut fake);
            assert_eq!(fake.reasons, [reason]);
            assert_eq!(fake.lanes(), published, "{reason:?} on modifier {modifier}");
        }
    }
}

/// The ladder as the plan sees it: the evidence a capture gathers, then the rung it builds.
#[cfg(test)]
mod ladder_rows {
    use super::super::capturer::{convicted_lane, TimeoutOffer, TimeoutVerdict};
    use super::tests::{nvenc, vaapi_native_nv12};
    use super::{
        import_failure, negotiation_plan, resolved_capture_arm, CaptureArm, ImportFailure,
        NegotiationInputs,
    };
    use pf_zerocopy::{step, Evidence, Lane, Refusal};

    /// What a capture sees: evidence for the ladder, or a first-frame expiry it judges first.
    #[derive(Clone, Copy)]
    enum Seen {
        Ev(Evidence),
        Expiry(TimeoutOffer, TimeoutVerdict),
        /// A failed CUDA import: tiled, worker dead, the stream's tiled streak.
        Failed(bool, bool, u32),
    }

    /// The rung a build takes after `seen`, and whether it offers the producer's planar formats.
    fn rung_after(base: NegotiationInputs, seen: &[Seen]) -> (CaptureArm, bool) {
        let ladder = seen.iter().fold(base.ladder, |l, s| match *s {
            Seen::Ev(e) => step(l, e),
            Seen::Expiry(offer, verdict) => match convicted_lane(offer, verdict) {
                Some(lane) => step(l, Evidence::OfferTimedOut(lane)),
                None => l,
            },
            Seen::Failed(tiled, dead, streak) => match import_failure(tiled, dead, streak) {
                (_, true) => step(l, Evidence::ImportFailed(Lane::Gpu)),
                (_, false) => l,
            },
        });
        let p = negotiation_plan(NegotiationInputs { ladder, ..base });
        let have_importer = p.build_importer;
        let arm = resolved_capture_arm(&p, have_importer, p.want_dmabuf(have_importer, &[0]));
        (arm, p.prefer_native_nv12 || p.prefer_native_p010)
    }

    /// A dead worker strikes on each failure; tiled imports on a live one strike once per stream,
    /// when the stream is lost; a LINEAR failure retires the importer.
    #[test]
    fn a_lost_import_stream_strikes_the_gpu_lane_once() {
        use ImportFailure::{DropFrame, LoseStream, RetireImporter};
        let rows = [
            ((false, false, 0), (RetireImporter, false)),
            ((false, true, 0), (RetireImporter, true)),
            ((true, false, 1), (DropFrame, false)),
            ((true, false, 2), (DropFrame, false)),
            ((true, false, 3), (LoseStream, true)),
            ((true, false, 4), (LoseStream, false)),
            ((true, true, 1), (LoseStream, true)),
            ((true, true, 3), (LoseStream, true)),
        ];
        for ((tiled, dead, streak), want) in rows {
            let got = import_failure(tiled, dead, streak);
            assert_eq!(got, want, "tiled={tiled} dead={dead} streak={streak}");
        }
    }

    /// One row per fix in this ladder's history, then a lane that recovers and one that does not.
    #[test]
    fn the_ladder_rows() {
        use CaptureArm::{Cpu, CudaImport, DmabufPassthrough, DmabufToEncoder};
        use Evidence::{ImportFailed, Imported, OfferTaken, Refused};
        use Seen::{Ev, Expiry, Failed};
        use TimeoutOffer::{GpuDmabuf, Hdr, RawDmabuf};
        use TimeoutVerdict::{Conclusive, Provisional};
        let vaapi = NegotiationInputs {
            backend_is_vaapi: true,
            ..nvenc()
        };
        let nvenc_raw = NegotiationInputs {
            nvenc_raw: true,
            ..nvenc()
        };
        // One capture whose tiled imports never succeed on a live worker.
        let lost_stream = [
            Failed(true, false, 1),
            Failed(true, false, 2),
            Failed(true, false, 3),
        ];
        type Row = (
            &'static str,
            NegotiationInputs,
            Vec<Seen>,
            (CaptureArm, bool),
        );
        let rows: Vec<Row> = vec![
            (
                "b815e00a8: one raw offer timeout retries the offer",
                vaapi,
                vec![Expiry(RawDmabuf, Conclusive)],
                (DmabufPassthrough, false),
            ),
            (
                "b815e00a8: the second one goes to the CPU",
                vaapi,
                vec![Expiry(RawDmabuf, Conclusive); 2],
                (Cpu, false),
            ),
            (
                "b815e00a8: a taken offer credits the retry",
                vaapi,
                [Expiry(RawDmabuf, Conclusive), Ev(OfferTaken(Lane::Raw))].repeat(5),
                (DmabufPassthrough, false),
            ),
            (
                "022ede651: a short first attempt convicts no raw offer",
                vaapi,
                vec![Expiry(RawDmabuf, Provisional); 5],
                (DmabufPassthrough, false),
            ),
            (
                "022ede651: nor a GPU offer",
                nvenc(),
                vec![Expiry(GpuDmabuf, Provisional); 5],
                (CudaImport, false),
            ),
            (
                "one full GPU offer timeout goes to the CPU",
                nvenc(),
                vec![Expiry(GpuDmabuf, Conclusive)],
                (Cpu, false),
            ),
            (
                "an HDR expiry is the HDR latch's, not a lane's",
                nvenc(),
                vec![Expiry(Hdr, Conclusive); 3],
                (CudaImport, false),
            ),
            (
                "71bd171ac: a tiled refusal moves NVENC off its raw lane",
                nvenc_raw,
                vec![Ev(Refused(Refusal::Tiled))],
                (CudaImport, false),
            ),
            (
                "d1797bc4f: a planar failure keeps the passthrough on packed RGB",
                vaapi_native_nv12(),
                vec![Ev(Refused(Refusal::Planar))],
                (DmabufPassthrough, false),
            ),
            (
                "0d888d788: tiled imports that keep failing end on the CPU, not in a loop",
                nvenc(),
                lost_stream.repeat(3),
                (Cpu, false),
            ),
            (
                "a GPU lane that fails once recovers",
                nvenc_raw,
                vec![
                    Ev(ImportFailed(Lane::Gpu)),
                    Ev(Imported(Lane::Gpu)),
                    Ev(ImportFailed(Lane::Gpu)),
                    Ev(ImportFailed(Lane::Gpu)),
                ],
                (DmabufToEncoder, false),
            ),
            (
                "a raw lane that fails for good",
                vaapi_native_nv12(),
                vec![
                    Ev(ImportFailed(Lane::Raw)),
                    Ev(ImportFailed(Lane::Raw)),
                    Ev(ImportFailed(Lane::Raw)),
                    Ev(Imported(Lane::Raw)),
                    Ev(OfferTaken(Lane::Raw)),
                ],
                (Cpu, false),
            ),
        ];
        for (why, base, seen, want) in rows {
            assert_eq!(rung_after(base, &seen), want, "{why}");
        }
        // The rows start from the best rung: what each fix demotes from.
        assert_eq!(
            rung_after(vaapi_native_nv12(), &[]),
            (DmabufPassthrough, true)
        );
        assert_eq!(rung_after(nvenc_raw, &[]), (DmabufToEncoder, false));
        assert_eq!(rung_after(nvenc(), &[]), (CudaImport, false));
    }
}
