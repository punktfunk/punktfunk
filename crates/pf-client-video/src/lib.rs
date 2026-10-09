//! Client video decode: access units in, pictures on the presenter's device out.
//!
//! `desktop` builds the ladder on Linux and Windows. Its items sit at this crate's root,
//! which pf-client-core re-exports as `video`. Without `desktop` the crate is the colour
//! vocabulary, the `VkDevice` handoff, the presenter's SPIR-V and PyroWave decode: the
//! modules Android links directly. webOS, Apple and wasm never build it.

// Every `unsafe` block and `unsafe impl` in this crate carries a `// SAFETY:` proof.

// Decoder-input capture behind `PUNKTFUNK_DUMP_VIDEO`, written by the session pump.
#[cfg(desktop)]
pub mod au_dump;
// The ladder. Every item is re-exported at the root, so `pf_client_core::video::X` is `X` here.
#[cfg(desktop)]
mod video;
#[cfg(desktop)]
pub use video::*;
// What this client can decode and advertise: rung evidence, admission and the Hello gates.
// `video` re-exports every item.
#[cfg(desktop)]
mod video_caps;
// Decode counters, picture shape, and the DXGI driver-version split.
// Built for `desktop`, or Windows `d3d11va` alone. `video` re-exports them
// when the ladder is built; this module is the path when it is not.
#[cfg(any(desktop, all(feature = "d3d11va", windows)))]
pub mod video_types;
// Colour vocabulary + the CSC coefficient rows. Portable (no ash, no decode ladder): the
// PyroWave lane needs them on Android too, where `video` itself is not built.
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod video_color;
// The `VkDevice` handoff + shared queue lock. `video` re-exports both, so desktop call
// sites are unchanged; Android names this module directly.
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod video_vk;
// Committed SPIR-V for the presenter shaders. Here rather than in pf-presenter because the
// Android PyroWave lane builds the same planar CSC pipeline without that crate.
#[cfg(any(target_os = "linux", windows, target_os = "android"))]
pub mod video_csc_spv;
#[cfg(desktop)]
mod video_software;
// Native VAAPI: pf-vaapi plans into dlopen'd libva, DRM-PRIME dmabufs for the presenter.
// Only VAAPI rung; `auto` reaches it when vendor order puts VAAPI first, or pin `PUNKTFUNK_DECODER=native-vaapi`. Evidence: `video`.
#[cfg(all(desktop, target_os = "linux"))]
pub mod video_vaapi_native;
// V4L2 decode: the hardware rung of SoCs with no Vulkan Video and no VA-API. `auto` reaches it after both; pin `PUNKTFUNK_DECODER=native-v4l2`.
#[cfg(all(desktop, target_os = "linux"))]
mod video_v4l2;
// The stateless half of that rung: HEVC on decoders that take parsed slices (Raspberry Pi 5, RK3588).
#[cfg(all(desktop, target_os = "linux"))]
mod video_v4l2_hevc;
// Native Vulkan Video (H.264/H.265/AV1) on the presenter's device. Auto's top rung on both desktop OSes; pin `PUNKTFUNK_DECODER=native-vulkan`. Evidence: `video`.
#[cfg(desktop)]
mod video_vk_native;
// D3D11 decode-device: shareable-texture hand-off ring, device creation, `display_hdr_volume`. `video_d3d11_native` and `clients/session` build on it.
#[cfg(all(feature = "d3d11va", windows))]
pub mod video_d3d11;
// Native D3D11VA: `ID3D11VideoDecoder` from pf-bitstream plans into `video_d3d11`'s hand-off ring.
// Only DXVA rung; in `auto` for H.264/H.265/AV1. Pin `PUNKTFUNK_DECODER=native-d3d11va`. Evidence: `video`.
#[cfg(all(feature = "d3d11va", windows))]
pub mod video_d3d11_native;
// PyroWave: Vulkan compute on the device the frame is presented from (no fds, no dmabuf,
// no D3D11 interop). Linux + Windows + Android; Apple Metal is a separate port.
// 64-bit Android only, mirroring pyrowave-sys's own gate: Vulkan's armv7 calling
// convention has no bindgen representation, so the sys crate is an empty stub there.
#[cfg(all(
    any(
        target_os = "linux",
        windows,
        all(target_os = "android", target_pointer_width = "64")
    ),
    feature = "pyrowave"
))]
pub mod video_pyrowave;
