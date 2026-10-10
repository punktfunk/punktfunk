//! Ownership-safe provisioning for Windows local seat accounts.
//!
//! A managed account is identified only by its exact NetUser comment marker,
//! which embeds the full seat ID. New accounts use a random persisted password,
//! `USER_PRIV_USER`, direct membership in the localized Remote Desktop Users
//! alias resolved from SID S-1-5-32-555 and in [`SEATS_GROUP`], and
//! `SeDenyInteractiveLogonRight`.
//! Existing accounts are accepted only after marker and password verification.
//! Administrator membership and remote-interactive denial make a seat unusable;
//! deletion requires the same exact marker and an explicit caller action, and takes the
//! account's profile with it.

use crate::model::{Seat, SeatId};
use crate::windows::credentials::{Credential, CredentialStore};
use crate::windows::util::{
    backend_error, computer_name, io_error, require_local_system, status_error, wide, WinResult,
};
use rand::Rng as _;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ALIAS_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_MEMBER_IN_ALIAS,
    HANDLE, HLOCAL, NTSTATUS,
};
use windows::Win32::NetworkManagement::NetManagement::{
    NERR_GroupExists, NERR_Success, NERR_UserExists, NERR_UserInGroup, NERR_UserNotFound,
    NetApiBufferFree, NetLocalGroupAdd, NetLocalGroupAddMembers, NetUserAdd, NetUserDel,
    NetUserGetInfo, NetUserGetLocalGroups, LG_INCLUDE_INDIRECT, LOCALGROUP_INFO_1,
    LOCALGROUP_MEMBERS_INFO_3, LOCALGROUP_USERS_INFO_0, MAX_PREFERRED_LENGTH,
    UF_DONT_EXPIRE_PASSWD, UF_NORMAL_ACCOUNT, UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_ACCOUNT_FLAGS,
    USER_INFO_1, USER_INFO_10, USER_PRIV_USER,
};
use windows::Win32::Security::Authentication::Identity::{
    LsaAddAccountRights, LsaClose, LsaEnumerateAccountRights, LsaFreeMemory, LsaNtStatusToWinError,
    LsaOpenPolicy, LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, POLICY_CREATE_ACCOUNT,
    POLICY_LOOKUP_NAMES,
};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::{
    LogonUserW, LookupAccountNameW, LookupAccountSidW, LOGON32_LOGON_NETWORK,
    LOGON32_PROVIDER_DEFAULT, PSID, SID_NAME_USE,
};
use windows::Win32::UI::Shell::DeleteProfileW;
use zeroize::Zeroizing;

/// The local group every seat account joins, so an ACL can refuse seat accounts by one name.
/// A deny on it is the whole use: membership grants nothing.
pub const SEATS_GROUP: &str = "punktfunk-seats";
const RDP_USERS_SID: &str = "S-1-5-32-555";
const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const DENY_CONSOLE: &str = "SeDenyInteractiveLogonRight";
const DENY_REMOTE: &str = "SeDenyRemoteInteractiveLogonRight";
const PASSWORD_ALPHABET: &[u8; 64] =
    b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789!#$%+-_";

#[derive(Clone)]
pub(super) struct AccountManager {
    credentials: CredentialStore,
}

pub(super) struct AccountInspection {
    pub marker_matches: bool,
    pub credential_blob: bool,
    pub rdp_member: bool,
    pub administrator: bool,
    pub deny_console: bool,
    pub deny_remote: bool,
    pub seats_member: bool,
}

impl AccountManager {
    pub(super) fn new(credentials: CredentialStore) -> Self {
        Self { credentials }
    }

