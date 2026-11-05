//! The password check of an adopted account: PAM, once, when the row is made.
//!
//! The stack is the `login` service's, which `punktfunk-seat@.service` runs its unit under
//! (`PAMName=login`), so an account that stack would refuse to log in is refused here:
//! `pam_authenticate` takes the password, `pam_acct_mgmt` a locked, expired or must-change
//! account. libpam loads when first asked and stays loaded, since its modules keep references
//! into it. Nothing is kept: the password lives in one wiped buffer for the length of the call.
#![allow(unsafe_code)]

use crate::backend::BackendError;
use crate::secret::Secret;
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;
use zeroize::Zeroizing;

const SERVICE: &CStr = c"login";

/// `PAM_MAX_RESP_SIZE`: the longest answer PAM takes to a prompt.
const MAX_PASSWORD: usize = 512;
/// Prompts one conversation answers; a stack asks for a password once or twice.
const MAX_PROMPTS: usize = 16;

// <security/_pam_types.h>
const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_PERM_DENIED: c_int = 6;
const PAM_AUTH_ERR: c_int = 7;
const PAM_CRED_INSUFFICIENT: c_int = 8;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_MAXTRIES: c_int = 11;
const PAM_NEW_AUTHTOK_REQD: c_int = 12;
const PAM_ACCT_EXPIRED: c_int = 13;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
/// Keeps the modules from printing to the daemon's journal.
const PAM_SILENT: c_int = 0x8000;

#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}

#[repr(C)]
struct Reply {
    text: *mut c_char,
    code: c_int,
}

type Converse =
    unsafe extern "C" fn(c_int, *mut *const Message, *mut *mut Reply, *mut c_void) -> c_int;

#[repr(C)]
struct Conversation {
    converse: Converse,
    data: *mut c_void,
}

type Start = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    *const Conversation,
    *mut *mut c_void,
) -> c_int;
type Step = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;

/// The libpam entry points. The functions are valid for as long as `_library` is loaded, which
/// is the life of the process.
struct Pam {
    _library: Library,
    start: Start,
    authenticate: Step,
    account: Step,
    end: Step,
}

fn pam() -> Result<&'static Pam, BackendError> {
    static PAM: OnceLock<Result<Pam, String>> = OnceLock::new();
    PAM.get_or_init(load)
        .as_ref()
        .map_err(|why| unavailable(why))
}

fn load() -> Result<Pam, String> {
    // SAFETY: libpam's initializers only set up its own tables, and the library is never
    // unloaded, so no function pointer taken below outlives it.
    let library = unsafe { Library::new("libpam.so.0") }.map_err(|e| e.to_string())?;
    // SAFETY: each symbol is a libpam function whose C signature the type above spells; the
    // pointers are copied out of their `Symbol`s and valid while `library` is, which `Pam` holds.
    unsafe {
        let get = |name: &[u8]| library.get::<Step>(name).map(|s| *s);
        let start = *library
            .get::<Start>(b"pam_start\0")
            .map_err(|e| e.to_string())?;
        let authenticate = get(b"pam_authenticate\0").map_err(|e| e.to_string())?;
        let account = get(b"pam_acct_mgmt\0").map_err(|e| e.to_string())?;
        let end = get(b"pam_end\0").map_err(|e| e.to_string())?;
        Ok(Pam {
            _library: library,
            start,
            authenticate,
            account,
            end,
        })
    }
}

/// Frees the first `filled` answers, wiping the passwords in them, then the array.
///
/// # Safety
/// `replies` came from `calloc` for at least `filled` entries, whose texts are each null or a
/// `strdup` copy.
unsafe fn drop_replies(replies: *mut Reply, filled: usize) {
    for i in 0..filled {
        // SAFETY: `i` is below `filled`, so the entry exists; a non-null text is a NUL-ended
        // heap copy this module made.
        unsafe {
            let text = (*replies.add(i)).text;
            if !text.is_null() {
                libc::memset(text.cast(), 0, libc::strlen(text));
                libc::free(text.cast());
            }
        }
    }
    // SAFETY: the array came from `calloc`.
    unsafe { libc::free(replies.cast()) };
}

