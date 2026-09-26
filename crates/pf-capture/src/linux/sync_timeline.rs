//! PipeWire explicit sync (`SPA_META_SyncTimeline`), consumer side.
//!
//! A buffer that carries the meta ends in two `SyncObj` datas: acquire, then release. The
//! producer's render lands at `acquire_point` on the first; this side signals
//! `release_point` on the second once it no longer reads the pixels, and the producer
//! waits on that point before it paints the buffer again. Without the meta, KWin on
//! NVIDIA `glFinish()`es its compositor thread after every cast frame instead.
//!
//! Syncobj fds are device-agnostic: any DRM node whose driver serves the syncobj ioctls
//! imports them, so the node need not be the compositor's.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pf_zerocopy::dmabuf_fence::WaitOutcome;
use pipewire as pw;
use pw::spa;

/// `_IOWR('d', nr, size)`: direction 3 at bit 30, size at 16, type at 8.
const fn drm_iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | (0x64 << 8) | nr
}

/// Kernel ABI from `<libdrm/drm.h>`. The ioctls read the fields, so rustc sees no reader.
#[allow(dead_code)]
mod abi {
    #[repr(C)]
    pub(super) struct SyncobjCreate {
        pub handle: u32,
        pub flags: u32,
    }

    #[repr(C)]
    pub(super) struct SyncobjHandle {
        pub handle: u32,
        pub flags: u32,
        pub fd: i32,
        pub pad: u32,
        pub point: u64,
    }

    #[repr(C)]
    pub(super) struct SyncobjDestroy {
        pub handle: u32,
        pub pad: u32,
    }

    #[repr(C)]
    pub(super) struct SyncobjTimelineWait {
        pub handles: u64,
        pub points: u64,
        /// Absolute, `CLOCK_MONOTONIC` ns.
        pub timeout_nsec: i64,
        pub count_handles: u32,
        pub flags: u32,
        pub first_signaled: u32,
        pub pad: u32,
        pub deadline_nsec: u64,
    }

    #[repr(C)]
    pub(super) struct SyncobjTimelineArray {
        pub handles: u64,
        pub points: u64,
        pub count_handles: u32,
        pub flags: u32,
    }
}
use abi::{
    SyncobjCreate, SyncobjDestroy, SyncobjHandle, SyncobjTimelineArray, SyncobjTimelineWait,
};

const SYNCOBJ_CREATE: u64 = drm_iowr(0xBF, std::mem::size_of::<SyncobjCreate>());
const SYNCOBJ_DESTROY: u64 = drm_iowr(0xC0, std::mem::size_of::<SyncobjDestroy>());
const SYNCOBJ_FD_TO_HANDLE: u64 = drm_iowr(0xC2, std::mem::size_of::<SyncobjHandle>());
const SYNCOBJ_TIMELINE_WAIT: u64 = drm_iowr(0xCA, std::mem::size_of::<SyncobjTimelineWait>());
const SYNCOBJ_TIMELINE_SIGNAL: u64 = drm_iowr(0xCD, std::mem::size_of::<SyncobjTimelineArray>());
/// Wait even before the producer attaches a fence at the point; a bare wait is `EINVAL`.
const WAIT_FOR_SUBMIT: u32 = 1 << 1;
/// `spa_meta_sync_timeline.flags`: the producer sets it; clearing it promises the release signal.
const UNSCHEDULED_RELEASE: u32 = 1 << 0;

/// libspa's explicit-sync ABI (PipeWire 1.2), written out because older headers lack it and
/// bindgen then fails the host build: Ubuntu 24.04 ships 1.0. `sync_abi_matches_libspa` pins
/// each value where the headers carry it.
pub(super) const META_SYNC_TIMELINE: u32 = 9;
pub(super) const DATA_SYNC_OBJ: u32 = 5;
pub(super) const PARAM_BUFFERS_META_TYPE: u32 = 7;

/// `struct spa_meta_sync_timeline`.
#[repr(C)]
pub(super) struct MetaSyncTimeline {
    flags: u32,
    _padding: u32,
    acquire_point: u64,
    release_point: u64,
}

/// A DRM node whose driver serves the syncobj ioctls. One per stream.
pub(super) struct SyncDevice {
    node: OwnedFd,
}

impl SyncDevice {
    /// The first render node that creates a syncobj. `None` keeps the stream on implicit sync.
    pub(super) fn open() -> Option<SyncDevice> {
        let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        for path in nodes {
            let Ok(file) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
            else {
                continue;
            };
            let dev = SyncDevice { node: file.into() };
            let mut probe = SyncobjCreate {
                handle: 0,
                flags: 0,
            };
            // SAFETY: `probe` is the struct `DRM_IOCTL_SYNCOBJ_CREATE` fills.
            if unsafe { dev.ioctl(SYNCOBJ_CREATE, &mut probe) }.is_ok() {
                let _ = dev.destroy(probe.handle);
                tracing::info!(node = %path.display(), "explicit sync: syncobj device");
                return Some(dev);
            }
        }
        None
    }

