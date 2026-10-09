//! Worker-IPC rails shared by every punktfunk worker subprocess
//! (`design/zerocopy-worker-isolation.md`). Two halves, both free of any
//! worker's vocabulary — message enums live with the worker and stay
//! independently versioned:
//!
//! - **Framing.** A `SOCK_SEQPACKET` unix socketpair carries serde bodies;
//!   descriptors ride as `SCM_RIGHTS`. Zero-length is reserved: `recvmsg` of
//!   0 is EOF (peer closed), and a serialized message is never empty.
//! - **Process rails.** Spawn on a pinned executable with the socket on fd 3,
//!   `PR_SET_PDEATHSIG` so host death kills children, reap without blocking a
//!   caller behind a driver ioctl.
//!
//! The zerocopy worker execs this process's own image ([`self_exe`]). The
//! encode worker is a **separate file** — a shared inode would share the file
//! capability — so it passes its own path to [`spawn_worker`].

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
    SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::File;
use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 64 KiB ceiling. Real bodies (a modifier list) sit far below; a message
/// truncated at this size is a protocol error.
pub const MAX_MSG: usize = 64 * 1024;

/// Four: one fd per plane of a multi-planar dmabuf. [`send`]/[`recv`] stay
/// allocation-free on the single-fd hot path.
pub const MAX_FDS: usize = 4;

/// `CMSG_SPACE` of [`MAX_FDS`] fds.
const CMSG_RECV: usize = rustix::cmsg_aligned_space!(ScmRights(MAX_FDS));

/// Receive control space for exactly [`MAX_FDS`] fds. Aligned for `cmsghdr`, so none of it
/// is lost to alignment: a peer sending more trips `MSG_CTRUNC` instead of landing them.
#[repr(C, align(8))]
struct CmsgStore([MaybeUninit<u8>; CMSG_RECV]);

/// A CLOEXEC `SOCK_SEQPACKET` socketpair — `(host_end, worker_end)`.
pub fn socketpair_seqpacket() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

/// `SO_RCVTIMEO`. A hung worker then fails [`recv`] with `WouldBlock` instead
/// of wedging the calling thread. `None`, or under a microsecond, clears the timeout.
pub fn set_recv_timeout(sock: BorrowedFd, timeout: Option<Duration>) -> io::Result<()> {
    // Whole microseconds, as `timeval` holds them: rustix rounds a sub-µs part up, and
    // 999_999.5 µs rounds to a `tv_usec` of 1_000_000, which the kernel refuses (EDOM).
    let timeout = timeout
        .map(|d| Duration::from_micros(d.as_micros() as u64))
        .filter(|d| !d.is_zero());
    Ok(rustix::net::sockopt::set_socket_timeout(
        sock,
        rustix::net::sockopt::Timeout::Recv,
        timeout,
    )?)
}

/// Single-fd fast path over [`send_fds`].
pub fn send<T: Serialize>(
    sock: BorrowedFd,
    msg: &T,
    pass_fd: Option<BorrowedFd>,
) -> io::Result<()> {
    match pass_fd {
        Some(fd) => send_fds(sock, msg, &[fd]),
        None => send_fds(sock, msg, &[]),
    }
}

/// One SEQPACKET datagram: body plus up to [`MAX_FDS`] fds. Atomic, so
/// concurrent senders need no lock. `MSG_NOSIGNAL` turns a dead peer into
/// `EPIPE` instead of `SIGPIPE`. Over [`MAX_FDS`] is `InvalidInput`, not a panic.
pub fn send_fds<T: Serialize>(sock: BorrowedFd, msg: &T, fds: &[BorrowedFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "worker ipc: {} fds in one message (max {MAX_FDS})",
                fds.len()
            ),
        ));
    }
    let body =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    debug_assert!(
        !body.is_empty(),
        "zero-length messages are reserved for EOF"
    );
    if body.len() > MAX_MSG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker ipc message too large",
        ));
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker ipc: descriptors do not fit the control buffer",
        ));
    }
    // The kernel dups the fds; the `BorrowedFd`s stay with the caller.
    let n = rustix::net::sendmsg(
        sock,
        &[IoSlice::new(&body)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    if n != body.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short sendmsg on SEQPACKET socket",
        ));
    }
    Ok(())
}

/// Single-fd fast path over [`recv_fds`]. Extra descriptors the peer attached
/// are closed so a protocol mix-up cannot leak them into a one-fd caller.
pub fn recv<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
) -> io::Result<(T, Option<OwnedFd>)> {
    let (msg, fds) = recv_fds(sock, buf)?;
    Ok((msg, fds.into_iter().next()))
}

