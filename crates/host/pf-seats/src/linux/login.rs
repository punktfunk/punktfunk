//! `punktfunk-seats login <account>`: a person's session opened with their password, as a
//! display manager opens one, so what their own login unlocks with it unlocks here too.
//!
//! It is the main process of `punktfunk-seat@<account>.service` for a cold start the person
//! proved with their password (the drop-in of `session::render_login`), and runs as root. It
//! takes the password once from the supervisor ([`super::handover`]), runs one transaction on
//! the `punktfunk-seat` PAM service ([`pam::Login`]: authenticate, account, credentials,
//! session), then forks a child that drops to the person and execs `seat-session`. The root
//! parent forwards the stop signals, closes the session when the child ends and exits with its
//! status. Before a session exists it exits with a code of [`outcome`]'s, which tells the
//! supervisor to start the row without the helper or to refuse the credential.

#![allow(unsafe_code)]

use super::pam;
use super::session::Ended;
#[cfg(target_os = "linux")]
use std::path::Path;

/// The verb of `punktfunk-seats` that runs the helper.
pub const VERB: &str = "login";
/// The PAM service the packages install, in `/usr/lib/pam.d` or `/etc/pam.d`.
pub(super) const SERVICE: &str = "punktfunk-seat";

/// Exit codes before a session exists. After one, the helper exits with the session's code and
/// never with one of these.
const NO_SESSION: u8 = 90;
const WRONG: u8 = 91;
const LOCKED: u8 = 92;
const MUST_CHANGE: u8 = 93;
const NO_SERVICE: u8 = 94;
const NO_PASSWORD: u8 = 95;
/// The auth phase's limit. Past it the default action of `SIGALRM` ends the helper.
const AUTH_SECONDS: u32 = 30;
const SIGALRM: i32 = 14;

/// What a start through the helper that ended with `ended` leaves the supervisor to do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// No session was opened, for a reason that isn't the credential: start the row the plain
    /// way. The cause is the operator's.
    Fallback(String),
    /// The stack refused the credential: refuse the start with this code.
    Refused(&'static str),
}

/// [`Outcome`] of a helper that ended with `ended`; `None` for an end after the session opened,
/// which is a plain failure of the seat.
pub(super) fn outcome(ended: Option<Ended>) -> Option<Outcome> {
    let fallback = |cause: String| Some(Outcome::Fallback(cause));
    match ended? {
        Ended::Exited(code) => match u8::try_from(code).ok()? {
            WRONG => Some(Outcome::Refused(pam::WRONG)),
            LOCKED => Some(Outcome::Refused(pam::LOCKED)),
            MUST_CHANGE => Some(Outcome::Refused(pam::MUST_CHANGE)),
            NO_SERVICE => fallback(format!("the {SERVICE} PAM service isn't installed")),
            NO_PASSWORD => fallback("the login helper got no password".into()),
            NO_SESSION => fallback("PAM opened no session; the unit's journal says why".into()),
            code @ 200..=254 => fallback(format!("systemd didn't start the login helper ({code})")),
            _ => None,
        },
        Ended::Killed(SIGALRM) => fallback(format!("PAM took longer than {AUTH_SECONDS} s")),
        Ended::Killed(_) => None,
    }
}

/// The helper's exit code for its session's end: the session's own code, or 128 plus the
/// signal that ended it, never one [`outcome`] reads as "no session".
fn session_code(code: Option<i32>, signal: Option<i32>) -> u8 {
    let reserved = |code: u8| (NO_SESSION..=99).contains(&code) || (200..=254).contains(&code);
    let code = match (code, signal) {
        (Some(code), _) => u8::try_from(code).unwrap_or(1),
        (None, Some(signal)) => u8::try_from(128 + signal).unwrap_or(1),
        (None, None) => 1,
    };
    if reserved(code) {
        1
    } else {
        code
    }
}

/// `punktfunk-seats login <account>`, as the unit runs it: the password from
/// [`super::handover::SOCKET_PATH`], then `seat-session` beside this binary.
#[cfg(target_os = "linux")]
pub fn run(account: &str) -> u8 {
    crate::logging::to_stderr();
    let program = std::env::current_exe().map(|exe| exe.with_file_name("seat-session"));
    let Ok(program) = program else {
        tracing::error!("seat-session not found beside the login helper");
        return NO_SESSION;
    };
    let socket = Path::new(super::handover::SOCKET_PATH);
    run_in(account, SERVICE, socket, &program)
}