    pub(super) fn provision(&self, seat: &Seat) -> WinResult<()> {
        require_local_system()?;
        let expected = marker(&seat.id);
        match account_comment(&seat.account)? {
            Some(comment) if comment != expected => {
                return Err(backend_error(
                    "account_not_owned",
                    format!(
                        "local account '{}' exists without the exact marker for seat {}",
                        seat.account, seat.id
                    ),
                ));
            }
            Some(_) => {
                let credential = self.credentials.load(&seat.id)?;
                verify_password(&seat.account, &credential)?;
                add_to_rdp_users(&seat.account)?;
                join_seats_group(&seat.account)?;
                ensure_safe_membership(&seat.account)?;
                ensure_no_remote_deny(&seat.account)?;
                add_account_right(&seat.account, DENY_CONSOLE)?;
                return Ok(());
            }
            None => {}
        }

        let had_blob = self.credentials.exists(&seat.id)?;
        let credential = if had_blob {
            self.credentials.load(&seat.id)?
        } else {
            let credential = random_credential()?;
            self.credentials.store(&seat.id, &credential)?;
            credential
        };
        match add_user(seat, &credential) {
            Ok(()) => {}
            Err(error) if error.code == "account_exists_race" => {
                let comment = account_comment(&seat.account)?;
                if comment.as_deref() != Some(expected.as_str()) {
                    return Err(backend_error(
                        "account_not_owned",
                        format!(
                            "local account '{}' appeared without our marker",
                            seat.account
                        ),
                    ));
                }
            }
            Err(error) => {
                if !had_blob {
                    let _ = self.credentials.delete(&seat.id);
                }
                return Err(error);
            }
        }

        verify_password(&seat.account, &credential)?;
        add_to_rdp_users(&seat.account)?;
        join_seats_group(&seat.account)?;
        ensure_safe_membership(&seat.account)?;
        ensure_no_remote_deny(&seat.account)?;
        add_account_right(&seat.account, DENY_CONSOLE)
    }

    /// Checks the account before a start, and puts it back in [`SEATS_GROUP`] if it is missing:
    /// an account provisioned before the group existed joins on its next start.
    pub(super) fn credential_for_start(&self, seat: &Seat) -> WinResult<Credential> {
        require_local_system()?;
        require_marker(seat)?;
        let credential = self.credentials.load(&seat.id)?;
        verify_password(&seat.account, &credential)?;
        join_seats_group(&seat.account)?;
        ensure_safe_membership(&seat.account)?;
        ensure_no_remote_deny(&seat.account)?;
        if !has_account_right(&seat.account, DENY_CONSOLE)? {
            return Err(backend_error(
                "account_policy",
                format!("managed account '{}' lacks {DENY_CONSOLE}", seat.account),
            ));
        }
        Ok(credential)
    }

    pub(super) fn delete(&self, seat: &Seat) -> WinResult<()> {
        require_local_system()?;
        match account_comment(&seat.account)? {
            None => self.credentials.delete(&seat.id),
            Some(comment) if comment == marker(&seat.id) => {
                if let Some(sid) = sid_string(&seat.account) {
                    delete_profile(&sid);
                }
                let account = wide(&seat.account, "account")?;
                // SAFETY: account is a live NUL-terminated local name; null server selects this machine.
                let status = unsafe { NetUserDel(PCWSTR::null(), PCWSTR(account.as_ptr())) };
                if status != NERR_Success && status != NERR_UserNotFound {
                    return Err(status_error("account_delete", "NetUserDel failed", status));
                }
                self.credentials.delete(&seat.id)
            }
            Some(_) => Err(backend_error(
                "account_not_owned",
                format!(
                    "refusing to delete local account '{}' because its marker does not match seat {}",
                    seat.account, seat.id
                ),
            )),
        }
    }

