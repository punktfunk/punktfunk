//! Ownership-safe provisioning for Windows local seat accounts.
//!
//! A made account is identified only by its exact NetUser comment marker,
//! which embeds the full seat ID. New accounts use a random persisted password,
//! `USER_PRIV_USER`, direct membership in the localized Remote Desktop Users
//! alias resolved from SID S-1-5-32-555 and in [`SEATS_GROUP`], and
//! `SeDenyInteractiveLogonRight`.
//! Existing accounts are accepted only after marker and password verification.
//! An adopted account is a person's own, identified by the SID its row stores. It gets the
//! same groups and right and the person's password, never a comment or a flag, and its
//! release takes back only what adopt gave.
//! Administrator membership and remote-interactive denial make a seat unusable;
//! deletion requires the same exact identity and an explicit caller action. A made account
//! goes with its profile, at the next service start if the profile won't go at once; an
//! adopted one is never deleted.

use crate::backend::BackendError;
use crate::doctor::AccountInspection;
use crate::model::{Seat, SeatId};
use crate::secret::Secret;
use crate::windows::credentials::{Credential, CredentialStore};
use crate::windows::util::{
    backend_error, computer_name, io_error, require_local_system, status_error, wide, WinResult,
};
use crate::windows::{steam, wts};
use rand::Rng as _;
use std::time::{Duration, Instant};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_ACCOUNT_LOCKED_OUT, ERROR_ALIAS_EXISTS, ERROR_FILE_NOT_FOUND,
    ERROR_LOGON_FAILURE, ERROR_LOGON_TYPE_NOT_GRANTED, ERROR_MEMBER_IN_ALIAS,
    ERROR_MEMBER_NOT_IN_ALIAS, ERROR_NO_SUCH_ALIAS, ERROR_NO_SUCH_MEMBER, ERROR_PASSWORD_EXPIRED,
    ERROR_PASSWORD_MUST_CHANGE, HANDLE, HLOCAL, NTSTATUS, STATUS_OBJECT_NAME_NOT_FOUND,
    WIN32_ERROR,
};
use windows::Win32::NetworkManagement::NetManagement::{
    NERR_GroupExists, NERR_GroupNotFound, NERR_Success, NERR_UserExists, NERR_UserInGroup,
    NERR_UserNotFound, NERR_UserNotInGroup, NetApiBufferFree, NetLocalGroupAdd,
    NetLocalGroupAddMembers, NetLocalGroupDelMembers, NetUserAdd, NetUserDel, NetUserGetInfo,
    NetUserGetLocalGroups, LG_INCLUDE_INDIRECT, LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_0,
    LOCALGROUP_MEMBERS_INFO_3, LOCALGROUP_USERS_INFO_0, MAX_PREFERRED_LENGTH,
    UF_DONT_EXPIRE_PASSWD, UF_NORMAL_ACCOUNT, UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_ACCOUNT_FLAGS,
    USER_INFO_1, USER_INFO_10, USER_PRIV_USER,
};
use windows::Win32::Security::Authentication::Identity::{
    LsaAddAccountRights, LsaClose, LsaEnumerateAccountRights, LsaFreeMemory, LsaNtStatusToWinError,
    LsaOpenPolicy, LsaRemoveAccountRights, LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSidToSidW};
use windows::Win32::Security::{
    LogonUserW, LookupAccountNameW, LookupAccountSidW, LOGON32_LOGON_NETWORK,
    LOGON32_PROVIDER_DEFAULT, PSID, SID_NAME_USE,
};
use windows::Win32::UI::Shell::DeleteProfileW;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;
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

impl AccountManager {
    pub(super) fn new(credentials: CredentialStore) -> Self {
        Self { credentials }
    }

