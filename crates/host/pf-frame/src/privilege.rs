//! Enable a privilege this process's token holds but has disabled.

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Enable the privilege `name` (`"SeShutdownPrivilege"`) on this process token.
///
/// `AdjustTokenPrivileges` succeeds on a token that lacks the privilege and reports
/// `ERROR_NOT_ALL_ASSIGNED` through `GetLastError`; that is an `Err` here. A UAC-filtered
/// token lacks most of them.
pub fn enable(name: &str) -> windows::core::Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: the current-process pseudo-handle is always valid and never closed; `token` is a
    // local the callee only writes, used below only on success.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }?;
    let enabled = enable_on(token, &HSTRING::from(name));
    // SAFETY: `token` was opened above, is owned here, and is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    enabled
}

fn enable_on(token: HANDLE, name: &HSTRING) -> windows::core::Result<()> {
    let mut luid = LUID::default();
    // SAFETY: a null system name means the local system; `name` is a NUL-terminated HSTRING
    // borrowed for the call, and `luid` is a local the callee only writes.
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) }?;
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: `token` is live and opened with TOKEN_ADJUST_PRIVILEGES; `tp` is a local whose
    // `PrivilegeCount` matches its one-element array, borrowed only for the call.
    unsafe { AdjustTokenPrivileges(token, false, Some(&raw const tp), 0, None, None) }?;
    // SAFETY: reads this thread's last-error value, set by the call directly above.
    let last = unsafe { GetLastError() };
    if last == ERROR_NOT_ALL_ASSIGNED {
        return Err(last.into());
    }
    Ok(())
}