/// [`run`] on the PAM service `service`, asking `socket`, starting `program`.
#[cfg(target_os = "linux")]
fn run_in(account: &str, service: &str, socket: &Path, program: &Path) -> u8 {
    use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
    let core = getrlimit(Resource::Core);
    // The soft limit is the one a dump obeys; the hard one stays, so the session gets its own
    // back without a capability.
    let none = Rlimit {
        current: Some(0),
        maximum: core.maximum,
    };
    let hardened =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
            .and_then(|()| setrlimit(Resource::Core, none));
    if let Err(error) = hardened {
        tracing::error!(%error, "core dumps not turned off");
        return NO_SESSION;
    }
    let person = match Person::of(account) {
        Ok(person) => person,
        Err(cause) => {
            tracing::error!(account, %cause, "login helper refused the account");
            return NO_SESSION;
        }
    };
    let installed = ["/etc/pam.d", "/usr/lib/pam.d"]
        .iter()
        .any(|dir| Path::new(dir).join(service).is_file());
    let Ok(service) = std::ffi::CString::new(service) else {
        return NO_SERVICE;
    };
    if !installed {
        tracing::error!(service = ?service, "login service not installed");
        return NO_SERVICE;
    }
    if let Err(error) = person.own_runtime_dir() {
        tracing::error!(%error, "seat runtime directory not handed to its person");
        return NO_SESSION;
    }
    let password = match super::handover::fetch(socket, account) {
        Ok(password) => password,
        Err(cause) => {
            tracing::error!(account, %cause, "login helper got no password");
            return NO_PASSWORD;
        }
    };
    // SAFETY: alarm only arms this process's timer.
    unsafe { libc::alarm(AUTH_SECONDS) };
    let authenticated = pam::Login::authenticate(&service, account, &password, &person.env);
    // SAFETY: as above; 0 disarms it.
    unsafe { libc::alarm(0) };
    drop(password);
    let mut login = match authenticated {
        Ok(login) => login,
        Err(pam::Refusal::Credential(code)) => {
            tracing::warn!(account, code, "login refused the credential");
            return match code {
                pam::LOCKED => LOCKED,
                pam::MUST_CHANGE => MUST_CHANGE,
                _ => WRONG,
            };
        }
        Err(pam::Refusal::Unavailable(cause)) => {
            tracing::error!(account, %cause, "PAM gave no verdict on the password");
            return NO_SESSION;
        }
    };
    // From here a stop reaches the parent as a signal it forwards, never as its own end.
    let signals = block_signals();
    if let Err(cause) = login.open() {
        tracing::error!(account, %cause, "PAM opened no session");
        return NO_SESSION;
    }
    // The session is open: no code below is one `outcome` reads as "no session".
    let core = (getrlimit(Resource::Core) == none).then_some(core);
    let mut child = match person.spawn(program, login.env(), core) {
        Ok(child) => child,
        Err(error) => {
            tracing::error!(account, %error, "seat-session did not start in the session");
            return 1;
        }
    };
    let status = wait_forwarding(&mut child, &signals);
    drop(login);
    use std::os::unix::process::ExitStatusExt as _;
    session_code(status.code(), status.signal())
}

/// The person the helper logs in, as the system knows them now.
#[cfg(target_os = "linux")]
struct Person {
    uid: rustix::process::Uid,
    gid: rustix::process::Gid,
    groups: Vec<rustix::process::Gid>,
    runtime_dir: std::path::PathBuf,
    /// The environment PAM starts from: this process's, with the person's `USER`, `LOGNAME`,
    /// `HOME` and `SHELL`, as systemd's `User=` sets them.
    env: Vec<std::ffi::CString>,
}