    /// Creates the account and its password blob, or accepts the ones a crashed provision left.
    /// A step that fails removes what this call made, so no orphan account refuses the next
    /// create of the same name.
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
                return set_up_account(&seat.account, &credential);
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
        let result = add_marked_user(seat, &credential, &expected).and_then(|created| {
            set_up_account(&seat.account, &credential).inspect_err(|_| {
                if created {
                    let _ = delete_account(&seat.account);
                }
            })
        });
        if result.is_err() && !had_blob {
            let _ = self.credentials.delete(&seat.id);
        }
        result
    }

    /// Checks the account before a start, and puts it back in [`SEATS_GROUP`] if it is missing:
    /// an account provisioned before the group existed joins on its next start. An adopted
    /// account whose password no longer signs in answers `credential_changed`, the sentence
    /// that sends the person to the profile's card.
    pub(super) fn credential_for_start(&self, seat: &Seat) -> WinResult<Credential> {
        require_local_system()?;
        require_identity(seat)?;
        let credential = self.credentials.load(&seat.id)?;
        let adopted = seat.adopted_sid.is_some();
        verify_password(&seat.account, &credential, |error| {
            if adopted {
                changed_fault(&seat.account)
            } else {
                stored_fault(&seat.account, error)
            }
        })?;
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

    /// Removes a made account with its profile and blob, or releases an adopted one by its SID.
    /// A made account that is gone leaves only its blob to delete; one that isn't the row's is
    /// refused. The SID outlives the account, so a profile still loaded goes after it.
    pub(super) fn delete(&self, seat: &Seat) -> WinResult<()> {
        require_local_system()?;
        if let Some(sid) = &seat.adopted_sid {
            return self.release(seat, sid);
        }
        match account_comment(&seat.account)? {
            None => self.credentials.delete(&seat.id),
            Some(comment) if comment == marker(&seat.id) => {
                let kept = sid_string(&seat.account).filter(|sid| !delete_profile(sid));
                delete_account(&seat.account)?;
                if let Some(sid) = kept {
                    delete_profile_later(&sid);
                }
                self.credentials.delete(&seat.id)
            }
            Some(_) => Err(not_ours(seat)),
        }
    }

    /// Whether `seat` is an adopted row whose name now holds another account: the person deleted
    /// and made it again. Its sessions are a stranger's.
    pub(super) fn name_reassigned(&self, seat: &Seat) -> bool {
        seat.adopted_sid
            .as_deref()
            .is_some_and(|sid| sid_string(&seat.account).is_some_and(|found| found != sid))
    }

    /// Refuses an account that exists and isn't the row's by [`identity_matches`]. One that
    /// doesn't exist passes: there is nothing of ours to stop or delete.
    pub(super) fn ensure_owned_or_absent(&self, seat: &Seat) -> WinResult<()> {
        let owned = match &seat.adopted_sid {
            Some(sid) => sid_string(&seat.account).is_none_or(|found| found == *sid),
            None => {
                account_comment(&seat.account)?.is_none_or(|comment| comment == marker(&seat.id))
            }
        };
        if owned {
            Ok(())
        } else {
            Err(not_ours(seat))
        }
    }

    /// Makes `seat`'s account, a person's own standard account, ready to run as a seat with
    /// `password`: its blob, Remote Desktop Users, [`SEATS_GROUP`] and the console deny, each
    /// step idempotent. Sets no account flag and writes no comment. A refusal's message is the
    /// console's sentence; a failed step undoes what this call added. `true`: this call put the
    /// account in Remote Desktop Users.
    pub(super) fn adopt(&self, seat: &Seat, password: &Secret) -> WinResult<bool> {
        require_local_system()?;
        let account = seat.account.as_str();
        let Some(sid) = seat
            .adopted_sid
            .as_deref()
            .filter(|sid| sid_string(account).as_deref() == Some(*sid))
        else {
            return Err(backend_error(
                "account_not_owned",
                format!("No Windows account {account} on this PC."),
            )
            .said("account_missing_windows")
            .param("account", account));
        };
        if contains_name(
            &local_groups(account)?,
            &localized_alias(ADMINISTRATORS_SID)?,
        ) {
            return Err(backend_error(
                "account_is_administrator",
                format!(
                    "{account} is an administrator. A seat is a standard user: make {account} a \
                     standard user in Windows, or pick another account."
                ),
            )
            .said("account_is_administrator")
            .param("account", account));
        }
        if has_account_right(account, DENY_REMOTE)? {
            return Err(backend_error(
                "account_policy",
                format!("{account} isn't allowed to sign in over Remote Desktop on this PC."),
            )
            .said("account_policy")
            .param("account", account));
        }
        if wts::has_session(account)? {
            return Err(signed_in(account));
        }
        let credential = typed_credential(password)?;
        verify_password(account, &credential, |error| {
            typed_fault(account, error, false)
        })?;

        let had_blob = self.credentials.exists(&seat.id)?;
        let (mut rdp_added, mut seats_added) = (false, false);
        let result = (|| {
            self.credentials.store(&seat.id, &credential)?;
            rdp_added = add_to_rdp_users(account)?;
            seats_added = join_seats_group(account)?;
            add_account_right(account, DENY_CONSOLE)
        })();
        let Err(error) = result else {
            return Ok(rdp_added);
        };
        // The console deny is the last step, so a failure never added it.
        let undone = with_sid(sid, |sid| {
            if seats_added {
                warn_unless_done(account, remove_member(SEATS_GROUP, sid));
            }
            if rdp_added {
                let rdp = localized_alias(RDP_USERS_SID);
                warn_unless_done(account, rdp.and_then(|group| remove_member(&group, sid)));
            }
            Ok(())
        });
        warn_unless_done(account, undone);
        if !had_blob {
            warn_unless_done(account, self.credentials.delete(&seat.id));
        }
        Err(error)
    }

    /// Verifies `password`, an adopted `seat`'s new one, and stores it in place of the old. The
    /// console deny is in place, which a must-change answer's sentence works around.
    pub(super) fn reseal(&self, seat: &Seat, password: &Secret) -> WinResult<()> {
        require_local_system()?;
        require_identity(seat)?;
        let credential = typed_credential(password)?;
        verify_password(&seat.account, &credential, |error| {
            typed_fault(&seat.account, error, true)
        })?;
        self.credentials.store(&seat.id, &credential)
    }

    /// Gives an adopted `seat`'s account back by `sid`, the row's SID, so a renamed account gets
    /// it back too: the console deny off, out of [`SEATS_GROUP`], out of Remote Desktop Users
    /// when adopt put it there, the blob gone, the Steam launchers undone. Never deletes the
    /// account or its profile, and never acts by name. Every step runs and treats "already gone"
    /// as done; the first failure is the answer, so Remove can run again. When the name is gone
    /// or holds another account, the SID's steps only warn.
    fn release(&self, seat: &Seat, sid: &str) -> WinResult<()> {
        let present = sid_string(&seat.account).is_some_and(|found| found == sid);
        let mut results = with_sid(sid, |sid| {
            let mut results = vec![
                remove_account_right(sid, DENY_CONSOLE),
                remove_member(SEATS_GROUP, sid),
            ];
            if seat.rdp_granted {
                let rdp = localized_alias(RDP_USERS_SID);
                results.push(rdp.and_then(|group| remove_member(&group, sid)));
            }
            Ok(results)
        })
        .unwrap_or_else(|error| vec![Err(error)]);
        for error in results.iter().filter_map(|result| result.as_ref().err()) {
            tracing::warn!(account = %seat.account, %error, "adopted account not fully released");
        }
        if !present {
            results.clear();
        }
        results.push(self.credentials.delete(&seat.id));
        steam::unwire(sid);
        results.into_iter().collect()
    }

    pub(super) fn inspect(&self, seat: &Seat) -> WinResult<AccountInspection> {
        let identity_matches = identity_matches(seat)?;
        let credential_blob = self.credentials.exists(&seat.id)?;
        let memberships = local_groups(&seat.account).unwrap_or_default();
        let rdp = localized_alias(RDP_USERS_SID).unwrap_or_default();
        let admins = localized_alias(ADMINISTRATORS_SID).unwrap_or_default();
        Ok(AccountInspection {
            identity_matches,
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

/// `account_not_owned` for `seat`. An adopted row's message is the console's sentence.
fn not_ours(seat: &Seat) -> BackendError {
    let message = match seat.adopted_sid {
        None => format!(
            "local account '{}' is not marked for seat {}",
            seat.account, seat.id
        ),
        Some(_) => format!(
            "Couldn't find the Windows account {} this profile was set up with.",
            seat.account
        ),
    };
    backend_error("account_not_owned", message)
}

/// Whether `seat.account` exists and is the row's own: a made row's carries the seat's comment
/// marker, an adopted row's name resolves to the SID the row stores.
fn identity_matches(seat: &Seat) -> WinResult<bool> {
    Ok(match &seat.adopted_sid {
        Some(sid) => sid_string(&seat.account).as_deref() == Some(sid.as_str()),
        None => account_comment(&seat.account)?.as_deref() == Some(marker(&seat.id).as_str()),
    })
}

fn require_identity(seat: &Seat) -> WinResult<()> {
    if identity_matches(seat)? {
        Ok(())
    } else {
        Err(not_ours(seat))
    }
}

/// Creates the account. `true` when this call made it, `false` when it appeared meanwhile with
/// our marker.
fn add_marked_user(seat: &Seat, credential: &Credential, expected: &str) -> WinResult<bool> {
    match add_user(seat, credential) {
        Ok(()) => Ok(true),
        Err(error) if error.code == "account_exists_race" => {
            if account_comment(&seat.account)?.as_deref() == Some(expected) {
                Ok(false)
            } else {
                Err(backend_error(
                    "account_not_owned",
                    format!(
                        "local account '{}' appeared without our marker",
                        seat.account
                    ),
                ))
            }
        }
        Err(error) => Err(error),
    }
}

/// The password, groups and rights every seat account holds. Each step is idempotent.
fn set_up_account(account: &str, credential: &Credential) -> WinResult<()> {
    verify_password(account, credential, |error| stored_fault(account, error))?;
    add_to_rdp_users(account)?;
    join_seats_group(account)?;
    ensure_safe_membership(account)?;
    ensure_no_remote_deny(account)?;
    add_account_right(account, DENY_CONSOLE)
}

/// Deletes the local account; one that is already gone counts as deleted.
fn delete_account(account: &str) -> WinResult<()> {
    let account = wide(account, "account")?;
    // SAFETY: account is a live NUL-terminated local name; null server selects this machine.
    let status = unsafe { NetUserDel(PCWSTR::null(), PCWSTR(account.as_ptr())) };
    if status != NERR_Success && status != NERR_UserNotFound {
        return Err(status_error("account_delete", "NetUserDel failed", status));
    }
    Ok(())
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
    #[expect(
        clippy::cast_ptr_alignment,
        reason = "NetAPI aligns the buffer for the level's struct"
    )]
    // SAFETY: level 10 returned a USER_INFO_10 allocation that remains live until free below.
    let info = unsafe { &*buffer.cast::<USER_INFO_10>() };
    let comment = pwstr_to_string(info.usri10_comment);
    // SAFETY: `buffer` is the allocation returned by NetUserGetInfo and is freed once.
    unsafe {
        let _ = NetApiBufferFree(Some(buffer.cast()));
    }
    Ok(Some(comment?))
}

/// The operator's line for a password the ledger minted that no longer signs in.
fn stored_fault(account: &str, error: windows::core::Error) -> BackendError {
    io_error(
        "credential_verify",
        &format!("stored credential did not verify for managed account '{account}'"),
        error,
    )
}

/// The sentence for an adopted account whose stored password no longer signs in.
fn changed_fault(account: &str) -> BackendError {
    backend_error(
        "credential_changed",
        format!(
            "{account}'s Windows password changed or expired. Enter the new one on the profile's card."
        ),
    )
    .said("credential_changed")
    .param("account", account)
}

/// The sentence for an adopted account its person is signed in to. The seat signs the account in
/// itself, so neither adopt nor a seat start goes ahead beside the person's session.
pub(super) fn signed_in(account: &str) -> BackendError {
    backend_error(
        "account_signed_in",
        format!("{account} is signed in on this PC. Sign {account} out first."),
    )
    .said("account_signed_in")
    .param("account", account)
}

/// The sentence for a password just typed for `account` that Windows refused. `denied`: the
/// account already carries the console deny, so it can't sign in at the PC.
fn typed_fault(account: &str, error: windows::core::Error, denied: bool) -> BackendError {
    match WIN32_ERROR::from_error(&error) {
        Some(code) => logon_fault(account, code.0, denied),
        None => stored_fault(account, error),
    }
}

/// [`typed_fault`] by the Win32 `code` of the refusal. One it doesn't know keeps the operator's
/// line.
fn logon_fault(account: &str, code: u32, denied: bool) -> BackendError {
    let said = |error: BackendError, code: &str| error.said(code).param("account", account);
    match WIN32_ERROR(code) {
        ERROR_LOGON_FAILURE => said(
            backend_error(
                "credential_wrong",
                format!("That isn't {account}'s Windows password."),
            ),
            "credential_wrong_windows",
        ),
        ERROR_PASSWORD_MUST_CHANGE | ERROR_PASSWORD_EXPIRED if denied => said(
            backend_error(
                "credential_must_change",
                format!(
                    "Windows wants {account}'s password changed. Remove the profile, sign in to \
                     {account} at the PC and set the new password, then add the profile again."
                ),
            ),
            "credential_must_change_windows_made",
        ),
        ERROR_PASSWORD_MUST_CHANGE | ERROR_PASSWORD_EXPIRED => said(
            backend_error(
                "credential_must_change",
                format!(
                    "Windows wants {account}'s password changed. Sign in to {account} at the PC \
                     once and set it, then enter the new one here."
                ),
            ),
            "credential_must_change_windows",
        ),
        ERROR_ACCOUNT_LOCKED_OUT => said(
            backend_error(
                "account_locked",
                format!(
                    "Windows locked {account} out after too many wrong passwords. Wait ten minutes."
                ),
            ),
            "account_locked_windows",
        ),
        _ => status_error(
            "credential_verify",
            &format!("stored credential did not verify for managed account '{account}'"),
            code,
        ),
    }
}

/// `password`, typed for a person's account, as a credential, or the sentence for why it
/// can't be one.
fn typed_credential(password: &Secret) -> WinResult<Credential> {
    Credential::new(password.expose().to_owned()).map_err(|error| {
        let (said, sentence) = match password.expose() {
            "" => (
                "credential_invalid_no_password",
                "This account has no password. Set one in Windows first.",
            ),
            typed if typed.len() > 256 => {
                ("credential_invalid_too_long", "The password is too long.")
            }
            _ => (
                "credential_invalid_control_char",
                "The password can't hold a tab or a line break.",
            ),
        };
        backend_error(&error.code, sentence).said(said)
    })
}

/// Signs `account` in over the network with `credential` and closes the token: proof the
/// password is the account's. Each refusal counts toward the account's lockout; `fault` words it.
/// A policy that denies the account network sign-ins is [`network_logon_denied`] instead, which
/// neither counts as a wrong password nor retries.
fn verify_password(
    account: &str,
    credential: &Credential,
    fault: impl FnOnce(windows::core::Error) -> BackendError,
) -> WinResult<()> {
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
    .map_err(|error| match WIN32_ERROR::from_error(&error) {
        Some(ERROR_LOGON_TYPE_NOT_GRANTED) => network_logon_denied(account),
        _ => fault(error),
    })?;
    // SAFETY: LogonUserW returned this owned token and no later operation uses it.
    unsafe {
        let _ = CloseHandle(token);
    }
    Ok(())
}

/// The sentence for an account local policy keeps from network sign-ins, which the password
/// check needs and Remote Desktop doesn't.
fn network_logon_denied(account: &str) -> BackendError {
    backend_error(
        "account_policy",
        format!(
            "Couldn't check {account}'s password — this PC's security policy denies it sign-in \
             from the network. In Local Security Policy, remove the entry that covers it from \
             Deny access to this computer from the network, then try again."
        ),
    )
    .said("account_network_denied")
    .param("account", account)
}

/// Puts `account` in Remote Desktop Users. `true`: this call added it.
fn add_to_rdp_users(account: &str) -> WinResult<bool> {
    add_member(&localized_alias(RDP_USERS_SID)?, account)
}

/// Puts `account` in [`SEATS_GROUP`], creating the group the first time. Both steps are
/// idempotent. `true`: this call added it.
fn join_seats_group(account: &str) -> WinResult<bool> {
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

/// How long a profile gets to unload its hive after the logoff before its delete gives up.
const PROFILE_UNLOAD: Duration = Duration::from_secs(5);
/// The SIDs of made seat accounts deleted with their profile still on disk, one value each.
/// Only [`delete_profile_later`] adds one, once the account is gone.
const OWED_PROFILES: &str = r"SOFTWARE\Punktfunk\SeatProfiles";

/// Removes the profile `sid` names and the seat's Steam copy inside it, as `userdel -r` takes a
/// Linux seat's home. The delete retries while the hive unloads. `true` once no profile is left.
fn delete_profile(sid: &str) -> bool {
    let Ok(sid_w) = wide(sid, "account sid") else {
        return false;
    };
    let give_up = Instant::now() + PROFILE_UNLOAD;
    loop {
        // SAFETY: `sid_w` is a live NUL-terminated string; null path and computer name this machine.
        let deleted =
            unsafe { DeleteProfileW(PCWSTR(sid_w.as_ptr()), PCWSTR::null(), PCWSTR::null()) };
        match deleted {
            Ok(()) => return true,
            // An account that never logged on has no profile.
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return true,
            Err(error) if Instant::now() >= give_up => {
                tracing::warn!(%error, sid, "seat profile not deleted");
                return false;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// Tries the profile of a deleted made account once more, then notes its SID under
/// [`OWED_PROFILES`] for [`delete_owed_profiles`].
fn delete_profile_later(sid: &str) {
    if delete_profile(sid) {
        return;
    }
    let noted = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey_with_flags(OWED_PROFILES, KEY_SET_VALUE | KEY_WOW64_64KEY)
        .and_then(|(owed, _)| owed.set_value(sid, &1_u32));
    match noted {
        Ok(()) => tracing::warn!(sid, "seat profile left for the next service start"),
        Err(error) => tracing::warn!(%error, sid, "seat profile left behind"),
    }
}

/// Deletes the profiles [`delete_profile_later`] noted; each that goes leaves the note. For the
/// service's start, when no seat holds a hive.
pub(super) fn delete_owed_profiles() {
    let Ok(owed) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(OWED_PROFILES, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY)
    else {
        return;
    };
    let sids: Vec<String> = owed.enum_values().flatten().map(|(sid, _)| sid).collect();
    for sid in sids {
        if delete_profile(&sid) {
            tracing::info!(sid, "seat profile deleted");
            if let Err(error) = owed.delete_value(&sid) {
                tracing::warn!(%error, sid, "seat profile note not removed");
            }
        }
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
/// `ERROR_MEMBER_IN_ALIAS` for it, not `NERR_UserInGroup`. `true`: this call added it.
fn add_member(group: &str, account: &str) -> WinResult<bool> {
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
        Ok(status == NERR_Success)
    } else {
        Err(status_error(
            "account_group",
            &format!("NetLocalGroupAddMembers({group}) failed"),
            status,
        ))
    }
}

/// Takes the account `sid` names out of the local `group`. Not a member, no such account and no
/// such group are done.
fn remove_member(group: &str, sid: PSID) -> WinResult<()> {
    let group_w = wide(group, "local group")?;
    let member = LOCALGROUP_MEMBERS_INFO_0 { lgrmi0_sid: sid };
    // SAFETY: `group_w` and `member` live across this call, and the caller keeps `sid` live.
    let status = unsafe {
        NetLocalGroupDelMembers(
            PCWSTR::null(),
            PCWSTR(group_w.as_ptr()),
            0,
            std::ptr::from_ref(&member).cast(),
            1,
        )
    };
    let gone = [
        NERR_UserNotInGroup,
        NERR_GroupNotFound,
        ERROR_MEMBER_NOT_IN_ALIAS.0,
        ERROR_NO_SUCH_MEMBER.0,
        ERROR_NO_SUCH_ALIAS.0,
    ];
    if status == NERR_Success || gone.contains(&status) {
        Ok(())
    } else {
        Err(status_error(
            "account_group",
            &format!("NetLocalGroupDelMembers({group}) failed"),
            status,
        ))
    }
}

/// Logs a step of an undo that didn't complete; the undo goes on.
fn warn_unless_done(account: &str, result: WinResult<()>) {
    if let Err(error) = result {
        tracing::warn!(account, %error, "seat account step not undone");
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
            #[expect(
                clippy::cast_ptr_alignment,
                reason = "NetAPI aligns the buffer for the level's struct"
            )]
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

/// `work` on the binary SID `text` names, which lives for the call.
fn with_sid<T>(text: &str, work: impl FnOnce(PSID) -> WinResult<T>) -> WinResult<T> {
    let text = wide(text, "SID")?;
    let mut sid = PSID::default();
    // SAFETY: SID text is NUL-terminated; `sid` receives one LocalAlloc allocation.
    unsafe { ConvertStringSidToSidW(PCWSTR(text.as_ptr()), &mut sid) }
        .map_err(|error| io_error("account_sid", "ConvertStringSidToSidW failed", error))?;
    let result = work(sid);
    // SAFETY: ConvertStringSidToSidW returned this LocalAlloc allocation; free it once.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sid.0)));
    }
    result
}

/// The local name of the built-in group `sid_text` names, such as Remote Desktop Users.
fn localized_alias(sid_text: &str) -> WinResult<String> {
    with_sid(sid_text, |sid| {
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
    })
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

/// Takes `right` from the account `sid` names. An account LSA holds no rights for is done.
fn remove_account_right(sid: PSID, right: &str) -> WinResult<()> {
    let mut right_w = wide(right, "account right")?;
    let right = lsa_string(&mut right_w)?;
    let policy = open_policy()?;
    // SAFETY: policy and SID are live; the one-element right slice points to live UTF-16.
    let status = unsafe { LsaRemoveAccountRights(policy, sid, false, Some(&[right])) };
    close_policy(policy);
    if status.0 == STATUS_OBJECT_NAME_NOT_FOUND.0 {
        return Ok(());
    }
    ntstatus_result("LsaRemoveAccountRights failed", status)
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

    /// A refused typed password answers the console's sentence for its Win32 code, naming the
    /// account; any other code keeps the operator's line and the cause. Must-change sends the
    /// person to the PC at adopt, and through Remove once the console deny is in place.
    #[test]
    fn a_refused_password_answers_by_its_logon_code() {
        const AT_PC: &str = "Windows wants Bärbel's password changed. Sign in to Bärbel at the PC";
        const REMOVE: &str = "Windows wants Bärbel's password changed. Remove the profile, sign in";
        for (code, denied, want, sentence) in [
            (
                1326,
                false,
                "credential_wrong",
                "That isn't Bärbel's Windows password.",
            ),
            (
                1326,
                true,
                "credential_wrong",
                "That isn't Bärbel's Windows password.",
            ),
            (1907, false, "credential_must_change", AT_PC),
            (1330, false, "credential_must_change", AT_PC),
            (1907, true, "credential_must_change", REMOVE),
            (1330, true, "credential_must_change", REMOVE),
            (1909, true, "account_locked", "Windows locked Bärbel out"),
            (
                1331,
                false,
                "credential_verify",
                "stored credential did not verify",
            ),
        ] {
            let fault = logon_fault("Bärbel", code, denied);
            assert_eq!(fault.code, want, "{code} {denied}");
            assert!(
                fault.message.starts_with(sentence),
                "{code} {denied}: {}",
                fault.message
            );
        }
    }

    /// A policy refusal is no wrong password: the attempt limits and the seat's retry rule read
    /// `account_policy` as neither.
    #[test]
    fn a_denied_network_sign_in_is_a_policy_refusal() {
        let refused = network_logon_denied("Bärbel");
        assert_eq!(refused.code, "account_policy");
        assert!(
            refused
                .message
                .starts_with("Couldn't check Bärbel's password"),
            "{}",
            refused.message
        );
    }
}
