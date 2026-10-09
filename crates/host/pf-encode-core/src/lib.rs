//! The encode contract every backend implements: the [`Encoder`] trait and
//! its codec vocabulary. Plus the pieces the Linux backends in `pf-encode`
//! and the Windows ones in `pf-encode-win` share: the NVENC glue and session,
//! the slot-RFI policy, the loss-recovery env knobs, and the PyroWave wire
//! framing.
//!
//! Speaks `pf-frame` only, so the IddCx driver can host it through
//! `pf-encode-win` without QUIC or GPU inventory. Backend selection stays in
//! `pf-encode`. Evidence: `design/windows-video-plane-overhaul.md` §2.6.

mod codec;
/// The encoder knobs (`host.env` via the driver request, or the environment).
pub mod knobs;
pub use codec::*;

// `NVENCSTATUS` → cause for both direct-NVENC backends. Splits the two
// opposite failures the driver reports as the same `INVALID_VERSION`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
pub mod nvenc_status;
// Shared `nvEncodeAPI` glue (`NvStatusExt`/`nv_ok`, `codec_guid`). Sibling of `nvenc_status`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
pub mod nvenc_core;
// The direct-NVENC session both backends drive: open ladder, submit, RFI, retrieve.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
pub mod nvenc_session;
// Slot-family RFI policy (taint sweep + pre-loss anchor) for AMF, QSV, Vulkan
// Video and the native VAAPI encoder. Mechanisms stay in each backend. Cfg is
// the union of callers, and the VAAPI one is featureless on Linux.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod rfi;
pub mod smoke_pattern;
// Shared loss-recovery env knobs. Defaults and API clamps stay per-backend.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod policy;
// Shared PyroWave AU wire-framing — both platform backends emit this layout.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
pub mod pyrowave_wire;
// The pyrowave-sys calls both PyroWave backends make the same way, and the tests' decoder.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
pub mod pyrowave_ffi;

/// Whether a PyroWave mode fits the rate controller's packed 16-bit block
/// index: false ≈ 8K-class 4:4:4. Negotiator downgrades to 4:2:0; encoders refuse.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
pub fn pyrowave_mode_fits_rdo(width: u32, height: u32, chroma444: bool) -> bool {
    pyrowave_wire::block_count_32x32(width, height, chroma444) <= u16::MAX as u32
}
#[cfg(not(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave")))]
pub fn pyrowave_mode_fits_rdo(_width: u32, _height: u32, _chroma444: bool) -> bool {
    false
}

/// Marker in an encoder error's `anyhow` chain: the failure is a deterministic
/// config consequence, so an in-place rebuild can never succeed. The reset
/// ladder downcasts this and ends the session instead of burning rebuilds.
/// Attach with `Error::new(TerminalEncoderError).context("the actual cause")`.
#[derive(Clone, Copy, Debug)]
pub struct TerminalEncoderError;

impl std::fmt::Display for TerminalEncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deterministic configuration error — an encoder rebuild cannot fix this")
    }
}

impl std::error::Error for TerminalEncoderError {}

/// Codecs the active GPU can encode. AV1 encode is narrow — probe, don't assume.
/// All-`false` means the probe found nothing (GPU unusable at probe time), not
/// "zero codecs"; `pf_encode` maps that to the static superset.
#[derive(Clone, Copy, Debug)]
pub struct CodecSupport {
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
}
