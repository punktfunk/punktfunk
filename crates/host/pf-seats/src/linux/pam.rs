//! The password check of an adopted account: PAM, once, when the row is made.
//!
//! The stack is the `login` service's, which `punktfunk-seat@.service` runs its unit under
//! (`PAMName=login`), so an account that stack would refuse to log in is refused here:
//! `pam_authenticate` takes the password, `pam_acct_mgmt` a locked, expired or must-change
//! account. The daemon never loads libpam: [`check`] runs its own binary as
//! `punktfunk-seats pam-check <account>` ([`run_check`]), hands it the password on stdin and
//! reads one code back, and kills it after [`CHECK_TIMEOUT`]. The child's environment is empty
//! and its argv holds the account alone. Nothing is kept: each side holds the password in one
//! wiped buffer for the length of the check.
#![allow(unsafe_code)]

use crate::backend::BackendError;
use crate::secret::Secret;
use libloading::Library;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const SERVICE: &CStr = c"login";

/// The verb of `punktfunk-seats` that runs one check.
pub const CHECK_VERB: &str = "pam-check";
/// A check still running then is killed and answers `pam_unavailable`.
pub(super) const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The codes the child prints. A refusal's code is [`BackendError::code`]'s.
const PASSED: &str = "ok";
const WRONG: &str = "credential_wrong";
const MUST_CHANGE: &str = "credential_must_change";
const LOCKED: &str = "account_locked";
const UNAVAILABLE: &str = "pam_unavailable";

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

/// libpam, loaded on first use: in the `pam-check` child only.
fn pam() -> Result<&'static Pam, &'static str> {
    static PAM: OnceLock<Result<Pam, String>> = OnceLock::new();
    PAM.get_or_init(load).as_ref().map_err(String::as_str)
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

/// Checks `password` for `account`, an existing user, in a `pam-check` child of the binary this
/// process runs, which is the one running even after an upgrade replaced its file. The refusal's
/// message is the console's sentence.
#[cfg(not(test))]
pub(super) fn check(account: &str, password: &Secret) -> Result<(), BackendError> {
    check_in(
        Path::new("/proc/self/exe"),
        account,
        password,
        CHECK_TIMEOUT,
    )
}

/// A test answers from [`super::fake`], and no process starts, unless it named a real program.
#[cfg(test)]
pub(super) fn check(account: &str, password: &Secret) -> Result<(), BackendError> {
    if let Some(program) = tests::REAL.with(|real| real.borrow().clone()) {
        return check_in(&program, account, password, CHECK_TIMEOUT);
    }
    let output = super::fake::answer("punktfunk-seats", &[CHECK_VERB, account])
        .expect("a test that checks a password installs the fake");
    refusal(account, String::from_utf8_lossy(&output.stdout).trim())
}

/// [`check`] with `program`'s `pam-check`, killed with what it started once `timeout` passes. A
/// password PAM can't take whole is wrong before any process starts. The password fits a pipe's
/// buffer, so writing it never waits on the child.
fn check_in(
    program: &Path,
    account: &str,
    password: &Secret,
    timeout: Duration,
) -> Result<(), BackendError> {
    use std::os::unix::process::CommandExt as _;
    let bytes = password.expose().as_bytes();
    if !takes(bytes) {
        return refusal(account, WRONG);
    }
    let mut child = std::process::Command::new(program)
        .arg0("punktfunk-seats")
        .args([CHECK_VERB, account])
        .env_clear()
        .current_dir("/")
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| unanswered(account, &format!("start {CHECK_VERB}: {e}")))?;
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(bytes)
    {
        tracing::warn!(error = %e, "password not handed to the check");
    }
    let deadline = Instant::now() + timeout;
    let exited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            waited => {
                let group = rustix::process::Pid::from_child(&child);
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
                let _ = child.kill();
                let _ = child.wait();
                let cause = match waited {
                    Err(error) => format!("wait for the check: {error}"),
                    _ => format!("the check ran past {} s", timeout.as_secs()),
                };
                return Err(unanswered(account, &cause));
            }
        }
    };
    let code = child.stdout.take().map(read_code).unwrap_or_default();
    if !exited {
        return Err(unanswered(account, "the check exited with a failure"));
    }
    refusal(account, &code)
}

/// What the exited child printed, without waiting on a helper of its that kept the pipe open.
fn read_code(stdout: std::process::ChildStdout) -> String {
    let _ = rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK);
    let mut code = Vec::new();
    let _ = stdout.take(64).read_to_end(&mut code);
    String::from_utf8_lossy(&code).trim().to_owned()
}

