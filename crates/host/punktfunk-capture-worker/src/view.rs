//! The worker's mapping of the host's AU section, as the session's writer reaches it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use pf_encode_session::section::SectionView;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Memory::{
    MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
};

/// `len` bytes of the section, mapped read/write until drop.
pub struct View {
    base: *mut u8,
    len: usize,
}

// SAFETY: the pointer names a mapping this value owns for its whole life, and every access goes
// through atomics or the single-writer heap copy, so moving it between threads moves nothing
// thread-bound.
unsafe impl Send for View {}
// SAFETY: as above — shared access is atomics and one writer's `memcpy` into bytes no reader
// looks at before the publishing store.
unsafe impl Sync for View {}

impl View {
    /// Map `len` bytes of the section `handle` names. `None` when it names no read/write
    /// section of at least that size; the handle is left as it was either way.
    pub fn map(handle: HANDLE, len: usize) -> Option<Self> {
        // SAFETY: `MapViewOfFile` validates the handle against this process's table and fails
        // rather than faults on anything that is not a mappable section.
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, len) };
        (!view.Value.is_null()).then(|| Self {
            base: view.Value.cast(),
            len,
        })
    }

    /// `off..off + n` is inside the view and `align`-aligned. The base is page-aligned, so
    /// field alignment is offset alignment.
    fn check(&self, off: usize, n: usize, align: usize) {
        assert!(
            off % align == 0 && off.checked_add(n).is_some_and(|end| end <= self.len),
            "AU section access out of bounds (off={off}, n={n}, len={})",
            self.len
        );
    }
}

impl SectionView for View {
    fn atomic_u32(&self, off: usize) -> &AtomicU32 {
        self.check(off, 4, 4);
        // SAFETY: `check` proved the word is inside the live mapping and 4-aligned; every bit
        // pattern is a valid u32.
        unsafe { &*self.base.add(off).cast::<AtomicU32>() }
    }

    fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        self.check(off, 8, 8);
        // SAFETY: as `atomic_u32`, with 8-byte size and alignment checked.
        unsafe { &*self.base.add(off).cast::<AtomicU64>() }
    }

    fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        self.check(off, dst.len(), 1);
        for (i, byte) in dst.iter_mut().enumerate() {
            // SAFETY: `check` proved the byte is inside the live mapping. The host may write it
            // meanwhile, so it is read as the atomic it is.
            *byte = unsafe { &*self.base.add(off + i).cast::<AtomicU8>() }.load(Ordering::Relaxed);
        }
    }

    fn copy_from_slice(&self, off: usize, src: &[u8]) {
        self.check(off, src.len(), 1);
        // SAFETY: `check` proved the range is inside the live mapping; the writer's protocol
        // keeps every reader off it until the publishing store.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(off), src.len()) }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let addr = MEMORY_MAPPED_VIEW_ADDRESS {
            Value: self.base.cast::<c_void>(),
        };
        // SAFETY: `base` is the live view from `MapViewOfFile`, unmapped exactly once, here.
        let _ = unsafe { UnmapViewOfFile(addr) };
    }
}
