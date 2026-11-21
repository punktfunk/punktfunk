//! Per-thread OS scheduling QoS for the data plane.
//!
//! Capture/encode, send and the input path raise their own priority so a CPU-saturating
//! game cannot deschedule them. Native, GameStream, and direct-NVENC send threads
//! all call [`boost_thread_priority`]; the host's runtime threads take [`raise_pool_thread`].
//!
//! On Linux a thread's nice inherits to the threads it spawns, except when RealtimeKit
//! raised it: rtkit sets `SCHED_RESET_ON_FORK`, so a child starts at nice 0. A helper
//! thread or process on the frame path therefore calls [`boost_thread_priority`] itself.

/// Raise this thread so a CPU-heavy game cannot deschedule capture/encode/send.
///
/// GPU HIGH only favours commands already submitted; a descheduled
/// normal-priority thread submits late and the GPU priority never bites.
/// `critical` is highest non-realtime (capture+encode); otherwise above-normal
/// (send/relay, input). On Linux the thread's timer slack also drops to 1 ns, so a timed
/// wait ends on time rather than up to 50 µs late; its children inherit that. macOS takes the
/// user-interactive QoS class for `critical`, user-initiated otherwise; threads it spawns
/// inherit the class.
pub fn boost_thread_priority(critical: bool) {
    crate::session_tuning::on_hot_thread();
    #[cfg(target_os = "windows")]
    // SAFETY: `GetCurrentThread()` returns the constant pseudo-handle for the calling thread — always
    // valid, thread-local in meaning, and never closed (no leak/double-close). `SetThreadPriority`
    // takes that handle plus a `THREAD_PRIORITY_*` value the windows crate defines (HIGHEST or
    // ABOVE_NORMAL here); it only reprioritizes this OS thread, borrows no Rust memory, and its
    // `Result` is matched (a failure is logged, never UB). No pointers, lifetimes, or aliasing.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
            THREAD_PRIORITY_HIGHEST,
        };
        let prio = if critical {
            THREAD_PRIORITY_HIGHEST
        } else {
            THREAD_PRIORITY_ABOVE_NORMAL
        };
        match SetThreadPriority(GetCurrentThread(), prio) {
            Ok(()) => tracing::debug!(critical, "thread priority raised"),
            Err(e) => {
                tracing::debug!(critical, error = ?e, "SetThreadPriority failed")
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        tighten_timer_slack();
        let _ = renice(if critical { -10 } else { -5 }, || true);
    }
    #[cfg(target_os = "macos")]
    {
        use libc::qos_class_t::{QOS_CLASS_USER_INITIATED, QOS_CLASS_USER_INTERACTIVE};
        let class = if critical {
            QOS_CLASS_USER_INTERACTIVE
        } else {
            QOS_CLASS_USER_INITIATED
        };
        // SAFETY: by-value arguments only, and it changes only the calling thread's class.
        let rc = unsafe { libc::pthread_set_qos_class_self_np(class, 0) };
        if rc == 0 {
            tracing::debug!(critical, "thread QoS class raised");
        } else {
            tracing::debug!(critical, rc, "pthread_set_qos_class_self_np refused");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = critical;
    }
}

/// Raise a runtime's pool thread to nice -5, and only that thread (Linux; a no-op elsewhere).
/// For `on_thread_start` of the runtime that carries QUIC datagrams and control messages.
///
/// What a pool thread spawns (hooks, helpers, plugins, threads) starts at nice 0: the thread
/// resets on fork, as RealtimeKit's raise does. Its timer slack keeps the default: the runtime's
/// timers tick in milliseconds, and slack, unlike nice, has no reset and would pass to children.
/// Not a hot thread: Windows' session raises stay tied to the session's own threads. A refused
/// `setpriority` asks RealtimeKit once per process, since its burst limit belongs to the hot
/// threads. A thread that later calls [`boost_thread_priority`] still gets that boost.
pub fn raise_pool_thread() {
    #[cfg(target_os = "linux")]
    {
        static RTKIT_ASKED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if renice(-5, || {
            !RTKIT_ASKED.swap(true, std::sync::atomic::Ordering::Relaxed)
        }) {
            reset_on_fork();
        }
    }
}

/// Children of the calling thread, threads and processes alike, start at nice 0 rather than its
/// raised nice. Needs no privilege; once set, only CAP_SYS_NICE clears it.
#[cfg(target_os = "linux")]
fn reset_on_fork() {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: `sched_setscheduler` reads the live local `param` during the call and keeps no
    // pointer to it. `0` is the calling thread. SCHED_OTHER is its policy already and the kernel
    // keeps its nice, so the call only adds the flag; a refusal leaves the thread as it was.
    let rc = unsafe {
        libc::sched_setscheduler(0, libc::SCHED_OTHER | libc::SCHED_RESET_ON_FORK, &param)
    };
    if rc != 0 {
        let error = std::io::Error::last_os_error();
        tracing::debug!(%error, "reset-on-fork not set: children keep this thread's nice");
    }
}

/// Best-effort nice of the calling thread; RealtimeKit after a refusal when `ask_rtkit` says
/// so. `true` when the direct call took. Never SCHED_RR/FIFO: realtime can preempt the
/// compositor and the game's render thread.
#[cfg(target_os = "linux")]
fn renice(nice: i32, ask_rtkit: impl FnOnce() -> bool) -> bool {
    // SAFETY: `setpriority` takes three by-value integers and no pointers, so there is nothing to
    // alias or outlive. `PRIO_PROCESS` with `who == 0` targets the calling task on Linux and
    // `nice` is in range; the call only adjusts this thread's scheduling nice value and returns an
    // `int` we inspect. No memory is touched.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
    if rc == 0 {
        tracing::debug!(nice, "thread nice raised");
        return true;
    }
    // Direct call needs CAP_SYS_NICE or a raised RLIMIT_NICE. Do not file-cap the
    // host binary: a capped process's /proc/<pid>/exe is unreadable to KWin.
    // RealtimeKit is the unprivileged path; a LimitNICE drop-in only applies at next login.
    if !ask_rtkit() {
        tracing::debug!(
            nice,
            "setpriority(nice) refused, RealtimeKit not asked again"
        );
        return false;
    }
    match linux_rtkit::make_high_priority(nice) {
        Ok(()) => tracing::debug!(nice, "thread nice raised via rtkit"),
        Err(e) => tracing::debug!(
            nice,
            reason = %e,
            "setpriority(nice) no-op (needs CAP_SYS_NICE / RLIMIT_NICE, and rtkit \
             was unavailable)"
        ),
    }
    false
}

/// The kernel's default timer slack, which a program the host launches starts with.
#[cfg(target_os = "linux")]
const DEFAULT_TIMER_SLACK_NS: libc::c_ulong = 50_000;

/// Start `cmd`'s program at nice 0 and the default timer slack, whatever the spawning thread was
/// raised to (Linux). For a program launched into the session: it runs beside the game, never
/// level with capture and encode. The host's own workers are not spawned through this.
#[cfg(target_os = "linux")]
pub fn unraised(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the child between fork and exec, where only async-signal-safe
    // calls are sound. `setpriority` and `prctl` are bare syscalls on by-value integers: no
    // allocation, no lock, no pointer. It captures nothing and returns `Ok` whatever they answer,
    // so a refusal never fails the spawn.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpriority(libc::PRIO_PROCESS, 0, 0);
            libc::prctl(libc::PR_SET_TIMERSLACK, DEFAULT_TIMER_SLACK_NS);
            Ok(())
        })
    }
}