#[cfg(target_os = "linux")]
impl Person {
    /// `account`, a person's own account, with its groups.
    fn of(account: &str) -> Result<Self, String> {
        use std::os::unix::ffi::OsStrExt as _;
        super::accounts::check_name(account).map_err(|e| e.message)?;
        let found = super::accounts::lookup(account)
            .map_err(|e| e.message)?
            .ok_or("no such user")?;
        if !super::accounts::is_ordinary(&found) {
            return Err("not a person's account".into());
        }
        let shell = super::accounts::shell(account)
            .map_err(|e| e.message)?
            .unwrap_or_else(|| "/bin/sh".into());
        let ids = super::run("id_failed", "id", &["-G", "--", account]).map_err(|e| e.message)?;
        let groups = ids
            .split_whitespace()
            .map(|gid| gid.parse().map(rustix::process::Gid::from_raw))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read the groups: {e}"))?;
        let ours = [
            ("USER", account.to_owned()),
            ("LOGNAME", account.to_owned()),
            ("HOME", found.home.to_string_lossy().into_owned()),
            ("SHELL", shell),
        ];
        let mut env: Vec<std::ffi::CString> = std::env::vars_os()
            .filter(|(key, _)| {
                !ours
                    .iter()
                    .any(|(name, _)| key.as_bytes() == name.as_bytes())
            })
            .filter_map(|(key, value)| {
                let pair = [key.as_bytes(), b"=", value.as_bytes()].concat();
                std::ffi::CString::new(pair).ok()
            })
            .collect();
        for (key, value) in ours {
            env.push(std::ffi::CString::new(format!("{key}={value}")).map_err(|e| e.to_string())?);
        }
        Ok(Self {
            uid: rustix::process::Uid::from_raw(found.uid),
            gid: rustix::process::Gid::from_raw(found.gid),
            groups,
            runtime_dir: super::session::runtime_dir(account),
            env,
        })
    }

    /// The unit's runtime directory, which systemd made root's, handed to the person:
    /// `seat-session` records its session and readiness there.
    fn own_runtime_dir(&self) -> std::io::Result<()> {
        use rustix::fs::{chownat, AtFlags, CWD};
        chownat(
            CWD,
            &self.runtime_dir,
            Some(self.uid),
            Some(self.gid),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(Into::into)
    }

    /// `program` in a child that takes this process's descriptors 0 to 2 and no other, the
    /// session's environment `env`, no blocked signal, the core limit `core` when there is one to
    /// give back, then the person's groups, gid and uid, in that order, and no way back to root.
    fn spawn(
        &self,
        program: &Path,
        env: Vec<Vec<u8>>,
        core: Option<rustix::process::Rlimit>,
    ) -> std::io::Result<std::process::Child> {
        use std::os::unix::ffi::OsStrExt as _;
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new(program);
        command.env_clear().current_dir("/");
        for pair in &env {
            if let Some(at) = pair.iter().position(|&b| b == b'=') {
                let (key, value) = (&pair[..at], &pair[at + 1..]);
                command.env(
                    std::ffi::OsStr::from_bytes(key),
                    std::ffi::OsStr::from_bytes(value),
                );
            }
        }
        let (uid, gid, groups) = (self.uid, self.gid, self.groups.clone());
        let unblocked = signal_set(&[]);
        let become_person = move || {
            // SAFETY: sigprocmask reads an initialized set and changes the mask of the forked
            // child's only thread.
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &unblocked, std::ptr::null_mut()) };
            if let Some(core) = core {
                let _ = rustix::process::setrlimit(rustix::process::Resource::Core, core);
            }
            close_on_exec_from(3);
            rustix::thread::set_thread_groups(&groups)?;
            rustix::thread::set_thread_res_gid(gid, gid, gid)?;
            rustix::thread::set_thread_res_uid(uid, uid, uid)?;
            dropped(uid, gid)
        };
        // SAFETY: the closure runs in the forked child before exec and makes raw system calls
        // only: no allocation, no lock, nothing a thread of the parent could hold.
        unsafe { command.pre_exec(become_person) };
        command.spawn()
    }
}

