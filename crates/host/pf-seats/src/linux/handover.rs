//! The password a person's cold start was proved with, kept for the login helper in that row's
//! unit (`super::login`) and handed to it once.
//!
//! The supervisor keeps it only for a start through the helper, for at most [`LIFETIME`], and
//! wipes it when it is handed out, when it expires and when the start stops waiting for it. The
//! helper asks on [`SOCKET_PATH`], a socket only root can open; the door's socket has no request
//! that answers with a password. A request names its account and uses up that account's
//! password whether it is served or not, and only a peer that is root inside that account's
//! seat unit is served.

use super::LinuxBackend;
use crate::ipc::{read_frame_bytes, write_frame_bytes};
use crate::secret::Secret;
use crate::service::SeatService;
use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Where the supervisor hands the password out: root's alone, `0600`.
pub const SOCKET_PATH: &str = "/run/punktfunk/seats-login.sock";
/// The longest the supervisor keeps a password for its helper.
pub(super) const LIFETIME: Duration = Duration::from_secs(30);
/// A peer sends its request at once and reads the answer at once.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// The passwords waiting for their helper, by account.
#[derive(Default)]
pub(super) struct Handover {
    waiting: Mutex<HashMap<String, Waiting>>,
}

struct Waiting {
    password: Secret,
    until: Instant,
}

impl Handover {
    /// Keeps a copy of `password` for `account`'s helper from `now`. The copy goes when the
    /// returned [`Offer`] drops, if nothing took it first.
    pub(super) fn offer(&self, account: &str, password: &Secret, now: Instant) -> Offer<'_> {
        let until = now + LIFETIME;
        let waiting = Waiting {
            password: password.clone(),
            until,
        };
        self.lock().insert(account.to_owned(), waiting);
        Offer {
            handover: self,
            account: account.to_owned(),
            until,
        }
    }

    /// The password waiting for `account`, once: it is gone after this whatever it answers. One
    /// past its lifetime is wiped and not handed out.
    pub(super) fn take(&self, account: &str, now: Instant) -> Option<Secret> {
        let waiting = self.lock().remove(account)?;
        (now < waiting.until).then_some(waiting.password)
    }

    /// Whether a password waits for `account`.
    pub(super) fn waits(&self, account: &str) -> bool {
        self.lock().contains_key(account)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Waiting>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A password offered to a helper; dropping it wipes the password if it still waits.
pub(super) struct Offer<'a> {
    handover: &'a Handover,
    account: String,
    until: Instant,
}

impl Offer<'_> {
    /// Returns once the helper took the password, the password expired, or `live` says the unit
    /// is no longer up to ask for it.
    pub(super) fn wait(&self, live: impl Fn() -> bool) {
        while self.handover.waits(&self.account) && Instant::now() < self.until && live() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Offer<'_> {
    fn drop(&mut self) {
        self.handover.lock().remove(&self.account);
    }
}

/// Binds `path` for root alone.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt as _;
    let listener = super::socket::bind_fresh(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves `listener` until the process ends, one request at a time.
pub fn serve(listener: UnixListener, service: Arc<SeatService<LinuxBackend>>) {
    let handover = &service.backend().inner.handover;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => hand(handover, stream, peer_in_unit),
            Err(error) => tracing::warn!(%error, "login socket did not accept a connection"),
        }
    }
}

/// Answers one request on `stream`: the account it names, then that account's password if
/// `in_unit` vouches for the peer. A refused peer gets no answer.
pub(super) fn hand(
    handover: &Handover,
    mut stream: UnixStream,
    in_unit: impl Fn(&UnixStream, &str) -> Result<(), String>,
) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let named = read_frame_bytes(&mut stream).ok().and_then(|bytes| {
        let account = std::str::from_utf8(&bytes).ok()?;
        crate::model::valid_owner_account(account).then(|| account.to_owned())
    });
    let Some(account) = named else {
        tracing::warn!("login socket got a malformed request");
        return;
    };
    let password = handover.take(&account, Instant::now());
    if let Err(cause) = in_unit(&stream, &account) {
        tracing::warn!(%account, %cause, "login socket refused a peer");
        return;
    }
    let Some(password) = password else {
        tracing::warn!(%account, "no password waits for the login helper");
        let _ = write_frame_bytes(&mut stream, &[]);
        return;
    };
    match write_frame_bytes(&mut stream, password.expose().as_bytes()) {
        Ok(()) => tracing::info!(%account, "password handed to the login helper"),
        Err(error) => tracing::warn!(%account, %error, "password not handed to the login helper"),
    }
}

