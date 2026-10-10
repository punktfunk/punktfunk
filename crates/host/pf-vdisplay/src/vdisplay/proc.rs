//! Time-bounded child-process helpers.
//!
//! Compositor queries shell out. Those helpers are clients of the thing being diagnosed, so a
//! wedged compositor makes `kscreen-doctor` block in connect forever. `Command::status` /
//! `Command::output` have no timeout; on the host that hang is the session stream thread.
//!
//! Poll until `budget`, kill the **process tree**, return [`std::io::ErrorKind::TimedOut`].
//! Callers take the existing failure path.
//!
//! The budget is the tree, not the one process we spawned — a Job object on Windows, a process
//! group on Unix. See [`tree`]. A unit the *user manager* forks (`systemd-run --pipe`) is
//! outside both; [`DRAIN_GRACE`] bounds the reader, not the writer.

// `Take::read_to_end` needs `Read` in scope: `Take<R>` is a concrete type, so the generic
// bound does not bring the trait methods with it.
use std::io::{Error, ErrorKind, Read, Result};
use std::process::{Command, ExitStatus, Output};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Poll interval while waiting for a child to exit. 20 ms is well under a fast helper
/// (`kscreen-doctor` answers in tens of ms) and not a busy-loop.
const POLL: Duration = Duration::from_millis(20);

/// Ceiling on how long [`output_within`] waits for its reader threads once the child and
/// its process group are dead.
///
/// With every write end we can reach closed, readers hit EOF in a scheduler slice. This
/// bounds the one writer we cannot: [`tree`] ends a *group*, so a descendant that left it
/// (`systemd-run --pipe`, forked by the user manager) keeps the write end open. Waiting on
/// that reader would pin the caller forever. The call is bounded here; the reader thread
/// is left behind. A call can return up to this much after its own budget.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Ceiling on what one drained pipe may buffer.
///
/// `read_to_end` is unbounded, and a reader that outlived its call (see [`DRAIN_GRACE`])
/// has nobody left to stop it. Closing the read end at this cap also gives the escaped
/// writer an EPIPE. 16 MiB is an order of magnitude above the largest `pw-dump` a populated
/// PipeWire graph produces; hitting it means a helper that ran away, logged rather than
/// returned as quietly short output.
const DRAIN_CAP: u64 = 16 * 1024 * 1024;

/// Stdout/stderr stay as the caller configured them (inherited by default). Use
/// [`output_within`] when the output is read.
pub(crate) fn status_within(cmd: &mut Command, budget: Duration) -> Result<ExitStatus> {
    tree::prepare(cmd);
    let mut child = cmd.spawn()?;
    let tree = tree::Guard::attach(&child);
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait()? {
            Some(status) => {
                // The helper is gone; anything it left running still holds the stdio it inherited.
                tree.terminate();
                return Ok(status);
            }
            None if Instant::now() >= deadline => {
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait(); // reap — never leave a zombie
                return Err(timed_out(cmd, budget));
            }
            None => std::thread::sleep(POLL),
        }
    }
}

/// Both pipes are drained **concurrently with the wait**. Reading them only after exit
/// deadlocks on any helper that fills the pipe: Linux pipes hold 64 KiB, a chatty helper
/// blocks in `write()`, never exits, and is killed at the budget with its output discarded.
/// `pw-dump` on a populated graph clears 64 KiB routinely.
pub(crate) fn output_within(cmd: &mut Command, budget: Duration) -> Result<Output> {
    tree::prepare(cmd);
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let tree = tree::Guard::attach(&child);
    // Taken off `Child` so the reader threads own them: `wait_with_output` must not also
    // read these, and `try_wait` below needs `&mut child` while they run.
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let (out_rx, err_rx) = (drain(stdout), drain(stderr));

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait()? {
            Some(status) => {
                // The helper is gone, but a grandchild may still hold the pipes' WRITE ends.
                // Ending the tree closes them for every descendant that stayed in the group;
                // one that left it is bounded by the collection below (see [`DRAIN_GRACE`]).
                tree.terminate();
                break status;
            }
            None if Instant::now() >= deadline => {
                tree.terminate();
                let _ = child.kill();
                let _ = child.wait(); // reap — never leave a zombie

                // Bounded collect, not a join and not a drop: dropping detaches threads that
                // still own the read ends, so an escaped writer never gets EPIPE; joining
                // would hand that writer the caller's thread forever. Log if one cannot be
                // reclaimed.
                let until = Instant::now() + DRAIN_GRACE;
                let (out, err) = (collect(&out_rx, until), collect(&err_rx, until));
                if out.is_none() || err.is_none() {
                    stuck_reader(cmd, "killed at its budget");
                }
                return Err(timed_out(cmd, budget));
            }
            None => std::thread::sleep(POLL),
        }
    };
    // Both halves or none: a caller parsing half a `pw-dump` takes a silently wrong answer,
    // and already has a failure path for a helper that did not answer.
    let until = Instant::now() + DRAIN_GRACE;
    let (Some(stdout), Some(stderr)) = (collect(&out_rx, until), collect(&err_rx, until)) else {
        stuck_reader(cmd, "exited");
        let program = cmd.get_program().to_string_lossy().to_string();
        return Err(Error::new(
            ErrorKind::TimedOut,
            format!(
                "`{program}` exited but its output could not be drained within {DRAIN_GRACE:?}"
            ),
        ));
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Read one of a child's pipes on its own thread so the child never blocks in `write()`,
/// and hand the result back over a channel — not a `JoinHandle`, because the caller must
/// be able to give up on a reader it cannot unblock (see [`DRAIN_GRACE`]). A read error
/// yields the partial buffer; the caller's failure signal is the budget, not a short pipe.
fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = pipe {
            let mut r = r.take(DRAIN_CAP);
            let _ = r.read_to_end(&mut buf);
            if buf.len() as u64 >= DRAIN_CAP {
                tracing::warn!(
                    cap_bytes = DRAIN_CAP,
                    "a helper outran the drain cap — its output is truncated here, which the \
                     caller sees as an unparseable answer (i.e. a failed query)"
                );
            }
        }
        // Send fails only when the call already returned (timeout or grace ran out).
        let _ = tx.send(buf);
    });
    rx
}

