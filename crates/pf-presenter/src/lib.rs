//! Vulkan session presenter: SDL3 window + ash swapchain over the shared session
//! pump. Captures input on the `ui_stream` state machine and prints the unified
//! stats window on stdout. No UI toolkit in the crate graph.
//!
//! Three frame paths, all placed by `video_fit`: software (`CpuPlanarFrame` — I420 planes
//! staged into three R8 images, then the same CICP-driven CSC pass as hardware),
//! Vulkan Video (the decoder's VkImage on this device), and on Linux VAAPI
//! (NV12 dmabuf imported per-plane — `dmabuf.rs`). Missing import extensions,
//! or an import/present failure streak, demote the decoder via the pump's
//! `force_software` contract, same as the GTK presenter.
//!
//! Linux and Windows. `dmabuf` is Linux-only (no DRM-PRIME on Windows);
//! `d3d11` is the Windows counterpart (D3D11VA shared-texture import). Decode
//! chain there is Vulkan → D3D11VA → software.

// Unsafe-proof program: every `unsafe {}` in this crate carries a `// SAFETY:` proof.

// VULKAN CONTRACT. CREATE/ALLOCATE: this type owns the live device; CreateInfo
// outlives the call; Drop destroys the handle. RECORD: into a buffer we own and
// have begun. DESTROY: GPU must not still be using the object (fence / idle /
// retired swapchain) — that is the per-site proof. Anything else needs its own.

#[cfg(any(target_os = "linux", windows))]
pub(crate) mod csc;
#[cfg(any(target_os = "linux", windows))]
pub mod cursor;
#[cfg(windows)]
pub(crate) mod d3d11;
#[cfg(target_os = "linux")]
pub(crate) mod dmabuf;
#[cfg(any(target_os = "linux", windows))]
pub mod input;
#[cfg(any(target_os = "linux", windows))]
pub mod keymap_sdl;
#[cfg(any(target_os = "linux", windows))]
pub mod overlay;
#[cfg(any(target_os = "linux", windows))]
mod present_pace;
#[cfg(any(target_os = "linux", windows))]
mod run;
#[cfg(any(target_os = "linux", windows))]
pub(crate) mod scale;
// Pure scroll-frame accumulation — no Wayland or Vulkan dependency.
pub mod scroll;
#[cfg(any(target_os = "linux", windows))]
mod scroll_routing;
// Pure gesture logic with no SDL or Vulkan dependency: built (and tested) on every platform.
pub mod touch;
#[cfg(any(target_os = "linux", windows))]
pub mod vk;
#[cfg(target_os = "linux")]
pub mod wayland_scroll;
#[cfg(windows)]
mod win32;
#[cfg(target_os = "linux")]
pub mod wl_feedback;
#[cfg(target_os = "linux")]
pub mod wl_native;

#[cfg(any(target_os = "linux", windows))]
pub use run::{run_browse, run_session, ActionOutcome, Outcome, SessionOpts};