/// Answers each password prompt with the password `data` points at and each notice with
/// nothing. Any other prompt, such as one for a user name, fails the conversation.
///
/// # Safety
/// PAM calls it with `count` message pointers at `messages`, a slot at `answers` and the `data`
/// the conversation was started with: the NUL-ended password of [`verify`], alive for the call.
unsafe extern "C" fn converse(
    count: c_int,
    messages: *mut *const Message,
    answers: *mut *mut Reply,
    data: *mut c_void,
) -> c_int {
    let len = usize::try_from(count).unwrap_or(0);
    if len == 0 || len > MAX_PROMPTS || messages.is_null() || answers.is_null() || data.is_null() {
        return PAM_CONV_ERR;
    }
    // SAFETY: a zeroed array of `len` replies; PAM frees it and the texts with `free()`.
    let replies = unsafe { libc::calloc(len, size_of::<Reply>()) }.cast::<Reply>();
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..len {
        // SAFETY: PAM passes `len` pointers to messages, each readable.
        let style = unsafe { (**messages.add(i)).style };
        match style {
            PAM_PROMPT_ECHO_OFF => {
                // SAFETY: `data` is NUL-ended, per the contract above.
                let copy = unsafe { libc::strdup(data.cast::<c_char>()) };
                if copy.is_null() {
                    // SAFETY: entries below `i` hold copies, the rest are zeroed.
                    unsafe { drop_replies(replies, i) };
                    return PAM_BUF_ERR;
                }
                // SAFETY: `i` is below `len`.
                unsafe { (*replies.add(i)).text = copy };
            }
            PAM_ERROR_MSG | PAM_TEXT_INFO => {}
            _ => {
                // SAFETY: entries below `i` hold copies, the rest are zeroed.
                unsafe { drop_replies(replies, i) };
                return PAM_CONV_ERR;
            }
        }
    }
    // SAFETY: `answers` is the slot PAM gave for the array.
    unsafe { *answers = replies };
    PAM_SUCCESS
}

/// Checks `password` for `account`, an existing user: the `login` stack must accept the
/// password and the account. The refusal's message is the console's sentence.
pub(super) fn verify(account: &str, password: &Secret) -> Result<(), BackendError> {
    let bytes = password.expose().as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_PASSWORD || bytes.contains(&0) {
        return Err(wrong_password(account));
    }
    let user = CString::new(account).map_err(|_| wrong_password(account))?;
    let pam = pam()?;
    // One allocation, so no copy of the password is left behind by a growing buffer.
    let mut secret = Zeroizing::new(Vec::with_capacity(bytes.len() + 1));
    secret.extend_from_slice(bytes);
    secret.push(0);
    let conversation = Conversation {
        converse,
        data: secret.as_mut_ptr().cast(),
    };
    let mut handle = std::ptr::null_mut();
    // SAFETY: `SERVICE` and `user` are NUL-ended and outlive the call; `conversation` and the
    // buffer `data` points at outlive `pam_end` below; `handle` is the slot for the new handle.
    let started =
        unsafe { (pam.start)(SERVICE.as_ptr(), user.as_ptr(), &conversation, &mut handle) };
    if started != PAM_SUCCESS {
        return Err(unavailable(&format!("pam_start answered {started}")));
    }
    // SAFETY: `handle` is the live handle `pam_start` returned, ended exactly once below.
    let mut status = unsafe { (pam.authenticate)(handle, PAM_SILENT) };
    let checking_account = status == PAM_SUCCESS;
    if checking_account {
        // SAFETY: as above.
        status = unsafe { (pam.account)(handle, PAM_SILENT) };
    }
    // SAFETY: as above; the handle is not used again.
    unsafe { (pam.end)(handle, status) };
    verdict(account, checking_account, status)
}

/// What PAM's `status` means for `account`, from `pam_authenticate` or, when `checking_account`,
/// from `pam_acct_mgmt`.
fn verdict(account: &str, checking_account: bool, status: c_int) -> Result<(), BackendError> {
    let refused = |code: &str, why: String| Err(BackendError::new(code, why));
    match (checking_account, status) {
        (_, PAM_SUCCESS) => Ok(()),
        (false, PAM_AUTH_ERR | PAM_USER_UNKNOWN | PAM_MAXTRIES | PAM_CRED_INSUFFICIENT) => {
            Err(wrong_password(account))
        }
        (true, PAM_NEW_AUTHTOK_REQD) => refused(
            "credential_must_change",
            format!("{account}'s password has expired. Choose a new one at the machine, then try again."),
        ),
        (true, PAM_ACCT_EXPIRED | PAM_PERM_DENIED | PAM_AUTH_ERR) => refused(
            "account_locked",
            format!("{account}'s account is locked or has expired."),
        ),
        (_, other) => Err(unavailable(&format!("PAM answered {other}"))),
    }
}

