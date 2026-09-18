//! Shared NVENC session config for the Windows D3D11 and Linux CUDA backends.
//!
//! Owns codec GUIDs, slice / sub-frame / split arbitration, the process-lifetime
//! bitrate-ceiling cache, range-RFI policy, and the low-latency `NV_ENC_CONFIG`
//! author. Entry-table load, device bind, surface register, and Windows async
//! retrieve stay in those backends. Sibling of [`super::nvenc_status`].
//!
//! Union reads, borrows, and bitfield setters sit in their own `unsafe` blocks
//! and name the codec arm they rely on. Plain union-arm writes are safe by
//! language rule; the hazard is a mismatched read.
//!
//! Pin: `split_subframe_tests`, `range_policy_tests`, `arbiter_tests`.

// Every union READ, borrow, or bitfield setter is its own `unsafe` and names
// the codec arm. Plain arm writes are safe; the hazard is a mismatched read.
#![deny(clippy::multiple_unsafe_ops_per_block)]

use super::Codec;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// `NVENCSTATUS` → `Result` without the SDK `safe` module (these backends must
/// not pull it in). Callers fold the raw status through [`super::nvenc_status`].
pub trait NvStatusExt {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS>;
}
impl NvStatusExt for nv::NVENCSTATUS {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS> {
        match self {
            nv::NVENCSTATUS::NV_ENC_SUCCESS => Ok(()),
            err => Err(err),
        }
    }
}

/// NVENC codec GUID. PyroWave never opens this backend.
pub fn codec_guid(codec: Codec) -> nv::GUID {
    match codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
        Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
    }
}

/// H.264/HEVC slice count: `PUNKTFUNK_NVENC_SLICES` (1..=32; 1 = single-slice
/// escape) else `default_slices`. AV1 is always 1 (tiles, not slices). Shared by
/// [`apply_low_latency_config`] and the Linux chunked-poll arm so they cannot
/// disagree. A client that never advertised `VIDEO_CAP_MULTI_SLICE` stays at 1.
pub fn resolve_slices(codec: Codec, default_slices: u32) -> u32 {
    if !matches!(codec, Codec::H264 | Codec::H265) {
        return 1;
    }
    match crate::knobs::get().nvenc_slices {
        0 => default_slices,
        n => u32::from(n),
    }
}

/// Sub-frame readback tri-state (`enableSubFrameWrite` + `reportSliceOffsets`,
/// sync sessions only — see [`build_init_params`]): `PUNKTFUNK_NVENC_SUBFRAME`
/// `0` never, `1` force, unset = `default_on` (the GPU's `SUBFRAME_READBACK`
/// cap on both backends).
pub fn resolve_subframe(default_on: bool) -> bool {
    match crate::knobs::get().nvenc_subframe {
        1 => false,
        2 => true,
        _ => default_on,
    }
}

/// True when `PUNKTFUNK_NVENC_SUBFRAME=1`. Latch once next to the resolved
/// subframe flag — a re-read at reconfigure would diverge from open.
#[cfg(any(target_os = "linux", windows))]
pub fn subframe_env_forced() -> bool {
    crate::knobs::get().nvenc_subframe == 2
}

/// Split-encode × sub-frame arbitration (`nvEncodeAPI.h` `splitEncodeMode`).
///
/// * H.264: split is not applicable — hard-DISABLE so config, cache key, log,
///   and rejection-retry stay truthful.
/// * HEVC: split is unsupported with subframe. Forced split (TWO/THREE/
///   AUTO_FORCED) wins; plain AUTO leaves both bits as given so the driver
///   arbitrates. Do not rewrite AUTO to DISABLE: the ceiling-cache key must
///   record what was passed.
/// * AV1: split passes through; subframe is always off. [`resolve_slices`]
///   returns 1, so the chunked reader never arms; arming the writer would
///   publish tiles and then `lock_bitstream` would return only the first.
///   `poll_chunk` cuts on contiguous Annex-B; AV1 OBUs are not that.
///
/// Returns `(split_mode, subframe)` actually configured. Store both: without
/// `reportSliceOffsets`, `poll_chunk` busy-polls (`numSlices` stays 0).
pub fn resolve_split_subframe(
    codec: Codec,
    split_mode: u32,
    subframe: bool,
    subframe_forced: bool,
) -> (u32, bool) {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    if codec == Codec::H264 {
        return (M::NV_ENC_SPLIT_DISABLE_MODE as u32, subframe);
    }
    // AV1: disarm subframe, keep split. The reader cannot arm (`resolve_slices` = 1).
    if codec == Codec::Av1 && subframe {
        if subframe_forced {
            tracing::warn!(
                split_mode,
                "PUNKTFUNK_NVENC_SUBFRAME=1 cannot be honoured on AV1 — its sub-frame units are \
                 TILES and the chunked reader cuts on Annex-B slice boundaries, so arming the \
                 writer would ship only the first tile of every frame; sub-frame readback \
                 disabled for this session (split encode is unaffected)"
            );
        } else {
            tracing::debug!(
                split_mode,
                "NVENC: sub-frame readback disarmed on AV1 (tiles, not slices — nothing consumes \
                 the chunks); split encode is unaffected"
            );
        }
        return (split_mode, false);
    }
    let split_forced = split_mode == M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        || split_mode == M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32
        || split_mode == M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32;
    if codec == Codec::H265 && split_forced && subframe {
        if subframe_forced {
            tracing::warn!(
                split_mode,
                "HEVC forced split-encode and PUNKTFUNK_NVENC_SUBFRAME=1 are mutually \
                 unsupported (nvEncodeAPI.h) — sub-frame readback disabled for this session; \
                 set PUNKTFUNK_SPLIT_ENCODE=0 to choose sub-frame instead"
            );
        } else {
            tracing::info!(
                split_mode,
                "HEVC forced split-encode supersedes default-on sub-frame readback (mutually \
                 unsupported per nvEncodeAPI.h; split is the 4K120 throughput lever) — set \
                 PUNKTFUNK_SPLIT_ENCODE=0 to choose sub-frame instead"
            );
        }
        return (split_mode, false);
    }
    // HEVC AUTO + subframe: the driver cannot split, but leave AUTO as passed so
    // the ceiling-cache key matches the session.
    if codec == Codec::H265 && subframe && split_mode == M::NV_ENC_SPLIT_AUTO_MODE as u32 {
        tracing::debug!(
            "NVENC: split-encode AUTO with sub-frame readback on — the driver cannot split HEVC \
             in this combination, so this session runs SINGLE-ENGINE (measured). Set \
             PUNKTFUNK_NVENC_SUBFRAME=0 to trade sub-frame for a real split."
        );
    }
    (split_mode, subframe)
}

#[cfg(test)]
mod split_subframe_tests {
    use super::{resolve_slices, resolve_split_subframe, Codec};
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_SPLIT_ENCODE_MODE as M;

