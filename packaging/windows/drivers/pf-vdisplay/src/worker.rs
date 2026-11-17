//! Thread + handle ownership for the driver's background workers.
//!
//! A thread that closed its own handles at exit would put a handle's lifetime where no owner can
//! observe it: a caller that copied the value out under the registry lock could hand a closed —
//! and by then possibly reused — handle to a DDI. The rule here is the opposite: a handle or
//! mapping belongs to one value, its `Drop` closes it exactly once, and a thread only ever
//! borrows what its owner outlives.
//!
//! [`Worker`] owns the stop event and the join handle, so dropping it signals, joins, and only
//! then closes. [`OwnedHandle`] and [`OwnedView`] are the RAII wrappers for `CloseHandle` and
//! `MapViewOfFile`/`UnmapViewOfFile`.

use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::{
    Win32::Foundation::WAIT_OBJECT_0,
    Win32::System::Threading::{
        AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentThread,
        SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL, WaitForSingleObject,
    },
    core::w,
};

/// Carries a raw handle across a thread spawn: a WDF/IddCx handle (`*mut T`) or a Win32
/// [`HANDLE`]. Nothing else is `Send` through it.
///
/// Those handles are `!Send`, but they are plain process-wide values whose lifetime the
/// framework — not the compiler — governs. Rebind the WHOLE wrapper inside the closure
/// (`let x = x;`) before touching `.0`: disjoint closure captures would otherwise capture the
/// `!Send` field itself and defeat the wrapper.
pub struct Sendable<T>(pub T);
// SAFETY: an opaque framework handle, never dereferenced in Rust; the owner that handed it
// over outlives the thread using it, and the DDIs it is passed to do their own locking.
unsafe impl<T> Send for Sendable<*mut T> {}
// SAFETY: a shared `&Sendable<*mut T>` yields only by-value copies of that opaque handle.
unsafe impl<T> Sync for Sendable<*mut T> {}
// SAFETY: a Win32 handle is a process-wide token, not thread-affine; the owner that handed it
// over closes it only after the thread using it has joined.
unsafe impl Send for Sendable<HANDLE> {}

/// A Win32 handle this process owns; `Drop` closes it exactly once.
pub struct OwnedHandle(HANDLE);
// SAFETY: a Win32 handle is a process-wide token, not thread-affine; this wrapper hands out only
// borrowed copies and is the handle's sole closer, so moving it between threads is sound.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above — a shared reference yields only by-value copies, and the OS serializes
// every operation on the handle itself.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    /// An unnamed event owned by the returned value. `manual_reset` keeps it signalled until
    /// something resets it (a stop flag); auto-reset releases one waiter per signal (a data
    /// event). `None` if the OS refused, which every caller treats as "run without it".
    pub fn event(manual_reset: bool) -> Option<Self> {
        // SAFETY: plain event creation — unsignalled, unnamed, no security descriptor.
        let h = unsafe { CreateEventW(None, manual_reset, false, None) }.ok()?;
        Some(Self(h))
    }

    // unsafe-fn-no-op-ok: the marker IS the transfer. A safe fn here would let safe code hand in
    // a borrowed or already-owned handle and get a second CloseHandle when this value drops.
    /// Adopt `h`: from here on this value alone decides when the handle closes.
    ///
    /// # Safety
    /// `h` must be a live handle this process owns, and no other owner may close it.
    pub unsafe fn from_raw(h: HANDLE) -> Self {
        Self(h)
    }

    /// A borrowed copy for a DDI call or a thread wait — valid only while `self` lives.
    pub fn as_raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: we adopted this handle and only ever hand out borrowed copies of it, so this
        // is its sole close.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A mapped section view and the mapping handle behind it.
///
/// One value because the two have one lifetime: `Drop` unmaps the view, then field order drops
/// `mapping` and closes it. Move it into the thread that reads the view and the mapping is
/// released when that thread returns — never earlier, and never twice.
pub struct OwnedView {
    base: *mut core::ffi::c_void,
    /// Never read: held so it closes AFTER the unmap in `Drop` (field order).
    _mapping: OwnedHandle,
}
// SAFETY: a mapped view is process-wide, not thread-affine; this value is its only unmapper, so
// moving it to the thread that reads the view moves the whole ownership.
unsafe impl Send for OwnedView {}

impl OwnedView {
    // unsafe-fn-no-op-ok: same transfer as OwnedHandle::from_raw, plus mapped-exactly-once --
    // UnmapViewOfFile cannot tell a second view of one mapping from the first.
    /// Adopt the view at `base` over `mapping`.
    ///
    /// # Safety
    /// `base` must be a live `MapViewOfFile` result for `mapping`, mapped exactly once, and
    /// `mapping` must be a section handle this process owns.
    pub unsafe fn from_raw(base: *mut core::ffi::c_void, mapping: OwnedHandle) -> Self {
        Self {
            base,
            _mapping: mapping,
        }
    }

