//! A read-only `mmap` of an fd's first bytes, unmapped on drop.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd};

/// The object's size as `fstat` reports it.
pub fn byte_len(fd: BorrowedFd<'_>) -> io::Result<u64> {
    Ok(u64::try_from(rustix::fs::fstat(fd)?.st_size).unwrap_or(0))
}

/// How a [`ReadMap`] maps the object's pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Share {
    /// `MAP_SHARED`. dma-buf exporters refuse a copy-on-write mapping.
    Shared,
    /// `MAP_PRIVATE`. Before Linux 6.7 a write-sealed memfd refuses every shared mapping.
    Private,
}

/// The first `len` bytes of an fd, mapped `PROT_READ`. The mapping holds the object on its
/// own, so the fd may close first. The object must not shrink while mapped (a dma-buf cannot;
/// seal a memfd): a page past the new end is a SIGBUS on touch.
pub struct ReadMap {
    ptr: *mut libc::c_void,
    len: usize,
}

impl ReadMap {
    /// Map the first `len` bytes of `fd`. A `len` past the object's end is refused: touching
    /// a mapped page beyond it is a SIGBUS, not an error. `len == 0` fails in `mmap`.
    pub fn new(fd: BorrowedFd<'_>, len: usize, share: Share) -> io::Result<ReadMap> {
        let size = byte_len(fd)?;
        if size < len as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("fd holds {size} bytes, asked to map {len}"),
            ));
        }
        let flags = match share {
            Share::Shared => libc::MAP_SHARED,
            Share::Private => libc::MAP_PRIVATE,
        };
        // SAFETY: a null `addr` lets the kernel place a fresh read-only mapping of `len` bytes
        // of `fd`, open for this call; it aliases no Rust object. The object holds at least
        // `len` bytes (checked above). `MAP_FAILED` is checked before `ReadMap` adopts `ptr`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                flags,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(ReadMap { ptr, len })
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is a live `PROT_READ` mapping of `len` bytes until `drop`, and the
        // slice borrows `self`. Nothing in this process writes through it.
        unsafe { std::slice::from_raw_parts(self.ptr.cast::<u8>(), self.len) }
    }
}

impl Drop for ReadMap {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are the mapping `new` made; this `ReadMap` owns it and drops once.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::fd::AsFd as _;

    fn memfd(bytes: &[u8]) -> std::fs::File {
        let fd = rustix::fs::memfd_create("pf-dmabuf-test", rustix::fs::MemfdFlags::CLOEXEC)
            .expect("memfd_create");
        let mut f = std::fs::File::from(fd);
        f.write_all(bytes).expect("write memfd");
        f
    }

    /// Both modes read the object's bytes; the mapping outlives the fd.
    #[test]
    fn maps_a_prefix_in_either_mode() {
        for share in [Share::Shared, Share::Private] {
            let f = memfd(b"punktfunk");
            let map = ReadMap::new(f.as_fd(), 4, share).expect("map");
            drop(f);
            assert_eq!(map.bytes(), b"punk");
        }
    }

    /// A length past the end would SIGBUS on touch, so it never maps.
    #[test]
    fn refuses_a_length_past_the_end() {
        let f = memfd(b"abc");
        assert_eq!(byte_len(f.as_fd()).unwrap(), 3);
        assert!(ReadMap::new(f.as_fd(), 4, Share::Shared).is_err());
        assert!(ReadMap::new(f.as_fd(), 0, Share::Private).is_err());
    }
}