/// Whether `cgroup`, a `/proc/<pid>/cgroup` text, puts its process in `account`'s seat unit.
fn in_unit(cgroup: &str, account: &str) -> bool {
    let unit = super::session::unit(account);
    cgroup.lines().any(|line| {
        line.strip_prefix("0::")
            .is_some_and(|path| path.rsplit('/').next() == Some(unit.as_str()))
    })
}

/// Root inside `account`'s seat unit, and so its login helper before it opens a session. Defence
/// in depth: the socket's mode already admits root alone. The process is read through a pidfd
/// for the peer where the kernel gives one (`SO_PEERPIDFD`), so a recycled pid can't stand in.
#[cfg(target_os = "linux")]
fn peer_in_unit(stream: &UnixStream, account: &str) -> Result<(), String> {
    let cred = rustix::net::sockopt::socket_peercred(stream)
        .map_err(|e| format!("peer credentials: {e}"))?;
    if !cred.uid.is_root() {
        return Err(format!("uid {} is not root", cred.uid.as_raw()));
    }
    let cgroup = match peer_pidfd(stream)? {
        Some(pidfd) => {
            let pid = pidfd_pid(&pidfd)?;
            let cgroup = read_cgroup(pid)?;
            // Not yet reaped, so `pid` was this process throughout the read.
            if pidfd_pid(&pidfd)? != pid {
                return Err("the peer exited".into());
            }
            cgroup
        }
        None => read_cgroup(cred.pid.as_raw_nonzero().get())?,
    };
    if in_unit(&cgroup, account) {
        Ok(())
    } else {
        Err(format!("not in {}", super::session::unit(account)))
    }
}

#[cfg(not(target_os = "linux"))]
fn peer_in_unit(_stream: &UnixStream, _account: &str) -> Result<(), String> {
    Err("peer credentials need Linux".into())
}

/// The peer's pidfd, or `None` on a kernel older than 6.5, which has no `SO_PEERPIDFD`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn peer_pidfd(stream: &UnixStream) -> Result<Option<std::os::fd::OwnedFd>, String> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    // `<asm-generic/socket.h>`; the libc crate doesn't name it yet.
    const SO_PEERPIDFD: libc::c_int = 77;
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `fd`, an int, for a socket `stream`
    // keeps open through the call.
    let got = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERPIDFD,
            (&raw mut fd).cast(),
            &mut len,
        )
    };
    if got != 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOPROTOOPT) => Ok(None),
            _ => Err(format!("SO_PEERPIDFD: {error}")),
        };
    }
    // SAFETY: the kernel installed `fd` for this call alone; nothing else owns it.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

/// The pid `pidfd` refers to, from its fdinfo. A reaped process reads as an error.
#[cfg(target_os = "linux")]
fn pidfd_pid(pidfd: &std::os::fd::OwnedFd) -> Result<i32, String> {
    use std::os::fd::AsRawFd as _;
    let path = format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd());
    let info = std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))?;
    let pid = info
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))
        .and_then(|pid| pid.trim().parse::<i32>().ok());
    pid.filter(|pid| *pid > 0)
        .ok_or_else(|| "the peer exited".to_owned())
}

#[cfg(target_os = "linux")]
fn read_cgroup(pid: i32) -> Result<String, String> {
    let path = format!("/proc/{pid}/cgroup");
    std::fs::read_to_string(&path).map_err(|e| format!("read {path}: {e}"))
}