/// `punktfunk-seats pam-check <account>`: reads the password from stdin, at most
/// [`MAX_PASSWORD`] bytes and one more to see a longer one, checks it and prints the code. stdin
/// is read through its own descriptor, past std's buffer, so the password is in one wiped buffer.
pub fn run_check(account: &str) {
    crate::logging::to_stderr();
    let mut password = Zeroizing::new([0_u8; MAX_PASSWORD + 1]);
    let code = match read_password(&mut password[..]) {
        Ok(length) => judge(account, &password[..length]),
        Err(error) => unavailable(&format!("read the password: {error}")),
    };
    println!("{code}");
}

fn read_password(buffer: &mut [u8]) -> std::io::Result<usize> {
    use std::os::fd::AsFd as _;
    let mut stdin = std::fs::File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let mut length = 0;
    while length < buffer.len() {
        match stdin.read(&mut buffer[length..]) {
            Ok(0) => break,
            Ok(read) => length += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(length)
}

/// Whether PAM can take `password` whole: 1 to [`MAX_PASSWORD`] bytes, no NUL.
fn takes(password: &[u8]) -> bool {
    !password.is_empty() && password.len() <= MAX_PASSWORD && !password.contains(&0)
}

/// Runs the `login` stack for `account` with `password` in this process and answers the code.
fn judge(account: &str, password: &[u8]) -> &'static str {
    if !takes(password) {
        return WRONG;
    }
    let Ok(user) = CString::new(account) else {
        return WRONG;
    };
    let pam = match pam() {
        Ok(pam) => pam,
        Err(cause) => return unavailable(cause),
    };
    // One allocation, so no copy of the password is left behind by a growing buffer.
    let mut secret = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    secret.extend_from_slice(password);
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
        return unavailable(&format!("pam_start answered {started}"));
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
    verdict(checking_account, status)
}

/// The code for PAM's `status`, from `pam_authenticate` or, when `checking_account`, from
/// `pam_acct_mgmt`.
fn verdict(checking_account: bool, status: c_int) -> &'static str {
    match (checking_account, status) {
        (_, PAM_SUCCESS) => PASSED,
        (false, PAM_AUTH_ERR | PAM_USER_UNKNOWN | PAM_MAXTRIES | PAM_CRED_INSUFFICIENT) => WRONG,
        (true, PAM_NEW_AUTHTOK_REQD) => MUST_CHANGE,
        (true, PAM_ACCT_EXPIRED | PAM_PERM_DENIED | PAM_AUTH_ERR) => LOCKED,
        (_, other) => unavailable(&format!("PAM answered {other}")),
    }
}

/// What `code`, a check's answer, means for `account`, with the console's sentence. A code this
/// doesn't know is a check that didn't run.
fn refusal(account: &str, code: &str) -> Result<(), BackendError> {
    let sentence = match code {
        PASSED => return Ok(()),
        WRONG => format!("That isn't {account}'s password."),
        MUST_CHANGE => format!(
            "{account}'s password has expired. Choose a new one at the machine, then try again."
        ),
        LOCKED => format!("{account}'s account is locked or has expired."),
        _ => return Err(unanswered(account, &format!("the check answered {code:?}"))),
    };
    Err(BackendError::new(code, sentence))
}

/// The check couldn't run; `cause` is for the journal.
fn unavailable(cause: &str) -> &'static str {
    tracing::warn!(cause, "password check did not run");
    UNAVAILABLE
}