/// The default 50 µs slack lets `sleep`, `poll` and `recv_timeout` end late. 1 ns is the floor.
#[cfg(target_os = "linux")]
fn tighten_timer_slack() {
    // SAFETY: `PR_SET_TIMERSLACK` takes one by-value integer (nanoseconds) and no pointers, so
    // nothing aliases or outlives the call. It sets only the calling thread's slack, needs no
    // privilege, and touches no memory; a refusal leaves the default slack.
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong) };
}

/// `(policy, rt_priority, nice)` for the calling thread.
///
/// A boost we asked for and a boost the hot thread has are different
/// questions: library callbacks (PipeWire `RT_PROCESS`) run on a thread
/// that library created. Report this from inside the hot callback.
#[cfg(target_os = "linux")]
pub fn current_thread_sched() -> (&'static str, i32, i32) {
    // SAFETY: all three calls take by-value integers (plus, for `sched_getparam`, a pointer to a
    // fully-initialised local we own and outlive) and return integers. `0` means "the calling
    // task" on Linux, so nothing outside this thread is read or written, and no allocation,
    // locking or blocking happens — which is what makes this callable from an RT callback.
    unsafe {
        let policy = libc::sched_getscheduler(0);
        let mut param: libc::sched_param = std::mem::zeroed();
        let rt_priority = if libc::sched_getparam(0, &mut param) == 0 {
            param.sched_priority
        } else {
            -1
        };
        // `getpriority` legitimately returns -1, so errno is the only way to tell a nice of -1
        // from a failure.
        *libc::__errno_location() = 0;
        let nice = libc::getpriority(libc::PRIO_PROCESS, 0);
        let nice = if *libc::__errno_location() == 0 {
            nice
        } else {
            0
        };
        let policy = match policy {
            libc::SCHED_FIFO => "SCHED_FIFO",
            libc::SCHED_RR => "SCHED_RR",
            libc::SCHED_OTHER => "SCHED_OTHER",
            libc::SCHED_BATCH => "SCHED_BATCH",
            libc::SCHED_IDLE => "SCHED_IDLE",
            _ => "unknown",
        };
        (policy, rt_priority, nice)
    }
}