    pub(super) fn inspect(&self, seat: &Seat) -> WinResult<AccountInspection> {
        let marker_matches =
            account_comment(&seat.account)?.as_deref() == Some(marker(&seat.id).as_str());
        let credential_blob = self.credentials.exists(&seat.id)?;
        let memberships = local_groups(&seat.account).unwrap_or_default();
        let rdp = localized_alias(RDP_USERS_SID).unwrap_or_default();
        let admins = localized_alias(ADMINISTRATORS_SID).unwrap_or_default();
        Ok(AccountInspection {
            marker_matches,
            credential_blob,
            rdp_member: contains_name(&memberships, &rdp),
            administrator: contains_name(&memberships, &admins),
            deny_console: has_account_right(&seat.account, DENY_CONSOLE).unwrap_or(false),
            deny_remote: has_account_right(&seat.account, DENY_REMOTE).unwrap_or(false),
            seats_member: contains_name(&memberships, SEATS_GROUP),
        })
    }
}

pub(super) fn marker(id: &SeatId) -> String {
    format!("Punktfunk managed seat {id}")
}

fn require_marker(seat: &Seat) -> WinResult<()> {
    match account_comment(&seat.account)? {
        Some(comment) if comment == marker(&seat.id) => Ok(()),
        _ => Err(backend_error(
            "account_not_owned",
            format!(
                "local account '{}' is not marked for seat {}",
                seat.account, seat.id
            ),
        )),
    }
}

fn random_credential() -> WinResult<Credential> {
    let mut random = Zeroizing::new([0_u8; 48]);
    rand::rng().fill_bytes(&mut *random);
    let mut password: Vec<u8> = random
        .iter()
        .map(|byte| PASSWORD_ALPHABET[usize::from(byte & 63)])
        .collect();
    password[..4].copy_from_slice(b"Aa2!");
    let password = String::from_utf8(password)
        .map_err(|_| backend_error("credential_invalid", "generated password is not ASCII"))?;
    Credential::new(password)
}

fn add_user(seat: &Seat, credential: &Credential) -> WinResult<()> {
    let mut account = wide(&seat.account, "account")?;
    let mut password = Zeroizing::new(wide(credential.expose(), "password")?);
    let mut comment = wide(marker(&seat.id), "account marker")?;
    let info = USER_INFO_1 {
        usri1_name: PWSTR(account.as_mut_ptr()),
        usri1_password: PWSTR(password.as_mut_ptr()),
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: PWSTR::null(),
        usri1_comment: PWSTR(comment.as_mut_ptr()),
        usri1_flags: USER_ACCOUNT_FLAGS(UF_NORMAL_ACCOUNT)
            | UF_SCRIPT
            | UF_DONT_EXPIRE_PASSWD
            | UF_PASSWD_CANT_CHANGE,
        usri1_script_path: PWSTR::null(),
    };
    let mut parameter_error = 0_u32;
    // SAFETY: `info` and every pointed-to UTF-16 vector remain live for this synchronous call.
    let status = unsafe {
        NetUserAdd(
            PCWSTR::null(),
            1,
            std::ptr::from_ref(&info).cast(),
            Some(&mut parameter_error),
        )
    };
    if status == NERR_Success {
        Ok(())
    } else if status == NERR_UserExists {
        Err(backend_error(
            "account_exists_race",
            "the local account appeared during provisioning",
        ))
    } else {
        Err(status_error(
            "account_create",
            &format!("NetUserAdd failed at parameter {parameter_error}"),
            status,
        ))
    }
}

fn account_comment(account: &str) -> WinResult<Option<String>> {
    let account = wide(account, "account")?;
    let mut buffer: *mut u8 = std::ptr::null_mut();
    // SAFETY: `account` is NUL-terminated and `buffer` receives NetAPI-owned memory.
    let status =
        unsafe { NetUserGetInfo(PCWSTR::null(), PCWSTR(account.as_ptr()), 10, &mut buffer) };
    if status == NERR_UserNotFound {
        return Ok(None);
    }
    if status != NERR_Success {
        return Err(status_error(
            "account_query",
            "NetUserGetInfo(level 10) failed",
            status,
        ));
    }
    if buffer.is_null() {
        return Err(backend_error(
            "account_query",
            "NetUserGetInfo returned a null buffer",
        ));
    }
    // SAFETY: level 10 returned a USER_INFO_10 allocation that remains live until free below.
    let info = unsafe { &*buffer.cast::<USER_INFO_10>() };
    let comment = pwstr_to_string(info.usri10_comment);
    // SAFETY: `buffer` is the allocation returned by NetUserGetInfo and is freed once.
    unsafe {
        let _ = NetApiBufferFree(Some(buffer.cast()));
    }
    Ok(Some(comment?))
}

