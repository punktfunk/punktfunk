//! Video encode behind one [`Encoder`] trait, opened by [`open_video`]. Pin a backend with
//! `PUNKTFUNK_ENCODER`. The Windows backends are `pf-encode-win`'s, re-exported here. Capture to
//! encode is one-way: this crate never depends on capture.
//!
//! - `open`: `open_video`, `OpenParams`, `TrackedEncoder`.
//! - `resolve` and its `resolve/{linux,windows}` arms: `ResolveInputs`, `Resolved`, `resolve()`.
//! - `env_knobs`: `Knobs`, every environment read. `cursor_cache`: when a bitmap uploads again.
//! - `cpu_ingest`: the stage every CPU frame passes. `enc/sw`: openh264.
//! - `enc/linux/nvenc_cuda` (shell), `nvenc_lane` (the lane, blend and retrieve model).
//! - `enc/linux/vulkan_video` (shell), `vk_ring` (ring model), `vk_record` (per-frame stages),
//!   `vk_build` (open builders), `vk_util` (device pick, `ImportCache`), `vk_import` (its policy),
//!   `vk_csc` (CSC and cursor plane), `vk_reframe` (crop and scale), and the vendored
//!   `vk_av1_encode`, `vk_valve_rgb`, `vk_intra_refresh`.
//! - `enc/linux/pyrowave` (shell), `pyrowave_open` (open plan), `pyrowave_remote` (host half),
//!   `worker` (the `punktfunk-encode-worker` half). `enc/linux/vaapi_native`: native VAAPI.
//!
//! Model and shell: enum state, pure `step`, fix-named table rows; a model builds on every target.
//! Resolve once: `resolve()` per host entry, `open_video(&Resolved, ..)`; ADR 0002 in `docs/adr/`.
//! Knobs once: `env_knobs.rs` is the only environment reader.

// The `Encoder` contract and the pieces every backend shares, plus the Windows
// backends, which re-export the same contract. One namespace: `pf_encode::*`.
pub use pf_encode_core::*;
#[cfg(target_os = "windows")]
pub use pf_encode_win::*;

// The cursor upload cache the Linux backends key on. Pure, so its table runs on every target.
#[cfg_attr(
    not(all(
        target_os = "linux",
        feature = "nvenc",
        any(feature = "vulkan-encode", feature = "pyrowave")
    )),
    allow(dead_code)
)]
mod cursor_cache;
// The CPU-frame stage every Linux backend uploads through. Pure, so its table runs on every target.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod cpu_ingest;
mod env_knobs;
mod open;
mod resolve;
pub use open::*;
pub use resolve::*;

/// `quic::CODEC_*` bit → [`Codec`]. Unknown / `0` maps to HEVC (pre-negotiation
/// default). Inverse of [`codec_to_wire`].
pub fn codec_from_wire(bit: u8) -> Codec {
    match bit {
        punktfunk_core::quic::CODEC_H264 => Codec::H264,
        punktfunk_core::quic::CODEC_AV1 => Codec::Av1,
        punktfunk_core::quic::CODEC_PYROWAVE => Codec::PyroWave,
        _ => Codec::H265,
    }
}

pub fn codec_to_wire(codec: Codec) -> u8 {
    match codec {
        Codec::H264 => punktfunk_core::quic::CODEC_H264,
        Codec::H265 => punktfunk_core::quic::CODEC_HEVC,
        Codec::Av1 => punktfunk_core::quic::CODEC_AV1,
        Codec::PyroWave => punktfunk_core::quic::CODEC_PYROWAVE,
    }
}

/// HEVC `chroma_format_idc`: `1` (4:2:0) or `3` (4:4:4). Same numeric
/// value as [`punktfunk_core::quic::Welcome::chroma_format`].
pub fn chroma_idc(chroma: ChromaFormat) -> u8 {
    match chroma {
        ChromaFormat::Yuv420 => punktfunk_core::quic::CHROMA_IDC_420,
        ChromaFormat::Yuv444 => punktfunk_core::quic::CHROMA_IDC_444,
    }
}

/// Wire volume ([`punktfunk_core::quic::HdrMeta`]) → the encoders'
/// [`pf_frame::HdrMeta`]. Same seven fields; both types are foreign here, so
/// a field copy stands in for `From`. A client panel that reports zero primaries,
/// white point or peak keeps the generic HDR10 value there: a 0-nit mastering
/// display tone-maps to black.
pub fn hdr_meta_from_wire(m: punktfunk_core::quic::HdrMeta) -> pf_frame::HdrMeta {
    let g = pf_frame::hdr::generic_hdr10();
    pf_frame::HdrMeta {
        display_primaries: if m.display_primaries == [[0; 2]; 3] {
            g.display_primaries
        } else {
            m.display_primaries
        },
        white_point: if m.white_point == [0; 2] {
            g.white_point
        } else {
            m.white_point
        },
        max_display_mastering_luminance: match m.max_display_mastering_luminance {
            0 => g.max_display_mastering_luminance,
            v => v,
        },
        min_display_mastering_luminance: m.min_display_mastering_luminance,
        max_cll: m.max_cll,
        max_fall: m.max_fall,
    }
}

