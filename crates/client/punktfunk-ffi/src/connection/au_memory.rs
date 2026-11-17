//! AU memory the embedder lends ([`punktfunk_connection_set_au_memory`]): the core assembles
//! each access unit in a buffer the embedder's decoder reads. A data shard whose frame is
//! already in flight decrypts straight into it; any other is copied in once
//! ([`punktfunk_core::au`]).

#[cfg(feature = "quic")]
use crate::*;
#[cfg(feature = "quic")]
use punktfunk_core::au::{AuMemory, AuStore};
use std::ffi::c_void;

/// One buffer the embedder lent for an AU, filled in by [`PunktfunkAuMemory::take`].
#[repr(C)]
pub struct PunktfunkAuStore {
    /// Writable for `len` bytes until `release`; 4-byte aligned.
    pub data: *mut u8,
    pub len: usize,
    /// Handed back to `release`.
    pub token: u64,
}

/// How the embedder lends AU memory. `take` fills `out` with a buffer of at least `len`
/// bytes and returns true, or returns false for the heap. `release` gives a buffer back:
/// after the last [`punktfunk_connection_next_au`] pointer into it is dead, on any thread.
/// Both run on core threads and must return at once.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkAuMemory {
    pub take: Option<
        unsafe extern "C" fn(userdata: *mut c_void, len: usize, out: *mut PunktfunkAuStore) -> bool,
    >,
    pub release: Option<unsafe extern "C" fn(userdata: *mut c_void, token: u64)>,
    pub userdata: *mut c_void,
}

/// The embedder's callbacks as the core's lender.
#[cfg(feature = "quic")]
struct Lender(PunktfunkAuMemory);

// SAFETY: the `PunktfunkAuMemory` contract: its callbacks and `userdata` serve any thread.
#[cfg(feature = "quic")]
unsafe impl Send for Lender {}
// SAFETY: as above.
#[cfg(feature = "quic")]
unsafe impl Sync for Lender {}

#[cfg(feature = "quic")]
impl AuMemory for Lender {
    fn take(&self, len: usize) -> Option<Box<dyn AuStore>> {
        let take = self.0.take?;
        let mut out = PunktfunkAuStore {
            data: std::ptr::null_mut(),
            len: 0,
            token: 0,
        };
        // SAFETY: the embedder's `take` with its own `userdata`; `out` is a local.
        if !unsafe { take(self.0.userdata, len, &mut out) } || out.data.is_null() {
            return None;
        }
        // A short buffer is still released: `AuBuf::take` drops it for the heap.
        Some(Box::new(Lent {
            data: out.data,
            len: out.len,
            token: out.token,
            memory: self.0,
        }))
    }
}

/// One lent buffer; dropping it is the embedder's `release`.
#[cfg(feature = "quic")]
struct Lent {
    data: *mut u8,
    len: usize,
    token: u64,
    memory: PunktfunkAuMemory,
}

// SAFETY: the buffer is the embedder's, writable from any thread until `release`.
#[cfg(feature = "quic")]
unsafe impl Send for Lent {}

#[cfg(feature = "quic")]
impl AuStore for Lent {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `data` is readable for `len` bytes until `release`, which only `Drop` calls.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the access exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.data, self.len) }
    }
}

#[cfg(feature = "quic")]
impl Drop for Lent {
    fn drop(&mut self) {
        if let Some(release) = self.memory.release {
            // SAFETY: the embedder's `release` with its own `userdata` and the token it gave.
            unsafe { release(self.memory.userdata, self.token) };
        }
    }
}

/// Assemble video AUs in memory the embedder lends, from the next frame on; a null
/// `memory`, or one without `take`, goes back to the heap. The lender outlives every buffer
/// it lent: install the heap, then close the connection, before freeing it.
///
/// # Safety
/// `c` is a valid connection handle; `memory` is null or readable for one
/// `PunktfunkAuMemory` whose callbacks keep its contract.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_au_memory(
    c: *mut PunktfunkConnection,
    memory: *const PunktfunkAuMemory,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: null or readable for one struct, per the fn contract.
        let lender = unsafe { memory.as_ref() }.filter(|m| m.take.is_some());
        let memory: std::sync::Arc<dyn AuMemory> = match lender {
            Some(m) => std::sync::Arc::new(Lender(*m)),
            None => std::sync::Arc::new(punktfunk_core::au::HeapPool::default()),
        };
        c.inner.set_au_memory(memory);
        PunktfunkStatus::Ok
    })
}