fn verify_password(account: &str, credential: &Credential) -> WinResult<()> {
    let mut account_w = wide(account, "account")?;
    let mut domain_w = wide(computer_name()?, "computer name")?;
    let mut password_w = Zeroizing::new(wide(credential.expose(), "password")?);
    let mut token = HANDLE::default();
    // SAFETY: all strings are live and NUL-terminated; `token` receives one owned handle on success.
    unsafe {
        LogonUserW(
            PCWSTR(account_w.as_mut_ptr()),
            PCWSTR(domain_w.as_mut_ptr()),
            PCWSTR(password_w.as_mut_ptr()),
            LOGON32_LOGON_NETWORK,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    }
    .map_err(|error| {
        io_error(
            "credential_verify",
            &format!("stored credential did not verify for managed account '{account}'"),
            error,
        )
    })?;
    // SAFETY: LogonUserW returned this owned token and no later operation uses it.
    unsafe {
        let _ = CloseHandle(token);
    }
    Ok(())
}

fn add_to_rdp_users(account: &str) -> WinResult<()> {
    add_member(&localized_alias(RDP_USERS_SID)?, account)
}

/// Puts `account` in [`SEATS_GROUP`], creating the group the first time. Both steps are
/// idempotent.
fn join_seats_group(account: &str) -> WinResult<()> {
    let mut name = wide(SEATS_GROUP, "seats group")?;
    let mut comment = wide("Punktfunk seat accounts", "seats group comment")?;
    let info = LOCALGROUP_INFO_1 {
        lgrpi1_name: PWSTR(name.as_mut_ptr()),
        lgrpi1_comment: PWSTR(comment.as_mut_ptr()),
    };
    // SAFETY: `info` and both UTF-16 buffers remain live for this synchronous call.
    let status =
        unsafe { NetLocalGroupAdd(PCWSTR::null(), 1, std::ptr::from_ref(&info).cast(), None) };
    if status != NERR_Success && status != NERR_GroupExists && status != ERROR_ALIAS_EXISTS.0 {
        return Err(status_error(
            "account_group",
            "NetLocalGroupAdd(punktfunk-seats) failed",
            status,
        ));
    }
    add_member(SEATS_GROUP, account)
}

/// Removes the profile of the account `sid` names, and the seat's Steam copy inside it, as
/// `userdel -r` takes a Linux seat's home. A failure is logged: the account goes regardless.
fn delete_profile(sid: &str) {
    let Ok(sid_w) = wide(sid, "account sid") else {
        return;
    };
    // SAFETY: `sid_w` is a live NUL-terminated string; null path and computer name this machine.
    let deleted = unsafe { DeleteProfileW(PCWSTR(sid_w.as_ptr()), PCWSTR::null(), PCWSTR::null()) };
    match deleted {
        Ok(()) => {}
        // An account that never logged on has no profile.
        Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {}
        Err(error) => tracing::warn!(%error, "seat profile not deleted"),
    }
}

/// [`SEATS_GROUP`]'s SID as text, or `None` on a box that never provisioned a seat.
pub fn seats_group_sid() -> Option<String> {
    sid_string(SEATS_GROUP)
}

/// A local account's or group's SID as text, or `None` when it doesn't exist.
pub(super) fn sid_string(account: &str) -> Option<String> {
    let sid = account_sid(account).ok()?;
    let mut text = PWSTR::null();
    // SAFETY: `sid` holds the SID LookupAccountNameW wrote; `text` receives one LocalAlloc string.
    unsafe { ConvertSidToStringSidW(sid.psid(), &mut text) }.ok()?;
    let out = pwstr_to_string(text).ok();
    // SAFETY: ConvertSidToStringSidW allocated `text` with LocalAlloc; it is freed once.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(text.0.cast())));
    }
    out
}