    const AUTO: u32 = M::NV_ENC_SPLIT_AUTO_MODE as u32;
    const TWO: u32 = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;
    const AUTO_F: u32 = M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32;
    const DISABLE: u32 = M::NV_ENC_SPLIT_DISABLE_MODE as u32;

    /// Plain AUTO + default-on subframe must pass through. Keying on `!= DISABLE`
    /// would disarm subframe on every default HEVC session (AUTO == 0 is the
    /// resolver fallthrough).
    #[test]
    fn hevc_auto_keeps_subframe() {
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO, true, false),
            (AUTO, true)
        );
    }

    #[test]
    fn hevc_forced_split_drops_subframe() {
        assert_eq!(
            resolve_split_subframe(Codec::H265, TWO, true, false),
            (TWO, false)
        );
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO_F, true, true),
            (AUTO_F, false)
        );
        assert_eq!(
            resolve_split_subframe(Codec::H265, TWO, false, false),
            (TWO, false)
        );
        // Split disabled: subframe stays (the escape).
        assert_eq!(
            resolve_split_subframe(Codec::H265, DISABLE, true, true),
            (DISABLE, true)
        );
    }

    /// H.264: split is not applicable (`nvEncodeAPI.h`) — DISABLE regardless of
    /// the resolved mode. Subframe (H.264 slices) is unaffected.
    #[test]
    fn h264_split_hard_disabled() {
        assert_eq!(
            resolve_split_subframe(Codec::H264, TWO, true, false),
            (DISABLE, true)
        );
        assert_eq!(
            resolve_split_subframe(Codec::H264, AUTO, false, false),
            (DISABLE, false)
        );
    }

    /// Do not drop the AUTO arm. AUTO + subframe cannot split; AUTO without
    /// subframe does. Retiring AUTO from the first measurement would cost every
    /// subframe-off session its second engine.
    #[test]
    fn auto_survives_the_arbitration_in_both_subframe_states() {
        // Subframe on: keep AUTO. Rewriting to DISABLE would mis-key the ceiling cache.
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO, true, false),
            (AUTO, true)
        );
        // Subframe off: still AUTO, and here it really splits.
        assert_eq!(
            resolve_split_subframe(Codec::H265, AUTO, false, false),
            (AUTO, false)
        );
    }

    /// AV1 keeps split and never arms subframe, even when
    /// `PUNKTFUNK_NVENC_SUBFRAME=1`. Writer and reader arm independently; the
    /// reader needs `slices >= 2` and [`resolve_slices`] returns 1 for AV1.
    #[test]
    fn av1_keeps_split_but_never_arms_subframe() {
        assert_eq!(
            resolve_split_subframe(Codec::Av1, TWO, true, true),
            (TWO, false),
            "AV1 must keep its split mode and drop sub-frame readback"
        );
        assert_eq!(
            resolve_split_subframe(Codec::Av1, AUTO, true, false),
            (AUTO, false)
        );
        assert_eq!(
            resolve_split_subframe(Codec::Av1, AUTO_F, true, false),
            (AUTO_F, false)
        );
        assert_eq!(
            resolve_split_subframe(Codec::Av1, TWO, false, false),
            (TWO, false)
        );
    }

    /// AV1 writer and reader must agree. [`resolve_slices`] returns 1 before the
    /// env override, so `subframe_chunks` can never arm — therefore the writer
    /// must not. H.264/HEVC single-slice really is one unit; AV1's unit count is
    /// the driver's tiles. If AV1 ever becomes multi-slice here, the first assert
    /// fires.
    #[test]
    fn av1_can_never_arm_the_chunked_reader_so_it_must_not_arm_the_writer() {
        assert_eq!(
            resolve_slices(Codec::Av1, 4),
            1,
            "AV1 is single-slice by construction — `subframe_chunks` (slices >= 2) cannot arm"
        );
        let (_, subframe) = resolve_split_subframe(Codec::Av1, AUTO, true, false);
        assert!(
            !subframe,
            "the sub-frame WRITER is armed on a session whose chunked READER cannot arm: every \
             frame would reach the wire truncated to its first tile"
        );
    }
}

// Gated to both backends: `nvenc_core` also builds with neither, and an
// ungated item is the item-level `dead_code` trap (see `subframe_env_forced`).
#[cfg(any(target_os = "linux", windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArbAction {
    /// Reconfigure in place. `resetEncoder=0` emits no IDR.
    SwitchTo(u32),
    /// This mode won; the arbiter will not ask again.
    Settled(u32),
}

#[cfg(any(target_os = "linux", windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArbState {
    MeasuringIncumbent,
    Settling,
    MeasuringChallenger,
    Done,
}

#[cfg(any(target_os = "linux", windows))]
/// Picks the faster of two NVENC split modes on the live session.
///
/// Open-time prediction cannot work: Automatic clients do not know steady-state
/// bitrate at open (ABR climbs afterwards). `nvEncReconfigureEncoder` accepts a
/// changed `splitEncodeMode` with `resetEncoder=0` and no IDR, so both arms
/// can be tried on the wire.
///
/// Measure, do not model: a hard-coded per-architecture constant is how the
/// previous rule went wrong (one 10-bit datapoint became a fleet veto).
///
/// `SETTLE_FRAMES` is load-bearing. Split-encode does not reach steady state
/// on the first frame; judging immediately after a switch reads the transient
/// and would cache a wrong verdict intermittently.
pub struct SplitArbiter {
    state: ArbState,
    incumbent: u32,
    challenger: u32,
    samples: Vec<u64>,
    incumbent_us: u64,
    settle_left: u32,
    /// Extra latency the challenger costs beyond encode time. Non-zero when winning
    /// the split means giving up subframe send-overlap (`send_spread × (slices−1)/slices`).
    /// Without it the arbiter compares encode to encode, always prefers split on HEVC,
    /// and reports a win while making end-to-end latency worse.
    challenger_handicap_us: u64,
}

/// Frames discarded after a switch before the challenger is judged.
#[cfg(any(target_os = "linux", windows))]
const SETTLE_FRAMES: u32 = 16;
/// Samples per arm. Long enough to median out content, short enough that
/// arbitration finishes in well under a second at 60 fps.
#[cfg(any(target_os = "linux", windows))]
const SAMPLE_FRAMES: usize = 24;
/// Challenger must beat the incumbent by this percent. Switching is a
/// reconfigure (and on HEVC costs subframe), so a coin-flip leaves the session.
#[cfg(any(target_os = "linux", windows))]
const WIN_MARGIN_PCT: u64 = 10;

#[cfg(any(target_os = "linux", windows))]
impl SplitArbiter {
    /// `handicap_us` is cost outside the measured encode — `0` when the challenger
    /// gives up nothing. See [`Self::challenger_handicap_us`].
    pub fn with_handicap(incumbent: u32, challenger: u32, handicap_us: u64) -> Self {
        Self {
            state: ArbState::MeasuringIncumbent,
            incumbent,
            challenger,
            samples: Vec::with_capacity(SAMPLE_FRAMES),
            incumbent_us: 0,
            settle_left: 0,
            challenger_handicap_us: handicap_us,
        }
    }

