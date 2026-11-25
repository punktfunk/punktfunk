//! The session presenter for Linux and Windows: an SDL3 window and a Vulkan (or DXGI) swapchain
//! over the shared session pump, with input capture, the console overlay and the stats line.
//!
//! The shell: `run/` (the session loop, events, the paced drain, restarts, further screens).
//! It draws through `backend`: `vk/`, or on Windows the DXGI presenter `d3d/`.
//! The Vulkan shell: `vk/` (bring-up, the targets, the frame, the glass clock and three of its
//! instruments; `wl_feedback` is the fourth), the passes `csc` and `scale`, the imports
//! `dmabuf` (Linux) and `d3d11` (Windows). The models: `vk/reconfig_model`, `vk/glass`'s
//! lifecycle, `run/pace_step`, `run/restart`, `run/route`, `present_pace`, `slot_pick`,
//! `swapchain_state`, `cursor_model`, `scroll`, `touch`. The env: `knobs`. The lane:
//! `wl_native` puts a dma-buf on the window itself (Linux), fed by `vk/export_ring`. Input:
//! `input`, `keymap_sdl`, `scroll_routing`, `wayland_scroll`; `cursor` draws the host's
//! pointer. The console draws through `overlay::Overlay`; `wl_side` and `win32` are glue.
//!
//! Model and shell: a model is an enum state, a pure `step` over plain values and a table
//! of named rows; the shell samples, calls `step` and performs the answer. A transition is
//! decided in its model, never inline in the shell.
//! Knobs: only `knobs::read`, called once at `Shell::open` or by `probe_backend`, reads a
//! `PUNKTFUNK_*` variable; everything else takes a `Knobs` field.
//! Vulkan: the contract below holds at every site; a site that needs more proves it there.

// VULKAN CONTRACT. CREATE/ALLOCATE: this type owns the live device; CreateInfo
// outlives the call; Drop destroys the handle. RECORD: into a buffer we own and
// have begun. DESTROY: GPU must not still be using the object (fence / idle /
// retired swapchain) — that is the per-site proof. Anything else needs its own.

// The backend enum, and its pure picks built (and tested) on every platform.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod backend;
#[cfg(any(target_os = "linux", windows))]
pub(crate) mod csc;
#[cfg(any(target_os = "linux", windows))]
pub mod cursor;
// Pure cursor decisions: built (and tested) on every platform, used by `run`.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod cursor_model;
#[cfg(windows)]
mod d3d;
#[cfg(windows)]
pub(crate) mod d3d11;
// The picture size a connect sends: built (and tested) on every platform, used by `run`.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod display_size;
// The D3D11 glass clock; its statistics rules are built (and tested) on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
#[path = "d3d/glass.rs"]
mod d3d_glass;
#[cfg(target_os = "linux")]
pub(crate) mod dmabuf;
// The glass clock; its lifecycle and rank are built (and tested) on every platform.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
#[path = "vk/glass.rs"]
mod glass;
// The D3D11 backend's swapchain format rule: built (and tested) on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
#[path = "d3d/hdr_model.rs"]
mod hdr_model;
#[cfg(any(target_os = "linux", windows))]
pub mod input;
#[cfg(any(target_os = "linux", windows))]
pub mod keymap_sdl;
// The env knobs, read once per open: built on every platform, used by `run` and `vk`.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod knobs;
#[cfg(any(target_os = "linux", windows))]
pub mod overlay;
#[cfg(any(target_os = "linux", windows))]
mod present_pace;
// Pure swapchain transitions: built (and tested) on every platform, performed by `vk`.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
#[path = "vk/reconfig_model.rs"]
mod reconfig_model;
#[cfg(any(target_os = "linux", windows))]
mod run;
// Pure event routing: built (and tested) on every platform, performed by `run`.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
#[path = "run/route.rs"]
mod route;
#[cfg(any(target_os = "linux", windows))]
pub(crate) mod scale;
// The D3D11 backend's slice-view cache and uv rule: built (and tested) on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
#[path = "d3d/slice_model.rs"]
mod slice_model;
// Pure scroll-frame accumulation — no Wayland or Vulkan dependency.
pub mod scroll;
#[cfg(any(target_os = "linux", windows))]
mod scroll_routing;
// Pure swapchain bookkeeping: built (and tested) on every platform, used by `vk`.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod swapchain_state;
// Pure slot pick for the smoothness drain: built (and tested) on every platform.
#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
mod slot_pick;
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
#[cfg(target_os = "linux")]
mod wl_side;

#[cfg(any(target_os = "linux", windows))]
pub use backend::probe_backend;
#[cfg(any(target_os = "linux", windows))]
pub use run::{run_browse, run_session, ActionOutcome, Outcome, SessionOpts};