/// Adds `account` to the local `group`. Already a member is success: a local group answers
/// `ERROR_MEMBER_IN_ALIAS` for it, not `NERR_UserInGroup`.
fn add_member(group: &str, account: &str) -> WinResult<()> {
    let qualified = format!("{}\\{account}", computer_name()?);
    let mut group_w = wide(group, "local group")?;
    let mut member_w = wide(qualified, "local account")?;
    let member = LOCALGROUP_MEMBERS_INFO_3 {
        lgrmi3_domainandname: PWSTR(member_w.as_mut_ptr()),
    };
    // SAFETY: group/member UTF-16 and the level-3 structure remain live for this call.
    let status = unsafe {
        NetLocalGroupAddMembers(
            PCWSTR::null(),
            PCWSTR(group_w.as_mut_ptr()),
            3,
            std::ptr::from_ref(&member).cast(),
            1,
        )
    };
    if status == NERR_Success || status == NERR_UserInGroup || status == ERROR_MEMBER_IN_ALIAS.0 {
        Ok(())
    } else {
        Err(status_error(
            "account_group",
            &format!("NetLocalGroupAddMembers({group}) failed"),
            status,
        ))
    }
}

fn ensure_safe_membership(account: &str) -> WinResult<()> {
    let memberships = local_groups(account)?;
    let admins = localized_alias(ADMINISTRATORS_SID)?;
    if contains_name(&memberships, &admins) {
        return Err(backend_error(
            "account_is_administrator",
            format!("managed account '{account}' belongs to Administrators"),
        ));
    }
    let rdp = localized_alias(RDP_USERS_SID)?;
    if !contains_name(&memberships, &rdp) {
        return Err(backend_error(
            "account_group",
            format!("managed account '{account}' is not in Remote Desktop Users"),
        ));
    }
    Ok(())
}

fn local_groups(account: &str) -> WinResult<Vec<String>> {
    let account = wide(account, "account")?;
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut entries = 0_u32;
    let mut total = 0_u32;
    // SAFETY: output pointers are live; NetAPI owns any returned allocation until the free below.
    let status = unsafe {
        NetUserGetLocalGroups(
            PCWSTR::null(),
            PCWSTR(account.as_ptr()),
            0,
            LG_INCLUDE_INDIRECT,
            &mut buffer,
            MAX_PREFERRED_LENGTH,
            &mut entries,
            &mut total,
        )
    };
    if status != NERR_Success {
        if !buffer.is_null() {
            // SAFETY: NetUserGetLocalGroups owns any partial allocation returned on failure.
            unsafe {
                let _ = NetApiBufferFree(Some(buffer.cast()));
            }
        }
        return Err(status_error(
            "account_group",
            "NetUserGetLocalGroups failed",
            status,
        ));
    }
    let result = (|| {
        let mut groups = Vec::with_capacity(entries as usize);
        if entries != 0 {
            if buffer.is_null() {
                return Err(backend_error(
                    "account_group",
                    "NetUserGetLocalGroups returned a null buffer",
                ));
            }
            // SAFETY: level 0 returned `entries` consecutive structures in the NetAPI allocation.
            let values = unsafe {
                std::slice::from_raw_parts(
                    buffer.cast::<LOCALGROUP_USERS_INFO_0>(),
                    entries as usize,
                )
            };
            for value in values {
                groups.push(pwstr_to_string(value.lgrui0_name)?);
            }
        }
        Ok(groups)
    })();
    if !buffer.is_null() {
        // SAFETY: this is the allocation returned by NetUserGetLocalGroups and is freed once.
        unsafe {
            let _ = NetApiBufferFree(Some(buffer.cast()));
        }
    }
    result
}

