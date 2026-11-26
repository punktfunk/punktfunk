//! Small Win32 helpers shared by the Windows seat modules.
//!
//! - Paths and account names become NUL-terminated UTF-16 vectors, checked on the way.
//! - Win32 and NetAPI failures become stable backend errors that include no secret input.
//! - Process identity is compared by SID, never by a localized account name.
//! - A handle goes into an `OwnedHandle` at once. The one exception is the token in
//!   `with_privilege`, which that function closes itself.
//! - Every read of raw memory is limited by the size that the originating API returned.
//!
//! These helpers change no account, session, service or registry state. The one token change, a
//! privilege enabled for one call, is undone after the call.

use crate::backend::BackendError;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, CheckTokenMembership, CreateWellKnownSid, EqualSid, GetTokenInformation,
    LookupPrivilegeValueW, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    LUID_AND_ATTRIBUTES, PSID, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::WindowsProgramming::GetComputerNameW;

pub(super) type WinResult<T> = Result<T, BackendError>;

pub(super) fn backend_error(code: &str, message: impl Into<String>) -> BackendError {
    BackendError::new(code, message)
}

pub(super) fn io_error(code: &str, context: &str, error: impl std::fmt::Display) -> BackendError {
    backend_error(code, format!("{context}: {error}"))
}

pub(super) fn status_error(code: &str, context: &str, status: u32) -> BackendError {
    let error = std::io::Error::from_raw_os_error(status as i32);
    io_error(code, context, error)
}

pub(super) fn wide(value: impl AsRef<OsStr>, field: &str) -> WinResult<Vec<u16>> {
    let value = value.as_ref();
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    if encoded.contains(&0) {
        return Err(backend_error(
            "invalid_windows_string",
            format!("{field} contains an embedded NUL"),
        ));
    }
    encoded.push(0);
    Ok(encoded)
}

pub(super) fn computer_name() -> WinResult<String> {
    let mut buffer = vec![0_u16; 256];
    let mut length = buffer.len() as u32;
    // SAFETY: `buffer` is writable for `length` UTF-16 units and `length` is a live out-parameter.
    unsafe { GetComputerNameW(Some(PWSTR(buffer.as_mut_ptr())), &mut length) }
        .map_err(|error| io_error("computer_name", "GetComputerNameW failed", error))?;
    buffer.truncate(length as usize);
    String::from_utf16(&buffer)
        .map_err(|error| io_error("computer_name", "computer name is not valid UTF-16", error))
}

pub(super) fn is_local_system() -> WinResult<bool> {
    // SAFETY: the current-process pseudo-handle needs no close and stays valid.
    process_is_local_system(unsafe { GetCurrentProcess() })
}

/// Whether `process` runs as LocalSystem. The handle needs `PROCESS_QUERY_LIMITED_INFORMATION`.
pub(super) fn process_is_local_system(process: HANDLE) -> WinResult<bool> {
    let mut raw_token = HANDLE::default();
    // SAFETY: `process` is a live process handle; `raw_token` receives one owned handle.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut raw_token) }
        .map_err(|error| io_error("identity", "OpenProcessToken failed", error))?;
    // SAFETY: `raw_token` is fresh and has no other owner; this transfers ownership once.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token.0) };

    let mut required = 0_u32;
    // SAFETY: a null output buffer with length zero is the documented size query.
    let _ = unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            None,
            0,
            &mut required,
        )
    };
    if required < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(backend_error(
            "identity",
            "GetTokenInformation returned an invalid TOKEN_USER size",
        ));
    }
    let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut token_buffer = vec![0_usize; words];
    // SAFETY: the aligned allocation has at least `required` writable bytes and remains live.
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(token_buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
    }
    .map_err(|error| io_error("identity", "GetTokenInformation(TokenUser) failed", error))?;
    // SAFETY: the successful call initialized a TOKEN_USER at the aligned buffer start.
    let token_user = unsafe { &*(token_buffer.as_ptr().cast::<TOKEN_USER>()) };

    let mut system_sid = [0_usize; 16];
    let mut system_sid_bytes = std::mem::size_of_val(&system_sid) as u32;
    let system_psid = PSID(system_sid.as_mut_ptr().cast());
    // SAFETY: `system_sid` is writable for the advertised size and receives a well-known SID.
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(system_psid),
            &mut system_sid_bytes,
        )
    }
    .map_err(|error| io_error("identity", "CreateWellKnownSid(LocalSystem) failed", error))?;
    // SAFETY: both SIDs came from successful Windows APIs and remain live for this call.
    Ok(unsafe { EqualSid(token_user.User.Sid, system_psid) }.is_ok())
}