    pub fn on_frame(&mut self, us: u64) -> Option<ArbAction> {
        match self.state {
            ArbState::Done => None,
            ArbState::Settling => {
                self.settle_left = self.settle_left.saturating_sub(1);
                if self.settle_left == 0 {
                    self.state = ArbState::MeasuringChallenger;
                    self.samples.clear();
                }
                None
            }
            ArbState::MeasuringIncumbent => {
                self.samples.push(us);
                if self.samples.len() < SAMPLE_FRAMES {
                    return None;
                }
                self.incumbent_us = median(&mut self.samples);
                self.state = ArbState::Settling;
                self.settle_left = SETTLE_FRAMES;
                Some(ArbAction::SwitchTo(self.challenger))
            }
            ArbState::MeasuringChallenger => {
                self.samples.push(us);
                if self.samples.len() < SAMPLE_FRAMES {
                    return None;
                }
                // Total cost, not encode cost: HEVC subframe send-overlap is charged here.
                let challenger_us = median(&mut self.samples) + self.challenger_handicap_us;
                self.state = ArbState::Done;
                // Equal-ish keeps the incumbent: we are already there.
                let threshold = self
                    .incumbent_us
                    .saturating_sub(self.incumbent_us.saturating_mul(WIN_MARGIN_PCT) / 100);
                if challenger_us < threshold {
                    tracing::info!(
                        winner = self.challenger,
                        winner_us = challenger_us,
                        loser = self.incumbent,
                        loser_us = self.incumbent_us,
                        "NVENC split arbitration: challenger wins — keeping it"
                    );
                    Some(ArbAction::Settled(self.challenger))
                } else {
                    tracing::info!(
                        winner = self.incumbent,
                        winner_us = self.incumbent_us,
                        loser = self.challenger,
                        loser_us = challenger_us,
                        "NVENC split arbitration: incumbent held — switching back"
                    );
                    // Live session is the challenger; returning to the incumbent is a reconfigure.
                    Some(ArbAction::SwitchTo(self.incumbent))
                }
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == ArbState::Done
    }
}

#[cfg(any(target_os = "linux", windows))]
fn median(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Identity of one session config for the process-lifetime bitrate-ceiling
/// cache ([`cached_ceiling`]/[`store_ceiling`]). Keys the driver's codec-level
/// validation: GPU generation, luma rate (dims/fps), profile (depth/chroma),
/// and the split mode the session actually opened with.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CeilingKey {
    /// GPU identity — Linux: process-global `CUcontext` pointer; Windows: render
    /// adapter LUID (`0` if unresolved). Advisory: a colliding identity costs one
    /// failed open + re-search, never a wrong session.
    pub gpu: u64,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bit_depth: u8,
    pub chroma_444: bool,
    pub split_mode: u32,
}

fn ceilings() -> &'static std::sync::Mutex<std::collections::HashMap<CeilingKey, u64>> {
    static CEILINGS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CeilingKey, u64>>,
    > = std::sync::OnceLock::new();
    CEILINGS.get_or_init(Default::default)
}

/// Codec-level bitrate ceiling (bps) found for `key` this process, if any.
/// Advisory: a failed open at the cached value is stale — fall back to the
/// full search, which rewrites via [`store_ceiling`]. An ABR overshoot on a
/// known config then opens at the ceiling instead of a ~6-open binary search.
pub fn cached_ceiling(key: &CeilingKey) -> Option<u64> {
    ceilings().lock().unwrap().get(key).copied()
}

pub fn store_ceiling(key: CeilingKey, bps: u64) {
    ceilings().lock().unwrap().insert(key, bps);
}

#[cfg(any(target_os = "linux", windows))]
/// Split-arbitration verdict cache key: [`CeilingKey`] minus `split_mode`,
/// because split mode is the thing being decided.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SplitKey {
    pub gpu: u64,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bit_depth: u8,
    pub chroma_444: bool,
}

#[cfg(any(target_os = "linux", windows))]
fn split_verdicts() -> &'static std::sync::Mutex<std::collections::HashMap<SplitKey, u32>> {
    static V: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<SplitKey, u32>>> =
        std::sync::OnceLock::new();
    V.get_or_init(Default::default)
}

#[cfg(any(target_os = "linux", windows))]
/// Split mode a previous arbitration found fastest for `key` this process.
/// Advisory like [`cached_ceiling`]: not persisted — a driver update can
/// change the answer, and a disk verdict would outlive its evidence.
pub fn cached_split_verdict(key: &SplitKey) -> Option<u32> {
    split_verdicts().lock().unwrap().get(key).copied()
}

#[cfg(any(target_os = "linux", windows))]
pub fn store_split_verdict(key: SplitKey, mode: u32) {
    split_verdicts().lock().unwrap().insert(key, mode);
}