/// Body plus `SCM_RIGHTS` fds, sender order, up to [`MAX_FDS`]. `buf` is
/// caller scratch, grown to [`MAX_MSG`] once. Excess fds: kernel flags
/// `MSG_CTRUNC` and we return `InvalidData`. Empty-fd `Vec` does not allocate.
pub fn recv_fds<T: DeserializeOwned>(
    sock: BorrowedFd,
    buf: &mut Vec<u8>,
) -> io::Result<(T, Vec<OwnedFd>)> {
    buf.resize(MAX_MSG, 0);
    // Control space for MAX_FDS only: excess fds are dropped by the kernel and
    // flagged `MSG_CTRUNC` (checked below). Do not trust the peer's count.
    let mut store = CmsgStore([MaybeUninit::uninit(); CMSG_RECV]);
    let mut control = RecvAncillaryBuffer::new(&mut store.0);
    let msg = rustix::net::recvmsg(
        sock,
        &mut [IoSliceMut::new(buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let n = msg.bytes;
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "worker ipc peer closed",
        ));
    }
    // Own every received fd before any early return; dropping `got` on error
    // closes them (so would `control`'s drop, for any left undrained).
    let mut got: Vec<OwnedFd> = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = m {
            got.extend(fds);
        }
    }
    if msg.flags.contains(ReturnFlags::CTRUNC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("worker ipc message carried more than {MAX_FDS} descriptors"),
        ));
    }
    if msg.flags.contains(ReturnFlags::TRUNC) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker ipc message truncated",
        ));
    }
    let msg = serde_json::from_slice(&buf[..n])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((msg, got))
}

/// Executable pinned by open fd, exec'd via [`PinnedExe::exec_path`]. The fd
/// names the inode, so a replaced or deleted binary still runs the pinned
/// bytes. `current_exe()` readlinks: a gone file is `"<path> (deleted)"`
/// (ENOENT), and a still-present path may be a newer build whose protocol
/// mismatches this process.
pub struct PinnedExe(File);

impl PinnedExe {
    pub fn open(path: &Path) -> io::Result<PinnedExe> {
        let f = File::open(path)?;
        if f.as_raw_fd() != 3 {
            return Ok(PinnedExe(f));
        }
        // Fd 3 is the worker socket (`dup2` in [`spawn_worker`]). Pinning the
        // exe there would clobber it before exec resolves `/proc/self/fd/3`.
        // Clone while 3 is still held so the duplicate cannot land on 3.
        let clone = f.try_clone().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("re-numbering the pinned exe fd off fd 3 failed: {e}"),
            )
        })?;
        Ok(PinnedExe(clone))
    }

    /// `/proc/self/fd/<n>` of the pinned inode. The kernel resolves it at exec
    /// in the forked child, whose fd table still holds this fd (CLOEXEC fires
    /// only after exec succeeds).
    pub fn exec_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }
}

/// This process's image, pinned once via `/proc/self/exe`. `None` if open
/// failed — callers then use `current_exe()` and inherit its replacement trap.
///
/// Only for a worker that is the same file as its host (the zerocopy worker
/// re-execs this binary). The encode worker is a **separate file** — it
/// carries a file capability the host must never have — and pins its own path
/// with [`PinnedExe::open`].
pub fn self_exe() -> Option<&'static PinnedExe> {
    static SELF_EXE: OnceLock<Option<PinnedExe>> = OnceLock::new();
    SELF_EXE
        .get_or_init(|| match PinnedExe::open(Path::new("/proc/self/exe")) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "cannot pin /proc/self/exe — worker spawns use the current_exe() path, \
                     which breaks if this binary is replaced on disk"
                );
                None
            }
        })
        .as_ref()
}

