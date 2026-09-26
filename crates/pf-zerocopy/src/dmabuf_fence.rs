//! Consumer wait on a dmabuf's implicit fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`).
//!
//! The ioctl snapshots in-flight GPU writes on the reservation object into a
//! sync_file fd; `poll` is readable once those writes complete. Sampling
//! without that wait can encode the buffer's previous contents when the
//! producer hands the buffer over at GPU-submit time.
//!
//! No attached fence → already-signaled sync_file (`WaitOutcome::NoFence`);
//! zero-copy can still race. Timeout is fail-open (`TimedOut`).
//!
//! Pin: `ioctl_number_matches_dma_buf_h`, `poll_readable_reports_the_truth`.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::time::{Duration, Instant};

// linux/dma-buf.h: DMA_BUF_BASE is 'b' (0x62). _IOWR = dir(3)<<30 | size<<16 | base<<8 | nr.
const DMA_BUF_BASE: u64 = 0x62;
const fn iowr(nr: u32, size: usize) -> u64 {
    (3u64 << 30) | ((size as u64) << 16) | (DMA_BUF_BASE << 8) | nr as u64
}

#[repr(C)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u64 = iowr(2, std::mem::size_of::<DmaBufExportSyncFile>());
/// Wait for outstanding writes. WRITE would also wait for readers we never attach.
const DMA_BUF_SYNC_READ: u32 = 1 << 0;

/// Observed wait. `TimedOut` may be mid-render; `Signaled` waited out the write;
/// `NoFence` means the driver attached nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaitOutcome {
    /// Already signaled or no fence attached. Zero-copy can still race.
    NoFence,
    Signaled,
    /// Fail-open after `timeout_ms`. Blocking longer stalls capture; the buffer may be mid-render.
    TimedOut,
}

/// Snapshot the producer's pending writes on `dmabuf_fd` into an owned sync_file.
/// `None` when the kernel attached no fence. `Err` when the kernel lacks the ioctl.
pub fn export_sync_file(dmabuf_fd: RawFd) -> std::io::Result<Option<OwnedFd>> {
    let mut req = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    // SAFETY: `dmabuf_fd` is a live borrowed dmabuf; we never close it.
    // The ioctl size is `size_of::<DmaBufExportSyncFile>()`. `&mut req` is a
    // live `#[repr(C)]` value the kernel reads (`flags`) and writes (`fd`);
    // it outlives this call and is not aliased.
    let r = unsafe { libc::ioctl(dmabuf_fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut req) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if req.fd < 0 {
        return Ok(None);
    }
    // SAFETY: `req.fd` is the sync_file the ioctl just created for this process.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(req.fd) }))
}

/// Wait for a sync_file (from [`export_sync_file`]) to signal. Negative `timeout_ms`
/// is infinite.
pub fn wait_sync_file(sync_fd: RawFd, timeout_ms: i32) -> std::io::Result<WaitOutcome> {
    poll_readable(sync_fd, timeout_ms)
}

/// Wait for producer writes on `dmabuf_fd`. Negative `timeout_ms` is infinite.
/// `Err` if the ioctl or poll failed (kernel lacks `EXPORT_SYNC_FILE`).
pub fn wait_read_ready(dmabuf_fd: RawFd, timeout_ms: i32) -> std::io::Result<WaitOutcome> {
    match export_sync_file(dmabuf_fd)? {
        None => Ok(WaitOutcome::NoFence),
        Some(sync) => poll_readable(sync.as_raw_fd(), timeout_ms),
    }
}

/// Poll `fd` for `POLLIN`. Already readable at the probe is [`WaitOutcome::NoFence`].
/// Negative `timeout_ms` is infinite. Retry `EINTR` with the remaining budget —
/// skipping the wait would sample a still-in-flight buffer.
fn poll_readable(fd: RawFd, timeout_ms: i32) -> std::io::Result<WaitOutcome> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let probed = loop {
        // SAFETY: `pfd` is one live `pollfd`; `nfds == 1`. `fd` is the caller's
        // live sync_file. `poll` reads `fd`/`events`, writes `revents`; `pfd`
        // outlives this timeout-0 probe and is not aliased.
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r >= 0 {
            break r;
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    };
    if probed > 0 {
        if pfd.revents & libc::POLLIN != 0 {
            return Ok(WaitOutcome::NoFence);
        }
        // POLLERR/POLLNVAL without POLLIN — the fd is broken, not signaled.
        return Err(std::io::Error::other(format!(
            "poll(sync_file) revents {:#x} without POLLIN",
            pfd.revents
        )));
    }
    let deadline =
        (timeout_ms >= 0).then(|| Instant::now() + Duration::from_millis(timeout_ms as u64));
    loop {
        let remaining = match deadline {
            None => -1, // poll's "no timeout"
            Some(d) => match d.checked_duration_since(Instant::now()) {
                None => return Ok(WaitOutcome::TimedOut),
                // +1: round up so a sub-millisecond remainder still waits instead of busy-polling.
                Some(rem) => (rem.as_millis() as i32).saturating_add(1),
            },
        };
        pfd.revents = 0;
        // SAFETY: same live single-element `pfd` (`revents` reset above), `nfds == 1`.
        // `fd` stays open until the caller returns. `poll` reads `fd`/`events`,
        // writes `revents`, and returns before `pfd` ends.
        let r = unsafe { libc::poll(&mut pfd, 1, remaining) };
        match r {
            0 => return Ok(WaitOutcome::TimedOut),
            r if r > 0 => {
                if pfd.revents & libc::POLLIN != 0 {
                    return Ok(WaitOutcome::Signaled);
                }
                // POLLERR/POLLNVAL without POLLIN — the fd is broken, not signaled.
                return Err(std::io::Error::other(format!(
                    "poll(sync_file) revents {:#x} without POLLIN",
                    pfd.revents
                )));
            }
            _ => {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EINTR) {
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `iowr(2, size)` must equal linux/dma-buf.h `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`.
    #[test]
    fn ioctl_number_matches_dma_buf_h() {
        assert_eq!(DMA_BUF_IOCTL_EXPORT_SYNC_FILE, 0xC008_6202);
    }

    /// Pipe stand-in: quiet fd → `TimedOut`; already-readable → `NoFence`.
    #[test]
    fn poll_readable_reports_the_truth() {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        let (r, mut w) = std::io::pipe().unwrap();
        assert_eq!(
            poll_readable(r.as_raw_fd(), 10).unwrap(),
            WaitOutcome::TimedOut
        );
        w.write_all(b"x").unwrap();
        assert_eq!(
            poll_readable(r.as_raw_fd(), 10).unwrap(),
            WaitOutcome::NoFence
        );
    }
}
