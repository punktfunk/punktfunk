//! Handle-duplication broker for the driver's sealed sections.
//!
//! The AU and cursor sections are unnamed. The driver reaches them only through
//! handles this broker duplicates into its WUDFHost, delivered as bare values by
//! the owning IOCTL. The process handle doubles as the driver-death probe.
//! Evidence: `design/idd-push-security.md`.

use super::*;

/// On IOCTL success the driver owns (and closes) the duplicates; on any failure the
/// deliverer reaps what it planted ([`Self::close_remote`]).
pub(super) struct ChannelBroker {
    /// WUDFHost process (`ProcessSharingDisabled` — exclusively pf-vdisplay's).
    /// `SYNCHRONIZE` doubles as the driver-death probe ([`Self::driver_alive`]).
    process: OwnedHandle,
    pub(super) wudf_pid: u32,
}

impl ChannelBroker {
    /// Open the WUDFHost duplication target. `wudf_pid == 0` is a driver that
    /// predates the sealed channel. [`open_wudfhost`] proves the image path before
    /// any section handle is duplicated into it: a spoofed ADD pid (same interface
    /// GUID, different process) would otherwise receive the stream.
    pub(super) fn open(wudf_pid: u32) -> Result<Self> {
        let process = open_wudfhost(wudf_pid, "driver-channel")?;
        Ok(Self { process, wudf_pid })
    }

    /// Raise WUDFHost's GPU scheduling class. The driver's encoder submits its GPU work
    /// there, so the host's own class covers none of it. Best-effort. `self.process` keeps
    /// the verified WUDFHost alive, so `wudf_pid` cannot name another process meanwhile.
    pub(super) fn raise_gpu_priority(&self) {
        // SAFETY: plain open by pid; the result is checked before use.
        let h = match unsafe { OpenProcess(PROCESS_SET_INFORMATION, false, self.wudf_pid) } {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(wudf_pid = self.wudf_pid, error = %e, "WUDFHost GPU priority not raised");
                return;
            }
        };
        // SAFETY: `h` was just opened here; `OwnedHandle` becomes its sole owner.
        let owned = unsafe { OwnedHandle::from_raw_handle(h.0 as _) };
        // SAFETY: `owned` is live for the call and carries PROCESS_SET_INFORMATION.
        unsafe {
            pf_frame::dxgi::elevate_gpu_priority_of(HANDLE(owned.as_raw_handle()), "WUDFHost")
        };
    }

    /// `SYNCHRONIZE` wait: signaled ⇔ WUDFHost exited. A dead driver and an idle desktop
    /// both just stop advancing the source counter, so this is the only death signal.
    pub(super) fn driver_alive(&self) -> bool {
        // SAFETY: `process` is this broker's live `OwnedHandle` (borrowed for the call); a
        // 0 ms wait only reads the handle's signaled state.
        unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) != WAIT_OBJECT_0 }
    }

    /// Duplicate `h` into the WUDFHost table. The returned value is valid only there.
    /// `Some(rights)` grants exactly those rights; `None` copies the source
    /// (`DUPLICATE_SAME_ACCESS`).
    ///
    /// # Safety
    /// `h` must be a live handle of the current process.
    pub(super) unsafe fn dup_into(&self, h: HANDLE, access: Option<u32>) -> Result<u64> {
        let mut out = HANDLE::default();
        let (desired, options) = match access {
            Some(rights) => (rights, DUPLICATE_HANDLE_OPTIONS(0)),
            None => (0, DUPLICATE_SAME_ACCESS),
        };
        // SAFETY: `h` is live per the contract; `self.process` is the live PROCESS_DUP_HANDLE
        // target; `&mut out` is a valid out-param. Explicit mask (options == 0) or
        // `DUPLICATE_SAME_ACCESS` (desired ignored) — never both.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                h,
                HANDLE(self.process.as_raw_handle()),
                &mut out,
                desired,
                false,
                options,
            )
        }
        .context("DuplicateHandle into the driver's WUDFHost")?;
        Ok(out.0 as usize as u64)
    }

    /// Duplicate a cursor section into WUDFHost with the same `SECTION_MAP_RW` as the
    /// AU section.
    ///
    /// # Safety
    /// `h` must be a live handle of the current process.
    pub(super) unsafe fn dup_into_public(&self, h: HANDLE) -> Result<u64> {
        // SAFETY: forwarded — `h` is live per this fn's contract.
        unsafe { self.dup_into(h, Some(SECTION_MAP_RW)) }
    }

    /// Failure-path reaper for a cursor-channel duplicate the driver never adopted.
    pub(super) fn close_remote_public(&self, value: u64) {
        self.close_remote(value);
    }

    /// Close a handle VALUE in the WUDFHost table. `DUPLICATE_CLOSE_SOURCE` with no
    /// target closes the source; the result is ignored.
    pub(super) fn close_remote(&self, value: u64) {
        if value == 0 {
            return;
        }
        // SAFETY: `self.process` is the live duplication target; `value` is a handle this
        // broker just created there and the driver never received. Closing it cannot touch
        // any other process's handles.
        unsafe {
            let _ = DuplicateHandle(
                HANDLE(self.process.as_raw_handle()),
                HANDLE(value as usize as *mut core::ffi::c_void),
                HANDLE::default(),
                std::ptr::null_mut(),
                0,
                false,
                DUPLICATE_CLOSE_SOURCE,
            );
        }
    }
}