/// Spawn `exe` (normally [`PinnedExe::exec_path`]) with a SEQPACKET socket on
/// **fd 3**. Returns the host end and the child.
///
/// `argv0` is what `ps` shows — `exe` is usually opaque `/proc/self/fd/<n>`.
pub fn spawn_worker(exe: &Path, argv0: &str, args: &[&str]) -> io::Result<(OwnedFd, Child)> {
    sweep_reaper();
    let (host_end, worker_end) = socketpair_seqpacket()?;
    let mut cmd = Command::new(exe);
    cmd.arg0(argv0);
    cmd.args(args);
    let raw = worker_end.as_raw_fd();
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: `pre_exec` is between fork and exec: only async-signal-safe
    // calls (`prctl`, `getppid`, `dup2`, `fcntl`). The closure captures `Copy`
    // ints; `from_raw_os_error` does not allocate. `PR_SET_PDEATHSIG(SIGKILL)`
    // plus `getppid != parent` refuse-to-exec closes the race where the host
    // dies between fork and prctl and the signal is never delivered.
    // `dup2(raw, 3)` installs the socket and clears CLOEXEC; `dup2(3, 3)`
    // would preserve CLOEXEC, so that case `F_SETFD`s it off instead.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            if raw == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(raw, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    drop(worker_end); // child's dup is the remaining live end
    Ok((host_end, child))
}