pub(super) fn require_local_system() -> WinResult<()> {
    if is_local_system()? {
        Ok(())
    } else {
        Err(backend_error(
            "local_system_required",
            "seat runtime and DPAPI operations must run as LocalSystem",
        ))
    }
}

pub(super) fn require_elevated_admin() -> WinResult<()> {
    if is_local_system()? {
        return Ok(());
    }
    let mut admin_sid = [0_usize; 16];
    let mut sid_bytes = std::mem::size_of_val(&admin_sid) as u32;
    let sid = PSID(admin_sid.as_mut_ptr().cast());
    // SAFETY: the aligned buffer is writable for `sid_bytes` and receives a well-known SID.
    unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(sid), &mut sid_bytes) }
        .map_err(|error| {
            io_error(
                "elevation",
                "CreateWellKnownSid(Administrators) failed",
                error,
            )
        })?;
    let mut member = windows::core::BOOL::default();
    // SAFETY: null token checks the effective thread/process token; SID and output remain live.
    unsafe { CheckTokenMembership(None, sid, &mut member) }
        .map_err(|error| io_error("elevation", "CheckTokenMembership failed", error))?;
    if member.as_bool() {
        Ok(())
    } else {
        Err(backend_error(
            "elevation_required",
            "this command requires an elevated Administrator token",
        ))
    }
}

/// `work` with the privilege `name` (`"SeRestorePrivilege"`) enabled on this process's token,
/// then put back as it was. A token without the privilege runs nothing.
pub(super) fn with_privilege<T>(name: &str, work: impl FnOnce() -> T) -> WinResult<T> {
    let name_w = wide(name, "privilege")?;
    let mut luid = LUID::default();
    // SAFETY: `name_w` is NUL-terminated and `luid` is a live out-parameter.
    unsafe { LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name_w.as_ptr()), &mut luid) }
        .map_err(|error| io_error("privilege", "LookupPrivilegeValueW failed", error))?;
    let mut token = HANDLE::default();
    // SAFETY: the current-process pseudo-handle needs no close; `token` receives one owned handle.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }
    .map_err(|error| io_error("privilege", "OpenProcessToken failed", error))?;
    let enable = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let mut previous = TOKEN_PRIVILEGES::default();
    let mut length = 0_u32;
    // SAFETY: `token` is open for adjusting; `enable` and `previous` are live one-entry locals
    // and `previous` is as large as the length passed.
    let adjusted = unsafe {
        AdjustTokenPrivileges(
            token,
            false,
            Some(std::ptr::from_ref(&enable)),
            std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
            Some(std::ptr::from_mut(&mut previous)),
            Some(std::ptr::from_mut(&mut length)),
        )
    };
    // SAFETY: reads this thread's last error, which the call above set.
    let assigned = adjusted.is_ok() && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED;
    let out = assigned.then(|| {
        let out = work();
        // An empty previous state: the privilege was on already and stays on.
        if previous.PrivilegeCount != 0 {
            // SAFETY: `token` is still open; `previous` is the state the call above returned.
            let _ = unsafe {
                AdjustTokenPrivileges(
                    token,
                    false,
                    Some(std::ptr::from_ref(&previous)),
                    0,
                    None,
                    None,
                )
            };
        }
        out
    });
    // SAFETY: `token` was opened above and is closed once here.
    let _ = unsafe { CloseHandle(token) };
    out.ok_or_else(|| backend_error("privilege", format!("this process lacks {name}")))
}

/// Whether something accepts a loopback TCP connection on `port`: the seat's
/// management port. The native port is QUIC; [`udp_port_owners`] answers for it.
pub(super) fn port_open(port: u16) -> bool {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
    TcpStream::connect_timeout(
        &SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        std::time::Duration::from_millis(100),
    )
    .is_ok()
}

