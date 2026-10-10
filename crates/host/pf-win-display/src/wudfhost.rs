//! The WUDFHost a driver channel duplicates its handles into: open it with the shared
//! rights mask, then prove its image path. A security boundary
//! (`design/idd-push-security.md`): no path-helper fallbacks.

use anyhow::{bail, Context, Result};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use windows::core::PWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_DUP_HANDLE, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

/// Image path is `%SystemRoot%\System32\WUDFHost.exe` before duplicating
/// handles into `process`. `what` names the channel in the error.
///
/// Path only — not our UMDF host, and not authorization. Callers judge
/// sufficiency (`design/idd-push-security.md`). A token/session check
/// false-negatives: genuine host and spawned copy are both session 0
/// LocalService. A `process` without `PROCESS_QUERY_LIMITED_INFORMATION` fails the query.
pub fn verify_is_wudfhost(process: BorrowedHandle<'_>, wudf_pid: u32, what: &str) -> Result<()> {
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `process` is a live borrowed handle; `buf`/`len` are a valid out-buffer.
    // On success `len` is the UTF-16 unit count written (no NUL).
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .with_context(|| format!("QueryFullProcessImageNameW on the {what} pid"))?;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let got = path.to_ascii_lowercase().replace('/', "\\");
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let expected = format!("{}\\system32\\wudfhost.exe", sysroot.to_ascii_lowercase());
    if got != expected {
        bail!(
            "{what} pid {wudf_pid} is not the system WUDFHost (image={path:?}, expected \
             {expected:?}) — refusing to duplicate the channel's handles into it (spoofed driver / \
             wrong devnode?)"
        );
    }
    Ok(())
}

/// Open `pid` as a handle-duplication target and prove it is the system WUDFHost.
///
/// The mask and the check travel together: `DUP_HANDLE` to place section handles,
/// `QUERY_LIMITED_INFORMATION` for the image-path proof, `SYNCHRONIZE` so the
/// retained handle doubles as the incumbent-liveness probe. `what` names the
/// channel in the error. Brokers diverge after this point — what they duplicate
/// and with which rights is theirs.
pub fn open_wudfhost(pid: u32, what: &str) -> Result<OwnedHandle> {
    if pid == 0 {
        bail!("no WUDFHost pid for the {what} sections");
    }
    // SAFETY: `pid` is a copy. The handle (`?`-checked) is owned solely here and moved into
    // `OwnedHandle` (single owner, closes on drop).
    let process = unsafe {
        let h = OpenProcess(
            PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
        .with_context(|| format!("OpenProcess(PROCESS_DUP_HANDLE) on the {what} pid"))?;
        OwnedHandle::from_raw_handle(h.0 as _)
    };
    verify_is_wudfhost(process.as_handle(), pid, what)?;
    Ok(process)
}