/// Take one drained pipe, waiting no longer than `until`. `None` means the reader is still
/// parked on a write end nothing we can signal is holding open.
fn collect(rx: &Receiver<Vec<u8>>, until: Instant) -> Option<Vec<u8>> {
    match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok(buf) => Some(buf),
        // The reader panicked: that loses its half of the output, never the call.
        Err(RecvTimeoutError::Disconnected) => Some(Vec::new()),
        Err(RecvTimeoutError::Timeout) => None,
    }
}

/// A stuck reader is a leaked thread: there is no portable way to unblock a thread already
/// inside `read()` on a pipe (`dup2` does not retarget a read in flight; closing the fd
/// under the thread is a use-after-free waiting for reuse). It ends when the escaped
/// writer closes or [`DRAIN_CAP`] is reached.
fn stuck_reader(cmd: &Command, what: &str) {
    tracing::warn!(
        program = %cmd.get_program().to_string_lossy(),
        grace_ms = DRAIN_GRACE.as_millis() as u64,
        "helper {what} but its pipes never reached EOF — something it started is outside our \
         process group and still holds the write end (`systemd-run --pipe` is the known case). \
         The call is bounded; the reader thread is detached until that writer closes."
    );
}

fn timed_out(cmd: &Command, budget: Duration) -> Error {
    let program = cmd.get_program().to_string_lossy().to_string();
    tracing::warn!(
        program,
        budget_ms = budget.as_millis() as u64,
        "helper did not exit within its budget — killed it (a wedged compositor/session bus is the \
         usual cause); treating it as a failed query"
    );
    Error::new(
        ErrorKind::TimedOut,
        format!("`{program}` did not exit within {budget:?}"),
    )
}

/// Sets its flag on drop: the keepalive whose drop stops the worker thread parked on the flag.
pub(crate) struct StopFlag(pub(crate) std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for StopFlag {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The calling process's real uid.
///
/// Session/gamescope lookups that derive `/run/user/<uid>` (or filter `/proc` to "our"
/// processes) all need this. `getuid()` is parameterless, always succeeds, and touches no
/// memory, so there is no contract for a caller to uphold — one `unsafe` here, none at the
/// call sites.
#[cfg(target_os = "linux")]
pub(crate) fn current_uid() -> u32 {
    // SAFETY: parameterless POSIX call that always succeeds and touches no memory — it just
    // returns the calling process's real uid. Nothing is aliased, read, or freed.
    unsafe { libc::getuid() }
}

/// Whether `pid` is running: in `/proc` and not a zombie. An exited child keeps its `/proc`
/// entry until reaped, so presence alone reads a crashed one as alive.
#[cfg(target_os = "linux")]
pub(crate) fn pid_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| stat_alive(&stat))
}

/// The parent of `pid`, from `/proc/<pid>/stat`. `None` once it is gone.
#[cfg(target_os = "linux")]
pub(crate) fn ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat_fields(&stat)?.nth(1)?.parse().ok()
}

/// The `/proc/<pid>/stat` fields after `comm`, from the state letter on. `comm` is
/// parenthesized and may hold `)`: the fields follow the last one.
#[cfg(target_os = "linux")]
fn stat_fields(stat: &str) -> Option<std::str::SplitWhitespace<'_>> {
    Some(stat.rsplit_once(')')?.1.split_whitespace())
}

#[cfg(target_os = "linux")]
fn stat_alive(stat: &str) -> bool {
    let state = stat_fields(stat).and_then(|mut f| f.next()?.chars().next());
    state.is_some_and(|s| !matches!(s, 'Z' | 'X' | 'x'))
}

/// Every `/proc/<pid>` directory, as `(pid, path)`.
#[cfg(target_os = "linux")]
pub(crate) fn pids() -> impl Iterator<Item = (u32, std::path::PathBuf)> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((name.parse().ok()?, e.path()))
        })
}

/// [`pids`] owned by this process's uid.
#[cfg(target_os = "linux")]
pub(crate) fn own_pids() -> impl Iterator<Item = (u32, std::path::PathBuf)> {
    use std::os::unix::fs::MetadataExt;
    let uid = current_uid();
    pids().filter(move |(_, path)| std::fs::metadata(path).is_ok_and(|md| md.uid() == uid))
}