/// The worker half of [`spawn_worker`]: adopt the socket on `--fd N` (default 3).
///
/// Refuses anything that cannot be the spawning host's socket. A negative fd is UB inside
/// `OwnedFd` (its niche) and 0–2 would close stdio on exit; `fstat` then confirms an open
/// socket, since a worker subcommand is runnable by hand and adopting an inherited fd would
/// close it behind its real owner.
pub fn adopt_spawned_socket(args: &[String]) -> anyhow::Result<OwnedFd> {
    use anyhow::Context;
    let fd: RawFd = args
        .iter()
        .skip_while(|a| *a != "--fd")
        .nth(1)
        .map(|s| s.parse())
        .transpose()
        .context("parse --fd")?
        .unwrap_or(3);
    anyhow::ensure!(fd >= 3, "--fd must be >= 3 (got {fd})");
    // SAFETY: `libc::stat` is plain-old-data for which all-zero is a valid value, so
    // `mem::zeroed()` is a sound initializer; `fstat` writes into the live, correctly-sized
    // `&mut st` and only reads `fd`. `st_mode` is read only after the return value is checked.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(fd, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    };
    anyhow::ensure!(
        is_socket,
        "--fd {fd} is not an open socket (workers are spawned by punktfunk-host, not run by hand)"
    );
    // SAFETY: the spawning host `dup2`'d its socketpair end onto exactly this fd number before
    // exec (the worker's contract, just verified to be an open socket ≥ 3) and nothing else in
    // this fresh process owns it, so `OwnedFd` takes sole ownership and closes it once at exit.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Parked children not yet exited (a worker exits on socket EOF after the
/// last in-flight frame). Swept on spawn and drop so they do not linger as
/// zombies past one generation.
static REAPER: Mutex<Vec<(Child, Instant)>> = Mutex::new(Vec::new());

/// 20 s past park before SIGKILL. A worker wedged in a driver ioctl never
/// sees socket EOF, so `try_wait` alone would keep it forever.
const REAPER_KILL_DEADLINE: Duration = Duration::from_secs(20);

/// Park a still-running worker. Call from `Drop` after the socket is closed:
/// the worker sees EOF; the next [`sweep_reaper`] collects it.
pub fn park_child(child: Child) {
    REAPER.lock().unwrap().push((child, Instant::now()));
}

/// Reap exited workers; SIGKILL those parked past [`REAPER_KILL_DEADLINE`].
pub fn sweep_reaper() {
    // Partition under the lock; kill/reap outside it. A D-state ioctl ignores
    // SIGKILL; a blocking `wait()` under this mutex would stall every later
    // spawn and drop behind a process that may never die.
    let mut expired: Vec<Child> = Vec::new();
    {
        let mut list = REAPER.lock().unwrap();
        let now = Instant::now();
        let mut i = 0;
        while i < list.len() {
            if matches!(list[i].0.try_wait(), Ok(Some(_))) {
                list.swap_remove(i);
            } else if now.duration_since(list[i].1) > REAPER_KILL_DEADLINE {
                expired.push(list.swap_remove(i).0);
            } else {
                i += 1;
            }
        }
    }
    for mut c in expired {
        let _ = c.kill();
        // ~100 ms of polls (10 × 10 ms). SIGKILL reaps instantly unless D-state;
        // then re-park (re-killing later is harmless) instead of blocking here.
        let mut reaped = false;
        for _ in 0..10 {
            if matches!(c.try_wait(), Ok(Some(_))) {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !reaped {
            tracing::warn!(
                pid = c.id(),
                "worker ignored SIGKILL (likely wedged in a driver call, D state) — \
                 parked for a later sweep"
            );
            park_child(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    /// Stand-in body. Real message enums live with the workers; this crate
    /// must not depend on one.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Msg {
        tag: String,
        n: u32,
    }

    fn msg(tag: &str) -> Msg {
        Msg {
            tag: tag.into(),
            n: 7,
        }
    }

    /// Pipe pre-loaded with `payload`. Reading it back through the received
    /// fd is the proof the descriptor crossed, not just the claim.
    fn marked_pipe(payload: &[u8]) -> (std::io::PipeReader, std::io::PipeWriter) {
        let (pr, mut pw) = std::io::pipe().unwrap();
        pw.write_all(payload).unwrap();
        (pr, pw)
    }

    /// A peer bypassing `send_fds` with one fd too many: the receive store holds exactly
    /// `MAX_FDS`, so the kernel truncates and `recv_fds` refuses the message.
    #[test]
    fn recv_refuses_more_than_max_fds() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let pipes: Vec<_> = (0..MAX_FDS + 1).map(|_| std::io::pipe().unwrap()).collect();
        let fds: Vec<BorrowedFd> = pipes.iter().map(|(pr, _)| pr.as_fd()).collect();
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS + 1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        let body = serde_json::to_vec(&msg("over")).unwrap();
        rustix::net::sendmsg(
            a.as_fd(),
            &[IoSlice::new(&body)],
            &mut control,
            SendFlags::NOSIGNAL,
        )
        .unwrap();
        let mut buf = Vec::new();
        let err = recv_fds::<Msg>(b.as_fd(), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn round_trip_no_fd() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        send(a.as_fd(), &msg("hello"), None).unwrap();
        let (got, fd) = recv::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, msg("hello"));
        assert!(fd.is_none());
    }

    #[test]
    fn passes_an_fd() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let (mut pr, mut pw) = std::io::pipe().unwrap();
        send(a.as_fd(), &msg("one"), Some(pr.as_fd())).unwrap();
        let (got, fd) = recv::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, msg("one"));
        let fd = fd.expect("fd should have been passed");
        pw.write_all(b"hello").unwrap();
        drop(pw);
        let mut file = File::from(fd);
        let mut s = String::new();
        file.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");
        let mut nothing = [0u8; 1];
        assert_eq!(pr.read(&mut nothing).unwrap(), 0);
    }

    #[test]
    fn round_trip_three_fds() {
        // Distinct per-plane bytes: received fds must arrive in sender order.
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let planes = [marked_pipe(b"Y"), marked_pipe(b"U"), marked_pipe(b"V")];
        {
            let fds: Vec<BorrowedFd> = planes.iter().map(|(pr, _)| pr.as_fd()).collect();
            send_fds(a.as_fd(), &msg("planes"), &fds).unwrap();
        }
        let (got, fds) = recv_fds::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got, msg("planes"));
        assert_eq!(fds.len(), 3, "all three descriptors must cross");
        // Drop the write ends so each read sees EOF after its byte.
        drop(planes);
        let read_back: Vec<String> = fds
            .into_iter()
            .map(|fd| {
                let mut s = String::new();
                File::from(fd).read_to_string(&mut s).unwrap();
                s
            })
            .collect();
        assert_eq!(read_back, vec!["Y", "U", "V"]);
    }

    #[test]
    fn max_fds_round_trip_and_one_more_is_refused() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let pipes: Vec<_> = (0..MAX_FDS + 1).map(|_| std::io::pipe().unwrap()).collect();
        let fds: Vec<BorrowedFd> = pipes.iter().map(|(pr, _)| pr.as_fd()).collect();
        send_fds(a.as_fd(), &msg("full"), &fds[..MAX_FDS]).unwrap();
        let (_, got) = recv_fds::<Msg>(b.as_fd(), &mut buf).unwrap();
        assert_eq!(got.len(), MAX_FDS);
        let err = send_fds(a.as_fd(), &msg("over"), &fds).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn recv_keeps_the_first_fd_and_closes_the_rest() {
        let (a, b) = socketpair_seqpacket().unwrap();
        let mut buf = Vec::new();
        let (first, second) = (marked_pipe(b"1"), marked_pipe(b"2"));
        send_fds(a.as_fd(), &msg("two"), &[first.0.as_fd(), second.0.as_fd()]).unwrap();
        let (_, fd) = recv::<Msg>(b.as_fd(), &mut buf).unwrap();
        let fd = fd.expect("the first fd is returned");
        drop(first);
        let mut s = String::new();
        File::from(fd).read_to_string(&mut s).unwrap();
        assert_eq!(s, "1");
        // A concurrent spawn may briefly inherit this CLOEXEC fd before exec.
        // Once that window closes, EPIPE proves `recv` dropped its extra copy.
        drop(second.0);
        let mut pw = second.1;
        let mut closed = None;
        for _ in 0..50 {
            match pw.write_all(b"x") {
                Ok(()) => std::thread::sleep(Duration::from_millis(1)),
                Err(err) => {
                    closed = Some(err);
                    break;
                }
            }
        }
        assert_eq!(
            closed.expect("extra read fd stayed open").kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn eof_when_peer_closes() {
        let (a, b) = socketpair_seqpacket().unwrap();
        drop(a);
        let mut buf = Vec::new();
        let err = recv::<Msg>(b.as_fd(), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn send_to_dead_peer_is_epipe_not_sigpipe() {
        let (a, b) = socketpair_seqpacket().unwrap();
        drop(b);
        let err = send(a.as_fd(), &msg("gone"), None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// A remainder just under a whole second must not round into an invalid `tv_usec`.
    #[test]
    fn recv_timeout_takes_any_remainder() {
        let (a, _b) = socketpair_seqpacket().unwrap();
        for d in [Duration::new(0, 999_999_500), Duration::new(4, 999_999_999)] {
            set_recv_timeout(a.as_fd(), Some(d)).unwrap();
        }
        set_recv_timeout(a.as_fd(), Some(Duration::from_nanos(500))).unwrap();
        set_recv_timeout(a.as_fd(), None).unwrap();
    }

    #[test]
    fn recv_timeout_fires() {
        let (a, _b) = socketpair_seqpacket().unwrap();
        set_recv_timeout(a.as_fd(), Some(Duration::from_millis(50))).unwrap();
        let mut buf = Vec::new();
        let err = recv::<Msg>(a.as_fd(), &mut buf).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error kind: {err:?}"
        );
    }

    #[test]
    fn pinned_fd_exec_survives_on_disk_replacement() {
        // Pin a copy of /bin/sh, delete it, exec via the fd path: the inode
        // must still run after the directory entry is gone.
        let copy = std::env::temp_dir().join(format!("pf-zerocopy-exe-pin-{}", std::process::id()));
        std::fs::copy("/bin/sh", &copy).unwrap();
        let pinned = PinnedExe::open(&copy).unwrap();
        std::fs::remove_file(&copy).unwrap();
        // Retry ETXTBSY: `fs::copy`'s write fd can leak into other tests'
        // forked children until their execs (CLOEXEC is at exec). Production
        // pins a read-only fd nobody write-opens.
        let status = loop {
            match Command::new(pinned.exec_path())
                .arg("-c")
                .arg("exit 42")
                .status()
            {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => break other.expect("exec via /proc/self/fd of a deleted file"),
            }
        };
        assert_eq!(status.code(), Some(42));
    }

    #[test]
    fn spawn_worker_hands_the_socket_on_fd_3() {
        // `sh` writes through fd 3. A reply here means `dup2` landed and
        // CLOEXEC was cleared on the copy.
        let (host, mut child) = spawn_worker(
            Path::new("/bin/sh"),
            "pf-test-worker",
            &["-c", r#"printf '"pong"' >&3"#],
        )
        .unwrap();
        let mut buf = Vec::new();
        let (got, fds) = recv_fds::<String>(host.as_fd(), &mut buf).unwrap();
        assert_eq!(got, "pong");
        assert!(fds.is_empty());
        child.wait().unwrap();
    }

    /// Only an open socket at fd 3 or above is adopted; anything else is left to its owner.
    #[test]
    fn adopt_spawned_socket_takes_only_a_socket_it_may_own() {
        let args = |fd: &str| vec!["--fd".to_string(), fd.to_string()];
        assert!(
            adopt_spawned_socket(&args("2")).is_err(),
            "stdio is never adopted"
        );
        assert!(adopt_spawned_socket(&args("x")).is_err());
        let file = File::open("/dev/null").unwrap();
        let raw = file.as_raw_fd().to_string();
        assert!(
            adopt_spawned_socket(&args(&raw)).is_err(),
            "a file is not a socket"
        );
        let (a, b) = socketpair_seqpacket().unwrap();
        let adopted = adopt_spawned_socket(&args(&a.as_raw_fd().to_string())).unwrap();
        assert_eq!(adopted.as_raw_fd(), a.as_raw_fd());
        // `adopted` now owns the number; forget the original so it closes once.
        std::mem::forget(a);
        drop((adopted, b));
    }
}