/// RealtimeKit fallback: `org.freedesktop.RealtimeKit1` renices the calling
/// thread when `setpriority` was refused. No file capability — a cap on the
/// host binary breaks KWin's client identification.
///
/// High-priority (nice) only, never `MakeThreadRealtime` — [`boost_thread_priority`] avoids
/// SCHED_RR/FIFO, and rtkit-granted RR is the same policy. The RT verb also demands an
/// RLIMIT_RTTIME we do not set.
#[cfg(target_os = "linux")]
mod linux_rtkit {
    /// One-shot blocking D-Bus call, never from inside a task: callers are dedicated threads
    /// and a runtime's `on_thread_start`, which runs before its thread takes a task.
    /// Connection is per-call: a cached system-bus socket pinned for the session is a worse
    /// trade than a few µs at start.
    pub(super) fn make_high_priority(nice: i32) -> Result<(), zbus::Error> {
        // SAFETY: `gettid` takes no arguments, touches no memory, and returns the calling
        // thread's kernel tid — always valid on Linux.
        let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u64;
        let pid = u64::from(std::process::id());
        let conn = zbus::blocking::Connection::system()?;
        // `MakeThreadHighPriorityWithPID(u64 process, u64 thread, i32 priority)`.
        // Priority is a nice, floored by rtkit's MinNiceLevel. WithPID + our pid is
        // "this thread of this process"; rtkit still authenticates via the bus.
        conn.call_method(
            Some("org.freedesktop.RealtimeKit1"),
            "/org/freedesktop/RealtimeKit1",
            Some("org.freedesktop.RealtimeKit1"),
            "MakeThreadHighPriorityWithPID",
            &(pid, tid, nice),
        )?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    #[test]
    fn tightened_slack_is_one_nanosecond() {
        // A fresh thread, so the test runner's own slack stays as it was.
        let slack = std::thread::spawn(|| {
            super::tighten_timer_slack();
            // SAFETY: `PR_GET_TIMERSLACK` takes no pointer and returns the calling thread's
            // slack in nanoseconds; the three trailing arguments are unused zeros.
            unsafe { libc::prctl(libc::PR_GET_TIMERSLACK, 0, 0, 0, 0) }
        })
        .join()
        .expect("slack thread");
        assert_eq!(slack, 1);
    }

    /// Nice -5 where the box allows it, never further, and a thread it spawns starts unraised.
    /// A box that refuses a negative nice (the check container) shows only the refusal.
    #[test]
    fn a_pool_thread_raises_itself_alone() {
        let (before, after, child) = std::thread::spawn(|| {
            let before = super::current_thread_sched().2;
            super::raise_pool_thread();
            let child = std::thread::spawn(|| super::current_thread_sched().2)
                .join()
                .expect("child thread");
            (before, super::current_thread_sched().2, child)
        })
        .join()
        .expect("pool thread");
        assert!(after == -5 || after == before, "nice {before} → {after}");
        if after == -5 {
            assert_eq!(child, 0, "the raise passed to a child");
        }
    }

    /// The flag needs no privilege, so this runs where a negative nice is refused.
    #[test]
    fn reset_on_fork_takes_without_privilege() {
        let policy = std::thread::spawn(|| {
            super::reset_on_fork();
            // SAFETY: `sched_getscheduler` takes a by-value pid (0, the calling thread), touches
            // no memory, and returns its policy with the reset-on-fork bit.
            unsafe { libc::sched_getscheduler(0) }
        })
        .join()
        .expect("thread");
        assert_eq!(policy, libc::SCHED_OTHER | libc::SCHED_RESET_ON_FORK);
    }

    /// A launched program starts at nice 0 and the default slack, not the spawner's raise.
    /// Slack needs no privilege; the nice half runs only where a negative nice is allowed.
    #[test]
    fn an_unraised_program_drops_the_spawners_raise() {
        let (raised, plain, unraised) = std::thread::spawn(|| {
            super::tighten_timer_slack();
            // SAFETY: `setpriority` takes by-value integers and touches no memory; `who == 0` is
            // this test thread, and a refusal leaves it as it was.
            let raised = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, -5) } == 0;
            let report = |cmd: &mut std::process::Command| {
                let out = cmd
                    .args(["-c", "nice; cat /proc/self/timerslack_ns"])
                    .output()
                    .expect("run sh");
                String::from_utf8(out.stdout).expect("utf-8")
            };
            let plain = report(&mut std::process::Command::new("sh"));
            let unraised = report(super::unraised(&mut std::process::Command::new("sh")));
            (raised, plain, unraised)
        })
        .join()
        .expect("spawner thread");
        let lines = |s: &str| s.lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(lines(&plain)[1], "1", "a plain child inherits the slack");
        assert_eq!(lines(&unraised)[1], "50000");
        if raised {
            assert_eq!(lines(&plain)[0], "-5", "a plain child inherits the nice");
            assert_eq!(lines(&unraised)[0], "0");
        } else {
            eprintln!("nice -5 refused here: only the slack half ran");
        }
    }

    #[test]
    fn current_thread_sched_reports_a_real_policy() {
        let (policy, rt_priority, nice) = super::current_thread_sched();
        assert!(
            matches!(
                policy,
                "SCHED_OTHER" | "SCHED_RR" | "SCHED_FIFO" | "SCHED_BATCH" | "SCHED_IDLE"
            ),
            "unrecognised policy {policy}"
        );
        assert!(
            (0..=99).contains(&rt_priority),
            "rt priority {rt_priority} outside the kernel's range"
        );
        assert!((-20..=19).contains(&nice), "nice {nice} outside PRIO range");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use libc::qos_class_t::{self, QOS_CLASS_USER_INITIATED, QOS_CLASS_USER_INTERACTIVE};

    fn boosted(critical: bool) -> qos_class_t {
        std::thread::spawn(move || {
            super::boost_thread_priority(critical);
            let mut class = qos_class_t::QOS_CLASS_UNSPECIFIED;
            let mut relative = 0;
            // SAFETY: `pthread_self` names this live thread; both out-pointers are live locals.
            let rc = unsafe {
                libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative)
            };
            assert_eq!(rc, 0);
            class
        })
        .join()
        .expect("boosted thread")
    }

    #[test]
    fn a_boosted_thread_takes_the_class_of_its_role() {
        assert_eq!(boosted(true) as u32, QOS_CLASS_USER_INTERACTIVE as u32);
        assert_eq!(boosted(false) as u32, QOS_CLASS_USER_INITIATED as u32);
    }
}