/// The display keys of one process's environment. An empty value is absent.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayEnv {
    pub display: Option<String>,
    pub xauthority: Option<String>,
    pub wayland: Option<String>,
    /// `GAMESCOPE_WAYLAND_DISPLAY`: set only inside a gamescope session.
    pub gamescope_wayland: Option<String>,
}

impl DisplayEnv {
    /// Parse a NUL-separated `/proc/<pid>/environ` block.
    pub(crate) fn parse(raw: &[u8]) -> DisplayEnv {
        let mut env = DisplayEnv::default();
        for kv in raw.split(|&b| b == 0) {
            let kv = String::from_utf8_lossy(kv);
            let Some((key, value)) = kv.split_once('=') else {
                continue;
            };
            let slot = match key {
                "DISPLAY" => &mut env.display,
                "XAUTHORITY" => &mut env.xauthority,
                "WAYLAND_DISPLAY" => &mut env.wayland,
                "GAMESCOPE_WAYLAND_DISPLAY" => &mut env.gamescope_wayland,
                _ => continue,
            };
            if !value.is_empty() {
                *slot = Some(value.to_string());
            }
        }
        env
    }
}

/// [`DisplayEnv`] of the process at `pid_dir` (a `/proc/<pid>` directory). `None` when its
/// `environ` is unreadable: another uid, or gone.
#[cfg(target_os = "linux")]
pub(crate) fn display_env(pid_dir: &std::path::Path) -> Option<DisplayEnv> {
    std::fs::read(pid_dir.join("environ"))
        .ok()
        .map(|raw| DisplayEnv::parse(&raw))
}

/// The longest `/proc/<pid>/comm` the kernel will report: `TASK_COMM_LEN` is 16 *including* the
/// NUL, so a name of exactly this many bytes may be a truncation of a longer one.
#[cfg(target_os = "linux")]
const COMM_MAX: usize = 15;

/// The executable name to identify a process by, with nixpkgs wrapper decoration undone.
///
/// `comm` is the kernel's name for the **executed file**, truncated to [`COMM_MAX`] — not
/// `argv[0]`. nixpkgs `wrapProgram` moves the real ELF to `.<name>-wrapped` and `exec -a
/// "$0"`s it, so `comm` is `.kwin_wayland-w` while argv still says `kwin_wayland`.
///
/// Fast path: an undecorated name shorter than [`COMM_MAX`] is already the answer (one
/// read, no readlink). Otherwise resolve `/proc/<pid>/exe`, then `argv[0]` if the kernel
/// refused the link. `exe` is gated by `cap_ptrace_access_check`: a compositor holding
/// `cap_sys_nice` (NixOS `security.wrappers`) returns EACCES to an uncapped reader of the
/// same uid. `argv[0]` is the process's own claim — a same-uid process can set it to
/// anything — so it is consulted only when the kernel has refused the authoritative
/// answer. Worst case a spoof aims detection at a backend that then fails its own probe.
///
/// `pid_path` is a `/proc/<pid>` directory. `None` when the process vanished mid-scan.
#[cfg(target_os = "linux")]
pub(crate) fn match_name(pid_path: &std::path::Path) -> Option<String> {
    let comm = std::fs::read_to_string(pid_path.join("comm")).ok()?;
    let comm = comm.trim();
    if !comm.starts_with('.') && comm.len() < COMM_MAX {
        return Some(comm.to_string());
    }
    // Kernel's record of the executed file, untruncated. Absent for a kernel thread or a
    // process exiting under us; EACCES for a capability-holding one.
    let exe = std::fs::read_link(pid_path.join("exe")).ok();
    if let Some(full) = exe
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
    {
        return Some(undecorate(full).to_string());
    }
    // Refused or gone: fall back to what the process calls itself, then to truncated `comm`.
    match argv0_name(pid_path) {
        Some(name) => {
            tracing::debug!(
                comm = %comm,
                resolved = %name,
                "/proc/<pid>/exe unreadable (a capability-holding process refuses it); \
                 identified via argv[0]"
            );
            Some(name)
        }
        None => Some(comm.to_string()),
    }
}

/// The file name in `argv[0]`, with nixpkgs decoration undone — last rung of [`match_name`].
///
/// `/proc/<pid>/cmdline` is NUL-separated, so the first field is `argv[0]` whole. `None`
/// when unreadable or empty (kernel thread, zombie).
#[cfg(target_os = "linux")]
fn argv0_name(pid_path: &std::path::Path) -> Option<String> {
    let raw = std::fs::read(pid_path.join("cmdline")).ok()?;
    let argv0 = raw.split(|b| *b == 0).next()?;
    // setproctitle can leave anything here, including a non-path. `file_name` yields it
    // unchanged and it fails to match any compositor name, which is the correct outcome.
    let argv0 = std::str::from_utf8(argv0).ok()?;
    let name = std::path::Path::new(argv0).file_name()?.to_str()?;
    (!name.is_empty()).then(|| undecorate(name).to_string())
}

