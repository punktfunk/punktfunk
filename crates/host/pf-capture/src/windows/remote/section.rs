//! The host's sealed sections: a pagefile mapping owned by one value, and the AU section with
//! its ready event as a remote encoder's [`AuMemory`].
//!
//! Every section is unnamed. The far side reaches it only through a handle duplicated into its
//! process with the rights below, delivered as a bare value and adopted on success. Evidence:
//! `design/idd-push-security.md`.

use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::AtomicU32;
use std::sync::Arc;

use anyhow::{Context, Result};
use pf_driver_proto::section::SectionView;
use pf_paths_win::SecurityAttributes;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

use super::broker::RemoteProcess;
use super::transport::{AuMemory, Planted};

/// Map-only on the far side's section duplicate. No OWNER / `WRITE_DAC` / DELETE.
pub(crate) const SECTION_MAP_RW: u32 = 0x0004 | 0x0002;
/// The far side only `SetEvent`s; the host keeps `SYNCHRONIZE` on its own handle.
const EVENT_MODIFY_STATE: u32 = 0x0002;

/// `SECURITY_ATTRIBUTES` for the unnamed AU section and its event: SDDL `D:P(A;;GA;;;SY)`,
/// protected, `bInheritHandle: false`. The cursor section takes the process default instead.
///
/// The far side never opens by name; it receives duplicated handles (access travels with the
/// handle). See `design/idd-push-security.md`.
fn shared_object_sa() -> Result<SecurityAttributes> {
    SecurityAttributes::new("D:P(A;;GA;;;SY)").context("build SDDL for IDD-push shared objects")
}

/// A file mapping and its view of `len` bytes. Drop unmaps, then [`OwnedHandle`] closes.
///
/// The far side writes the same bytes, so this side reaches them only through the accessors:
/// each checks its range and reads or writes an atomic. Every accessor panics out of range.
pub(crate) struct MappedSection {
    pub(crate) handle: OwnedHandle,
    /// What `Drop` unmaps.
    base: MEMORY_MAPPED_VIEW_ADDRESS,
    /// Copied out only by [`AuSection::share`]; its `'static` ends with `Drop`.
    view: SectionView<'static>,
}

// SAFETY: `!Send` only through the view pointers. The mapping is process-wide, so its one
// owner may use, unmap and close it from any thread; the far side's concurrent writes arrive
// through the section's atomics. Not `Sync`.
unsafe impl Send for MappedSection {}

impl MappedSection {
    /// A pagefile-backed section of `len` bytes, mapped read/write. Windows hands its pages
    /// over zeroed. `sa` is its DACL; `None` takes the process default.
    pub(crate) fn create(len: usize, sa: Option<&SecurityAttributes>, what: &str) -> Result<Self> {
        let size = u32::try_from(len).with_context(|| format!("size the {what} section"))?;
        // SAFETY: `sa` outlives the call, and the handle it returns is new and owned from here.
        let handle = unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                sa.map(SecurityAttributes::as_ptr),
                PAGE_READWRITE,
                0,
                size,
                PCWSTR::null(),
            )
            .with_context(|| format!("create the {what} section"))?;
            OwnedHandle::from_raw_handle(map.0 as _)
        };
        // SAFETY: a live section handle of `len` bytes; a refused map returns null.
        let base = unsafe {
            MapViewOfFile(
                HANDLE(handle.as_raw_handle()),
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                len,
            )
        };
        if base.Value.is_null() {
            return Err(windows::core::Error::from_thread())
                .with_context(|| format!("map the {what} section"));
        }
        // SAFETY: `base` is a new page-aligned read/write mapping of `len` bytes. Only `Drop`
        // unmaps it, after the last access, and no Rust reference points into it.
        let view = unsafe { SectionView::from_raw(base.Value.cast(), len) };
        Ok(Self { handle, base, view })
    }

    /// The 4-aligned word at `off`, as the atomic both sides treat it as.
    pub(crate) fn word32(&self, off: usize) -> &AtomicU32 {
        self.view.atomic_u32(off)
    }

    /// Copy `dst.len()` bytes out from `off`. The far side may be writing them: the caller's
    /// sequence check decides whether the copy is whole.
    pub(crate) fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        self.view.read_bytes(off, dst);
    }
}

impl Drop for MappedSection {
    fn drop(&mut self) {
        // SAFETY: `base` is the live `MapViewOfFile` mapping; unmap before `handle` closes.
        unsafe {
            let _ = UnmapViewOfFile(self.base);
        }
    }
}

/// The mapped AU section and its ready event, alive for the encoder's life. The far side holds
/// its own duplicates; dropping this unmaps and closes only the host's handles. A
/// [`AuMemory::signal`] holds the event past that.
struct AuSection {
    section: MappedSection,
    event: Arc<OwnedHandle>,
}

impl AuSection {
    /// A sealed section of `bytes` and its auto-reset event, both SYSTEM-only.
    fn create(bytes: usize) -> Result<Self> {
        let sa = shared_object_sa()?;
        let section = MappedSection::create(bytes, Some(&sa), "AU")?;
        // SAFETY: `sa` outlives the call, and the handle it returns is new and owned from here.
        let event = unsafe {
            let event = CreateEventW(Some(sa.as_ptr()), false, false, PCWSTR::null())
                .context("CreateEvent(AU section)")?;
            OwnedHandle::from_raw_handle(event.0 as _)
        };
        Ok(Self {
            section,
            event: Arc::new(event),
        })
    }
}

impl AuMemory for AuSection {
    // unsafe-fn-no-op-ok: the contract is the mapping's lifetime, checked by every accessor.
    unsafe fn share(&self) -> SectionView<'static> {
        self.section.view
    }

    /// The auto-reset event is the wakeup only; the header is the truth, so a consumed signal
    /// is never a lost chunk.
    fn wait(&self, timeout_ms: u32) {
        // SAFETY: `event` is this section's live handle; the bounded wait only reads it.
        let _ = unsafe { WaitForSingleObject(HANDLE(self.event.as_raw_handle()), timeout_ms) };
    }

    /// The host's own `SetEvent` on its full-access handle: the wait returns, finds the
    /// header unchanged, and the caller decides.
    fn signal(&self) -> Box<dyn Fn() + Send + Sync> {
        let event = self.event.clone();
        Box::new(move || {
            // SAFETY: `event` holds the handle open for this closure's life; SetEvent only sets
            // the event's state.
            let _ = unsafe { SetEvent(HANDLE(event.as_raw_handle())) };
        })
    }
}

/// A new AU section of `bytes` and its event, duplicated into `process` with the map and
/// signal rights alone. A failed second duplicate reaps the first.
pub(crate) fn plant(process: &RemoteProcess, bytes: usize) -> Result<Planted> {
    let au = AuSection::create(bytes)?;
    let section = process.dup_into(au.section.handle.as_handle(), Some(SECTION_MAP_RW))?;
    let event = match process.dup_into(au.event.as_handle(), Some(EVENT_MODIFY_STATE)) {
        Ok(e) => e,
        Err(e) => {
            process.close_remote(section);
            return Err(e);
        }
    };
    Ok(Planted {
        memory: Box::new(au),
        section,
        event,
    })
}