fn localized_alias(sid_text: &str) -> WinResult<String> {
    let sid_text = wide(sid_text, "well-known SID")?;
    let mut sid = PSID::default();
    // SAFETY: SID text is NUL-terminated; `sid` receives one LocalAlloc allocation.
    unsafe { ConvertStringSidToSidW(PCWSTR(sid_text.as_ptr()), &mut sid) }
        .map_err(|error| io_error("account_sid", "ConvertStringSidToSidW failed", error))?;
    let result = (|| {
        let mut name = vec![0_u16; 256];
        let mut domain = vec![0_u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain_len = domain.len() as u32;
        let mut use_type = SID_NAME_USE::default();
        // SAFETY: SID is live; both writable name buffers and lengths remain live for the call.
        unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut use_type,
            )
        }
        .map_err(|error| io_error("account_sid", "LookupAccountSidW failed", error))?;
        name.truncate(name_len as usize);
        String::from_utf16(&name)
            .map_err(|error| io_error("account_sid", "localized alias is not UTF-16", error))
    })();
    // SAFETY: ConvertStringSidToSidW returned this LocalAlloc allocation; free it once.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid.0)));
    }
    result
}

fn ensure_no_remote_deny(account: &str) -> WinResult<()> {
    if has_account_right(account, DENY_REMOTE)? {
        Err(backend_error(
            "account_policy",
            format!("managed account '{account}' has {DENY_REMOTE}"),
        ))
    } else {
        Ok(())
    }
}

fn add_account_right(account: &str, right: &str) -> WinResult<()> {
    let sid = account_sid(account)?;
    let mut right_w = wide(right, "account right")?;
    let right = lsa_string(&mut right_w)?;
    let policy = open_policy()?;
    // SAFETY: policy and SID are live; the one-element right slice points to live UTF-16.
    let status = unsafe { LsaAddAccountRights(policy, sid.psid(), &[right]) };
    close_policy(policy);
    ntstatus_result("LsaAddAccountRights failed", status)
}

fn has_account_right(account: &str, expected: &str) -> WinResult<bool> {
    let sid = account_sid(account)?;
    let policy = open_policy()?;
    let mut rights: *mut LSA_UNICODE_STRING = std::ptr::null_mut();
    let mut count = 0_u32;
    // SAFETY: policy/SID are live and both output pointers remain valid for the call.
    let status = unsafe { LsaEnumerateAccountRights(policy, sid.psid(), &mut rights, &mut count) };
    close_policy(policy);
    if status.0 == windows::Win32::Foundation::STATUS_OBJECT_NAME_NOT_FOUND.0 {
        return Ok(false);
    }
    ntstatus_result("LsaEnumerateAccountRights failed", status)?;
    if rights.is_null() && count != 0 {
        return Err(backend_error(
            "account_policy",
            "LsaEnumerateAccountRights returned a null buffer",
        ));
    }
    let mut found = false;
    if count != 0 {
        // SAFETY: LSA returned `count` strings in one allocation, live until LsaFreeMemory below.
        let values = unsafe { std::slice::from_raw_parts(rights, count as usize) };
        for value in values {
            let units = usize::from(value.Length) / 2;
            // SAFETY: each LSA string buffer is readable for `Length` bytes in this allocation.
            let text = unsafe { std::slice::from_raw_parts(value.Buffer.0, units) };
            if String::from_utf16_lossy(text).eq_ignore_ascii_case(expected) {
                found = true;
                break;
            }
        }
    }
    if !rights.is_null() {
        // SAFETY: `rights` is the allocation returned by LsaEnumerateAccountRights and is freed once.
        unsafe {
            let _ = LsaFreeMemory(Some(rights.cast()));
        }
    }
    Ok(found)
}