/// Strip nixpkgs `wrapProgram` decoration: `.<name>-wrapped`, plus the `_` suffixes
/// make-wrapper appends when that hidden name is already taken (Qt *and* GApps wrap).
///
/// Both halves are required: KWin ships a real binary `kwin_wayland_wrapper` (the
/// session's parent). Stripping a `wrapper`-ish suffix alone would rewrite it to
/// `kwin_wayland` and hand the session probe the wrong PID. The leading `.` keeps it.
#[cfg(target_os = "linux")]
fn undecorate(name: &str) -> &str {
    let Some(rest) = name.strip_prefix('.') else {
        return name;
    };
    match rest.trim_end_matches('_').strip_suffix("-wrapped") {
        Some(real) if !real.is_empty() => real,
        _ => name,
    }
}

/// Ending the *tree* the helper started, not just the process we spawned.
///
/// [`std::process::Child::kill`] is one `TerminateProcess`: the process we launched.
/// On Windows every helper is reached through a shell (`cmd /c …`), so the process
/// that hangs is a **grandchild**. Killing the shell leaves it holding our stdio and
/// working directory — the budget has not bounded anything.
///
/// A Job object is the mechanism: membership is inherited across `CreateProcess`, so
/// assigning the child enrolls every descendant, and one `TerminateJobObject` ends
/// them all. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` makes that hold on paths that never
/// reach [`Guard::terminate`] — an early `?`, a panic — because closing the last
/// handle to the job is itself the kill.
///
/// Best-effort: if the job cannot be created or the child cannot be assigned, degrade
/// to the single-process kill rather than failing the query (same as ignoring
/// `Child::kill`).
#[cfg(windows)]
mod tree {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Nothing to arrange before spawn: job membership is assigned to the live process.
    pub(super) fn prepare(_cmd: &mut std::process::Command) {}

    /// Owns a Job object holding the spawned helper and everything it spawns. `None`
    /// when the job could not be set up (see the module doc: degrade, don't fail).
    pub(super) struct Guard(Option<HANDLE>);

    impl Guard {
        /// Enroll `child` — and, transitively, its descendants — in a fresh kill-on-close job.
        pub(super) fn attach(child: &Child) -> Self {
            // SAFETY: an unnamed job object with default security attributes; no pointer is passed
            // and none is retained. The handle it yields is owned by the `Guard` constructed from
            // it on the next line and closed exactly once, in that `Guard`'s `Drop`.
            let job = match unsafe { CreateJobObjectW(None, None) } {
                Ok(job) => job,
                Err(e) => {
                    tracing::debug!(error = %e, "no job object for this helper — a hung one's \
                         grandchildren will outlive its budget");
                    return Self(None);
                }
            };
            // Owned from here on, so both fallible steps below can bail without leaking it.
            let guard = Self(Some(job));
            if let Err(e) = guard.enroll(child) {
                tracing::debug!(error = %e, "could not enroll this helper in its job object — a \
                     hung one's grandchildren will outlive its budget");
            }
            guard
        }

        fn enroll(&self, child: &Child) -> windows::core::Result<()> {
            let Some(job) = self.0 else { return Ok(()) };
            let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
                BasicLimitInformation:
                    windows::Win32::System::JobObjects::JOBOBJECT_BASIC_LIMIT_INFORMATION {
                        LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                        ..Default::default()
                    },
                ..Default::default()
            };
            // SAFETY: `job` is the live handle this `Guard` owns. The pointer is to a fully
            // initialised local of exactly the type `JobObjectExtendedLimitInformation` selects,
            // passed with that type's own size, and the kernel copies the limits out before
            // returning — `info` is not retained past the call.
            unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )?;
            }
            // SAFETY: `job` is the live handle this `Guard` owns. `child` is borrowed for the call,
            // so the process handle it lends is open and stays open for the duration — `Child`
            // closes it only in its own `Drop`. The kernel duplicates what it needs; we keep
            // ownership of both handles.
            unsafe { AssignProcessToJobObject(job, HANDLE(child.as_raw_handle())) }
        }

        /// End every process still in the job. A no-op once they have all exited, so this
        /// is safe on the success path as well as the timeout one.
        pub(super) fn terminate(&self) {
            let Some(job) = self.0 else { return };
            // SAFETY: `job` is this `Guard`'s live, owned handle — it is closed only in `Drop`,
            // which cannot have run while `&self` is borrowed. The exit code is arbitrary; nothing
            // reads it, because a killed tree is reported to callers as the budget's `TimedOut`.
            let _ = unsafe { TerminateJobObject(job, 1) };
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let Some(job) = self.0.take() else { return };
            // SAFETY: `job` is the handle created in `attach` and owned solely by this `Guard`;
            // `take` makes this the one and only close. Per the module doc this close is also the
            // backstop kill — with KILL_ON_JOB_CLOSE set, dropping the last handle terminates
            // whatever is still in the job, so no early return can leak the tree.
            let _ = unsafe { CloseHandle(job) };
        }
    }
}

