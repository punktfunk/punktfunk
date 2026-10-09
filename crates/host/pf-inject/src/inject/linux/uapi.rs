//! Device-node plumbing shared by [`crate::uinput_abi`] and the raw_gadget Deck: `open`
//! through std (CLOEXEC, errno kept) and `ioctl` over typed references.
//!
//! Request numbers and layouts are the 64-bit kernel ABI (x86_64 and aarch64 agree).

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;

/// A type the kernel may read and overwrite whole: no padding, every bit pattern valid.
///
/// # Safety
/// Implement only for `#[repr(C)]` (or packed) aggregates of integers and integer arrays
/// with no padding bytes.
pub(crate) unsafe trait Pod {}

// SAFETY: a byte array has no padding and every bit pattern is a valid value.
unsafe impl<const N: usize> Pod for [u8; N] {}

/// The argument size an `_IOC` request number encodes.
pub(crate) const fn arg_size(req: libc::c_ulong) -> usize {
    ((req >> 16) & 0x3fff) as usize
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// `ioctl(fd, req, value)` for a request that takes its argument by value.
/// Panics on a request that encodes a copy out to the caller.
pub(crate) fn ioctl_value(
    fd: BorrowedFd<'_>,
    req: libc::c_ulong,
    value: libc::c_ulong,
) -> io::Result<libc::c_int> {
    assert!(
        req >> 31 == 0,
        "ioctl {req:#x}: the kernel writes through its argument"
    );
    // SAFETY: every caller's `req` reads `value` as an integer and writes through nothing;
    // `fd` is borrowed, so it stays open for the call.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), req as _, value) })
}

/// `ioctl(fd, req, arg)`: the kernel copies `arg` in, out, or both.
/// Panics unless `req` encodes `size_of::<T>()`.
pub(crate) fn ioctl_with<T: Pod>(
    fd: BorrowedFd<'_>,
    req: libc::c_ulong,
    arg: &mut T,
) -> io::Result<libc::c_int> {
    assert_eq!(
        arg_size(req),
        size_of::<T>(),
        "ioctl {req:#x}: argument size"
    );
    // SAFETY: `arg` is a live, unique `T` of exactly the size `req` makes the kernel copy,
    // and `Pod` makes any bytes it writes back a valid `T`.
    check(unsafe { libc::ioctl(fd.as_raw_fd(), req as _, std::ptr::from_mut(arg)) })
}

/// Open a device node read-write and non-blocking; std adds `O_CLOEXEC`.
pub(crate) fn open_nonblock(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}
