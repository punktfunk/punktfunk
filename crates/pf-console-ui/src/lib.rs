//! Skia console UI: one shell — home, library, settings, add-host, pairing —
//! drawn onto whatever `skia_safe::Canvas` a host hands it. Design:
//! `linux-client-rearchitecture.md`, `android-skia-console-port.md`.
//!
//! Two hosts sit on the portable [`Console`] driver:
//! - the Vulkan session's [`SkiaOverlay`] (feature `vulkan-overlay`) — an
//!   [`Overlay`](pf_presenter::overlay::Overlay) on the presenter's device,
//!   offscreen RGBA, composited as one premultiplied quad. Skia never
//!   touches the swapchain; the overlay draws only when it has something
//!   to show (`skia_overlay.rs`);
//! - the Android GL host (`clients/android/native`), which owns EGL and
//!   drives [`Console`] with `default-features = false`.
//!
//! Everything but `skia_overlay.rs` is platform-free: screens draw to
//! `&Canvas`, settings through [`store::SettingsStore`], keys as
//! [`input::Key`], platform rows as [`platform::Platform`].
//!
//! The drawing kit — [`theme`], [`widgets`], [`icons`], [`glyphs`], [`anim`],
//! [`pointer`], the mark tables — is `pub` for one consumer: the webOS
//! pointer UI (`webos-pointer-ui-overhaul.md` D3). No stability promise; a
//! kit change there is a re-pin plus a compile fix, by design.

// The shell runs on Linux, Windows, Android, wasm and Apple's platforms (Metal there; the
// Mac also runs the tests). Cargo.toml gates every dependency on the same list.
#![cfg(any(
    target_os = "linux",
    windows,
    target_os = "android",
    target_vendor = "apple",
    target_family = "wasm"
))]

pub mod anim;
pub mod art_stats;
pub mod brand;
// The sort/group policy moved to pf-client-core so the GTK and WinUI dialogs share it rather
// than growing a second order. Aliased here because every screen names `crate::collate`.
pub(crate) use pf_client_core::collate;
pub mod console;
pub mod el;
pub mod glyphs;
pub mod icons;
pub mod input;
pub mod launcher_icons;
pub mod library;
pub mod model;
pub mod os_marks;
pub mod os_theme;
pub mod platform;
pub mod pointer;
// In-stream ring: the desktop overlay hosts it, and so does a GL shell (webOS) through
// [`Ring`]. Android draws it only as the settings editor. The host-action cache is
// desktop-gated and is not consulted elsewhere.
mod ring;
mod screens;
mod shell;
#[cfg(all(any(target_os = "linux", windows), feature = "vulkan-overlay"))]
mod skia_overlay;
/// The settings rows' engine — ids, platform gate, spec, step — for a shell that lays the
/// same rows out its own way (the webOS pointer UI's page map). Same kit terms as
/// [`widgets`]: one consumer, no stability promise.
pub mod settings_rows {
    pub use crate::screens::settings::{adjust, detail, row_applies, row_on, row_spec, RowId};
    pub use crate::screens::Ctx;
}
pub mod store;
pub mod theme;
pub mod widgets;

pub use art_stats::{art_stats, ArtStats};
pub use console::{Console, ConsoleEntry, ConsoleHandles, InputSource, Insets, Viewport};
pub use input::Key;
pub use library::decode_poster_off_thread;
pub use library::{DecodedPoster, LibraryGame, LibraryPhase, LibraryShared, Stale};
pub use model::{
    ConsoleBus, ConsoleCmd, ConsoleShared, HostAction, HostRow, PairPhase, PresetChip, SpeedPhase,
    SpeedStatus, WakeStatus,
};
pub use platform::{Platform, PlatformScreen};
pub use ring::Ring;
pub use shell::{ConsoleOptions, DeviceScreen, DEFAULT_GPU_CACHE_BYTES, MIN_GPU_CACHE_BYTES};
#[cfg(all(any(target_os = "linux", windows), feature = "vulkan-overlay"))]
pub use skia_overlay::SkiaOverlay;
pub use store::{SettingsStore, SnapshotStore};