/// The Unix half — a **process group**, which is what Unix offers in place of a Job object.
///
/// Most Linux helpers here are a single exec (`kscreen-doctor`, `pw-dump`, `hyprctl`), but
/// `systemd-run --user` / `systemctl --user` work through the user manager, which forks
/// the actual process. `Child::kill` cannot reach it. With [`output_within`]'s readers
/// waiting until every write end closes, one surviving relative keeps a "bounded" call
/// going.
///
/// [`prepare`] puts the child in a new process group (it is the leader; the group id is
/// its pid) and [`Guard::terminate`] `killpg`s that group. `process_group` changes only
/// the group, not the session, so the helper keeps its controlling terminal and login
/// session — anything doing a logind/polkit lookup depends on that. A process the **user
/// manager** forks (`systemd-run --pipe`) is in another group and session, so `killpg`
/// misses it; [`DRAIN_GRACE`] bounds the reader. `pkexec` for the DM helper does not come
/// through this module: it calls `Command::output()` and is documented as unbounded,
/// because a `stop`/`restore` verb legitimately takes seconds.
///
/// Best-effort as on Windows: a failed `killpg` is ignored; the single-process
/// `Child::kill` on the timeout path still runs.
#[cfg(not(windows))]
mod tree {
    use std::os::unix::process::CommandExt;

    /// The child's process-group id, captured while the child is still ours to reap.
    pub(super) struct Guard(Option<i32>);

    /// Make the child the leader of its own process group, so its descendants are reachable as one.
    pub(super) fn prepare(cmd: &mut std::process::Command) {
        cmd.process_group(0);
    }

    impl Guard {
        pub(super) fn attach(child: &std::process::Child) -> Self {
            // `prepare` asked for `process_group(0)`, so the group id IS the child's pid.
            Self(i32::try_from(child.id()).ok())
        }

        /// End every process still in the group. A no-op once they have all exited, so this
        /// is safe on the success path as well as the timeout one.
        pub(super) fn terminate(&self) {
            let Some(pgid) = self.0 else { return };
            // `killpg` names the group we created. Pid reuse between reap and this call
            // would need a new group leader on the same pid; Linux allocates sequentially,
            // so there is no window. Not killing is the unbounded wait this module prevents.

            // SAFETY: a plain signal send by group id. No pointer is passed, nothing is aliased,
            // and the result is deliberately ignored — ESRCH just means the group is already gone.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

// `unix` gate, not `test` alone: this module is compiled on every platform (lib.rs
// declares it unconditionally), but the cases spawn `sleep`/`true`/`echo` as
// EXECUTABLES. On Windows `echo` is a shell builtin and there is no `sleep.exe`.
// The Windows twin is below.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_hung_child_is_killed_at_the_budget() {
        let started = Instant::now();
        let err = status_within(Command::new("sleep").arg("30"), Duration::from_millis(150))
            .expect_err("must time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must return at its budget, not the child's lifetime (took {:?})",
            started.elapsed()
        );
    }

    /// A helper whose output exceeds one pipe buffer must still be captured in full.
    ///
    /// Against wait-then-read, the child blocks in `write()` with the pipe full, never
    /// exits, and the budget turns a successful query into `TimedOut`. 1 MiB is ~16× a
    /// Linux pipe (64 KiB) and ~64× the smallest macOS one, so a buffer cannot absorb it.
    #[test]
    fn a_child_that_outruns_the_pipe_buffer_is_captured_in_full() {
        const BYTES: usize = 1024 * 1024;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(format!("yes punktfunk | head -c {BYTES}; echo done >&2"));
        let out = output_within(&mut cmd, Duration::from_secs(20)).expect("must not time out");
        assert!(out.status.success(), "helper failed: {:?}", out.status);
        assert_eq!(out.stdout.len(), BYTES, "stdout was truncated");
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "done");
    }

    /// A helper that exits while a background child still holds the pipe must not park
    /// the caller. Ending the process group closes the grandchild's write end.
    /// [`DRAIN_GRACE`] bounds collection so even a process outside the group detaches a
    /// reader instead of parking the caller.
    #[test]
    fn a_grandchild_holding_the_pipe_does_not_park_the_caller() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & echo punktfunk");
        let out = output_within(&mut cmd, Duration::from_secs(10)).expect("the helper exited");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "punktfunk");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the call waited on the grandchild's EOF (took {:?})",
            started.elapsed()
        );
    }

    /// A kept gamescope's `Child` stays unreaped until the pool drops it, so a crashed one is a
    /// zombie. It must read as dead, or keep-alive hands the corpse to the next client.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_unreaped_child_is_not_alive() {
        let mut child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(pid_alive(pid), "a running child must read as alive");
        child.kill().expect("kill");
        // No `wait` yet: the zombie is the case under test. SIGKILL lands asynchronously.
        let deadline = Instant::now() + Duration::from_secs(5);
        while pid_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        let zombie_alive = pid_alive(pid);
        let _ = child.wait();
        assert!(
            !zombie_alive,
            "an exited, unreaped child still reads as alive"
        );
    }

    #[test]
    fn a_quick_child_returns_normally() {
        let st = status_within(&mut Command::new("true"), Duration::from_secs(5)).expect("ran");
        assert!(st.success());

        let out = output_within(
            Command::new("echo").arg("punktfunk"),
            Duration::from_secs(5),
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "punktfunk");
    }
}