/// The helper's side: the password waiting for `account`, asked of the supervisor at `socket`.
pub(super) fn fetch(socket: &Path, account: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|e| format!("connect to {}: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    write_frame_bytes(&mut stream, account.as_bytes())
        .map_err(|e| format!("ask for the password: {e}"))?;
    let password = read_frame_bytes(&mut stream).map_err(|e| format!("read the password: {e}"))?;
    if password.is_empty() {
        return Err("no password waits for this account".into());
    }
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = "jdoe";

    fn secret() -> Secret {
        Secret::new("hunter2 Bärbel".into())
    }

    /// Asks `handover` for `account`'s password over a socket pair, as the helper does, with the
    /// peer vouched for or not.
    fn ask(handover: &Handover, account: &str, vouched: bool) -> Result<Vec<u8>, String> {
        let (mut helper, supervisor) = UnixStream::pair().unwrap();
        write_frame_bytes(&mut helper, account.as_bytes()).unwrap();
        let in_unit = |_: &UnixStream, _: &str| {
            if vouched {
                Ok(())
            } else {
                Err("not in the unit".to_owned())
            }
        };
        hand(handover, supervisor, in_unit);
        read_frame_bytes(&mut helper)
            .map(|bytes| bytes.to_vec())
            .map_err(|e| e.to_string())
    }

    /// The helper gets the password once; a second ask gets an empty answer, and nothing is left.
    #[test]
    fn the_password_is_handed_out_once() {
        let handover = Handover::default();
        let offer = handover.offer(ACCOUNT, &secret(), Instant::now());
        assert_eq!(
            ask(&handover, ACCOUNT, true).unwrap(),
            secret().expose().as_bytes()
        );
        assert!(!handover.waits(ACCOUNT));
        assert_eq!(ask(&handover, ACCOUNT, true).unwrap(), b"");
        drop(offer);
        assert!(handover.lock().is_empty());
    }

    /// A peer the unit check refuses gets no answer at all, and its request still uses the
    /// password up, so the real helper finds none.
    #[test]
    fn a_refused_peer_gets_nothing_and_ends_the_handover() {
        let handover = Handover::default();
        let _offer = handover.offer(ACCOUNT, &secret(), Instant::now());
        assert!(ask(&handover, ACCOUNT, false).is_err(), "closed unanswered");
        assert!(!handover.waits(ACCOUNT));
        assert_eq!(ask(&handover, ACCOUNT, true).unwrap(), b"");
    }

    /// A request for another account, or a malformed one, leaves the waiting password alone and
    /// is answered with nothing.
    #[test]
    fn only_the_named_account_s_password_is_handed_out() {
        let handover = Handover::default();
        let _offer = handover.offer(ACCOUNT, &secret(), Instant::now());
        assert_eq!(ask(&handover, "alice", true).unwrap(), b"");
        assert!(ask(&handover, "../jdoe", true).is_err());
        assert!(ask(&handover, &"a".repeat(65), true).is_err());
        assert!(handover.waits(ACCOUNT));
    }

    /// Past its lifetime a password is wiped, not handed out; dropping the offer wipes one that
    /// was never asked for.
    #[test]
    fn a_password_expires_and_goes_with_its_offer() {
        let handover = Handover::default();
        let start = Instant::now();
        let offer = handover.offer(ACCOUNT, &secret(), start);
        assert!(handover.take(ACCOUNT, start + LIFETIME).is_none());
        assert!(!handover.waits(ACCOUNT));
        drop(offer);
        let offer = handover.offer(ACCOUNT, &secret(), start);
        assert!(handover.waits(ACCOUNT));
        drop(offer);
        assert!(!handover.waits(ACCOUNT));
        let _offer = handover.offer(ACCOUNT, &secret(), start);
        let early = handover.take(ACCOUNT, start + LIFETIME - Duration::from_millis(1));
        assert_eq!(early, Some(secret()));
    }

    /// The helper's own request, over a socket file: the password once, then nothing.
    #[test]
    fn the_helper_fetches_its_password_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login.sock");
        let listener = bind(&path).unwrap();
        let handover = Arc::new(Handover::default());
        let _offer = handover.offer(ACCOUNT, &secret(), Instant::now());
        let served = Arc::clone(&handover);
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                hand(&served, stream, |_, _| Ok(()));
            }
        });
        let fetched = fetch(&path, ACCOUNT).unwrap();
        assert_eq!(&fetched[..], secret().expose().as_bytes());
        let again = fetch(&path, ACCOUNT).unwrap_err();
        assert_eq!(again, "no password waits for this account");
        server.join().unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The wait ends at once when the unit is down, and once the helper took the password.
    #[test]
    fn the_start_stops_waiting_when_the_unit_is_down_or_the_password_taken() {
        let handover = Handover::default();
        let offer = handover.offer(ACCOUNT, &secret(), Instant::now());
        let started = Instant::now();
        offer.wait(|| false);
        assert!(handover.waits(ACCOUNT), "the drop wipes it, not the wait");
        handover.take(ACCOUNT, Instant::now());
        offer.wait(|| true);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// Only the unified hierarchy's line, ending in this account's unit, counts.
    #[test]
    fn a_process_is_in_the_unit_its_cgroup_names() {
        let unit = "0::/system.slice/system-punktfunk\\x2dseat.slice/punktfunk-seat@jdoe.service\n";
        assert!(in_unit(unit, "jdoe"));
        assert!(!in_unit(unit, "jdo"));
        assert!(!in_unit(unit, "alice"));
        let session = "0::/user.slice/user-1500.slice/session-7.scope\n";
        assert!(!in_unit(session, "jdoe"));
        let v1 = "1:name=systemd:/system.slice/punktfunk-seat@jdoe.service\n";
        assert!(!in_unit(v1, "jdoe"));
        let nested = "0::/system.slice/punktfunk-seat@jdoe.service/extra\n";
        assert!(!in_unit(nested, "jdoe"));
    }
}
