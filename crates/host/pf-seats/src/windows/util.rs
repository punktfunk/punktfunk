//! Small Win32 conversions shared by the Windows seat modules.
//!
//! Paths and account names become checked NUL-terminated UTF-16 vectors. Win32
//! and NetAPI failures become stable backend errors without including secret
//! inputs. Process identity is compared by SID, not by a localized account
//! name. Handle ownership transfers immediately into `OwnedHandle`, and every
//! raw-memory read is limited by the size returned from the originating API.
//! These helpers perform no account, session, service, or registry mutation.

use crate::backend::BackendError;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use windows::core::PWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid, PSID, TOKEN_QUERY, TOKEN_USER,
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
    let mut raw_token = HANDLE::default();
    // SAFETY: the current-process pseudo-handle stays valid; `raw_token` receives one owned handle.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) }
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
    use windows::Win32::NetworkManagement::IpHelper::{
        MIB_UDP6ROW_OWNER_PID, MIB_UDPROW_OWNER_PID,
    };
    // `ADDRESS_FAMILY` values; the WinSock feature isn't pulled in for two numbers.
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 23;
    let v4 = udp_rows::<MIB_UDPROW_OWNER_PID>(AF_INET)
        .into_iter()
        .map(|r| (r.dwLocalPort, r.dwOwningPid));
    let v6 = udp_rows::<MIB_UDP6ROW_OWNER_PID>(AF_INET6)
        .into_iter()
        .map(|r| (r.dwLocalPort, r.dwOwningPid));
    // The port sits in the low 16 bits, in network byte order.
    v4.chain(v6)
        .filter(|(local, _)| u16::from_be(*local as u16) == port)
        .map(|(_, pid)| pid)
        .collect()
}

/// One `GetExtendedUdpTable(UDP_TABLE_OWNER_PID)` snapshot for `family`.
fn udp_rows<Row: Copy>(family: u32) -> Vec<Row> {
    use windows::Win32::NetworkManagement::IpHelper::{GetExtendedUdpTable, UDP_TABLE_OWNER_PID};
    const NO_ERROR: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
    let mut size = 0_u32;
    // The table grows between the sizing call and the read; three tries is plenty.
    for _ in 0..3 {
        // u64 words: the table's `u32` count and rows need 4-byte alignment.
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
        let bytes = buffer.len() * 8;
        let base = buffer.as_ptr().cast::<u8>();
        // SAFETY: the table starts with its `u32` row count.
        let count = unsafe { base.cast::<u32>().read() } as usize;
        let first = std::mem::size_of::<u32>();
        let fits = count
            .checked_mul(std::mem::size_of::<Row>())
            .and_then(|rows| rows.checked_add(first))
            .is_some_and(|end| end <= bytes);
        if !fits {
            return Vec::new();
        }
        // SAFETY: `count` rows of `Row` follow the count inside `buffer` (checked
        // above), and every row field is a `u32`, so offset 4 is aligned for it.
        let rows = unsafe { std::slice::from_raw_parts(base.add(first).cast::<Row>(), count) };
        return rows.to_vec();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
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
