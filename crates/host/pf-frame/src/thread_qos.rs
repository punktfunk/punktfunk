//! Per-thread OS scheduling QoS for the data plane.
//!
//! Capture/encode and send raise their own priority so a CPU-saturating game
//! cannot deschedule them. Native, GameStream, and direct-NVENC send threads
//! all call [`boost_thread_priority`].

/// Raise this thread so a CPU-heavy game cannot deschedule capture/encode/send.
///
/// GPU HIGH only favours commands already submitted; a descheduled
/// normal-priority thread submits late and the GPU priority never bites.
/// `critical` is highest non-realtime (capture+encode); otherwise above-normal
/// (send/relay).
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
        // Best-effort nice of the calling thread. Linux `setpriority(PRIO_PROCESS, 0, …)`
        // is the current task. Do not use SCHED_RR/FIFO by default: realtime can preempt
        // the compositor and the game's render thread (opt-in: PUNKTFUNK_SCHED_RR).
        let nice = if critical { -10 } else { -5 };
        // SAFETY: `setpriority` takes three by-value integers and no pointers, so there is nothing to
        // alias or outlive. `PRIO_PROCESS` with `who == 0` targets the calling task on Linux and
        // `nice` is in range; the call only adjusts this thread's scheduling nice value and returns an
        // `int` we inspect. No memory is touched.
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
        if rc == 0 {
            tracing::debug!(critical, nice, "thread nice raised");
        } else {
            // Direct call needs CAP_SYS_NICE or a raised RLIMIT_NICE. Do not file-cap the
            // host binary: a capped process's /proc/<pid>/exe is unreadable to KWin.
            // RealtimeKit is the unprivileged path; a LimitNICE drop-in only applies at next login.
            match linux_rtkit::make_high_priority(nice) {
                Ok(()) => tracing::debug!(critical, nice, "thread nice raised via rtkit"),
                Err(e) => tracing::debug!(
                    critical,
                    reason = %e,
                    "setpriority(nice) no-op (needs CAP_SYS_NICE / RLIMIT_NICE, and rtkit \
                     was unavailable)"
                ),
            }
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = critical;
    }
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
/// High-priority (nice) only, never `MakeThreadRealtime` — the SCHED_RR
/// reservations in [`boost_thread_priority`] apply to rtkit-granted RR too,
/// and the RT verb also demands an RLIMIT_RTTIME we do not set.
#[cfg(target_os = "linux")]
mod linux_rtkit {
    /// One-shot blocking D-Bus call. Must run on a plain worker thread, never
    /// from async — [`boost_thread_priority`] only runs inside dedicated
    /// capture/encode/send threads. Connection is per-call: a cached system-bus
    /// socket pinned for the session is a worse trade than a few µs at start.
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