    /// The view's base address — valid only while `self` lives.
    pub fn base(&self) -> *mut core::ffi::c_void {
        self.base
    }
}

impl Drop for OwnedView {
    fn drop(&mut self) {
        // SAFETY: our own single mapping of `self.base`; the mapping handle closes immediately
        // after, when the `mapping` field drops.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base });
        }
    }
}

/// A background thread and the manual-reset event that stops it.
///
/// [`spawn`](Self::spawn) hands the thread a BORROWED stop handle; this value closes it only
/// after the join, so the thread can never wait on a handle its owner already closed. Dropping
/// the `Worker` is the only way the thread ends, which makes the owner's lifetime the thread's
/// lifetime — the property every caller reasons from.
pub struct Worker {
    stop: OwnedHandle,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Start `body` on a thread called `name`, passing it the stop event to wait on.
    ///
    /// `None` when the event or the thread could not be created. On that path `body` is
    /// dropped, so whatever it captured — a mapping, a view — is released by its own `Drop`
    /// instead of leaking into the host process's handle table; a caller that must undo more
    /// than that gets the `None` to do it with.
    pub fn spawn(name: &str, body: impl FnOnce(HANDLE) + Send + 'static) -> Option<Worker> {
        let stop = OwnedHandle::event(true)?;
        let raw = Sendable(stop.as_raw());
        let join = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let raw = raw; // capture the wrapper, not the `!Send` handle inside it
                body(raw.0);
            })
            .ok()?;
        Some(Worker {
            stop,
            join: Some(join),
        })
    }

    /// Signal the stop event and join the thread. Idempotent — the stop handle stays open until
    /// `self` drops, which is strictly after this join.
    pub fn stop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        let name = join.thread().name().unwrap_or("?").to_string();
        // SAFETY: our own manual-reset event, alive until `self` drops (after this join).
        unsafe {
            let _ = SetEvent(self.stop.as_raw());
        }
        let started = Instant::now();
        let _ = join.join();
        let took = started.elapsed();
        if took > Duration::from_millis(250) {
            dbglog!("[pf-vd] worker {name} join took {} ms", took.as_millis());
        }
    }

    /// Signal the stop event and wait at most `bound` for the thread. `true` = joined. `false`
    /// = the thread is still inside something; it is DETACHED — this value is leaked whole, so
    /// the stop event it may still wait on stays open and signalled, and nothing here ever
    /// blocks on it again. A thread that never returns is the caller's accounting problem.
    #[must_use]
    pub fn stop_within(mut self, bound: Duration) -> bool {
        use std::os::windows::io::AsRawHandle;
        let Some(join) = self.join.take() else {
            return true;
        };
        // SAFETY: our own manual-reset event, alive until `self` drops or leaks.
        unsafe {
            let _ = SetEvent(self.stop.as_raw());
        }
        let thread = HANDLE(join.as_raw_handle());
        // SAFETY: `thread` is the live native handle `join` owns for the duration of the wait.
        let waited = unsafe { WaitForSingleObject(thread, bound.as_millis() as u32) };
        if waited == WAIT_OBJECT_0 {
            let _ = join.join();
            return true;
        }
        dbglog!(
            "[pf-vd] worker {} did not stop within {} ms — detached",
            join.thread().name().unwrap_or("?"),
            bound.as_millis()
        );
        std::mem::forget(join);
        std::mem::forget(self);
        false
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

/// An MMCSS "Distribution" registration for the calling thread, reverted on drop. The fallback
/// when MMCSS declines under the WUDFHost token is TIME_CRITICAL, the highest band without the
/// realtime class; the thread spends its life blocked on events, so it cannot starve others.
pub struct Mmcss(Option<HANDLE>);

impl Mmcss {
    pub fn distribution(what: &str) -> Self {
        let mut task = 0u32;
        // SAFETY: `w!("Distribution")` is a 'static null-terminated UTF-16 task name; `task` is
        // a valid local out-param. The returned handle is reverted in `Drop`.
        let res = unsafe { AvSetMmThreadCharacteristicsW(w!("Distribution"), &mut task) };
        match res {
            Ok(h) => Self(Some(h)),
            Err(e) => {
                // SAFETY: plain FFI; `GetCurrentThread` is a pseudo-handle (never fails, nothing
                // to close) and `SetThreadPriority` on it affects only this thread.
                let fallback =
                    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
                dbglog!(
                    "[pf-vd] {what}: MMCSS declined ({e:?}) — TIME_CRITICAL fallback ok={}",
                    fallback.is_ok()
                );
                Self(None)
            }
        }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: `h` is the live characteristics handle `distribution` registered, reverted
            // exactly once here.
            let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
        }
    }
}