fn wrong_password(account: &str) -> BackendError {
    BackendError::new(
        "credential_wrong",
        format!("That isn't {account}'s password."),
    )
}

/// The check couldn't run. `cause` is for the journal.
fn unavailable(cause: &str) -> BackendError {
    tracing::warn!(cause, "password check did not run");
    BackendError::new(
        "pam_unavailable",
        "Couldn't check the password: this machine's login service didn't answer.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every answer PAM gives lands on one sentence and code, and only a clean pass opens the
    /// account.
    #[test]
    fn a_pam_status_reads_as_one_refusal() {
        let code = |checking, status| verdict("alice", checking, status).unwrap_err().code;
        assert!(verdict("alice", false, PAM_SUCCESS).is_ok());
        assert!(verdict("alice", true, PAM_SUCCESS).is_ok());
        for status in [
            PAM_AUTH_ERR,
            PAM_USER_UNKNOWN,
            PAM_MAXTRIES,
            PAM_CRED_INSUFFICIENT,
        ] {
            assert_eq!(code(false, status), "credential_wrong", "{status}");
        }
        assert_eq!(code(true, PAM_NEW_AUTHTOK_REQD), "credential_must_change");
        for status in [PAM_ACCT_EXPIRED, PAM_PERM_DENIED, PAM_AUTH_ERR] {
            assert_eq!(code(true, status), "account_locked", "{status}");
        }
        assert_eq!(code(false, PAM_NEW_AUTHTOK_REQD), "pam_unavailable");
        assert_eq!(code(false, 4), "pam_unavailable");
        let wrong = verdict("alice", false, PAM_AUTH_ERR).unwrap_err();
        assert_eq!(wrong.message, "That isn't alice's password.");
    }

    /// A password PAM can't take whole is a wrong one, before any PAM call.
    #[test]
    fn a_password_that_can_t_cross_is_wrong() {
        for bad in ["", "a\0b", &"x".repeat(MAX_PASSWORD + 1)] {
            let error = verify("alice", &Secret::new(bad.into())).unwrap_err();
            assert_eq!(error.code, "credential_wrong");
        }
    }

    fn run(program: &str, args: &[&str]) {
        let status = std::process::Command::new(program).args(args).status();
        assert!(status.unwrap().success(), "{program} {args:?}");
    }

    /// Against the box's own `login` stack: the right password passes, a wrong one is refused
    /// with the wrong-password code, and a locked, an expired and a must-change account are
    /// each refused. Makes and removes the account `pfpamcheck`, so it needs root.
    #[test]
    #[ignore = "needs root, libpam and the login service; makes the account pfpamcheck"]
    fn the_login_stack_decides() {
        const ACCOUNT: &str = "pfpamcheck";
        const PASSWORD: &str = "right-password-1";
        struct Gone;
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::process::Command::new("userdel")
                    .args(["-r", ACCOUNT])
                    .status();
            }
        }
        let _ = std::process::Command::new("userdel")
            .args(["-r", ACCOUNT])
            .status();
        run("useradd", &["-m", ACCOUNT]);
        let _gone = Gone;
        let set = |entry: String| {
            let mut child = std::process::Command::new("chpasswd")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            std::io::Write::write_all(child.stdin.as_mut().unwrap(), entry.as_bytes()).unwrap();
            assert!(child.wait().unwrap().success());
        };
        set(format!("{ACCOUNT}:{PASSWORD}\n"));
        let check = |password: &str| verify(ACCOUNT, &Secret::new(password.into()));

        assert_eq!(check(PASSWORD), Ok(()));
        let wrong = check("wrong-password-2").unwrap_err();
        assert_eq!(wrong.code, "credential_wrong");
        assert_eq!(wrong.message, "That isn't pfpamcheck's password.");

        run("chage", &["-E", "1", ACCOUNT]);
        assert_eq!(check(PASSWORD).unwrap_err().code, "account_locked");
        run("chage", &["-E", "-1", ACCOUNT]);
        run("chage", &["-d", "0", ACCOUNT]);
        assert_eq!(check(PASSWORD).unwrap_err().code, "credential_must_change");
        run("chage", &["-d", "-1", ACCOUNT]);
        assert_eq!(check(PASSWORD), Ok(()));

        run("usermod", &["-L", ACCOUNT]);
        assert!(check(PASSWORD).is_err(), "a locked account is refused");
    }
}
