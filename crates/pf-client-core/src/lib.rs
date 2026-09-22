//! UI-agnostic client plumbing for the desktop shells and the Vulkan session binary
//! (Linux and Windows). Apple targets build only the portable modules the console crate
//! needs; `clients/apple` is the client there.
//!
//! Nothing here may depend on a UI toolkit. Frames reach the screen through `session`'s
//! `SessionHandle` channels and `video`'s `DecodedImage` (RGBA, dmabuf fds, or a decoded
//! VkImage).
//!
//! Audio is the one per-OS swap: `audio.rs` (PipeWire) vs `audio_wasapi.rs` (WASAPI),
//! selected by `#[path]` so the session pump only names `crate::audio`. `keymap` stays
//! Linux; the session path uses pf-presenter's SDL-scancode table.

// Every `unsafe` block and `unsafe impl` in this crate carries a `// SAFETY:` proof.

#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
mod au_dump;
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod audio;
#[cfg(all(feature = "desktop", windows))]
#[path = "audio_wasapi.rs"]
pub mod audio;
// Playback counters both audio backends publish. Atomics only: the PipeWire callback is the graph's realtime loop.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod audio_vitals;
// Priority for threads that feed the device callbacks (decode, pad-audio, WASAPI). rtkit / Realtime portal on Linux, MMCSS on Windows.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod audio_rt;
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod discovery;
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod gamepad;
// Menu-event synthesizer and pad descriptors. Desktop `gamepad` re-exports them; Android feeds the same synthesizer from Kotlin samples (`design/android-skia-console-port.md`).
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod menu_nav;
// Audio-format vocabulary (`session` re-exports) and decoder-preference migration (`video` re-exports). Split out so the platform-bound modules stay platform-bound.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod audio_format;
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod decoder_pref;
// Console actions, pointer input, and session phases. Shared by the Vulkan overlay and the Android GL host.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod console;
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod keymap;
// Library model (`GameEntry`, `Artwork`, running set) is portable; the ureq fetches stay desktop-gated.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod library;
// Library sort/group policy, shared by every Rust shell so the console and the two desktop
// dialogs cannot drift into three orders. Apple and Android port the same file.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod collate;
// Per-host catalog cache, so a library screen has titles to show while a sleeping host boots.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod library_cache;
// Poster bytes on disk behind the shells' texture maps, so the cached catalog above has covers.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod art_cache;
// Host power actions (`design/host-actions.md`). Android gets the row type and labels; ureq stays desktop-gated (Android uses OkHttp).
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod host_actions;
// Log ring (note/render, std only) on every platform. `send_to_host` stays desktop-gated; Android posts via OkHttp (`SkiaConsole.sendLogs`).
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod logring;
// `punktfunk://` grammar (`design/client-deep-links.md`). One parser/emitter, held to the Swift/Kotlin ports by a shared vector file.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod deeplink;
// Where a bare launch opens (`design/default-host.md`). One resolver, held to the Swift/Kotlin ports by a shared vector file.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod start;
// Connect, the wake state machine, and the session spawn + stdout contract (`design/client-architecture-split.md`).
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod orchestrate;
// Session grant snapshot, overlay chip, and AccessUpdate toast (`design/per-client-access.md`).
// Presentation only; Apple/Android mirror the rules. Gated with session: macOS has no punktfunk-core to name the grants.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod access;
// Host OS-identity from mDNS `os=` TXT: sanitize + icon-walk order. Apple/Android mirror it rather than link it.
pub mod os;
// Real gamescope compositor check. Built everywhere so callers stay cfg-free; off Linux the answer is no.
pub mod gamescope;
// Gamescope overlay-owns-controller signal. SDL's focus gate cannot provide it in Gaming Mode; this drives the gamepad input mask.
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod overlay_focus;
// Omarchy theme (state-dir file + palette). GTK recolour and the session follow-system palette both build from it.
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod omarchy;
// Opt-in Omarchy menu rows (Super+Space), synced from the known-hosts store by every binary that mutates it.
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod omarchy_menu;
// Lucide path data shared by Skia and GTK so a mark cannot differ between them.
pub mod lucide;
pub mod overlay_actions;
pub mod ring;
// DualSense voice-coil + speaker on the pad's 4-ch device (0xD1 plane): correlation, per-session renderer, tier-A registry the gamepad worker feeds.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod pad_audio;
// Steam Controller 2 raw passthrough beside its SDL slot.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
mod sc2_capture;
// Override catalog + connect-time resolver (`design/client-settings-profiles.md`). Bindings live on `trust`'s host records.
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod presets;
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod session;
// One decode-less connect and one host burst — the shared half of every "Test network
// speed…" row. Desktop-gated with `video`, whose codec advertisement the probe connect
// sends; Android measures through its own JNI session instead.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod speed;
#[cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]
pub mod trust;
// Client half of the signed-manifest update check (`design/host-update-from-web-console.md`).
// Linux only: Windows ships inside the host installer, macOS through `clients/apple`.
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod update;
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod video;
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
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
mod video_software;
// Native VAAPI: pf-vaapi plans into dlopen'd libva, DRM-PRIME dmabufs for the presenter.
// Only VAAPI rung; `auto` reaches it when vendor order puts VAAPI first, or pin `PUNKTFUNK_DECODER=native-vaapi`. Evidence: `video`.
#[cfg(all(feature = "desktop", target_os = "linux"))]
pub mod video_vaapi_native;
// Native Vulkan Video (H.264/H.265/AV1) on the presenter's device. Auto's top rung on both desktop OSes; pin `PUNKTFUNK_DECODER=native-vulkan`. Evidence: `video`.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
mod video_vk_native;
// OS clipboard bridge (`design/clipboard-and-file-transfer.md`). Session clients; Windows-real, stub elsewhere.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub mod clipboard;
// D3D11 decode-device: shareable-texture hand-off ring, device creation, `display_hdr_volume`. `video_d3d11_native` and `clients/session` build on it.
#[cfg(all(feature = "desktop", windows))]
pub mod video_d3d11;
// Native D3D11VA: `ID3D11VideoDecoder` from pf-bitstream plans into `video_d3d11`'s hand-off ring.
// Only DXVA rung; in `auto` for H.264/H.265/AV1. Pin `PUNKTFUNK_DECODER=native-d3d11va`. Evidence: `video`.
#[cfg(all(feature = "desktop", windows))]
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

pub mod wol;
