//! Kernel dma-buf helpers that need no GPU stack: the implicit-fence wait ([`fence`]), a
//! read-only mapping ([`ReadMap`]) and the i915 decode-clock wait ([`i915_boost`]). Linux-only;
//! on other targets this crate is an empty lib.

// Every `unsafe {}` carries a `// SAFETY:` proof (workspace `[workspace.lints]`).

/// Wait for a dmabuf's implicit read-ready fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE` + poll).
#[cfg(target_os = "linux")]
pub mod fence;
/// The `I915_GEM_WAIT` that keeps Intel's media engine clocked while it decodes.
#[cfg(target_os = "linux")]
pub mod i915_boost;
#[cfg(target_os = "linux")]
mod map;
#[cfg(target_os = "linux")]
pub use map::{byte_len, ReadMap, Share};
