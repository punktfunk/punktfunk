//! libgbm, declared once: a device on an open render node and the buffers allocated on it.
//! Callers keep their own allocation policy (modifier fallback, plane count) on top.

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut c_void;
    fn gbm_device_destroy(device: *mut c_void);
    fn gbm_bo_create(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_create_with_modifiers2(
        device: *mut c_void,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
        flags: u32,
    ) -> *mut c_void;
    fn gbm_bo_destroy(bo: *mut c_void);
    fn gbm_bo_get_fd(bo: *mut c_void) -> i32;
    fn gbm_bo_get_stride(bo: *mut c_void) -> u32;
    fn gbm_bo_get_offset(bo: *mut c_void, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut c_void) -> u64;
    fn gbm_bo_get_plane_count(bo: *mut c_void) -> i32;
}

/// The buffer is rendered into by a GPU.
pub const GBM_BO_USE_RENDERING: u32 = 1 << 2;
/// `gbm_bo_get_modifier` for a buffer the driver laid out implicitly.
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A `gbm_device` plus the render node it borrows. Drop destroys the device, then closes
/// the node. Not `Send`: an owner that moves it across threads states why that is sound.
pub struct GbmDevice {
    raw: *mut c_void,
    _node: OwnedFd,
}

impl GbmDevice {
    /// A device on `node`, an open DRM render node.
    pub fn open(node: impl Into<OwnedFd>) -> Result<GbmDevice> {
        let node = node.into();
        // SAFETY: `gbm_create_device` takes the fd by value and returns a device or null
        // (checked). The device borrows the fd, which `_node` keeps open until after
        // `gbm_device_destroy` in `Drop`.
        let raw = unsafe { gbm_create_device(node.as_raw_fd()) };
        if raw.is_null() {
            bail!("gbm_create_device");
        }
        Ok(GbmDevice { raw, _node: node })
    }

    /// The `gbm_device*`, for EGL's GBM platform display. Valid while `self` lives.
    pub fn as_ptr(&self) -> *mut c_void {
        self.raw
    }
}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        // SAFETY: `raw` is the non-null device from `gbm_create_device`, owned here and
        // destroyed once, before `_node` closes. Every bo on it holds an `Rc` to this device,
        // so none outlives it.
        unsafe { gbm_device_destroy(self.raw) };
    }
}

/// One buffer object and its first plane's layout. It keeps its device alive.
pub struct GbmBo {
    raw: *mut c_void,
    /// Owned once; `gbm_bo_get_fd` hands out a fresh descriptor.
    pub fd: OwnedFd,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub planes: i32,
    _device: Rc<GbmDevice>,
}

impl GbmBo {
    /// Allocate `width`×`height` in `fourcc`, laid out by one of `modifiers`; an empty list
    /// lets the driver choose implicitly.
    pub fn alloc(
        device: &Rc<GbmDevice>,
        width: u32,
        height: u32,
        fourcc: u32,
        modifiers: &[u64],
        flags: u32,
    ) -> Result<GbmBo> {
        // SAFETY: both calls take the live device plus plain integers; `modifiers` is read
        // as `count` u64s for the duration of the call. Each returns a bo or null (checked).
        let raw = unsafe {
            if modifiers.is_empty() {
                gbm_bo_create(device.raw, width, height, fourcc, flags)
            } else {
                gbm_bo_create_with_modifiers2(
                    device.raw,
                    width,
                    height,
                    fourcc,
                    modifiers.as_ptr(),
                    modifiers.len() as u32,
                    flags,
                )
            }
        };
        if raw.is_null() {
            bail!("gbm_bo_create for {width}x{height} fourcc {fourcc:#010x}");
        }
        // SAFETY: plain accessors on the bo just created, valid until `gbm_bo_destroy`.
        let (fd, stride, offset, modifier, planes) = unsafe {
            (
                gbm_bo_get_fd(raw),
                gbm_bo_get_stride(raw),
                gbm_bo_get_offset(raw, 0),
                gbm_bo_get_modifier(raw),
                gbm_bo_get_plane_count(raw),
            )
        };
        if fd < 0 {
            // SAFETY: `raw` is the live bo created above; nothing else holds it.
            unsafe { gbm_bo_destroy(raw) };
            bail!("gbm_bo_get_fd");
        }
        Ok(GbmBo {
            raw,
            // SAFETY: `fd` is the fresh descriptor `gbm_bo_get_fd` returned (>= 0), owned by
            // nobody else and independent of the bo's lifetime.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            stride,
            offset,
            modifier,
            planes,
            _device: Rc::clone(device),
        })
    }
}

impl Drop for GbmBo {
    fn drop(&mut self) {
        // SAFETY: `raw` is the non-null bo this value uniquely owns; `drop` runs once, and
        // `_device` keeps the device alive until after this call.
        unsafe { gbm_bo_destroy(self.raw) };
    }
}