/// Every descriptor from `first` up is closed when the child execs. In the forked child: raw
/// system calls only.
#[cfg(target_os = "linux")]
fn close_on_exec_from(first: u32) {
    // SAFETY: close_range only sets the close-on-exec flag on this process's own descriptors.
    let marked = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    } == 0;
    if marked {
        return;
    }
    // Before Linux 5.11, one descriptor at a time, up to the limit.
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile)
        .current
        .unwrap_or(1 << 20)
        .min(1 << 20);
    for fd in u64::from(first)..limit {
        // SAFETY: as above; a descriptor that isn't open answers EBADF.
        unsafe { libc::fcntl(fd as libc::c_int, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

/// Whether the drop held: the ids are the person's and root can't be had back. In the forked
/// child: raw system calls only, and an error that allocates nothing.
#[cfg(target_os = "linux")]
fn dropped(uid: rustix::process::Uid, gid: rustix::process::Gid) -> std::io::Result<()> {
    use rustix::process::{getegid, geteuid, getgid, getuid, Uid};
    let ids = getuid() == uid && geteuid() == uid && getgid() == gid && getegid() == gid;
    let regained = rustix::thread::set_thread_res_uid(Uid::ROOT, Uid::ROOT, Uid::ROOT).is_ok();
    let capable = rustix::thread::capabilities(None).map_or(true, |caps| {
        !caps.effective.is_empty() || !caps.permitted.is_empty()
    });
    if ids && !regained && !capable {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(libc::EPERM))
    }
}

/// The set of `signals`.
#[cfg(target_os = "linux")]
fn signal_set(signals: &[libc::c_int]) -> libc::sigset_t {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set before sigaddset changes it or it is read.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for &signal in signals {
            libc::sigaddset(set.as_mut_ptr(), signal);
        }
        set.assume_init()
    }
}

/// The signals the parent takes from here on, blocked so they wait for [`wait_forwarding`].
/// A child inherits the mask: [`Person::spawn`] clears it.
#[cfg(target_os = "linux")]
fn block_signals() -> libc::sigset_t {
    use libc::{SIGALRM, SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    let set = signal_set(&[SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGALRM, SIGCHLD]);
    // SAFETY: pthread_sigmask reads an initialized set and changes this thread's mask alone; the
    // helper has no other thread.
    unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) };
    set
}