/// `comm`-vs-real-name resolution ([`match_name`]). Linux-only: `comm` names the executed
/// file, truncated to 15 bytes, crossed with nixpkgs `wrapProgram`.
///
/// Driven against fixture `/proc/<pid>` directories rather than spawned processes: a
/// stand-in has to be a real ELF that tolerates being *renamed*, and multi-call
/// `/bin/sleep` (uutils, busybox) is not one — copied to `.kwin_wayland-wrapped` it
/// prints "unknown program" and exits. The truncated `comm` strings below were measured
/// from a live kernel against binaries exec'd the way nixpkgs does it.
#[cfg(all(test, target_os = "linux"))]
mod name_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// A fake `/proc/<pid>` directory: a `comm` file and, optionally, the `exe` symlink
    /// and a `cmdline`. Removed on drop.
    ///
    /// An absent `exe` stands in for both ways the real link yields nothing: a process
    /// exiting under the scan, and a capability-holding one the kernel refuses with
    /// EACCES. `match_name` cannot tell those apart and does not need to.
    struct FakePid {
        dir: PathBuf,
    }

    impl FakePid {
        /// `comm` is written exactly as the kernel would report it — already truncated.
        fn new(tag: &str, comm: &str, exe: Option<&str>) -> FakePid {
            FakePid::with_cmdline(tag, comm, exe, None)
        }

        /// `cmdline` is the NUL-separated argument vector; the fixture is given just
        /// `argv[0]` and appends the terminator.
        fn with_cmdline(tag: &str, comm: &str, exe: Option<&str>, argv0: Option<&str>) -> FakePid {
            let dir = std::env::temp_dir().join(format!("pf-vd-name-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            std::fs::write(dir.join("comm"), format!("{comm}\n")).expect("comm");
            if let Some(exe) = exe {
                // The target need not exist: `read_link` reports the link's contents, and a real
                // `/proc/<pid>/exe` routinely points at a path that has since been replaced.
                std::os::unix::fs::symlink(
                    format!("/nix/store/eeee-kwin-6.5.0/bin/{exe}"),
                    dir.join("exe"),
                )
                .expect("exe symlink");
            }
            if let Some(argv0) = argv0 {
                std::fs::write(dir.join("cmdline"), format!("{argv0}\0--session\0")).expect("cmd");
            }
            FakePid { dir }
        }
        fn path(&self) -> &Path {
            &self.dir
        }
    }

    impl Drop for FakePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// The `kwin_wayland_wrapper` rows earn their keep: it is a real KWin binary (the
    /// session's parent), so the rule must leave it under its own name in both plain and
    /// wrapped form rather than collapsing either into `kwin_wayland`.
    #[test]
    fn undecorate_strips_only_a_real_nixpkgs_wrapper() {
        for (raw, want) in [
            (".kwin_wayland-wrapped", "kwin_wayland"),
            (".gamescope-wrapped", "gamescope"),
            (".gnome-shell-wrapped", "gnome-shell"),
            (".Hyprland-wrapped", "Hyprland"),
            // make-wrapper appends `_`s when the hidden name is already taken (Qt + GApps
            // double-wrap), so the underscores come off before the suffix does.
            (".kwin_wayland-wrapped_", "kwin_wayland"),
            (".kwin_wayland-wrapped__", "kwin_wayland"),
            // Not decoration — every one of these keeps its exact name.
            ("kwin_wayland", "kwin_wayland"),
            ("kwin_wayland_wrapper", "kwin_wayland_wrapper"),
            (".kwin_wayland_wrapper-wrapped", "kwin_wayland_wrapper"),
            ("foo-wrapped", "foo-wrapped"),
            (".hidden", ".hidden"),
            (".-wrapped", ".-wrapped"),
        ] {
            assert_eq!(undecorate(raw), want, "undecorate({raw:?})");
        }
    }

    /// Every compositor the session probe matches is wrapped by nixpkgs, so the kernel
    /// reports a truncated, decorated `comm` that can never equal the name being compared.
    #[test]
    fn a_nixpkgs_wrapped_compositor_resolves_to_its_real_name() {
        for (tag, comm, exe, want) in [
            (
                "kwin",
                ".kwin_wayland-w",
                ".kwin_wayland-wrapped",
                "kwin_wayland",
            ),
            (
                "gamescope",
                ".gamescope-wrap",
                ".gamescope-wrapped",
                "gamescope",
            ),
            (
                "gnome",
                ".gnome-shell-wr",
                ".gnome-shell-wrapped",
                "gnome-shell",
            ),
            ("hypr", ".Hyprland-wrapp", ".Hyprland-wrapped", "Hyprland"),
        ] {
            let p = FakePid::new(tag, comm, Some(exe));
            assert_eq!(
                match_name(p.path()).as_deref(),
                Some(want),
                "a nixpkgs-wrapped {want} must resolve to the name the session probe matches"
            );
        }
    }

    /// KWin's own `kwin_wayland_wrapper` runs *alongside* `kwin_wayland`, and its wrapped
    /// `comm` (`.kwin_wayland_w`) differs by one byte. It must not resolve to
    /// `kwin_wayland`: the probe would match the parent PID as the compositor identity.
    #[test]
    fn kwins_own_wrapper_binary_does_not_masquerade_as_the_compositor() {
        let p = FakePid::new(
            "kwrap",
            ".kwin_wayland_w",
            Some(".kwin_wayland_wrapper-wrapped"),
        );
        assert_eq!(
            match_name(p.path()).as_deref(),
            Some("kwin_wayland_wrapper")
        );
    }

    /// A long name is truncated too, with no nix involved, and has to be recovered from
    /// `exe` rather than matched short.
    #[test]
    fn a_long_name_is_recovered_untruncated() {
        let p = FakePid::new(
            "long",
            "a-very-long-com",
            Some("a-very-long-compositor-name"),
        );
        assert_eq!(
            match_name(p.path()).as_deref(),
            Some("a-very-long-compositor-name")
        );
    }

    /// The fast path answers without consulting `exe` — one read per process on an
    /// ordinary distro, and an answer for a process whose `exe` is unreadable.
    #[test]
    fn an_ordinary_short_name_never_needs_the_exe_link() {
        let p = FakePid::new("plain", "kwin_wayland", None);
        assert_eq!(match_name(p.path()).as_deref(), Some("kwin_wayland"));
    }

    /// A decorated-or-truncated name with neither `exe` nor `cmdline` (kernel thread, or
    /// a process exiting under the scan) degrades to truncated `comm` instead of failing
    /// the entry.
    #[test]
    fn an_unreadable_exe_falls_back_to_comm() {
        let p = FakePid::new("noexe", ".kwin_wayland-w", None);
        assert_eq!(match_name(p.path()).as_deref(), Some(".kwin_wayland-w"));
    }

    /// nixpkgs wraps the binary so `comm` is `.kwin_wayland-w` and only `exe` carries the
    /// real name; NixOS `security.wrappers` hands KWin `cap_sys_nice+ep`, so the kernel
    /// refuses that link to an uncapped reader. `argv[0]` survives because make-wrapper
    /// `exec -a "$0"`s the hidden binary.
    #[test]
    fn a_capped_wrapped_compositor_is_identified_by_argv0() {
        for (tag, comm, argv0, want) in [
            // Absolute path, as Plasma's session execs it.
            (
                "capkwin",
                ".kwin_wayland-w",
                "/run/wrappers/bin/kwin_wayland",
                "kwin_wayland",
            ),
            ("capbare", ".kwin_wayland-w", "kwin_wayland", "kwin_wayland"),
            // gamescope often carries `cap_sys_nice` even when not wrapped.
            (
                "capgame",
                ".gamescope-wrap",
                "/nix/store/aaaa-gamescope/bin/gamescope",
                "gamescope",
            ),
            // Hidden path as `argv[0]` still undecorates.
            (
                "capraw",
                ".kwin_wayland-w",
                "/nix/store/eeee-kwin/bin/.kwin_wayland-wrapped",
                "kwin_wayland",
            ),
        ] {
            let p = FakePid::with_cmdline(tag, comm, None, Some(argv0));
            assert_eq!(
                match_name(p.path()).as_deref(),
                Some(want),
                "a capped, wrapped {want} must still be identified from argv[0]"
            );
        }
    }

    /// `exe` outranks `argv[0]` whenever the kernel allows it: `argv[0]` is the process's
    /// own claim and a same-uid process can set it to anything, so it may never override
    /// the kernel's answer — only stand in when there is none.
    #[test]
    fn a_readable_exe_outranks_a_lying_argv0() {
        let p = FakePid::with_cmdline(
            "liar",
            ".gamescope-wrap",
            Some(".gamescope-wrapped"),
            Some("kwin_wayland"),
        );
        assert_eq!(match_name(p.path()).as_deref(), Some("gamescope"));
    }

    /// A zombie's `cmdline` is empty, and `argv[0]` can be an empty string even when it
    /// is not — neither may yield an empty name (which would then be compared against a
    /// compositor name).
    #[test]
    fn an_empty_cmdline_does_not_produce_a_name() {
        for (tag, cmdline) in [("zombie", ""), ("nulls", "\0\0")] {
            let dir = std::env::temp_dir().join(format!("pf-vd-name-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("fixture dir");
            std::fs::write(dir.join("comm"), ".kwin_wayland-w\n").expect("comm");
            std::fs::write(dir.join("cmdline"), cmdline).expect("cmdline");
            assert_eq!(match_name(&dir).as_deref(), Some(".kwin_wayland-w"));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_vanished_process_yields_none() {
        assert_eq!(match_name(Path::new("/proc/0")), None);
    }

    /// Reading `/proc/<pid>/exe` is permitted for a process of our own uid. Checked
    /// against the only such process guaranteed to be running — this one. It holds because
    /// *we* are uncapped, and says nothing about the processes being scanned: a capped
    /// target refuses this same link, which is why [`match_name`] has an `argv[0]` rung.
    #[test]
    fn our_own_exe_link_is_readable() {
        let me = Path::new("/proc/self");
        let exe = std::fs::read_link(me.join("exe"))
            .expect("/proc/self/exe must be readable for our own uid");
        let name = exe.file_name().and_then(|n| n.to_str()).expect("exe name");
        let got = match_name(me).expect("our own name");
        // Whichever rung answered must agree with the real binary: the fast path returns
        // the (short, undecorated) comm, a prefix of it; the exe path returns it outright.
        assert!(
            name.starts_with(got.as_str()) || got == name,
            "resolved {got:?} disagrees with our real binary {name:?}"
        );
    }
}

/// The same two cases through `cmd /c`, so the budget logic is covered on the platform
/// whose process model differs most (job objects, no `SIGKILL`). `ping -n` is the
/// standard Windows no-extra-tooling sleep.
#[cfg(all(test, windows))]
mod tests_windows {
    use super::*;

    #[test]
    fn a_hung_child_is_killed_at_the_budget() {
        let started = Instant::now();
        let err = status_within(
            Command::new("cmd").args(["/c", "ping -n 60 127.0.0.1 >NUL"]),
            Duration::from_millis(150),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must return at its budget, not the child's lifetime (took {:?})",
            started.elapsed()
        );
    }

    #[test]
    fn a_quick_child_returns_normally() {
        let st = status_within(
            Command::new("cmd").args(["/c", "exit 0"]),
            Duration::from_secs(5),
        )
        .expect("ran");
        assert!(st.success());

        let out = output_within(
            Command::new("cmd").args(["/c", "echo punktfunk"]),
            Duration::from_secs(5),
        )
        .expect("ran");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "punktfunk");
    }

    /// A scratch directory for the tree test's marker files, removed on drop.
    /// `remove_dir_all` is un-asserted: if the tree *did* survive it still has this
    /// directory as its working directory and the removal fails — which the test reports.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("pf-vd-proc-tree-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The budget must end the whole tree, not just the process we spawned.
    ///
    /// Every Windows helper is reached through a shell, so the process that hangs is a
    /// grandchild `Child::kill` cannot see. Left alive it keeps our stdio handles and
    /// working directory.
    #[test]
    fn the_budget_ends_the_whole_tree_not_just_the_child() {
        let scratch = Scratch::new();
        let ran = scratch.0.join("grandchild-ran");
        let survived = scratch.0.join("grandchild-survived");
        let script = scratch.0.join("grandchild.cmd");
        // `%~dp0` — the script's own directory, resolved by cmd at run time — rather than
        // paths interpolated in. A .cmd file is read in the OEM code page, so an absolute
        // path baked into it is mangled the moment the temp dir contains a non-ASCII
        // character and every redirect fails with "path not found".
        std::fs::write(
            &script,
            format!(
                "@echo off\r\n\
                 echo up>\"%~dp0{}\"\r\n\
                 ping -n 4 127.0.0.1 >NUL\r\n\
                 echo up>\"%~dp0{}\"\r\n",
                ran.file_name().expect("marker name").to_string_lossy(),
                survived.file_name().expect("marker name").to_string_lossy(),
            ),
        )
        .expect("write the grandchild script");

        // `cmd /c cmd /c <script>`: the OUTER cmd is our child, the inner one is the
        // grandchild that `Child::kill` alone would leave running.
        let err = status_within(
            Command::new("cmd").args(["/c", "cmd", "/c", &script.display().to_string()]),
            Duration::from_millis(2500),
        )
        .expect_err("must time out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);

        // Without this the test could pass vacuously — a grandchild killed before it
        // ever started proves nothing about killing trees.
        assert!(
            ran.exists(),
            "the grandchild never got as far as its first marker, so this run proves nothing"
        );

        // Outlast the grandchild's own sleep: a surviving tree writes the second marker.
        std::thread::sleep(Duration::from_secs(5));
        assert!(
            !survived.exists(),
            "a grandchild outlived the budget — the shell was killed but not the tree under it"
        );
    }
}

#[cfg(test)]
mod env_tests {
    use super::DisplayEnv;

    fn environ(vars: &[&str]) -> Vec<u8> {
        vars.join("\0").into_bytes()
    }

    #[test]
    fn each_display_key_lands_in_its_own_field() {
        let env = DisplayEnv::parse(&environ(&[
            "HOME=/home/u",
            "WAYLAND_DISPLAY=wayland-kde",
            "DISPLAY=:0",
            "XAUTHORITY=/run/user/1000/xauth_ab",
            "GAMESCOPE_WAYLAND_DISPLAY=gamescope-0",
        ]));
        assert_eq!(env.display.as_deref(), Some(":0"));
        assert_eq!(env.xauthority.as_deref(), Some("/run/user/1000/xauth_ab"));
        assert_eq!(env.wayland.as_deref(), Some("wayland-kde"));
        assert_eq!(env.gamescope_wayland.as_deref(), Some("gamescope-0"));
    }

    /// An empty `GAMESCOPE_WAYLAND_DISPLAY=` names no gamescope session, for every reader.
    #[test]
    fn an_empty_value_is_absent() {
        let env = DisplayEnv::parse(&environ(&["DISPLAY=", "GAMESCOPE_WAYLAND_DISPLAY="]));
        assert_eq!(env, DisplayEnv::default());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod stat_tests {
    use super::{stat_alive, stat_fields};

    #[test]
    fn the_state_follows_the_last_paren_of_comm() {
        assert!(!stat_alive("9846 (gamescope-wl) Z 837 9846 9846 0 -1"));
        assert!(stat_alive("77 (odd) name)) S 1 77 77 0 -1"));
        assert!(!stat_alive("garbage"));
        let ppid = stat_fields("77 (odd) name)) S 1 77").and_then(|mut f| f.nth(1));
        assert_eq!(ppid, Some("1"));
    }

    /// `X` is a task being torn down: gone for every caller, a Steam wait included.
    #[test]
    fn a_dead_task_is_not_alive() {
        assert!(!stat_alive("12 (steam) X 1 12 12 0 -1"));
        assert!(!stat_alive("12 (steam) x 1 12 12 0 -1"));
    }
}
