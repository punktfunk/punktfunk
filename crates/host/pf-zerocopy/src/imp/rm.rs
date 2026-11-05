//! A LINEAR dmabuf as CUDA external memory, through the memory object NVIDIA's driver holds
//! behind it. No EGL, no Vulkan, no copy.
//!
//! nvidia-drm hands a same-GPU dmabuf import back its exporter's own GEM object, and that
//! object's resource-manager memory exports again as the fd `cuImportExternalMemory` takes on
//! Linux. The CUDA mapping then carries the allocation's own page kind, which on a pre-Turing
//! GPU decides the physical byte layout; an EGL import picks a kind of its own.
//!
//! Needs nvidia-drm with modeset; without it the export refuses and the caller falls back.

use super::cuda;
use anyhow::{Context as _, Result};
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd};

/// Linux `_IOC(dir, type, nr, size)`.
const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> u64 {
    ((dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32) as u64
}
const WRITE: u32 = 1;
const RW: u32 = 3;
const DRM: u8 = b'd';
/// nvidia-drm's own requests start at `DRM_COMMAND_BASE`.
const NV: u8 = 0x40;

#[repr(C)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct GemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
struct ExportNvkmsMemory {
    handle: u32,
    pad: u32,
    params_ptr: u64,
    params_size: u64,
}

#[repr(C)]
struct IdentifyObject {
    handle: u32,
    object_type: u32,
}

const PRIME_FD_TO_HANDLE: u64 = ioc(RW, DRM, 0x2e, size_of::<PrimeHandle>());
const GEM_CLOSE: u64 = ioc(WRITE, DRM, 0x09, size_of::<GemClose>());
const GEM_EXPORT_NVKMS_MEMORY: u64 = ioc(RW, DRM, NV + 0x09, size_of::<ExportNvkmsMemory>());
const GEM_IDENTIFY_OBJECT: u64 = ioc(RW, DRM, NV + 0x0e, size_of::<IdentifyObject>());

/// What nvidia-drm says the dmabuf's GEM object is. Only `Nvkms` memory exports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    Nvkms,
    Dmabuf,
    UserMemory,
    Other(u32),
}

impl From<u32> for Object {
    fn from(v: u32) -> Self {
        match v {
            0 => Object::Nvkms,
            1 => Object::Dmabuf,
            2 => Object::UserMemory,
            v => Object::Other(v),
        }
    }
}

/// `ioctl(fd, req, arg)`, with the errno on failure.
fn ioctl<T>(fd: BorrowedFd<'_>, req: u64, arg: &mut T) -> std::io::Result<()> {
    // SAFETY: each request constant above is paired with the struct the kernel reads and
    // writes for it, and `arg` is a live, writable value of that struct.
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A GEM handle on `drm`, closed on drop: an export holds its own reference to the memory.
struct Gem<'a> {
    drm: BorrowedFd<'a>,
    handle: u32,
}

impl Drop for Gem<'_> {
    fn drop(&mut self) {
        let mut close = GemClose {
            handle: self.handle,
            pad: 0,
        };
        let _ = ioctl(self.drm, GEM_CLOSE, &mut close);
    }
}

/// The dmabuf's own memory as CUDA external memory, with what nvidia-drm calls the object.
/// `drm` is the render node of the GPU that made the buffer. The shared context must be
/// current.
pub fn import(
    drm: BorrowedFd<'_>,
    dmabuf: BorrowedFd<'_>,
) -> Result<(cuda::ExternalDmabuf, Object)> {
    let mut prime = PrimeHandle {
        handle: 0,
        flags: 0,
        fd: dmabuf.as_raw_fd(),
    };
    ioctl(drm, PRIME_FD_TO_HANDLE, &mut prime).context("DRM_IOCTL_PRIME_FD_TO_HANDLE")?;
    let gem = Gem {
        drm,
        handle: prime.handle,
    };
    let mut identify = IdentifyObject {
        handle: gem.handle,
        object_type: u32::MAX,
    };
    let object = match ioctl(drm, GEM_IDENTIFY_OBJECT, &mut identify) {
        Ok(()) => Object::from(identify.object_type),
        Err(_) => Object::Other(u32::MAX),
    };
    let nvctl = std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/nvidiactl")
        .context("open /dev/nvidiactl")?;
    let nvctl = OwnedFd::from(nvctl);
    // The kernel copies `struct NvKmsKapiPrivExportMemoryParams { int memFd; }` from here.
    let mut mem_fd: libc::c_int = nvctl.as_raw_fd();
    let mut export = ExportNvkmsMemory {
        handle: gem.handle,
        pad: 0,
        params_ptr: std::ptr::from_mut(&mut mem_fd) as u64,
        params_size: size_of::<libc::c_int>() as u64,
    };
    ioctl(drm, GEM_EXPORT_NVKMS_MEMORY, &mut export)
        .with_context(|| format!("DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY ({object:?} object)"))?;
    drop(gem);
    let size = rustix::fs::seek(dmabuf, rustix::fs::SeekFrom::End(0)).context("dma-buf size")?;
    let ext = cuda::ExternalDmabuf::import_owned_fd(nvctl, size)
        .context("CUDA import of the dma-buf's memory object")?;
    Ok((ext, object))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The request numbers libdrm and nvidia-drm-ioctl.h define, so a struct that changes
    // size fails here and not in the kernel.
    #[test]
    fn the_requests_match_the_kernel_headers() {
        assert_eq!(PRIME_FD_TO_HANDLE, 0xc00c_642e);
        assert_eq!(GEM_CLOSE, 0x4008_6409);
        assert_eq!(GEM_EXPORT_NVKMS_MEMORY, 0xc018_6449);
        assert_eq!(GEM_IDENTIFY_OBJECT, 0xc008_644e);
    }

    #[test]
    fn object_types_follow_the_enum() {
        assert_eq!(Object::from(0), Object::Nvkms);
        assert_eq!(Object::from(1), Object::Dmabuf);
        assert_eq!(Object::from(2), Object::UserMemory);
        assert_eq!(Object::from(0x7fff_ffff), Object::Other(0x7fff_ffff));
    }
}