/// Waits for `child` to end, forwarding each stop signal in `signals` to it.
#[cfg(target_os = "linux")]
fn wait_forwarding(
    child: &mut std::process::Child,
    signals: &libc::sigset_t,
) -> std::process::ExitStatus {
    use rustix::process::{kill_process, Pid, Signal};
    let pid = Pid::from_child(child);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status;
        }
        let mut signal = 0;
        // SAFETY: `signals` is an initialized set, blocked in this thread, and `signal` an int.
        unsafe { libc::sigwait(signals, &mut signal) };
        let forward = match signal {
            libc::SIGTERM => Signal::TERM,
            libc::SIGINT => Signal::INT,
            libc::SIGHUP => Signal::HUP,
            libc::SIGQUIT => Signal::QUIT,
            _ => continue,
        };
        let _ = kill_process(pid, forward);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential refusal refuses; a helper that ended before a session for any other reason
    /// falls back, systemd's own failures to start it included; anything else is the seat's.
    #[test]
    fn only_an_end_before_a_session_falls_back() {
        let refused = |code| outcome(Some(Ended::Exited(code)));
        assert_eq!(refused(91), Some(Outcome::Refused("credential_wrong")));
        assert_eq!(refused(92), Some(Outcome::Refused("account_locked")));
        assert_eq!(
            refused(93),
            Some(Outcome::Refused("credential_must_change"))
        );
        for code in [90, 94, 95, 200, 203, 229, 254] {
            assert!(
                matches!(refused(code), Some(Outcome::Fallback(_))),
                "{code}"
            );
        }
        assert!(matches!(
            outcome(Some(Ended::Killed(SIGALRM))),
            Some(Outcome::Fallback(_))
        ));
        for ended in [
            Ended::Exited(0),
            Ended::Exited(1),
            Ended::Exited(96),
            Ended::Exited(137),
            Ended::Exited(255),
            Ended::Exited(-1),
            Ended::Killed(9),
            Ended::Killed(15),
        ] {
            assert_eq!(outcome(Some(ended)), None, "{ended:?}");
        }
        assert_eq!(outcome(None), None);
    }

    /// Once a session was opened the helper's code never reads as one before it.
    #[test]
    fn a_session_s_end_never_reads_as_no_session() {
        assert_eq!(session_code(Some(0), None), 0);
        assert_eq!(session_code(Some(1), None), 1);
        assert_eq!(session_code(None, Some(9)), 137);
        assert_eq!(session_code(None, Some(15)), 143);
        assert_eq!(session_code(None, None), 1);
        for code in (0..=255).chain([-1, 256]) {
            let mapped = session_code(Some(code), None);
            assert_eq!(outcome(Some(Ended::Exited(mapped.into()))), None, "{code}");
        }
        for signal in 1..=64 {
            let mapped = session_code(None, Some(signal));
            assert_eq!(
                outcome(Some(Ended::Exited(mapped.into()))),
                None,
                "{signal}"
            );
        }
    }

    /// The built helper as `punktfunk-seat@.service` runs it, as root, through this package's PAM
    /// file over the box's `login` stack, with this test as the supervisor. The session child is
    /// the person with their groups, no capability, descriptors 0 to 2 alone, `/` as its
    /// directory and the unit's environment plus the person's names, and no copy of the password
    /// in its or the helper's argv and environment, or in the helper's log; the helper keeps no
    /// core limit and forwards a stop. A wrong password, a locked, an expired and a must-change
    /// account, a missing PAM file and no password at all each end it before a session, with
    /// their own code. Makes and removes the account `pfwallet`, the group `pfwalletg`,
    /// `/etc/pam.d/punktfunk-seat`, the login socket and the row's runtime directory.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs root, libpam, the login service and a built punktfunk-seats; makes the account pfwallet"]
    fn the_helper_opens_one_session_as_the_person() {
        use super::super::handover::{self, Handover};
        use super::super::pam::tests::{run, seats_binary};
        use crate::secret::Secret;
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        use std::time::{Duration, Instant};
        const ACCOUNT: &str = "pfwallet";
        const GROUP: &str = "pfwalletg";
        const PASSWORD: &str = "wallet pass-7 Ü";
        let pam_file = Path::new("/etc/pam.d").join(SERVICE);
        let socket = Path::new(handover::SOCKET_PATH);
        let runtime = super::super::session::runtime_dir(ACCOUNT);
        for taken in [
            &pam_file,
            &Path::new("/usr/lib/pam.d").join(SERVICE),
            socket,
        ] {
            assert!(!taken.exists(), "{} exists; refusing", taken.display());
        }
        struct Gone(Vec<std::path::PathBuf>);
        impl Drop for Gone {
            fn drop(&mut self) {
                for path in &self.0 {
                    let _ = std::fs::remove_file(path);
                    let _ = std::fs::remove_dir_all(path);
                }
                let _ = std::process::Command::new("userdel")
                    .args(["-r", ACCOUNT])
                    .status();
                let _ = std::process::Command::new("groupdel").arg(GROUP).status();
            }
        }
        let _gone = Gone(vec![pam_file.clone(), socket.into(), runtime.clone()]);
        run("useradd", &["-m", "-s", "/bin/bash", ACCOUNT]);
        run("groupadd", &[GROUP]);
        run("usermod", &["-aG", GROUP, ACCOUNT]);
        let mut chpasswd = std::process::Command::new("chpasswd")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let entry = format!("{ACCOUNT}:{PASSWORD}\n");
        std::io::Write::write_all(chpasswd.stdin.as_mut().unwrap(), entry.as_bytes()).unwrap();
        assert!(chpasswd.wait().unwrap().success());
        let packaged = include_str!("../../../../../packaging/linux/punktfunk-seat.pam");
        std::fs::write(&pam_file, packaged).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        let id = |flag: &str, name: &str| -> u32 {
            let out = std::process::Command::new("id")
                .args([flag, name])
                .output()
                .unwrap();
            String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        let (uid, gid) = (id("-u", ACCOUNT), id("-g", ACCOUNT));
        let extra = String::from_utf8(
            std::process::Command::new("getent")
                .args(["group", GROUP])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let extra: u32 = extra.split(':').nth(2).unwrap().parse().unwrap();

        // The helper as the package installs it, with a session program beside it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let helper = dir.path().join("punktfunk-seats");
        std::fs::copy(seats_binary(), &helper).unwrap();
        let program = dir.path().join("seat-session");
        let script = "#!/bin/sh\necho \"$$\" > \"$RUNTIME_DIRECTORY/child\"\nexec sleep 60\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let listener = handover::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let handover = Handover::default();
        let root_only = |stream: &std::os::unix::net::UnixStream, _: &str| {
            let cred = rustix::net::sockopt::socket_peercred(stream).map_err(|e| e.to_string())?;
            cred.uid
                .is_root()
                .then_some(())
                .ok_or_else(|| "not root".to_owned())
        };
        let child_file = runtime.join("child");
        // Starts the helper with `offered` waiting for it; `inspect` sees the session child's
        // pid and the helper's, then stops the helper. The helper's exit code and its log.
        let start = |offered: Option<&str>, inspect: &dyn Fn(i32, i32)| -> (i32, String) {
            let _ = std::fs::remove_file(&child_file);
            let password = offered.map(|p| Secret::new(p.into()));
            let offer = password
                .as_ref()
                .map(|p| handover.offer(ACCOUNT, p, Instant::now()));
            let done = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    while !done.load(std::sync::atomic::Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream.set_nonblocking(false).unwrap();
                                return handover::hand(&handover, stream, root_only);
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(e) => panic!("accept: {e}"),
                        }
                    }
                });
                let mut process = std::process::Command::new("bash")
                    .args(["-c", "ulimit -S -c 4 && exec \"$0\" \"$@\""])
                    .arg(&helper)
                    .args([VERB, ACCOUNT])
                    .env_clear()
                    .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
                    .env("XDG_SESSION_TYPE", "wayland")
                    .env("XDG_SESSION_CLASS", "user")
                    .env("RUNTIME_DIRECTORY", &runtime)
                    .env("PF_TEST_MARK", "1")
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(20);
                while !child_file.exists() && Instant::now() < deadline {
                    if process.try_wait().unwrap().is_some() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if let Ok(text) = std::fs::read_to_string(&child_file) {
                    let helper_pid = process.id() as i32;
                    inspect(text.trim().parse().unwrap(), helper_pid);
                    let pid = rustix::process::Pid::from_child(&process);
                    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
                }
                let output = process.wait_with_output().unwrap();
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                let log = String::from_utf8_lossy(&output.stderr).into_owned();
                drop(offer);
                (output.status.code().unwrap_or(-1), log)
            })
        };
        let no_secret = |what: &str, bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            assert!(
                !text.contains(PASSWORD) && !text.contains("pass-7"),
                "{what}: {text}"
            );
        };
        let proc_read =
            |pid: i32, file: &str| std::fs::read(format!("/proc/{pid}/{file}")).unwrap();
        let as_person = |args: &[&str]| {
            let out = std::process::Command::new("runuser")
                .args(["-u", ACCOUNT, "--"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out.stdout
        };
        let own_status = std::fs::read_to_string("/proc/self/status").unwrap();
        let effective = own_status
            .lines()
            .find_map(|l| l.strip_prefix("CapEff:"))
            .unwrap();
        let may_ptrace = u64::from_str_radix(effective.trim(), 16).unwrap() & (1 << 19) != 0;
        let core_line = |limits: &str| {
            limits
                .lines()
                .find(|l| l.starts_with("Max core file size"))
                .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap()
        };

        let inspect = |child: i32, helper_pid: i32| {
            let status = String::from_utf8(proc_read(child, "status")).unwrap();
            let field = |name: &str| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(name))
                    .unwrap()
                    .split_whitespace()
                    .collect::<Vec<_>>()
            };
            assert_eq!(field("Uid:"), vec![uid.to_string(); 4]);
            assert_eq!(field("Gid:"), vec![gid.to_string(); 4]);
            let mut groups = field("Groups:");
            groups.sort();
            let mut expected = vec![gid.to_string(), extra.to_string()];
            expected.sort();
            assert_eq!(groups, expected);
            for caps in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
                assert_eq!(field(caps), ["0000000000000000"], "{caps}");
            }
            assert_eq!(
                field("SigBlk:"),
                ["0000000000000000"],
                "no blocked signal in the session"
            );
            // What only the process's own user may read without CAP_SYS_PTRACE, as that user.
            let fds = as_person(&["ls", &format!("/proc/{child}/fd")]);
            assert_eq!(
                String::from_utf8(fds)
                    .unwrap()
                    .split_whitespace()
                    .collect::<Vec<_>>(),
                ["0", "1", "2"]
            );
            let cwd = as_person(&["readlink", &format!("/proc/{child}/cwd")]);
            assert_eq!(cwd, b"/\n");
            let environ = as_person(&["cat", &format!("/proc/{child}/environ")]);
            let env: Vec<&[u8]> = environ.split(|&b| b == 0).collect();
            for wanted in [
                "XDG_SESSION_TYPE=wayland".to_owned(),
                "XDG_SESSION_CLASS=user".into(),
                "PF_TEST_MARK=1".into(),
                format!("USER={ACCOUNT}"),
                format!("LOGNAME={ACCOUNT}"),
                format!("HOME=/home/{ACCOUNT}"),
                "SHELL=/bin/bash".into(),
            ] {
                assert!(env.contains(&wanted.as_bytes()), "{wanted} in the session");
            }
            no_secret("child environ", &environ);
            no_secret("child cmdline", &proc_read(child, "cmdline"));
            no_secret("helper cmdline", &proc_read(helper_pid, "cmdline"));
            // Not dumpable: only CAP_SYS_PTRACE reads the helper's memory files, root's or not.
            let helper_env = std::fs::read(format!("/proc/{helper_pid}/environ"));
            match (helper_env, may_ptrace) {
                (Ok(bytes), true) => no_secret("helper environ", &bytes),
                (Err(e), false) => assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied),
                (read, _) => panic!(
                    "helper environ read {:?} with CAP_SYS_PTRACE {may_ptrace}",
                    read.map(|_| ())
                ),
            }
            assert_eq!(proc_read(child, "cmdline"), b"sleep\x0060\x00");
            let helper_limits = String::from_utf8(proc_read(helper_pid, "limits")).unwrap();
            let hard = core_line(&helper_limits)
                .rsplit(' ')
                .nth(1)
                .unwrap()
                .to_owned();
            assert_eq!(
                core_line(&helper_limits),
                format!("Max core file size 0 {hard} bytes")
            );
            let child_limits = String::from_utf8(proc_read(child, "limits")).unwrap();
            let given_back = format!("Max core file size 4096 {hard} bytes");
            assert_eq!(core_line(&child_limits), given_back);
            assert_eq!(std::fs::metadata(&runtime).unwrap().uid(), uid);
        };

        let (code, log) = start(Some(PASSWORD), &inspect);
        no_secret("helper log", log.as_bytes());
        assert_eq!(code, 128 + 15, "the stop reached the session: {log}");
        assert!(!handover.waits(ACCOUNT), "handed out once");

        let never: &dyn Fn(i32, i32) = &|_, _| panic!("no session before a refusal");
        let refused = |offered: Option<&str>| {
            let (code, log) = start(offered, never);
            no_secret("helper log", log.as_bytes());
            assert!(!child_file.exists(), "nothing ran: {log}");
            code
        };
        assert_eq!(refused(Some("wrong-password")), i32::from(WRONG));
        assert_eq!(refused(None), i32::from(NO_PASSWORD));
        run("chage", &["-E", "1", ACCOUNT]);
        assert_eq!(refused(Some(PASSWORD)), i32::from(LOCKED));
        run("chage", &["-E", "-1", ACCOUNT]);
        run("chage", &["-d", "0", ACCOUNT]);
        assert_eq!(refused(Some(PASSWORD)), i32::from(MUST_CHANGE));
        run("chage", &["-d", "-1", ACCOUNT]);
        run("usermod", &["-L", ACCOUNT]);
        assert_eq!(refused(Some(PASSWORD)), i32::from(WRONG));
        run("usermod", &["-U", ACCOUNT]);
        std::fs::remove_file(&pam_file).unwrap();
        assert_eq!(refused(Some(PASSWORD)), i32::from(NO_SERVICE));
    }
}