/// The pids holding a UDP endpoint on `port`, IPv4 and IPv6. Empty when the
/// tables can't be read.
pub(super) fn udp_port_owners(port: u16) -> Vec<u32> {
    use std::mem::{offset_of, size_of};
    use windows::Win32::NetworkManagement::IpHelper::{
        MIB_UDP6ROW_OWNER_PID as V6, MIB_UDPROW_OWNER_PID as V4,
    };
    // `ADDRESS_FAMILY` values; the WinSock feature isn't pulled in for two numbers.
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    let v4 = udp_rows(
        AF_INET,
        size_of::<V4>(),
        offset_of!(V4, dwLocalPort),
        offset_of!(V4, dwOwningPid),
    );
    let v6 = udp_rows(
        AF_INET6,
        size_of::<V6>(),
        offset_of!(V6, dwLocalPort),
        offset_of!(V6, dwOwningPid),
    );
    // The port sits in the low 16 bits, in network byte order.
    v4.into_iter()
        .chain(v6)
        .filter(|(local, _)| u16::from_be(*local as u16) == port)
        .map(|(_, pid)| pid)
        .collect()
}

/// `(dwLocalPort, dwOwningPid)` of every row in one `GetExtendedUdpTable(UDP_TABLE_OWNER_PID)`
/// snapshot for `family`. `row` is the row struct's size, `port` and `pid` those fields' offsets.
fn udp_rows(family: u32, row: usize, port: usize, pid: usize) -> Vec<(u32, u32)> {
    use windows::Win32::NetworkManagement::IpHelper::{GetExtendedUdpTable, UDP_TABLE_OWNER_PID};
    const NO_ERROR: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut size = 0_u32;
    // The table grows between the sizing call and the read; three tries is plenty.
    for _ in 0..3 {
        // u64 words: Windows writes the table's `u32` count and rows at 4-byte alignment.
        let mut buffer = vec![0_u64; (size as usize).div_ceil(8).max(1)];
        // SAFETY: `buffer` holds at least `size` writable bytes and outlives the call;
        // `size` is a live in/out parameter.
        let rc = unsafe {
            GetExtendedUdpTable(
                Some(buffer.as_mut_ptr().cast()),
                &mut size,
                false,
                family,
                UDP_TABLE_OWNER_PID,
                0,
            )
        };
        if rc == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if rc != NO_ERROR {
            return Vec::new();
        }
        let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_ne_bytes()).collect();
        return table_rows(&bytes, row, port, pid);
    }
    Vec::new()
}

/// The rows of a UDP table image: a `u32` row count, then that many `row`-byte rows. Empty
/// when the count claims more rows than `table` holds.
fn table_rows(table: &[u8], row: usize, port: usize, pid: usize) -> Vec<(u32, u32)> {
    let field = |bytes: &[u8], at: usize| {
        let mut word = [0; 4];
        word.copy_from_slice(&bytes[at..at + 4]);
        u32::from_ne_bytes(word)
    };
    let Some((count, rows)) = table.split_first_chunk::<4>() else {
        return Vec::new();
    };
    let count = u32::from_ne_bytes(*count) as usize;
    let fits = count.checked_mul(row).is_some_and(|len| len <= rows.len());
    if !fits {
        return Vec::new();
    }
    rows.chunks_exact(row)
        .take(count)
        .map(|r| (field(r, port), field(r, pid)))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_table_claiming_more_rows_than_it_holds_reads_as_empty() {
        // Two 12-byte rows of (addr, port, pid).
        let mut table = 2_u32.to_ne_bytes().to_vec();
        for word in [1_u32, 0x50, 7, 2, 0x51, 9] {
            table.extend(word.to_ne_bytes());
        }
        assert_eq!(super::table_rows(&table, 12, 4, 8), [(0x50, 7), (0x51, 9)]);
        table[..4].copy_from_slice(&3_u32.to_ne_bytes());
        assert!(super::table_rows(&table, 12, 4, 8).is_empty());
        assert!(super::table_rows(&table[..3], 12, 4, 8).is_empty());
    }

    #[test]
    fn a_bound_udp_port_names_this_process() {
        for addr in ["127.0.0.1:0", "[::1]:0"] {
            let socket = std::net::UdpSocket::bind(addr).unwrap();
            let port = socket.local_addr().unwrap().port();
            assert!(super::udp_port_owners(port).contains(&std::process::id()));
        }
    }
}

#[cfg(test)]
mod live_tests {
    #[test]
    #[ignore = "requires a live elevated Windows service host"]
    fn process_identity_is_queryable() {
        assert!(super::is_local_system().is_ok());
    }
}