/// Drop every cached verdict. For tests: the cache is process-global, so an
/// on-hardware arbitration would otherwise leak into later tests that open
/// the same config with `PUNKTFUNK_SPLIT_ENCODE` unset.
// Gated like its two siblings, NOT on `test`: the sole caller is `nvenc_cuda`'s
// on-hw test in pf-encode, and `cfg(test)` is false for a crate built as that
// crate's dependency. Public in a public module, so never dead code.
#[cfg(any(target_os = "linux", windows))]
pub fn clear_split_verdicts() {
    split_verdicts().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clamp_to_engines, max_forced_split_mode, resolve_split_mode};
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;

    // Assumes `PUNKTFUNK_SPLIT_ENCODE` is unset (CI); an operator override wins.

    /// `encodeCodecConfig` is a C union: the HEVC 4:4:4 arm must be codec-gated
    /// or it stamps `hevcConfig` bytes onto another codec. Ungated, this branch
    /// was any `chroma_444 && full_chroma_input` and stayed non-UB only because
    /// `lib.rs` degrades 4:4:4 for non-HEVC. It also swallowed the per-codec
    /// bit-depth arm (`if`/`else if`).
    fn low_latency_cfg(codec: Codec, chroma_444: bool, bit_depth: u8) -> LowLatencyConfig {
        LowLatencyConfig {
            codec,
            bitrate: 20_000_000,
            fps: 60,
            custom_vbv: false,
            chroma_444,
            full_chroma_input: true,
            bit_depth,
            av1_input_depth_minus8: 0,
            hdr: false,
            full_range: false,
            rfi_supported: false,
            intra_refresh_cnt: 0,
            slices: 0,
        }
    }

    #[test]
    fn hevc_444_still_takes_the_frext_path() {
        // Do not `mem::zeroed` `NV_ENC_CONFIG`: `frameFieldMode`/`mvPrecision`
        // discriminants start at 1, so all-zero is invalid and Rust aborts.
        // Production seeds from `Default` then overwrites from the driver's preset.

        // SAFETY: `apply_low_latency_config` only writes into the caller's config (union writes
        // included) and makes no driver calls, so this is pure in-memory work.
        let cfg = unsafe {
            let mut cfg = nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            };
            apply_low_latency_config(&mut cfg, low_latency_cfg(Codec::H265, true, 10));
            cfg
        };
        assert_eq!(cfg.profileGUID, nv::NV_ENC_HEVC_PROFILE_FREXT_GUID);
        // SAFETY: an HEVC session's union arm is `hevcConfig` — the one this path wrote.
        unsafe { assert_eq!(cfg.encodeCodecConfig.hevcConfig.chromaFormatIDC(), 3) };
        // SAFETY: same HEVC arm as above.
        unsafe { assert_eq!(cfg.encodeCodecConfig.hevcConfig.pixelBitDepthMinus8(), 2) };
    }

    #[test]
    fn av1_never_takes_the_hevc_444_union_write() {
        // SAFETY: as above — pure in-memory config authoring, no driver involvement.
        let cfg = unsafe {
            let mut cfg = nv::NV_ENC_CONFIG {
                version: nv::NV_ENC_CONFIG_VER,
                ..Default::default()
            };
            apply_low_latency_config(&mut cfg, low_latency_cfg(Codec::Av1, true, 10));
            cfg
        };
        // HEVC FREXT on an AV1 session is INVALID_PARAM at open.
        assert_ne!(
            cfg.profileGUID,
            nv::NV_ENC_HEVC_PROFILE_FREXT_GUID,
            "4:4:4 on AV1 must not stamp the HEVC FREXT profile"
        );
        // The AV1 arm must still have run; the old if/else-if skipped it.

        // SAFETY: an AV1 session's union arm is `av1Config`.
        unsafe {
            assert_eq!(
                cfg.encodeCodecConfig.av1Config.pixelBitDepthMinus8(),
                2,
                "AV1 10-bit setup was swallowed by the HEVC 4:4:4 branch"
            );
        }
    }

    #[test]
    fn split_forces_two_way_at_4k120() {
        // 3840×2160×120 = 995,328,000 sat 0.47% under the old `> 1_000_000_000`
        // gate and stayed AUTO — AUTO never engages at 2160 px height.
        let four_k_120 = 3840u64 * 2160 * 120;
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
    }

    #[test]
    fn split_leaves_1440p240_auto() {
        // 884.7 Mpix/s is single-engine; the threshold move must not drag it in.
        let qhd_240 = 2560u64 * 1440 * 240;
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, qhd_240, 2),
            M::NV_ENC_SPLIT_AUTO_MODE as u32
        );
    }

    #[test]
    fn split_rules_for_10bit_after_dropping_the_short_circuit() {
        let five_k_240 = 5120u64 * 1440 * 240;
        let four_k_120 = 3840u64 * 2160 * 120;
        let hd_60 = 1920u64 * 1080 * 60;

        // 10-bit over the pixel-rate bar now splits. The old Main10 veto was one
        // sample at low bits/frame; `PUNKTFUNK_SPLIT_ENCODE=0` is the escape.
        assert_eq!(
            resolve_split_mode(Codec::H265, 10, five_k_240, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        assert_eq!(
            resolve_split_mode(Codec::H265, 10, four_k_120, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        // Under the bar, HEVC Main10 stays single-engine — a second engine buys nothing.
        assert_eq!(
            resolve_split_mode(Codec::H265, 10, hd_60, 2),
            M::NV_ENC_SPLIT_DISABLE_MODE as u32
        );
    }

    /// AV1 10-bit follows the ordinary path. The Main10 veto was an HEVC
    /// measurement applied codec-blind.
    #[test]
    fn av1_10bit_is_no_longer_vetoed_by_an_hevc_measurement() {
        let hd_60 = 1920u64 * 1080 * 60;
        let four_k_120 = 3840u64 * 2160 * 120;
        assert_eq!(
            resolve_split_mode(Codec::Av1, 10, hd_60, 2),
            M::NV_ENC_SPLIT_AUTO_MODE as u32,
            "AV1 10-bit must follow the ordinary path, not inherit an HEVC veto"
        );
        assert_eq!(
            resolve_split_mode(Codec::Av1, 10, four_k_120, 2),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
    }

    /// High pixel-rate sessions use every engine the GPU has. The driver accepts
    /// an over- or under-wide request without comment.
    #[test]
    fn split_uses_every_engine_the_gpu_has() {
        let four_k_120 = 3840u64 * 2160 * 120;
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 3),
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
            "a 3-engine GPU must split three ways"
        );
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 1),
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            "a 1-engine GPU must not pretend to split — today this costs a wasted session open"
        );
        assert_eq!(
            resolve_split_mode(Codec::H265, 8, four_k_120, 0),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
            "unprobed engine count keeps the historical assumption; the rejection fallback corrects"
        );
    }

    /// `NV_ENC_SPLIT_ENCODE_MODE` names at most three (SDK 0.4.0 / NVENCAPI 12.1);
    /// a wider part falls back to AUTO_FORCED = "split, driver picks how many".
    #[test]
    fn split_beyond_three_engines_delegates_to_the_driver() {
        assert_eq!(
            max_forced_split_mode(4),
            M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
        );
        assert_eq!(
            max_forced_split_mode(8),
            M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
        );
    }

    /// Clamp an operator over-ask: the driver will honour THREE_FORCED on a
    /// 2-NVENC part and run it as two-way, so the log would claim three engines.
    #[test]
    fn operator_override_is_clamped_to_real_engine_count() {
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                max_forced_split_mode(2),
                2
            ),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
            "asking for 3 on a 2-engine card must clamp to 2"
        );
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
                max_forced_split_mode(3),
                3
            ),
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        // Unknown engine count: nothing to clamp against.
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                max_forced_split_mode(0),
                0
            ),
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32
        );
        // Ordering trap: on a >3-engine part `hw_max` is AUTO_FORCED (1), which is
        // not "narrower than" TWO_FORCED (2) despite comparing smaller. A naive
        // `min` would clamp a legitimate 3-way request down to AUTO.
        assert_eq!(
            clamp_to_engines(
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
                max_forced_split_mode(4),
                4
            ),
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
            "a 4-engine GPU must honour an explicit 3-way request, not collapse it to AUTO"
        );
    }

    #[test]
    fn ceiling_cache_round_trips_and_keys_precisely() {
        let key = CeilingKey {
            gpu: 0xB0B0,
            codec: Codec::H265,
            width: 3840,
            height: 2160,
            fps: 120,
            bit_depth: 8,
            chroma_444: false,
            split_mode: M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        };
        assert_eq!(cached_ceiling(&key), None);
        store_ceiling(key, 794_000_000);
        assert_eq!(cached_ceiling(&key), Some(794_000_000));
        // Any other identity field is a different ceiling — a miss, never a wrong clamp.
        assert_eq!(cached_ceiling(&CeilingKey { fps: 60, ..key }), None);
        assert_eq!(
            cached_ceiling(&CeilingKey {
                split_mode: M::NV_ENC_SPLIT_DISABLE_MODE as u32,
                ..key
            }),
            None
        );
        // Re-search overwrites (stale-entry path).
        store_ceiling(key, 620_000_000);
        assert_eq!(cached_ceiling(&key), Some(620_000_000));
    }
}

