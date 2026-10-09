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

// The session pump's decoder-input capture, so `session` keeps naming `crate::au_dump`.
#[cfg(desktop)]
use pf_client_video::au_dump;
#[cfg(all(desktop, target_os = "linux"))]
pub mod audio;
#[cfg(all(desktop, windows))]
#[path = "audio_wasapi.rs"]
pub mod audio;
// Playback counters both audio backends publish. Atomics only: the PipeWire callback is the graph's realtime loop.
#[cfg(desktop)]
pub mod audio_vitals;
// Priority for threads that feed the device callbacks (decode, pad-audio, WASAPI). rtkit / Realtime portal on Linux, MMCSS on Windows.
#[cfg(desktop)]
pub mod audio_rt;
// mDNS browse of `_punktfunk._udp`. Android folds the same events behind its JNI poll.
#[cfg(all(
    feature = "discovery",
    any(target_os = "linux", windows, target_os = "android")
))]
pub mod discovery;
#[cfg(desktop)]
pub mod gamepad;
// Menu-event synthesizer and pad descriptors. Desktop `gamepad` re-exports them; Android feeds the same synthesizer from Kotlin samples (`design/android-skia-console-port.md`).
#[cfg(portable)]
pub mod menu_nav;
// Audio-format vocabulary (`session` re-exports). Split out so the platform-bound modules stay platform-bound.
#[cfg(portable)]
pub mod audio_format;
// Stored decoder-pref migration, for the Skia settings screen on every target.
#[cfg(portable)]
pub use punktfunk_core::decoder_pref;
// Console actions, pointer input, and session phases. Shared by the Vulkan overlay and the Android GL host.
#[cfg(portable)]
pub mod console;
#[cfg(all(desktop, target_os = "linux"))]
pub mod keymap;
// Library model (`GameEntry`, `Artwork`, running set) is portable; the ureq fetches stay desktop-gated.
#[cfg(portable)]
pub mod library;
// Library sort/group policy, shared by every Rust shell so the console and the two desktop
// dialogs cannot drift into three orders. Apple and Android port the same file.
#[cfg(portable)]
pub mod collate;
// Per-host catalog cache, so a library screen has titles to show while a sleeping host boots.
#[cfg(desktop)]
pub mod library_cache;
// Poster bytes on disk behind the shells' texture maps, so the cached catalog above has covers.
#[cfg(desktop)]
pub mod art_cache;
/// A network check's findings in words, for every shell.
pub mod findings;
// Host power actions (`design/host-actions.md`). Android gets the row type and labels; ureq stays desktop-gated (Android uses OkHttp).
#[cfg(portable)]
pub mod host_actions;
// Log ring (note/render, std only) on every platform. `send_to_host` stays desktop-gated; Android posts via OkHttp (`SkiaConsole.sendLogs`).
#[cfg(portable)]
pub mod logring;
// `punktfunk://` grammar (`design/client-deep-links.md`). One parser/emitter, held to the Swift/Kotlin ports by a shared vector file.
#[cfg(portable)]
pub mod deeplink;
// Where a bare launch opens (`design/default-host.md`). One resolver, held to the Swift/Kotlin ports by a shared vector file.
#[cfg(portable)]
pub mod start;
// Connect, the wake state machine, and the session spawn + stdout contract (`design/client-architecture-split.md`).
#[cfg(desktop)]
pub mod orchestrate;
// Session grant snapshot, overlay chip, and AccessUpdate toast (`design/per-client-access.md`).
// Presentation only; Apple/Android mirror the rules. Gated with session: macOS has no punktfunk-core to name the grants.
#[cfg(desktop)]
pub mod access;
// Host OS-identity from mDNS `os=` TXT: sanitize + icon-walk order. Apple/Android mirror it rather than link it.
pub mod os;
// Real gamescope compositor check. Built everywhere so callers stay cfg-free; off Linux the answer is no.
pub mod gamescope;
// Gamescope overlay-owns-controller signal. SDL's focus gate cannot provide it in Gaming Mode; this drives the gamepad input mask.
#[cfg(all(desktop, target_os = "linux"))]
pub mod overlay_focus;
// The desktop's reduce-motion switch, for the console to follow.
#[cfg(desktop)]
pub mod os_prefs;
// Omarchy theme (state-dir file + palette). GTK recolour and the session follow-system palette both build from it.
#[cfg(all(desktop, target_os = "linux"))]
pub mod omarchy;
// Opt-in Omarchy menu rows (Super+Space), synced from the known-hosts store by every binary that mutates it.
#[cfg(all(desktop, target_os = "linux"))]
pub mod omarchy_menu;
// Lucide path data shared by Skia and GTK so a mark cannot differ between them.
pub mod lucide;
pub mod overlay_actions;
// sRGB mixes and WCAG contrast for every client theme, the console's included.
pub mod rgb;
pub mod ring;
// The in-stream keys, chords and gestures, for each client's reference screen.
pub mod shortcuts;
// DualSense voice-coil + speaker on the pad's 4-ch device (0xD1 plane): correlation, per-session renderer, tier-A registry the gamepad worker feeds.
#[cfg(desktop)]
pub mod pad_audio;
// One bounded PipeWire registry query, for the device pickers and the pad-audio graph walks.
#[cfg(all(desktop, target_os = "linux"))]
mod pw_oneshot;
// Raw HID beside an SDL slot: Steam Controller 2 passthrough, the descriptor log, the DualSense Bluetooth audio writer.
#[cfg(desktop)]
mod sc2_capture;
// Override catalog + connect-time resolver (`design/client-settings-profiles.md`). Bindings live on `trust`'s host records.
#[cfg(portable)]
pub mod presets;
#[cfg(desktop)]
pub mod session;
// The `Settings` model and the preset resolver; `trust` re-exports them.
#[cfg(portable)]
pub mod settings;
// One decode-less connect and one host burst — the shared half of every "Test network
// speed…" row. Desktop-gated with `video`, whose codec advertisement the probe connect
// sends; Android measures through its own JNI session instead.
#[cfg(desktop)]
pub mod speed;
// The XDG and System32 path rules, as the host's pf-paths applies them.
mod paths;
#[cfg(portable)]
pub mod trust;
// Profiles on a box and the picker rule every shell shares (`design/profiles-and-seats.md` §10).
#[cfg(portable)]
pub mod profiles;
// The `host_sort` / `host_grouping` order every shell on a device shares.
#[cfg(portable)]
pub mod host_order;
// A library's sections, favorites and play captions, shared the same way.
#[cfg(portable)]
pub mod library_layout;
// Client half of the signed-manifest update check (`design/host-update-from-web-console.md`).
// Linux only: Windows ships inside the host installer, macOS through `clients/apple`.
#[cfg(all(desktop, target_os = "linux"))]
pub mod update;
// OS clipboard bridge (`design/clipboard-and-file-transfer.md`). Session clients; Windows-real, stub elsewhere.
#[cfg(desktop)]
pub mod clipboard;
// The decode ladder lives in pf-client-video; these keep the `video` and `video_*` paths.
#[cfg(desktop)]
pub use pf_client_video as video;
#[cfg(all(desktop, feature = "pyrowave"))]
pub use pf_client_video::video_pyrowave;
#[cfg(all(desktop, target_os = "linux"))]
pub use pf_client_video::video_vaapi_native;
#[cfg(any(desktop, all(feature = "d3d11va", windows)))]
pub use pf_client_video::{video_color, video_csc_spv, video_types, video_vk};
#[cfg(all(feature = "d3d11va", windows))]
pub use pf_client_video::{video_d3d11, video_d3d11_native};

pub mod wol;

/// Explicit-off for a client `PUNKTFUNK_*` var, the host's grammar: trimmed, case-insensitive
/// `0`/`false`/`off`/`no` are off, any other present value is on, unset is `None`. A kill
/// switch reads `!= Some(false)`, an opt-in `== Some(true)`.
pub fn env_on(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| is_on(&v))
}

fn is_on(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

#[cfg(test)]
mod tests {
    /// The host reads `PUNKTFUNK_UPDATE_CHECK=no` as off; the client must too.
    #[test]
    fn env_switches_read_the_hosts_off_grammar() {
        for off in ["0", "false", "off", "no", "OFF", " No ", "0 "] {
            assert!(!super::is_on(off), "{off:?}");
        }
        for on in ["1", "true", "yes", "", "garbage"] {
            assert!(super::is_on(on), "{on:?}");
        }
    }
}