/// Inverse of [`hdr_meta_from_wire`], for the `0xCE` datagram.
pub fn hdr_meta_to_wire(m: pf_frame::HdrMeta) -> punktfunk_core::quic::HdrMeta {
    punktfunk_core::quic::HdrMeta {
        display_primaries: m.display_primaries,
        white_point: m.white_point,
        max_display_mastering_luminance: m.max_display_mastering_luminance,
        min_display_mastering_luminance: m.min_display_mastering_luminance,
        max_cll: m.max_cll,
        max_fall: m.max_fall,
    }
}

// `#[path]` keeps `crate::*` names flat. The shared NVENC/RFI/policy/PyroWave-wire
// modules arrive through the `pf_encode_core` glob, the Windows backends through `pf_encode_win`.
// Direct-SDK NVENC (CUDA). `.so` at runtime, so `--features nvenc` is safe
// on a driver-less/AMD box. See `design/linux-direct-nvenc.md`.
#[cfg(all(target_os = "linux", feature = "nvenc"))]
#[path = "enc/linux/nvenc_cuda.rs"]
mod nvenc_cuda;
// The lane, blend and retrieve decisions `nvenc_cuda` steps. Pure, so its table runs on every target.
#[cfg_attr(not(all(target_os = "linux", feature = "nvenc")), allow(dead_code))]
#[path = "enc/linux/nvenc_lane.rs"]
mod nvenc_lane;
// Software (openh264) H.264 — the GPU-less Linux path. Windows has none: the driver
// needs a render adapter to exist at all (plan §9-1).
#[cfg(target_os = "linux")]
#[path = "enc/sw.rs"]
mod sw;