/// RFI DPB depth (Apollo's 5). [`plan_range_recovery`] treats
/// `next_ts - RFI_DPB` as the oldest frame still in the DPB.
pub const RFI_DPB: u32 = 5;

/// One loss event's recovery for timestamp-range RFI. The per-timestamp
/// `nvEncInvalidateRefFrames` loop and `last_rfi_range`/`pending_anchor`
/// stores stay in each backend. Slot-RFI (AMF/QSV/Vulkan) is `crate::rfi`.
pub enum RangePlan {
    /// Last successful invalidation already covers this range. Re-arm the recovery
    /// anchor: the client re-asking means the previous anchor AU may itself have
    /// been lost.
    Covered,
    /// Invalidate `first..=last`, where `last` is the encode head. Record these
    /// values in `last_rfi_range` on success.
    Invalidate { first: i64, last: i64 },
    /// Recovery without an IDR is impossible. Do not clear `pending_anchor` on
    /// decline (Vulkan's decline; AMF/QSV clear `pending_force` — see `crate::rfi`).
    Decline,
}

/// Range-RFI policy, extracted from both backends' `invalidate_ref_frames`.
/// Step order is load-bearing:
///
/// 1. nonsense range (`first < 0 || first > last`) → [`RangePlan::Decline`];
/// 2. covering-range dedup with the client's `last`, before the DPB window,
///    so a covered re-ask never touches the driver after leaving the DPB;
/// 3. anchor check: a frame older than `first`, still in the DPB and not already
///    invalidated must exist, else Decline;
/// 4. `last` becomes `next_ts - 1`: every frame encoded since the loss predicts
///    from it, so the client decoded all of them against a hole. Invalidating
///    only the frames the client named left NVENC anchoring on one of these.
///    A `first` at or past the head is a prediction desync → Decline.
///
/// `next_ts` is `frame_idx`: the next timestamp to assign. `teardown()` clears
/// `last_rfi_range` but not `frame_idx`, so a post-reset call can see a
/// stale-high `next_ts` with `None` range.
pub fn plan_range_recovery(
    first: i64,
    last: i64,
    next_ts: i64,
    last_rfi_range: Option<(i64, i64)>,
) -> RangePlan {
    if first < 0 || first > last {
        return RangePlan::Decline;
    }
    if let Some((pf, pl)) = last_rfi_range {
        if first >= pf && last <= pl {
            return RangePlan::Covered;
        }
    }
    let oldest_in_dpb = (next_ts - RFI_DPB as i64).max(0);
    let newest_anchor = first - 1;
    let prior_covers =
        last_rfi_range.is_some_and(|(pf, pl)| pf <= oldest_in_dpb && pl >= newest_anchor);
    if oldest_in_dpb > newest_anchor || prior_covers {
        return RangePlan::Decline;
    }
    let last = next_ts - 1;
    if first > last {
        return RangePlan::Decline;
    }
    RangePlan::Invalidate { first, last }
}

#[cfg(test)]
mod range_policy_tests {
    use super::{plan_range_recovery, RangePlan, RFI_DPB};

    fn plan(first: i64, last: i64, next_ts: i64) -> RangePlan {
        plan_range_recovery(first, last, next_ts, None)
    }

    #[test]
    fn nonsense_ranges_decline() {
        assert!(matches!(plan(-1, 5, 100), RangePlan::Decline));
        assert!(matches!(plan(7, 5, 100), RangePlan::Decline));
    }

    /// `RFI_DPB` must not grow past what a mainstream client can allocate.
    /// NVENC writes `sps_max_dec_pic_buffering_minus1 = RFI_DPB` (5 refs + current
    /// = 6 pictures). Decoder backends then hold one more for the picture in
    /// flight, so demand is `RFI_DPB + 2 = 7`. NVIDIA Vulkan Video reports
    /// `maxDpbSlots = 16`; over that the stream is refused and a client with no
    /// software HEVC decoder loses the codec.
    #[test]
    fn rfi_dpb_fits_a_mainstream_vulkan_decoder() {
        /// Lowest `VkVideoCapabilitiesKHR::maxDpbSlots` among targeted decoders.
        const VULKAN_MAX_DPB_SLOTS: u32 = 16;
        // SPS pictures (`RFI_DPB + 1`) plus the picture being decoded.
        let slots_needed = RFI_DPB + 2;
        assert!(
            slots_needed <= VULKAN_MAX_DPB_SLOTS,
            "RFI_DPB = {RFI_DPB} makes the host emit a stream needing {slots_needed} DPB \
             slots, and mainstream Vulkan Video decode caps at {VULKAN_MAX_DPB_SLOTS} — \
             every access unit would be refused and the client would drop the codec"
        );
    }

    /// The client names the lost frames; the host has encoded past them by the time
    /// the ask lands, and each of those predicts from the hole. The range runs to
    /// the encode head so the anchor predates the loss.
    #[test]
    fn the_range_runs_to_the_encode_head() {
        assert!(matches!(
            plan(100, 100, 103),
            RangePlan::Invalidate {
                first: 100,
                last: 102
            }
        ));
        // Newest possible loss: the head itself is the only frame invalidated.
        assert!(matches!(
            plan(99, 99, 100),
            RangePlan::Invalidate {
                first: 99,
                last: 99
            }
        ));
    }

    #[test]
    fn covering_range_dedups_partial_overlap_does_not() {
        let prior = Some((90i64, 95i64));
        // Exact cover and sub-range stay Covered, including across a keyframe
        // (`last_rfi_range` is not cleared on IDR).
        assert!(matches!(
            plan_range_recovery(90, 95, 100, prior),
            RangePlan::Covered
        ));
        assert!(matches!(
            plan_range_recovery(92, 94, 100, prior),
            RangePlan::Covered
        ));
        // A later loss re-invalidates from its own `first` to the head: the prior
        // anchor (96) survives, so recovery is still possible.
        assert!(matches!(
            plan_range_recovery(97, 97, 98, prior),
            RangePlan::Invalidate {
                first: 97,
                last: 97
            }
        ));
    }

    /// Covering check runs before the DPB window: a covered re-ask stays Covered
    /// even after the range has aged out of the DPB.
    #[test]
    fn covered_is_checked_before_the_dpb_window() {
        let prior = Some((10i64, 12i64));
        assert!(matches!(
            plan_range_recovery(10, 12, 100, prior),
            RangePlan::Covered
        ));
        // Same range with no prior invalidation is outside the window → Decline.
        assert!(matches!(plan(10, 12, 100), RangePlan::Decline));
    }

    /// An anchor must be older than the loss and still resident: with the whole
    /// DPB inside the corrupt window nothing valid remains and NVENC must not be
    /// asked to reference it.
    #[test]
    fn dpb_window_boundary() {
        let next_ts = 100i64;
        let oldest = next_ts - RFI_DPB as i64;
        assert!(matches!(
            plan(oldest + 1, oldest + 1, next_ts),
            RangePlan::Invalidate { .. }
        ));
        assert!(matches!(plan(oldest, oldest, next_ts), RangePlan::Decline));
    }