/// [`unavailable`] as the console's sentence.
fn unanswered(account: &str, cause: &str) -> BackendError {
    tracing::warn!(account, cause, "password check did not run");
    BackendError::new(
        UNAVAILABLE,
        "Couldn't check the password: this machine's login service didn't answer.",
    )
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    thread_local! {
        /// The `pam-check` program a root test runs for real on this thread, in place of the fake.
        pub(in crate::linux) static REAL: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Every answer PAM gives lands on one code and sentence, and only a clean pass opens the
    /// account. A code the child can't have meant is a check that didn't run.
    #[test]
    fn a_pam_status_reads_as_one_refusal() {
        let code = |checking, status| {
            let code = verdict(checking, status);
            refusal("alice", code)
                .map_err(|e| e.code)
                .err()
                .unwrap_or_default()
        };
        assert_eq!(verdict(false, PAM_SUCCESS), PASSED);
        assert_eq!(verdict(true, PAM_SUCCESS), PASSED);
        assert_eq!(refusal("alice", PASSED), Ok(()));
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
        let wrong = refusal("alice", verdict(false, PAM_AUTH_ERR)).unwrap_err();
        assert_eq!(wrong.message, "That isn't alice's password.");
        for garbage in ["", "OK", "ok\nok", "credential_wrong_", "pam_unavailable"] {
            assert_eq!(
                refusal("alice", garbage).unwrap_err().code,
                "pam_unavailable"
            );
        }
    }

    /// A password PAM can't take whole is a wrong one, before any PAM call and before any
    /// process starts: the program below doesn't exist.
    #[test]
    fn a_password_that_can_t_cross_is_wrong() {
        let nowhere = Path::new("/nonexistent/punktfunk-seats");
        let check = |password: &str| {
            let checked = check_in(
                nowhere,
                "alice",
                &Secret::new(password.into()),
                CHECK_TIMEOUT,
            );
            checked.unwrap_err().code
        };
        for bad in ["", "a\0b", &"x".repeat(MAX_PASSWORD + 1)] {
            assert_eq!(judge("alice", bad.as_bytes()), WRONG);
            assert_eq!(check(bad), "credential_wrong");
        }
        assert_eq!(check("pw"), "pam_unavailable", "a check that can't start");
    }

    /// `script` as an executable file in a fresh directory, which the caller keeps.
    fn child(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pam-check");
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, path)
    }

    /// The child gets the verb and the account as its arguments, the password whole on stdin and
    /// none of this process's environment, and its code is the answer.
    #[test]
    fn the_password_rides_stdin_alone() {
        let (dir, program) =
            child("env > \"$0.env\"\necho \"$@\" > \"$0.args\"\ncat > \"$0.in\"\necho ok\n");
        let password = "pa ss\twörd \"$HOME\"";
        let checked = check_in(
            &program,
            "alice",
            &Secret::new(password.into()),
            CHECK_TIMEOUT,
        );
        assert_eq!(checked, Ok(()));
        let read = |ext: &str| std::fs::read(dir.path().join(format!("pam-check.{ext}"))).unwrap();
        assert_eq!(read("in"), password.as_bytes(), "untrimmed, unquoted");
        assert_eq!(read("args"), b"pam-check alice\n");
        let env = String::from_utf8(read("env")).unwrap();
        assert!(!env.contains("CARGO") && !env.contains("HOME="), "{env}");
    }

    /// A check that doesn't answer is killed and reads as one that didn't run.
    #[test]
    fn a_check_that_hangs_is_killed() {
        let (_dir, program) = child("cat > /dev/null\nsleep 600\n");
        let started = Instant::now();
        let secret = Secret::new("pw".into());
        let error = check_in(&program, "alice", &secret, Duration::from_millis(300)).unwrap_err();
        assert_eq!(error.code, "pam_unavailable");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// The `punktfunk-seats` binary beside this test's: `target/<profile>/punktfunk-seats`.
    pub(in crate::linux) fn seats_binary() -> std::path::PathBuf {
        let exe = std::env::current_exe().unwrap();
        let binary = exe
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .join("punktfunk-seats");
        let shown = binary.display();
        assert!(
            binary.is_file(),
            "{shown} is missing: cargo build -p punktfunk-seats first"
        );
        binary
    }

    pub(in crate::linux) fn run(program: &str, args: &[&str]) {
        let status = std::process::Command::new(program).args(args).status();
        assert!(status.unwrap().success(), "{program} {args:?}");
    }

    /// Against the box's own `login` stack, in this process and through the `pam-check` child of
    /// the built `punktfunk-seats`, which must agree: the right password passes, a wrong one is
    /// refused with the wrong-password code, and a locked, an expired and a must-change account
    /// are each refused. Makes and removes the account `pfpamcheck`, so it needs root.
    #[test]
    #[ignore = "needs root, libpam, the login service and a built punktfunk-seats; makes the account pfpamcheck"]
    fn the_login_stack_decides() {
        const ACCOUNT: &str = "pfpamcheck";
        const PASSWORD: &str = "right-password-1";
        let binary = seats_binary();
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
        let typed = Secret::new(PASSWORD.into());
        assert_eq!(check_in(&binary, ACCOUNT, &typed, CHECK_TIMEOUT), Ok(()));
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        assert!(
            !maps.contains("libpam"),
            "the child's check loads no libpam here"
        );
        let check = |password: &str| {
            let direct = refusal(ACCOUNT, judge(ACCOUNT, password.as_bytes()));
            let typed = Secret::new(password.into());
            let child = check_in(&binary, ACCOUNT, &typed, CHECK_TIMEOUT);
            assert_eq!(child, direct, "the child decides as this process does");
            direct
        };

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
