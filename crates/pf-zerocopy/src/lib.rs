//! Linux GPU zero-copy plumbing: shared CUDA context and device buffers, EGL/Vulkan dmabuf
//! importers, the isolated import-worker subprocess, and zero-copy policy latches. Linux-only; on
//! other targets this crate is an empty lib so dependents can take a plain (non-target-gated)
//! dependency. The dmabuf fence wait lives in `pf-dmabuf`.
//!
//! The DRM FourCC codes and fourcc → VkFormat live here (`drm`). `PixelFormat ↔ DRM FourCC`
//! (`drm_fourcc`) does not: it consumes the shared frame vocabulary above this crate. This crate provides the `DeviceBuffer` that vocabulary's
//! `FramePayload::Cuda` owns.

// Every `unsafe {}` / `unsafe impl` carries a `// SAFETY:` proof; `unsafe fn` bodies use
// explicit blocks. Both lints are in the workspace `[workspace.lints]` tables.

#[cfg(target_os = "linux")]
mod imp;
#[cfg(target_os = "linux")]
pub use imp::*;