    /// The previous loss already invalidated every candidate older than this one:
    /// its anchor was itself lost and nothing before it is left in the DPB.
    #[test]
    fn a_prior_invalidation_can_exhaust_the_anchors() {
        // Loss at 100 was repaired at head 103 (100..102 invalid, anchor 103).
        // The anchor is lost; the client reports it at head 105.
        assert!(matches!(
            plan_range_recovery(103, 103, 105, Some((100, 102))),
            RangePlan::Decline
        ));
        // With the anchor received and a later loss, 103 anchors the repair.
        assert!(matches!(
            plan_range_recovery(104, 104, 106, Some((100, 102))),
            RangePlan::Invalidate {
                first: 104,
                last: 105
            }
        ));
    }

    #[test]
    fn future_and_fresh_session_ranges_decline() {
        // Entirely in the future → Decline (prediction desync).
        assert!(matches!(plan(100, 150, 100), RangePlan::Decline));
        // Fresh session (`frame_idx == 0`): nothing encoded yet.
        assert!(matches!(plan(0, 3, 0), RangePlan::Decline));
        // The opening IDR lost with one frame encoded after it: no older anchor.
        assert!(matches!(plan(0, 0, 2), RangePlan::Decline));
        assert!(matches!(
            plan(1, 1, 2),
            RangePlan::Invalidate { first: 1, last: 1 }
        ));
    }
}

/// Per-session knobs both backends feed [`apply_low_latency_config`].
/// `full_chroma_input` / `av1_input_depth_minus8` are the CUDA vs D3D11
/// input-format divergence; everything else is identical across platforms.
#[derive(Clone, Copy)]
pub struct LowLatencyConfig {
    pub codec: Codec,
    pub bitrate: u64,
    pub fps: u32,
    /// GPU advertises custom VBV — else leave the preset default.
    pub custom_vbv: bool,
    pub chroma_444: bool,
    /// Input surface can carry full chroma (Linux YUV444, Windows packed-RGB
    /// that NVENC CSCs). 4:4:4 engages only with [`Self::chroma_444`].
    pub full_chroma_input: bool,
    pub bit_depth: u8,
    /// AV1 `inputPixelBitDepthMinus8`: Linux is 8-bit in (0); Windows derives
    /// it from the surface format. `u32` matches the SDK setter.
    pub av1_input_depth_minus8: u32,
    pub hdr: bool,
    /// The input samples are full range (only Linux's `PUNKTFUNK_444_FULLRANGE` YUV444).
    pub full_range: bool,
    pub rfi_supported: bool,
    /// Arm the on-demand intra refresh wave: the mode on with a period that never fires, so
    /// a per-picture `forceIntraRefreshWithFrameCnt` runs one when an RFI declines. The
    /// longest cycle the backend will force; 0 leaves the mode off.
    pub intra_refresh_cnt: u32,
    /// [`resolve_slices`] result. ≤ 1 leaves the preset's single slice.
    pub slices: u32,
}

/// A periodic wave that never comes: only the forced one runs.
const INTRA_REFRESH_NEVER: u32 = 1 << 30;

/// `PUNKTFUNK_NVENC_IR_ALWAYS=1` answers every RFI with a wave, process environment only.
/// A loopback never declines an RFI on its own, so the wave soak needs this to run waves.
pub fn wave_always() -> bool {
    std::env::var("PUNKTFUNK_NVENC_IR_ALWAYS").is_ok_and(|v| v == "1")
}

/// Rows the driver sweeps: 32-px blocks bound the cycle for every NVENC codec.
pub fn wave_rows(height: u32) -> u32 {
    height.div_ceil(32)
}

/// Shared `NV_ENC_INITIALIZE_PARAMS` (P1/ULL, PTD, session dims/rate) pointing
/// at `cfg`. The returned struct borrows `cfg` as a raw pointer; keep `cfg`
/// alive across the NVENC call. Open and in-place reconfigure must present
/// the same init params. `enable_async` is the Windows two-thread retrieve;
/// Linux is sync-only (`enableEncodeAsync = 0`).
#[allow(clippy::too_many_arguments)]
pub fn build_init_params(
    codec_guid: nv::GUID,
    width: u32,
    height: u32,
    fps: u32,
    cfg: &mut nv::NV_ENC_CONFIG,
    split_mode: u32,
    enable_async: bool,
    subframe: bool,
) -> nv::NV_ENC_INITIALIZE_PARAMS {
    let mut init = nv::NV_ENC_INITIALIZE_PARAMS {
        version: nv::NV_ENC_INITIALIZE_PARAMS_VER,
        encodeGUID: codec_guid,
        presetGUID: nv::NV_ENC_PRESET_P1_GUID,
        tuningInfo: nv::NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
        encodeWidth: width,
        encodeHeight: height,
        darWidth: width,
        darHeight: height,
        frameRateNum: fps,
        frameRateDen: 1,
        enablePTD: 1,
        enableEncodeAsync: enable_async as u32,
        encodeConfig: cfg,
        ..Default::default()
    };
    // splitEncodeMode is a C bitfield — set via the generated accessor, not a struct field.
    init.set_splitEncodeMode(split_mode);
    // Sub-frame readback: the driver writes each slice as it completes. Pair with
    // multi-slice. `reportSliceOffsets` requires `enableEncodeAsync = 0`, so
    // async (Windows) sessions never arm.
    if !enable_async && subframe {
        init.set_enableSubFrameWrite(1);
        init.set_reportSliceOffsets(1);
    }
    init
}