// Native VAAPI: reference invalidation, in-place retarget, HEVC Main 10 with
// HDR10. `design/native-vaapi-encoder.md`.
#[cfg(target_os = "linux")]
#[path = "enc/linux/vaapi_native.rs"]
mod vaapi_native;
// Vulkan Video on Linux (AMD/Intel). App-owned DPB (real RFI); on-GPU RGB→NV12
// CSC. Needs `--features vulkan-encode`. See `design/linux-vulkan-video-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vulkan_video.rs"]
mod vulkan_video;
// The ring `vulkan_video` steps. Pure, so its table runs on every target.
#[cfg_attr(
    not(all(target_os = "linux", feature = "vulkan-encode")),
    allow(dead_code)
)]
#[path = "enc/linux/vk_ring.rs"]
mod vk_ring;
// Vendored `VK_KHR_video_encode_av1` — pinned `ash` predates 1.3.290. Do not
// bump `ash` (breaks the SDL/Vulkan client).
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_av1_encode.rs"]
mod vk_av1_encode;
// The VCN firmware's version and the pairs its EFC converts. Pure but for the kernel query.
#[cfg_attr(
    not(all(target_os = "linux", feature = "vulkan-encode")),
    allow(dead_code)
)]
#[path = "enc/linux/vcn_firmware.rs"]
mod vcn_firmware;
pub use vcn_firmware::VcnFirmware;
// Vendored `VK_VALVE_video_encode_rgb_conversion`. Same ash-pin as `vk_av1_encode`.
// See `design/vulkan-rgb-direct-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_valve_rgb.rs"]
mod vk_valve_rgb;
// Vendored `VK_KHR_video_encode_intra_refresh`. Same ash-pin. See `design/vulkan-intra-refresh.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_intra_refresh.rs"]
mod vk_intra_refresh;
// Shared ash helpers (dmabuf import, image/memory, the device pick) for the
// Linux Vulkan backends — plus `sampled_capture_modifiers`, which the VAAPI
// modifier offer needs even without those features.
#[cfg(target_os = "linux")]
#[path = "enc/linux/vk_util.rs"]
mod vk_util;
// The import cache's policy `vk_util` steps. Pure, so its table runs on every target.
#[cfg_attr(
    not(all(
        target_os = "linux",
        any(feature = "vulkan-encode", feature = "pyrowave")
    )),
    allow(dead_code)
)]
#[path = "enc/linux/vk_import.rs"]
mod vk_import;
// The cursor plane and CSC layout PyroWave and Vulkan Video share.
#[cfg(all(
    target_os = "linux",
    any(feature = "vulkan-encode", feature = "pyrowave")
))]
#[path = "enc/linux/vk_csc.rs"]
mod vk_csc;
// The crop and scale ahead of the CSC, which PyroWave and Vulkan Video share.
#[cfg(all(
    target_os = "linux",
    any(feature = "vulkan-encode", feature = "pyrowave")
))]
#[path = "enc/linux/vk_reframe.rs"]
mod vk_reframe;
// PyroWave: Vulkan-compute intra wavelet. Explicit `PUNKTFUNK_ENCODER=pyrowave`.
// See `design/pyrowave-codec-plan.md`.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave.rs"]
mod pyrowave;
// The open plan `pyrowave` runs. Pure, so its table runs on every target.
#[cfg_attr(not(all(target_os = "linux", feature = "pyrowave")), allow(dead_code))]
#[path = "enc/linux/pyrowave_open.rs"]
mod pyrowave_open;
// `punktfunk-encode-worker` holds `CAP_SYS_NICE`; the host must not (a capped
// host is unidentifiable to KWin). `worker` is `pub` for that binary's `main`.
// See `design/gpu-priority-capability-worker.md`.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave_remote.rs"]
mod pyrowave_remote;
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/worker.rs"]
pub mod worker;
// CPU frames the Linux PyroWave and Vulkan Video tests share.
#[cfg(all(
    test,
    target_os = "linux",
    any(feature = "pyrowave", feature = "vulkan-encode")
))]
#[path = "enc/linux/test_frames.rs"]
mod test_frames;
// The LINEAR dmabuf the NVENC and Vulkan Video tests share.
#[cfg(all(
    test,
    target_os = "linux",
    any(feature = "nvenc", feature = "vulkan-encode")
))]
#[path = "enc/linux/test_dmabuf.rs"]
mod test_dmabuf;
// Live tests pairing a `pf_encode_win` backend with the P010 converter's real output.
#[cfg(all(test, target_os = "windows"))]
#[path = "enc/windows/live_tests.rs"]
mod live_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_wire_roundtrip_and_label() {
        for c in [Codec::H264, Codec::H265, Codec::Av1, Codec::PyroWave] {
            assert_eq!(codec_from_wire(codec_to_wire(c)), c);
        }
        assert_eq!(codec_from_wire(0), Codec::H265);
        assert_eq!(Codec::H264.label(), "h264");
        assert_eq!(Codec::H265.label(), "hevc");
        assert_eq!(Codec::Av1.label(), "av1");
        assert_eq!(chroma_idc(ChromaFormat::Yuv420), 1);
        assert_eq!(chroma_idc(ChromaFormat::Yuv444), 3);
    }

    /// Every field must survive wire → frame → wire; a field the copy forgets
    /// would silently zero the SEI the encoder emits.
    #[test]
    fn hdr_meta_wire_roundtrip_keeps_every_field() {
        let wire = punktfunk_core::quic::HdrMeta {
            display_primaries: [[1, 2], [3, 4], [5, 6]],
            white_point: [7, 8],
            max_display_mastering_luminance: 9,
            min_display_mastering_luminance: 10,
            max_cll: 11,
            max_fall: 12,
        };
        let frame = hdr_meta_from_wire(wire);
        assert_eq!(frame.display_primaries, [[1, 2], [3, 4], [5, 6]]);
        assert_eq!(frame.white_point, [7, 8]);
        assert_eq!(frame.max_display_mastering_luminance, 9);
        assert_eq!(frame.min_display_mastering_luminance, 10);
        assert_eq!(frame.max_cll, 11);
        assert_eq!(frame.max_fall, 12);
        assert_eq!(hdr_meta_to_wire(frame), wire);
        assert_eq!(
            hdr_meta_to_wire(pf_frame::HdrMeta::default()),
            punktfunk_core::quic::HdrMeta::default()
        );
    }

    /// A panel that reports no volume must not master the stream at 0 nits.
    #[test]
    fn hdr_meta_from_wire_fills_an_unreported_volume() {
        let g = pf_frame::hdr::generic_hdr10();
        let frame = hdr_meta_from_wire(punktfunk_core::quic::HdrMeta {
            max_cll: 700,
            ..Default::default()
        });
        assert_eq!(frame.display_primaries, g.display_primaries);
        assert_eq!(frame.white_point, g.white_point);
        assert_eq!(
            frame.max_display_mastering_luminance,
            g.max_display_mastering_luminance
        );
        assert_eq!(frame.min_display_mastering_luminance, 0);
        assert_eq!((frame.max_cll, frame.max_fall), (700, 0));
    }

    /// [`TerminalEncoderError`] must stay downcastable through `context` layers.
    /// A `format!`/stringify on any layer would break the reset ladder.
    #[test]
    fn terminal_encoder_error_survives_the_context_chain() {
        use anyhow::Context as _;
        let site: anyhow::Error = anyhow::Error::new(TerminalEncoderError)
            .context("capture device's adapter is not an Intel VPL implementation");
        let bubbled = Err::<(), _>(site)
            .context("QSV lazy bring-up")
            .context("encoder submit")
            .unwrap_err();
        assert!(bubbled.downcast_ref::<TerminalEncoderError>().is_some());
        assert!(format!("{bubbled:#}").contains("not an Intel VPL implementation"));
    }
}
