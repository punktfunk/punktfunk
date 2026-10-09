//! The Win32 half of `mgmt::theme`: the SID of the user signed in to this host's session, and a
//! DWORD or string out of their hive. It lives here because `mgmt` forbids `unsafe`, and each of
//! these reads is a Win32 call.

use windows::core::{Owned, HSTRING, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_USER};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_USERS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;

/// The SID of the user signed in to this host's own session, as the string `HKEY_USERS` is keyed
/// by: the console user for the box host, the seat's user for a seat host.
///
/// The host is SYSTEM, so `HKEY_CURRENT_USER` is the service account's empty profile; the
/// player's hive is only reachable through their SID. A signed-out session has no token, and a
/// host that isn't SYSTEM can't ask for one: both report nothing.
pub(crate) fn session_sid() -> Option<String> {
    let session = super::interactive::current_process_session_id().ok()?;
    let mut token = HANDLE::default();
    // SAFETY: `token` is a live out-param; needs SE_TCB, which SYSTEM has.
    unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
    // SAFETY: the query succeeded, so `token` is a handle this frame alone owns.
    let token = unsafe { Owned::new(token) };
    // Sized in two calls: the first asks how big the variable-length SID is.
    let mut needed = 0u32;
    // SAFETY: a deliberate size probe — null buffer with zero length is the documented way
    // to ask, and it fails with ERROR_INSUFFICIENT_BUFFER while setting `needed`.
    let _ = unsafe { GetTokenInformation(*token, TokenUser, None, 0, &mut needed) };
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is `needed` bytes, which is what the probe above asked for.
    unsafe {
        GetTokenInformation(
            *token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    // SAFETY: on success the buffer holds a TOKEN_USER whose `Sid` points inside it. A
    // `Vec<u8>` is only byte-aligned, so the struct is read unaligned.
    let sid = unsafe { buf.as_ptr().cast::<TOKEN_USER>().read_unaligned() }
        .User
        .Sid;
    let mut out = PWSTR::null();
    // SAFETY: `sid` is the live SID above; `out` is a live local out-param.
    unsafe { ConvertSidToStringSidW(sid, &mut out) }.ok()?;
    // SAFETY: `out` is the LocalAlloc'd string the call returned; `Owned` frees it once.
    let _free = unsafe { Owned::new(HLOCAL(out.0.cast())) };
    // SAFETY: `out` is a NUL-terminated wide string from the successful call above.
    unsafe { out.to_string() }.ok()
}

/// A DWORD under `HKEY_USERS\<subkey>`, or `None` when the value is absent or not a DWORD.
pub(crate) fn read_dword(subkey: &str, name: &str) -> Option<u32> {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated HSTRINGs; `value`/`size` are live out-params
    // sized for the DWORD the flags demand.
    let rc = unsafe {
        RegGetValueW(
            HKEY_USERS,
            &HSTRING::from(subkey),
            &HSTRING::from(name),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::addr_of_mut!(value).cast()),
            Some(&mut size),
        )
    };
    (rc == ERROR_SUCCESS).then_some(value)
}

/// A string under `HKEY_USERS\<subkey>`, or `None` when the value is absent or not a string.
pub(crate) fn read_string(subkey: &str, name: &str) -> Option<String> {
    let (subkey, name) = (HSTRING::from(subkey), HSTRING::from(name));
    let mut size = 0u32;
    // SAFETY: a size probe — null buffer; `size` is a live out-param filled in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_USERS,
            &subkey,
            &name,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    // SAFETY: `buf` is `size` bytes, what the probe asked for; `size` is updated in place.
    let rc = unsafe {
        RegGetValueW(
            HKEY_USERS,
            &subkey,
            &name,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}
