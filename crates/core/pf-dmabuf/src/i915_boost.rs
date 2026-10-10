//! Keep Intel's media engine clocked while it decodes for us.
//!
//! i915 raises a GT's clock for a waiter only on its own `I915_GEM_WAIT` ioctl; a
//! syncobj or sync_file wait takes the plain dma-fence path and never boosts. A Meteor
//! Lake Arc decodes a 4K HEVC picture in 2.7 ms boosted and 6.8 ms at the media GT's
//! 100 MHz floor. So: import a dma-buf the decode writes as a GEM handle on an i915
//! render node, and wait it there once per picture. `xe` has no such ioctl;
//! [`I915Boost::open`] finds no node there and the caller does nothing.
//!
//! Pin: `ioctl_numbers_match_drm_h`.

use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};

// drm.h: DRM_IOCTL_BASE 'd' (0x64); i915_drm.h: DRM_COMMAND_BASE 0x40, DRM_I915_GEM_WAIT 0x2c.
// _IOC = dir<<30 | size<<16 | base<<8 | nr; dir 1 = write, 3 = read/write.
const DRM_BASE: u64 = 0x64;
const fn ioc(dir: u64, nr: u32, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (DRM_BASE << 8) | nr as u64
}

#[repr(C)]
struct DrmVersion {
    major: i32,
    minor: i32,
    patchlevel: i32,
    name_len: usize,
    name: *mut u8,
    date_len: usize,
    date: *mut u8,
    desc_len: usize,
    desc: *mut u8,
}

#[repr(C)]
struct DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct DrmGemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
struct I915GemWait {
    bo_handle: u32,
    flags: u32,
    timeout_ns: i64,
}

const DRM_IOCTL_VERSION: u64 = ioc(3, 0x00, std::mem::size_of::<DrmVersion>());
const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = ioc(3, 0x2e, std::mem::size_of::<DrmPrimeHandle>());
const DRM_IOCTL_GEM_CLOSE: u64 = ioc(1, 0x09, std::mem::size_of::<DrmGemClose>());
const DRM_IOCTL_I915_GEM_WAIT: u64 = ioc(3, 0x40 + 0x2c, std::mem::size_of::<I915GemWait>());

/// An i915 render node and the one GEM handle it waits on.
pub struct I915Boost {
    node: OwnedFd,
    /// The imported dma-buf's inode and the handle. The inode names the buffer; an fd number
    /// comes back for the next buffer once the old one closes.
    tracked: Option<(libc::ino_t, u32)>,
}

impl I915Boost {
    /// The first render node whose driver is i915. `None` under `xe`, on AMD or NVIDIA,
    /// or without `/dev/dri`.
    pub fn open() -> Option<Self> {
        (128..192)
            .filter_map(|minor| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(format!("/dev/dri/renderD{minor}"))
                    .ok()
            })
            .map(OwnedFd::from)
            .find(|node| driver_name(node.as_raw_fd()).as_deref() == Some("i915"))
            .map(|node| Self {
                node,
                tracked: None,
            })
    }

    /// Wait on `dmabuf_fd` from now on. The same buffer again costs an `fstat`; a new one is
    /// imported and the old handle closed. `false` when the import was refused.
    pub fn track(&mut self, dmabuf_fd: RawFd) -> bool {
        let Some(ino) = inode(dmabuf_fd) else {
            return false;
        };
        if self.tracked.is_some_and(|(i, _)| i == ino) {
            return true;
        }
        let mut req = DrmPrimeHandle {
            handle: 0,
            flags: 0,
            fd: dmabuf_fd,
        };
        // SAFETY: `node` is a live render node; `req` is a live `#[repr(C)]` value the
        // kernel reads (`fd`) and writes (`handle`), sized as the ioctl says.
        let r = unsafe {
            libc::ioctl(
                self.node.as_raw_fd(),
                DRM_IOCTL_PRIME_FD_TO_HANDLE as _,
                &mut req,
            )
        };
        if r < 0 {
            return false;
        }
        self.close_tracked();
        self.tracked = Some((ino, req.handle));
        true
    }

    /// Wait every fence on the tracked buffer for up to `timeout_ns`; this is the wait
    /// i915 boosts for. `false` when nothing is tracked, or the wait failed or timed out.
    pub fn wait(&self, timeout_ns: i64) -> bool {
        let Some((_, handle)) = self.tracked else {
            return false;
        };
        let mut req = I915GemWait {
            bo_handle: handle,
            flags: 0,
            timeout_ns,
        };
        // SAFETY: as in `track`; `handle` is one this node handed out and has not closed.
        unsafe {
            libc::ioctl(
                self.node.as_raw_fd(),
                DRM_IOCTL_I915_GEM_WAIT as _,
                &mut req,
            ) == 0
        }
    }

    fn close_tracked(&mut self) {
        if let Some((_, handle)) = self.tracked.take() {
            let mut req = DrmGemClose { handle, pad: 0 };
            // SAFETY: as in `track`; closing a handle this node owns.
            unsafe { libc::ioctl(self.node.as_raw_fd(), DRM_IOCTL_GEM_CLOSE as _, &mut req) };
        }
    }
}

impl Drop for I915Boost {
    fn drop(&mut self) {
        self.close_tracked();
    }
}

fn inode(fd: RawFd) -> Option<libc::ino_t> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat fills the whole `stat` when it returns 0, and only then is it read.
    unsafe { (libc::fstat(fd, st.as_mut_ptr()) == 0).then(|| st.assume_init().st_ino) }
}

/// The DRM driver name behind `fd` (`i915`, `xe`, `amdgpu`, …).
fn driver_name(fd: RawFd) -> Option<String> {
    let mut name = [0u8; 32];
    let mut v = DrmVersion {
        major: 0,
        minor: 0,
        patchlevel: 0,
        name_len: name.len(),
        name: name.as_mut_ptr(),
        date_len: 0,
        date: std::ptr::null_mut(),
        desc_len: 0,
        desc: std::ptr::null_mut(),
    };
    // SAFETY: `fd` is a live DRM node; the kernel writes at most `name_len` bytes into
    // `name`, which outlives the call, and leaves the null-pointed fields alone.
    if unsafe { libc::ioctl(fd, DRM_IOCTL_VERSION as _, &mut v) } < 0 {
        return None;
    }
    let len = v.name_len.min(name.len());
    Some(String::from_utf8_lossy(&name[..len]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_drm_h() {
        assert_eq!(DRM_IOCTL_VERSION, 0xC040_6400);
        assert_eq!(DRM_IOCTL_PRIME_FD_TO_HANDLE, 0xC00C_642E);
        assert_eq!(DRM_IOCTL_GEM_CLOSE, 0x4008_6409);
        assert_eq!(DRM_IOCTL_I915_GEM_WAIT, 0xC010_646C);
    }

    #[test]
    fn nothing_tracked_waits_nothing() {
        let Some(mut boost) = I915Boost::open() else {
            return;
        };
        assert!(!boost.wait(0));
        assert!(!boost.track(-1), "a bad fd must not become a handle");
    }
}