    /// # Safety
    /// `req` is a DRM syncobj ioctl and `arg` the `#[repr(C)]` struct it reads and writes.
    unsafe fn ioctl<T>(&self, req: u64, arg: &mut T) -> std::io::Result<()> {
        // SAFETY: the caller's contract; `arg` outlives the call.
        let rc = unsafe { libc::ioctl(self.node.as_raw_fd(), req as _, arg as *mut T) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn import(&self, fd: RawFd) -> std::io::Result<u32> {
        let mut h = SyncobjHandle {
            handle: 0,
            flags: 0,
            fd,
            pad: 0,
            point: 0,
        };
        // SAFETY: `h` is the struct `DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE` reads and fills.
        unsafe { self.ioctl(SYNCOBJ_FD_TO_HANDLE, &mut h) }?;
        Ok(h.handle)
    }

    fn destroy(&self, handle: u32) -> std::io::Result<()> {
        let mut d = SyncobjDestroy { handle, pad: 0 };
        // SAFETY: `d` is the struct `DRM_IOCTL_SYNCOBJ_DESTROY` reads.
        unsafe { self.ioctl(SYNCOBJ_DESTROY, &mut d) }
    }

    /// Block until `point` on the timeline behind `fd` signals, or `timeout` passes.
    pub(super) fn wait(
        &self,
        fd: RawFd,
        point: u64,
        timeout: Duration,
    ) -> std::io::Result<WaitOutcome> {
        let handle = self.import(fd)?;
        let mut w = SyncobjTimelineWait {
            handles: std::ptr::from_ref(&handle) as u64,
            points: std::ptr::from_ref(&point) as u64,
            timeout_nsec: monotonic_ns().saturating_add(timeout.as_nanos() as i64),
            count_handles: 1,
            flags: WAIT_FOR_SUBMIT,
            first_signaled: 0,
            pad: 0,
            deadline_nsec: 0,
        };
        // SAFETY: `w` is the struct `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT` reads; `handle` and
        // `point` outlive the call and `count_handles` names exactly them.
        let res = unsafe { self.ioctl(SYNCOBJ_TIMELINE_WAIT, &mut w) };
        let _ = self.destroy(handle);
        match res {
            Ok(()) => Ok(WaitOutcome::Signaled),
            Err(e) if e.raw_os_error() == Some(libc::ETIME) => Ok(WaitOutcome::TimedOut),
            Err(e) => Err(e),
        }
    }

    /// Signal `point` on the timeline behind `fd` from the CPU: materialized at once.
    pub(super) fn signal(&self, fd: RawFd, point: u64) -> std::io::Result<()> {
        let handle = self.import(fd)?;
        let mut a = SyncobjTimelineArray {
            handles: std::ptr::from_ref(&handle) as u64,
            points: std::ptr::from_ref(&point) as u64,
            count_handles: 1,
            flags: 0,
        };
        // SAFETY: as `wait`, for `DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL`.
        let res = unsafe { self.ioctl(SYNCOBJ_TIMELINE_SIGNAL, &mut a) };
        let _ = self.destroy(handle);
        res
    }
}

fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer for `clock_gettime`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// One held buffer's timeline points. `acquire` fences the producer's render; this side
/// signals `release` on hand-back.
pub(super) struct SyncPoints {
    pub(super) acquire_fd: RawFd,
    pub(super) release_fd: RawFd,
    pub(super) acquire_point: u64,
    pub(super) release_point: u64,
    meta: *mut MetaSyncTimeline,
}

impl SyncPoints {
    /// Read `spa_buf`'s meta and its two trailing `SyncObj` datas. `None`: not negotiated.
    ///
    /// # Safety
    /// `spa_buf` is the `spa_buffer` of a buffer this side holds (dequeued, not requeued).
    pub(super) unsafe fn of(spa_buf: *mut spa::sys::spa_buffer) -> Option<SyncPoints> {
        if spa_buf.is_null() {
            return None;
        }
        // SAFETY: the caller holds the buffer; `find_meta_data` scans its metadata array for a
        // region of at least the struct's size, so a non-null result covers every field.
        let meta = unsafe {
            spa::sys::spa_buffer_find_meta_data(
                spa_buf,
                META_SYNC_TIMELINE,
                std::mem::size_of::<MetaSyncTimeline>(),
            )
        } as *mut MetaSyncTimeline;
        if meta.is_null() {
            return None;
        }
        // SAFETY: the caller's contract is `datas`'.
        let datas = unsafe { datas(spa_buf) };
        let [_, .., acquire, release] = datas else {
            return None;
        };
        if acquire.type_ != DATA_SYNC_OBJ || release.type_ != DATA_SYNC_OBJ {
            return None;
        }
        // SAFETY: `meta` is non-null and sized above.
        let (acquire_point, release_point) =
            unsafe { ((*meta).acquire_point, (*meta).release_point) };
        Some(SyncPoints {
            acquire_fd: acquire.fd as RawFd,
            release_fd: release.fd as RawFd,
            acquire_point,
            release_point,
            meta,
        })
    }

    /// Clear the producer's `UNSCHEDULED_RELEASE`: the release point is signalled here.
    ///
    /// # Safety
    /// The buffer of [`SyncPoints::of`] is still held.
    pub(super) unsafe fn promise_release(&self) {
        // SAFETY: `meta` points into the still-held buffer's metadata.
        unsafe { (*self.meta).flags &= !UNSCHEDULED_RELEASE };
    }
}

/// Hand `buf` back to the producer, its release point signalled first when the stream
/// negotiated explicit sync: the producer waits on that point before it paints the buffer
/// again, so a point never signalled parks one pool slot for good.
///
/// # Safety
/// Loop thread; `buf` is a live buffer of `stream` this side holds and touches no further.
pub(super) unsafe fn hand_back(
    sync: Option<&SyncDevice>,
    stream: *mut pw::sys::pw_stream,
    buf: *mut pw::sys::pw_buffer,
) {
    // SAFETY: `buf` is held (caller), so its `spa_buffer` is readable until the queue below.
    if let (Some(dev), Some(p)) = (sync, unsafe { SyncPoints::of((*buf).buffer) }) {
        match dev.signal(p.release_fd, p.release_point) {
            // SAFETY: still held.
            Ok(()) => unsafe { p.promise_release() },
            Err(e) => {
                static ONCE: AtomicBool = AtomicBool::new(true);
                if ONCE.swap(false, Ordering::Relaxed) {
                    tracing::warn!(
                        error = %e,
                        "explicit sync: release point not signalled — the producer may never \
                         paint this buffer again"
                    );
                }
            }
        }
    }
    // SAFETY: the caller's contract is `pw_stream_queue_buffer`'s.
    let _ = unsafe { pw::sys::pw_stream_queue_buffer(stream, buf) };
}

/// The buffer's datas; empty for a null array.
///
/// # Safety
/// `spa_buf` is non-null and held.
unsafe fn datas<'a>(spa_buf: *mut spa::sys::spa_buffer) -> &'a [spa::sys::spa_data] {
    // SAFETY: the caller's contract; libpipewire sizes `datas` by `n_datas`.
    unsafe {
        if (*spa_buf).datas.is_null() {
            &[]
        } else {
            std::slice::from_raw_parts((*spa_buf).datas, (*spa_buf).n_datas as usize)
        }
    }
}

/// Pixel planes: every data before the first `SyncObj`.
pub(super) fn planes_before_sync(types: impl IntoIterator<Item = u32>) -> u32 {
    types
        .into_iter()
        .take_while(|&t| t != DATA_SYNC_OBJ)
        .count() as u32
}

/// Plane count of a held buffer, the sync datas excluded.
///
/// # Safety
/// `spa_buf` is non-null and held.
pub(super) unsafe fn plane_count(spa_buf: *mut spa::sys::spa_buffer) -> u32 {
    // SAFETY: the caller's contract is `datas`'.
    planes_before_sync(unsafe { datas(spa_buf) }.iter().map(|d| d.type_))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Printed from `<libdrm/drm.h>` on Ubuntu 26.04; x86_64 and aarch64 share the layout.
    #[test]
    fn ioctl_requests_match_the_kernel_header() {
        assert_eq!(SYNCOBJ_CREATE, 0xC008_64BF);
        assert_eq!(SYNCOBJ_DESTROY, 0xC008_64C0);
        assert_eq!(SYNCOBJ_FD_TO_HANDLE, 0xC018_64C2);
        assert_eq!(SYNCOBJ_TIMELINE_WAIT, 0xC030_64CA);
        assert_eq!(SYNCOBJ_TIMELINE_SIGNAL, 0xC018_64CD);
    }

    /// Hand-written ABI vs the real libspa binding, wherever the headers carry it.
    #[test]
    fn sync_abi_matches_libspa() {
        use spa::sys::spa_meta_sync_timeline as Spa;
        use std::mem::{offset_of, size_of};
        assert_eq!(META_SYNC_TIMELINE, spa::sys::SPA_META_SyncTimeline);
        assert_eq!(DATA_SYNC_OBJ, spa::sys::SPA_DATA_SyncObj);
        assert_eq!(
            PARAM_BUFFERS_META_TYPE,
            spa::sys::SPA_PARAM_BUFFERS_metaType
        );
        assert_eq!(size_of::<MetaSyncTimeline>(), size_of::<Spa>());
        assert_eq!(offset_of!(MetaSyncTimeline, flags), offset_of!(Spa, flags));
        assert_eq!(
            offset_of!(MetaSyncTimeline, acquire_point),
            offset_of!(Spa, acquire_point)
        );
        assert_eq!(
            offset_of!(MetaSyncTimeline, release_point),
            offset_of!(Spa, release_point)
        );
    }

    #[test]
    fn planes_stop_at_the_first_syncobj() {
        let d = spa::sys::SPA_DATA_DmaBuf;
        let s = spa::sys::SPA_DATA_SyncObj;
        assert_eq!(planes_before_sync([d, s, s]), 1);
        assert_eq!(planes_before_sync([d, d, s, s]), 2);
        assert_eq!(planes_before_sync([d]), 1);
        assert_eq!(planes_before_sync([]), 0);
    }
}