fn open_policy() -> WinResult<LSA_HANDLE> {
    let attributes = LSA_OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<LSA_OBJECT_ATTRIBUTES>() as u32,
        ..Default::default()
    };
    let mut policy = LSA_HANDLE::default();
    // SAFETY: attributes and output handle are live; null system selects this machine.
    let status = unsafe {
        LsaOpenPolicy(
            None,
            &attributes,
            (POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT) as u32,
            &mut policy,
        )
    };
    ntstatus_result("LsaOpenPolicy failed", status)?;
    Ok(policy)
}

fn close_policy(policy: LSA_HANDLE) {
    // SAFETY: `policy` is the live handle returned by LsaOpenPolicy and is closed once here.
    unsafe {
        let _ = LsaClose(policy);
    }
}

fn ntstatus_result(context: &str, status: NTSTATUS) -> WinResult<()> {
    if status.is_ok() {
        return Ok(());
    }
    // SAFETY: conversion takes the status by value and returns a Win32 error code.
    let error = unsafe { LsaNtStatusToWinError(status) };
    Err(status_error("account_policy", context, error))
}

struct AccountSid {
    words: [usize; 16],
}

impl AccountSid {
    fn psid(&self) -> PSID {
        PSID(self.words.as_ptr().cast_mut().cast())
    }

    fn psid_mut(&mut self) -> PSID {
        PSID(self.words.as_mut_ptr().cast())
    }
}

fn account_sid(account: &str) -> WinResult<AccountSid> {
    let qualified = format!("{}\\{account}", computer_name()?);
    let qualified = wide(qualified, "local account")?;
    let mut sid = AccountSid { words: [0; 16] };
    let mut sid_len = std::mem::size_of_val(&sid.words) as u32;
    let mut domain = vec![0_u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut use_type = SID_NAME_USE::default();
    // SAFETY: account text, SID storage, domain buffer, and all size outputs remain live.
    unsafe {
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(qualified.as_ptr()),
            Some(sid.psid_mut()),
            &mut sid_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut use_type,
        )
    }
    .map_err(|error| io_error("account_sid", "LookupAccountNameW failed", error))?;
    Ok(sid)
}

fn lsa_string(value: &mut [u16]) -> WinResult<LSA_UNICODE_STRING> {
    let units = value.len().saturating_sub(1);
    let length = u16::try_from(units * 2)
        .map_err(|_| backend_error("account_policy", "account right name is too long"))?;
    let maximum_length = u16::try_from(value.len() * 2)
        .map_err(|_| backend_error("account_policy", "account right name is too long"))?;
    Ok(LSA_UNICODE_STRING {
        Length: length,
        MaximumLength: maximum_length,
        Buffer: PWSTR(value.as_mut_ptr()),
    })
}

fn contains_name(values: &[String], expected: &str) -> bool {
    values
        .iter()
        .any(|value| value.eq_ignore_ascii_case(expected))
}

fn pwstr_to_string(value: PWSTR) -> WinResult<String> {
    if value.is_null() {
        return Ok(String::new());
    }
    let mut length = 0_usize;
    // SAFETY: NetAPI and LookupAccountSidW return NUL-terminated UTF-16 strings.
    while unsafe { *value.0.add(length) } != 0 {
        length += 1;
        if length > 32 * 1024 {
            return Err(backend_error(
                "account_query",
                "Windows account string exceeds its safety cap",
            ));
        }
    }
    // SAFETY: the scan above found the terminator, so these `length` units are readable.
    let slice = unsafe { std::slice::from_raw_parts(value.0, length) };
    String::from_utf16(slice).map_err(|error| {
        io_error(
            "account_query",
            "Windows account string is not UTF-16",
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_marker_binds_the_full_exact_id() {
        let id = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            marker(&id),
            "Punktfunk managed seat 0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn membership_comparison_matches_windows_case_rules_without_prefixes() {
        let groups = vec!["Remote Desktop Users".into(), "Users".into()];
        assert!(contains_name(&groups, "REMOTE DESKTOP USERS"));
        assert!(!contains_name(&groups, "Desktop Users"));
    }
}
