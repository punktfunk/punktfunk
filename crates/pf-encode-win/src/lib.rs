//! Windows encoder backends behind one [`Encoder`] trait: direct-SDK NVENC,
//! native AMF, native QSV (VPL), Media Foundation, and PyroWave.
//!
//! Speaks `pf-frame` only. Backend selection, GPU inventory, and the wire
//! codec bits stay in `pf-encode`; the adapter a backend should open on
//! arrives as a `LUID` / vendor-device pair. The contract and the pieces the
//! Linux backends share live in `pf-encode-core`, re-exported whole so
//! `pf_encode_win::*` paths keep compiling.
//! Evidence: `design/windows-video-plane-overhaul.md` §2.6.

pub use pf_encode_core::*;

// D3D11 colour converters + cursor blend, run on the capture device before encode.
#[cfg(target_os = "windows")]
pub mod convert;

// The manual-reset completion event AMF, QSV and Media Foundation hand out through
// `Encoder::ready_event`. Private: it is how those three answer the trait, not an API.
#[cfg(target_os = "windows")]
#[path = "windows/retrieve.rs"]
mod retrieve;
// The LTR slot mirror AMF and QSV share. Pure bookkeeping, so its tests also run on Linux.
#[cfg(any(target_os = "windows", all(test, target_os = "linux")))]
#[path = "windows/ltr.rs"]
mod ltr;
// `#[path]` keeps `crate::*` names flat. Native AMF is unconditional on
// Windows — `amfrt64.dll` at runtime, like NVENC. See `design/native-amf-encoder.md`.
#[cfg(target_os = "windows")]
#[path = "windows/amf.rs"]
pub mod amf;
// Direct-SDK NVENC on D3D11. `nvEncodeAPI64.dll` at runtime, so `--features
// nvenc` is safe on an AMD/Intel box.
#[cfg(all(target_os = "windows", feature = "nvenc"))]
#[path = "windows/nvenc.rs"]
pub mod nvenc;
// Native QSV (VPL): `qsv` feature, vendored dispatcher, GPU runtime from the
// driver store. See `design/native-qsv-encoder.md`.
#[cfg(all(target_os = "windows", feature = "qsv"))]
#[path = "windows/qsv.rs"]
pub mod qsv;
// Media Foundation: the vendor-agnostic rung below the native SDKs, and the only
// hardware encoder on Adreno. `mfplat.dll` is an OS component, so no feature — a hand
// build cannot lose it. See `design/media-foundation-encoder.md`.
#[cfg(target_os = "windows")]
#[path = "windows/mf.rs"]
pub mod mf;
// Windows PyroWave: NV12 D3D11→Vulkan. See `design/pyrowave-windows-host-zerocopy.md`.
#[cfg(all(target_os = "windows", feature = "pyrowave"))]
#[path = "windows/pyrowave.rs"]
pub mod pyrowave;
// The D3D11 frames the wave smokes feed these backends; tests and `test-support` only.
#[cfg(all(target_os = "windows", any(test, feature = "test-support")))]
#[path = "windows/smoke_d3d11.rs"]
pub mod smoke_d3d11;