/// Low-latency NVENC config onto a **preset-seeded** `cfg`: CBR, infinite GOP,
/// P-only, ~1-frame VBV, per-codec tier/level, chroma + bit depth, colour
/// signaling, RFI DPB. Caller seeds from the P1/ULL preset (needs the
/// per-platform entry table).
///
/// # Safety
/// Writes codec-config union fields on `cfg`, which must be a valid,
/// preset-seeded `NV_ENC_CONFIG` whose active arm matches [`LowLatencyConfig::codec`].
pub unsafe fn apply_low_latency_config(cfg: &mut nv::NV_ENC_CONFIG, c: LowLatencyConfig) {
    cfg.gopLength = nv::NVENC_INFINITE_GOPLENGTH;
    cfg.frameIntervalP = 1;
    cfg.rcParams.rateControlMode = nv::NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    // Pin zero reorder delay so no preset/driver default can slip a frame in.
    cfg.rcParams.set_zeroReorderDelay(1);
    let bps = c.bitrate.min(u32::MAX as u64) as u32;
    cfg.rcParams.averageBitRate = bps;
    cfg.rcParams.maxBitRate = bps;
    // Shrink VBV with bitrate (NVENC validates it against the level ceiling),
    // but only when the GPU advertises custom-VBV — else keep the preset default.
    if c.custom_vbv {
        // ~1-frame VBV; `PUNKTFUNK_VBV_FRAMES` scales it (parity with AMF/VAAPI/QSV).
        let vbv = ((c.bitrate as f64 / c.fps.max(1) as f64) * crate::vbv_frames_env())
            .clamp(1.0, u32::MAX as f64) as u32;
        cfg.rcParams.vbvBufferSize = vbv;
        cfg.rcParams.vbvInitialDelay = vbv;
    }

    // Per-codec tier/level. HEVC HIGH for the per-level bitrate ceiling.
    // AV1 accepts Main only — tier=1 is INVALID_PARAM — and its level 0 is
    // LEVEL 2.0, not autoselect, so AV1 takes no writes. H.264 has no tier.
    match c.codec {
        Codec::H265 => {
            // Union-arm writes are safe; the hazard is a mismatched later read.
            cfg.encodeCodecConfig.hevcConfig.tier = 1;
            cfg.encodeCodecConfig.hevcConfig.level = 0;
        }
        Codec::Av1 => {}
        Codec::H264 => {}
        Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
    }

    // `c.slices` ≥ 2: sliceMode 3 = N slices per frame (the sub-frame unit).
    // H.264/HEVC only; AV1 partitions via tiles. ≤ 1 keeps the preset slice.
    if let Some(n) = Some(c.slices).filter(|n| *n >= 2) {
        match c.codec {
            Codec::H264 => {
                cfg.encodeCodecConfig.h264Config.sliceMode = 3;
                cfg.encodeCodecConfig.h264Config.sliceModeData = n;
            }
            Codec::H265 => {
                cfg.encodeCodecConfig.hevcConfig.sliceMode = 3;
                cfg.encodeCodecConfig.hevcConfig.sliceModeData = n;
            }
            Codec::Av1 | Codec::PyroWave => {}
        }
    }

    // `encodeCodecConfig` is a C union: `hevcConfig` writes are HEVC-only.
    // 4:4:4 (FREXT, chromaFormatIDC=3) composes with 10-bit. The codec test
    // is load-bearing — without it this was `chroma_444 && full_chroma_input`
    // and a non-HEVC 4:4:4 session skipped its own 10-bit arm (`if`/`else if`).
    let want_444 = c.chroma_444 && c.full_chroma_input;
    if want_444 && c.codec != Codec::H265 {
        tracing::warn!(
            codec = ?c.codec,
            "4:4:4 requested on a non-HEVC NVENC session — ignoring it (Range Extensions are \
             HEVC-only); the negotiator should have degraded this to 4:2:0 before the open"
        );
    }
    if want_444 && c.codec == Codec::H265 {
        cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_FREXT_GUID;
        // SAFETY: HEVC session (guarded by `c.codec == Codec::H265` on this branch), so
        // `hevcConfig` is the active arm.
        unsafe { cfg.encodeCodecConfig.hevcConfig.set_chromaFormatIDC(3) };
        if c.bit_depth == 10 {
            // SAFETY: same HEVC arm, same branch guard. (Main 4:4:4 10)
            unsafe { cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2) };
        }
    } else if c.bit_depth == 10 {
        match c.codec {
            Codec::H265 => {
                cfg.profileGUID = nv::NV_ENC_HEVC_PROFILE_MAIN10_GUID;
                // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active arm.
                unsafe { cfg.encodeCodecConfig.hevcConfig.set_pixelBitDepthMinus8(2) };
            }
            Codec::Av1 => {
                // SAFETY: AV1 session (matched on `c.codec`), so `av1Config` is the active arm.
                unsafe { cfg.encodeCodecConfig.av1Config.set_pixelBitDepthMinus8(2) };
                // SAFETY: same AV1 arm, same match guard.
                unsafe {
                    cfg.encodeCodecConfig
                        .av1Config
                        .set_inputPixelBitDepthMinus8(c.av1_input_depth_minus8)
                };
            }
            Codec::H264 => {} // NVENC has no 10-bit H.264; negotiation never asks
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }

    // Colour signaling is unconditional. NVENC converts RGB input with this same matrix
    // (BT.601, 709 and 2020 alike); YUV input is already CSC'd. Limited unless
    // `full_range`. HEVC/H.264: VUI; AV1: sequence-header CICP.
    {
        let (prim, trc, mat) = if c.hdr {
            (
                nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020,
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084,
                nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL,
            )
        } else {
            (
                nv::NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709,
                nv::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709,
                nv::NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709,
            )
        };
        match c.codec {
            Codec::H265 => {
                // SAFETY: HEVC session (matched on `c.codec`), so `hevcConfig` is the active
                // arm; the borrow is dropped before any other union access.
                let vui = unsafe { &mut cfg.encodeCodecConfig.hevcConfig.hevcVUIParameters };
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = u32::from(c.full_range);
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = prim;
                vui.transferCharacteristics = trc;
                vui.colourMatrix = mat;
            }
            Codec::H264 => {
                // SAFETY: H.264 session (matched on `c.codec`), so `h264Config` is the active
                // arm; the borrow is dropped before any other union access.
                let vui = unsafe { &mut cfg.encodeCodecConfig.h264Config.h264VUIParameters };
                vui.videoSignalTypePresentFlag = 1;
                vui.videoFullRangeFlag = u32::from(c.full_range);
                vui.colourDescriptionPresentFlag = 1;
                vui.colourPrimaries = prim;
                vui.transferCharacteristics = trc;
                vui.colourMatrix = mat;
            }
            Codec::Av1 => {
                // SAFETY: AV1 session (matched on `c.codec`), so `av1Config` is the active arm;
                // the borrow is dropped before any other union access.
                let av1 = unsafe { &mut cfg.encodeCodecConfig.av1Config };
                av1.colorPrimaries = prim;
                av1.transferCharacteristics = trc;
                av1.matrixCoefficients = mat;
                av1.colorRange = u32::from(c.full_range);
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }

    // RFI: deeper DPB + `numRefL0 = 1` (single-reference P-frames).
    if c.rfi_supported {
        let one = nv::NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
        match c.codec {
            Codec::H264 => {
                cfg.encodeCodecConfig.h264Config.maxNumRefFrames = RFI_DPB;
                cfg.encodeCodecConfig.h264Config.numRefL0 = one;
            }
            Codec::H265 => {
                cfg.encodeCodecConfig.hevcConfig.maxNumRefFramesInDPB = RFI_DPB;
                cfg.encodeCodecConfig.hevcConfig.numRefL0 = one;
            }
            Codec::Av1 => {
                cfg.encodeCodecConfig.av1Config.maxNumRefFramesInDPB = RFI_DPB;
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }
    // On-demand intra refresh: the mode on, the timer never, the recovery-point SEI where the
    // codec has one so a stock decoder heals in-band too. `intraRefreshCnt` is the forced
    // count's ceiling. Never `singleSliceIntraRefresh`: one slice per wave frame leaves the
    // band with no isolation and nothing ever decodes exact again.
    if c.rfi_supported && c.intra_refresh_cnt > 0 {
        let cnt = c.intra_refresh_cnt;
        match c.codec {
            // SAFETY: H.264 session (matched on `c.codec`): `h264Config` is the active arm.
            Codec::H264 => unsafe {
                let h = &mut cfg.encodeCodecConfig.h264Config;
                h.set_enableIntraRefresh(1);
                h.intraRefreshPeriod = INTRA_REFRESH_NEVER;
                h.intraRefreshCnt = cnt;
                h.set_outputRecoveryPointSEI(1);
            },
            // SAFETY: HEVC session: `hevcConfig` is the active arm.
            Codec::H265 => unsafe {
                let h = &mut cfg.encodeCodecConfig.hevcConfig;
                h.set_enableIntraRefresh(1);
                h.intraRefreshPeriod = INTRA_REFRESH_NEVER;
                h.intraRefreshCnt = cnt;
                h.set_outputRecoveryPointSEI(1);
            },
            // SAFETY: AV1 session: `av1Config` is the active arm.
            Codec::Av1 => unsafe {
                let a = &mut cfg.encodeCodecConfig.av1Config;
                a.set_enableIntraRefresh(1);
                a.intraRefreshPeriod = INTRA_REFRESH_NEVER;
                a.intraRefreshCnt = cnt;
            },
            Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
        }
    }
}

#[cfg(all(test, any(target_os = "linux", windows)))]
mod arbiter_tests {
    use super::{ArbAction, SplitArbiter, SETTLE_FRAMES};

    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_SPLIT_ENCODE_MODE as M;

    /// Same encode times both runs; only the handicap differs. Split can halve
    /// encode and still lose end-to-end if losing subframe costs more than the
    /// encode saving — the mistake an encode-only comparison makes.
    #[test]
    fn handicap_can_reverse_the_verdict() {
        let (inc, chal) = (
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        );
        let run = |handicap: u64| {
            let mut arb = SplitArbiter::with_handicap(inc, chal, handicap);
            let mut live = inc;
            for _ in 0..500 {
                if arb.is_done() {
                    break;
                }
                let us = if live == inc { 5000 } else { 2400 };
                if let Some(a) = arb.on_frame(us) {
                    match a {
                        ArbAction::SwitchTo(m) | ArbAction::Settled(m) => live = m,
                    }
                }
            }
            live
        };
        assert_eq!(run(500), chal, "with a cheap send, split should win");
        // 2400 + 3000 = 5400 against 5000: the faster encode is a loss end-to-end.
        assert_eq!(
            run(3000),
            inc,
            "when losing sub-frame costs more than split saves, the incumbent must hold — this is \
             the regression an encode-only arbiter would ship"
        );
    }

    fn drive(incumbent_us: u64, challenger_us: u64) -> (Vec<ArbAction>, u32) {
        let (inc, chal) = (
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        );
        let mut arb = SplitArbiter::with_handicap(inc, chal, 0);
        let mut actions = Vec::new();
        // Harness follows the arbiter's switches so reported cost matches the live arm.
        let mut live = inc;
        for _ in 0..500 {
            if arb.is_done() {
                break;
            }
            let us = if live == inc {
                incumbent_us
            } else {
                challenger_us
            };
            if let Some(a) = arb.on_frame(us) {
                actions.push(a);
                match a {
                    ArbAction::SwitchTo(m) => live = m,
                    ArbAction::Settled(m) => live = m,
                }
            }
        }
        (actions, live)
    }

    #[test]
    fn arbiter_adopts_a_clearly_faster_challenger() {
        let (actions, live) = drive(5000, 2400);
        assert_eq!(
            actions[0],
            ArbAction::SwitchTo(M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32),
            "must try the challenger before judging it"
        );
        assert_eq!(
            actions.last(),
            Some(&ArbAction::Settled(M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32))
        );
        assert_eq!(live, M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32);
    }

    /// A slower challenger is a real reconfigure back, not a no-op. Getting this
    /// wrong would strand every losing arbitration on the losing arm.
    #[test]
    fn arbiter_restores_the_incumbent_when_the_challenger_loses() {
        let (actions, live) = drive(2400, 5000);
        assert_eq!(
            actions.last(),
            Some(&ArbAction::SwitchTo(M::NV_ENC_SPLIT_DISABLE_MODE as u32)),
            "a losing experiment must be undone"
        );
        assert_eq!(live, M::NV_ENC_SPLIT_DISABLE_MODE as u32);
    }

    /// Within the margin the incumbent holds: switching costs a reconfigure and,
    /// on HEVC, subframe, so a coin-flip must not move the session.
    #[test]
    fn arbiter_keeps_the_incumbent_inside_the_margin() {
        // 5% better — under WIN_MARGIN_PCT.
        let (_, live) = drive(2400, 2280);
        assert_eq!(live, M::NV_ENC_SPLIT_DISABLE_MODE as u32);
    }

    /// Challenger must not be judged on frames taken immediately after the
    /// switch. Without the settle window the arbiter would read the transient,
    /// reject a better arm, and cache that verdict.
    #[test]
    fn arbiter_ignores_the_post_switch_transient() {
        let (inc, chal) = (
            M::NV_ENC_SPLIT_DISABLE_MODE as u32,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32,
        );
        let mut arb = SplitArbiter::with_handicap(inc, chal, 0);
        let mut switched_at = None;
        let mut frame = 0usize;
        let mut outcome = None;
        while outcome.is_none() && frame < 500 {
            let us = match switched_at {
                None => 5000,
                // Transient: as slow as the incumbent for exactly the settle window.
                Some(s) if frame - s <= SETTLE_FRAMES as usize => 5000,
                Some(_) => 2000,
            };
            match arb.on_frame(us) {
                Some(ArbAction::SwitchTo(m)) if m == chal => switched_at = Some(frame),
                Some(a) => outcome = Some(a),
                None => {}
            }
            frame += 1;
        }
        assert_eq!(
            outcome,
            Some(ArbAction::Settled(chal)),
            "the settle window must hide the post-switch transient — otherwise a better arm is \
             rejected on its own warmup"
        );
    }
}

/// Hand-written split constants in `codec.rs` must equal the SDK enum.
/// Duplicated so a build without the `nvenc` feature — which cannot see the
/// SDK enum — still carries the policy. This crate sees both.
#[cfg(test)]
mod split_constant_parity {
    use nvidia_video_codec_sdk::sys::nvEncodeAPI::NV_ENC_SPLIT_ENCODE_MODE as M;

    #[test]
    fn nvenc_split_constants_match_the_sdk() {
        assert_eq!(crate::SPLIT_AUTO, M::NV_ENC_SPLIT_AUTO_MODE as u32);
        assert_eq!(
            crate::SPLIT_AUTO_FORCED,
            M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32
        );
        assert_eq!(
            crate::SPLIT_TWO_FORCED,
            M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
        );
        assert_eq!(
            crate::SPLIT_THREE_FORCED,
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32
        );
        assert_eq!(crate::SPLIT_DISABLE, M::NV_ENC_SPLIT_DISABLE_MODE as u32);
    }
}
