//! The SCM runtime: the service entry, the event handles, the logs, and the supervisor that
//! launches the host into the active console session.

use super::*;

/// Manual-reset STOP and SESSION events. `OnceLock` so the SCM handler stays `'static` (`HANDLE` is
/// not `Send`); `OwnedHandle` for the process lifetime so the handler never signals a closed event.
pub(super) static STOP_EVENT: OnceLock<OwnedHandle> = OnceLock::new();

pub(super) static SESSION_EVENT: OnceLock<OwnedHandle> = OnceLock::new();

/// Borrow for `SetEvent`. `None` until `run_service` sets the events; the handler is registered after.
pub(super) fn event_handle(ev: &OnceLock<OwnedHandle>) -> Option<HANDLE> {
    ev.get().map(|h| HANDLE(h.as_raw_handle()))
}

pub fn service_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    // Logs carry webhook URLs: SYSTEM/Administrators only.
    let _ = pf_paths::create_private_dir(&dir);
    dir.join("service.log")
}

pub(super) fn host_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    let _ = pf_paths::create_private_dir(&dir);
    dir.join("host.log")
}

/// 10 MiB one-generation cap. Rotated at (re)open so a crash loop cannot grow logs without bound.
pub(super) const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rename `path` → `path.old` when over [`LOG_ROTATE_BYTES`]. Call only before open: rename under
/// a live appender silently redirects writes into `.old`.
pub(super) fn rotate_if_large(path: &std::path::Path) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= LOG_ROTATE_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = std::fs::rename(path, std::path::Path::new(&old));
    }
}

/// File logging for `service run`. The SCM gives no console; falls back to stderr. Tees into the
/// in-memory log ring so this init matches the interactive `main()` path.
pub fn init_file_logging(filter: tracing_subscriber::EnvFilter) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Layer;
    let ring =
        crate::log_capture::RingLayer.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
    let log_path = service_log_path();
    rotate_if_large(&log_path);
    // `install_global` (not `SubscriberInitExt::init`): the bridge ignores `wasapi`; see its doc.
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(file) => {
            crate::log_capture::install_global(
                tracing_subscriber::registry().with(ring).with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(move || file.try_clone().expect("clone service log handle"))
                        .with_filter(filter),
                ),
            );
        }
        Err(_) => {
            crate::log_capture::install_global(
                tracing_subscriber::registry().with(ring).with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(filter),
                ),
            );
        }
    }
}

windows_service::define_windows_service!(ffi_service_main, service_main);

pub(super) fn run() -> Result<()> {
    // Blocks until stop. The SCM then runs `service_main` on its own thread.
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        anyhow::anyhow!(
            "service_dispatcher failed ({e}). `service run` is launched by the Service Control \
             Manager, not by hand — use `punktfunk-host service install` then `service start`."
        )
    })
}

pub(super) fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service exited with error: {e:#}");
    }
}

pub(super) fn run_service() -> Result<()> {
    use windows_service::service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};

    // STOP is set once and never reset. SESSION is reset by the supervisor after it reacts.
    // SAFETY: CreateEventW with null attributes, manual-reset, initial-false, unnamed: no pointers
    // into Rust memory. Returns a fresh owned HANDLE (or Err via `?`). Nothing aliases the call.
    let stop_raw =
        unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.context("CreateEvent stop")?;
    // SAFETY: a second fresh unnamed manual-reset event; no pointers into Rust memory, no aliasing.
    let session_raw = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .context("CreateEvent session")?;
    // SAFETY: `stop_raw` is a fresh CreateEventW handle we own — take ownership exactly once.
    let stop_owned = unsafe { OwnedHandle::from_raw_handle(stop_raw.0) };
    // SAFETY: `session_raw` is the other fresh CreateEventW handle nothing else owns — take ownership once.
    let session_owned = unsafe { OwnedHandle::from_raw_handle(session_raw.0) };
    let stop = HANDLE(stop_owned.as_raw_handle());
    let session = HANDLE(session_owned.as_raw_handle());
    let _ = STOP_EVENT.set(stop_owned);
    let _ = SESSION_EVENT.set(session_owned);

    // Handler is `'static` via the statics. Lock/unlock is IDD-push in-process; only console
    // connect/disconnect/logon change the session we launch into.
    let handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Preshutdown | ServiceControl::Shutdown => {
                if let Some(h) = event_handle(&STOP_EVENT) {
                    // SAFETY: `h` borrows STOP_EVENT for the process lifetime; never closed before
                    // exit. SetEvent only signals; no Rust memory.
                    unsafe { SetEvent(h) }.ok();
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::SessionChange(param) => {
                use windows_service::service::SessionChangeReason::*;
                if matches!(
                    param.reason,
                    ConsoleConnect | ConsoleDisconnect | SessionLogon
                ) {
                    if let Some(h) = event_handle(&SESSION_EVENT) {
                        // SAFETY: `h` borrows SESSION_EVENT for the process lifetime; never closed
                        // before exit. SetEvent only signals; no Rust memory.
                        unsafe { SetEvent(h) }.ok();
                    }
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status_handle = service_control_handler::register(SERVICE_NAME, handler)
        .context("register service control handler")?;

    let accepted = ServiceControlAccept::STOP
        | ServiceControlAccept::PRESHUTDOWN
        | ServiceControlAccept::SESSION_CHANGE;
    let running = ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: ServiceState::Running,
        controls_accepted: accepted,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status_handle
        .set_service_status(running.clone())
        .context("set RUNNING")?;
    tracing::info!(
        "punktfunk service started — supervising the host (active console session) and the web \
         console (session 0)"
    );

    // Before the warner thread: `load_host_env` mutates the process env; the warner's child
    // snapshots it.
    load_host_env();

    // Own thread: `Get-NetConnectionProfile` is slow and must not delay the host.
    std::thread::spawn(warn_if_public_network);
    let result = supervise(stop, session);

    // Report the truth: `running` carries Win32(0), so a failed `supervise` used to stop as
    // cleanly as an operator-requested stop, and the SCM log — and any failure action keyed on
    // a non-zero exit — never saw the difference.
    let _ = status_handle.set_service_status(ServiceStatus {
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(u32::from(result.is_err())),
        ..running
    });
    // Leave the OnceLock events open: the SCM handler can still fire until process exit.
    result
}

/// Supervises the ordinary host in the active console session and the web console
/// in session 0. Every wait passes through [`WebSlot::wait`] so both children
/// remain covered while either supervision arm blocks.
pub(super) fn supervise(stop: HANDLE, session_ev: HANDLE) -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let host_cmd = std::env::var("PUNKTFUNK_HOST_CMD").unwrap_or_else(|_| DEFAULT_HOST_CMD.into());
    let cmdline = format!("\"{}\" {host_cmd}", exe.to_string_lossy());
    let workdir: Vec<u16> = exe
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    // KILL_ON_JOB_CLOSE prevents an orphaned SYSTEM host. BREAKAWAY_OK lets that host detach
    // session-user launches and update installers; dropping the job reaps everything else.
    let job = make_job(JOB_OBJECT_LIMIT_BREAKAWAY_OK).context("create job object")?;

    let mut web = WebSlot::new(&exe);

    let mut restarts: u32 = 0;
    // One-shot: a rollback that itself fails must not spawn installers in a loop.
    let mut rollback_attempted = false;
    loop {
        if wait_one(stop, 0) {
            break;
        }
        // SAFETY: takes no arguments; returns the session id (or 0xFFFFFFFF) by value.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == 0xFFFF_FFFF {
            // No console session (boot / logged out). Keep waiting so the web console stays up.
            tracing::debug!("no active console session — waiting");
            if web.wait(&[stop, session_ev], 3000) == Some(0) {
                break;
            }
            // SAFETY: `session_ev` borrows SESSION_EVENT for the process lifetime; ResetEvent only
            // clears the signalled state, no Rust memory.
            unsafe { ResetEvent(session_ev) }.ok();
            continue;
        }

        let job_h = HANDLE(job.as_raw_handle());
        // SAFETY: `spawn_host` is unsafe only for its Win32 FFI. `session` is a valid console
        // session id (checked != 0xFFFFFFFF above), `cmdline`/`workdir` are live borrows, and
        // `job_h` borrows the still-live `job` OwnedHandle — valid for the call.
        let child = match unsafe { spawn_host(session, &cmdline, &workdir, job_h) } {
            Ok(child) => child,
            Err(e) => {
                tracing::error!(
                    session,
                    "launch the host into the active console session: {e:#}"
                );
                // A host that never starts is the boot loop the rollback exists for.
                restarts += 1;
                maybe_boot_loop_rollback(restarts, &mut rollback_attempted);
                if web.wait(&[stop], 3000).is_some() {
                    break;
                }
                continue;
            }
        };
        tracing::info!(pid = child.pid, session, cmd = %host_cmd, "host launched");

        // `HANDLE` is Copy: `proc_h` does not close the process. `child` owns both handles and
        // closes them on drop (end of iteration / continue / break).
        let proc_h = HANDLE(child.process.as_raw_handle());

        // A notification for a session that is still ours (any logon signals the event) keeps
        // the child and the same wait set: dropping `session_ev` here would miss the console
        // switch that follows.
        let reason = loop {
            let r = web.wait(&[stop, session_ev, proc_h], INFINITE);
            if r != Some(1) {
                break (r, session);
            }
            // SAFETY: `session_ev` borrows SESSION_EVENT for the process lifetime; ResetEvent
            // only clears the signalled state, no Rust memory.
            unsafe { ResetEvent(session_ev) }.ok();
            // SAFETY: takes no arguments; returns the session id by value.
            let now = unsafe { WTSGetActiveConsoleSessionId() };
            if now != session {
                break (r, now);
            }
        };
        match reason {
            (Some(0), _) => {
                // SAFETY: `proc_h` copies the still-live `child.process` OwnedHandle (not dropped
                // until end of iteration). TerminateProcess only signals by handle; no Rust memory.
                unsafe {
                    let _ = TerminateProcess(proc_h, 0);
                }
                break;
            }
            (Some(1), now) => {
                tracing::info!(
                    old = session,
                    new = now,
                    "console session changed — relaunching host"
                );
                // SAFETY: `proc_h` copies the still-live `child.process` OwnedHandle (dropped
                // only at end of iteration). TerminateProcess only signals by handle.
                unsafe {
                    let _ = TerminateProcess(proc_h, 0);
                }
                restarts = 0;
                continue;
            }
            _ => {
                let mut code: u32 = 0;
                // SAFETY: `proc_h` copies the still-live `child.process` OwnedHandle (dropped only at
                // end of iteration); `code` is a live local out-param.
                let _ = unsafe { GetExitCodeProcess(proc_h, &mut code) };
                if code == crate::power::RESTART_EXIT_CODE {
                    tracing::info!(pid = child.pid, "host restarting on request — relaunching");
                    restarts = 0;
                    continue;
                }
                tracing::warn!(
                    pid = child.pid,
                    exit_code = format!("{code:#x}"),
                    "host process exited on its own — relaunching"
                );
            }
        }

        restarts += 1;
        maybe_boot_loop_rollback(restarts, &mut rollback_attempted);
        let backoff = restarts.min(10) * 500; // 0.5s..5s
        if web.wait(&[stop], backoff).is_some() {
            break;
        }
    }

    tracing::info!("supervision loop ended");
    Ok(())
}

pub(super) fn wait_one(h: HANDLE, ms: u32) -> bool {
    // SAFETY: `&[h]` is a live one-element HANDLE slice the caller keeps open across the wait.
    // The binding derives the count from the slice length; the array is only read for this call.
    unsafe { WaitForMultipleObjects(&[h], false, ms) == WAIT_OBJECT_0 }
}

/// Index of the first signalled handle, or `None` on timeout.
pub(super) fn wait_any(handles: &[HANDLE], ms: u32) -> Option<usize> {
    // SAFETY: `handles` is a live slice the caller keeps open across the wait. The binding
    // derives the count from the slice length; the array is only read for this call.
    let r = unsafe { WaitForMultipleObjects(handles, false, ms) };
    let idx = r.0.wrapping_sub(WAIT_OBJECT_0.0);
    (idx < handles.len() as u32).then_some(idx as usize)
}

/// Creates a kill-on-close job and returns its owned handle.
///
/// The host job permits breakaway for detached session-user launches and update
/// installers. The web-console job does not permit children to outlive the
/// service. Ownership starts before the first fallible configuration call.
pub(super) fn make_job(limits: JOB_OBJECT_LIMIT) -> Result<OwnedHandle> {
    // SAFETY: a null security descriptor and a null name are "unnamed, default security";
    // the returned handle is checked by `?` and owned on the next line.
    let job_raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }.context("CreateJobObjectW")?;
    // Own it immediately so any early return still closes it.
    // SAFETY: `job_raw` is the handle just created, non-null, and not owned anywhere else.
    let job = unsafe { OwnedHandle::from_raw_handle(job_raw.0) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | limits;
    // SAFETY: `job` is the live job object above; `info` is a local of the type
    // `JobObjectExtendedLimitInformation` expects, and the size argument is its `size_of`.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.as_raw_handle()),
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .context("SetInformationJobObject")?;
    Ok(job)
}

pub(super) struct Child {
    pub(super) process: OwnedHandle,
    /// Closed on drop. The web-console spawn `ResumeThread`s it; host spawn never reads it.
    pub(super) _thread: OwnedHandle,
    pub(super) pid: u32,
}

/// Launch the host as SYSTEM into `session_id`'s interactive desktop.
pub(super) unsafe fn spawn_host(
    session_id: u32,
    cmdline: &str,
    workdir: &[u16],
    job: HANDLE,
) -> Result<Child> {
    // Duplicate this process's LocalSystem token and set its session id. SYSTEM holds SE_TCB, so
    // `SetTokenInformation(TokenSessionId)` is permitted.
    let mut proc_token = HANDLE::default();
    // SAFETY: `GetCurrentProcess` is a pseudo-handle and needs no close; `proc_token` is a live
    // local out-param that receives an owned handle on `Ok`, closed once below.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            &mut proc_token,
        )
    }
    .context("OpenProcessToken (service must run as SYSTEM)")?;

    let mut primary = HANDLE::default();
    // SAFETY: `proc_token` is the live token just opened; `primary` is a live local out-param
    // that receives a second owned handle on `Ok`. Both are closed exactly once here.
    let dup = unsafe {
        DuplicateTokenEx(
            proc_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    };
    // SAFETY: `proc_token` is live and owned here, closed exactly once and not used after.
    let _ = unsafe { CloseHandle(proc_token) };
    dup.context("DuplicateTokenEx(TokenPrimary)")?;
    // SAFETY: `primary` is the owned handle `DuplicateTokenEx` just filled in; owning it here
    // closes it on every early return below, not only the success path.
    let _primary = unsafe { OwnedHandle::from_raw_handle(primary.0) };

    // SAFETY: `primary` is the live duplicated token; the value pointer is a local `u32` matching
    // what `TokenSessionId` expects, and the length argument is exactly its `size_of`.
    unsafe {
        SetTokenInformation(
            primary,
            TokenSessionId,
            &session_id as *const u32 as *const c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .context("SetTokenInformation(TokenSessionId)")?;

    // Session env merged with this process's `PUNKTFUNK_*`/`RUST_LOG` — same merge as interactive
    // launch, so host.env reaches the child.
    let mut env_block: *mut c_void = std::ptr::null_mut();
    // SAFETY: `env_block` is a live local out-param and `primary` the live token above; on success
    // the call stores an owned block pointer, destroyed exactly once below.
    let _ = unsafe { CreateEnvironmentBlock(&mut env_block, Some(primary), false) };
    // SAFETY: `env_block` is either still null (the call above failed) or the double-null-terminated
    // UTF-16 block `CreateEnvironmentBlock` just wrote — exactly the two states the helper accepts.
    let mut merged =
        unsafe { crate::interactive::merged_env_block(env_block as *const u16, false) };
    // Tells the host a restart request is answered (`crate::power::RESTART_EXIT_CODE`). The block
    // ends in its terminating NUL; the entry goes before it.
    merged.pop();
    merged.extend("PUNKTFUNK_SERVICE_CHILD=1".encode_utf16());
    merged.extend([0, 0]);
    if !env_block.is_null() {
        // SAFETY: `env_block` is the live block from the call above, destroyed exactly once and not
        // read after — `merged` owns its own copy of the parsed entries.
        let _ = unsafe { DestroyEnvironmentBlock(env_block) };
    }

    // Previous child has exited, so rotate is safe. A leaked orphan lacks FILE_SHARE_DELETE and
    // the rename just fails.
    let host_log = host_log_path();
    rotate_if_large(&host_log);
    let log = open_log_handle(&host_log)?;

    let mut si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: log,
        hStdError: log,
        ..Default::default()
    };
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    si.lpDesktop = PWSTR(desktop.as_mut_ptr());

    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let cwd = (!workdir.is_empty()).then_some(PCWSTR(workdir.as_ptr()));
    let mut pi = PROCESS_INFORMATION::default();

    // SAFETY: `primary` is the live retargeted token; `cmd`, `desktop` (via `si.lpDesktop`),
    // `workdir` (via `cwd`) and `merged` are live for the call and NUL-terminated as the API
    // requires — `merged` doubly so, per `merged_env_block`. `si.hStdOutput`/`hStdError` are the
    // live inheritable `log` handle. `pi` is a live local out-param; no pointer is retained.
    let created = unsafe {
        CreateProcessAsUserW(
            Some(primary),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true, // inherit handles
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(merged.as_ptr() as *const c_void),
            cwd.unwrap_or(PCWSTR::null()),
            &si,
            &mut pi,
        )
    };

    // SAFETY: `log` is live and owned here, closed exactly once and not used after — the child
    // holds its own inherited copy. `primary` closes with `_primary`.
    unsafe {
        let _ = CloseHandle(log);
    }
    created.context("CreateProcessAsUserW(host)")?;

    // Best-effort (the web spawn treats assignment failure as fatal).
    // SAFETY: `job` is a live job object per this fn's contract; `pi.hProcess` is the live child
    // handle just created (`created` was `Ok`), still owned here.
    let _ = unsafe { AssignProcessToJobObject(job, pi.hProcess) };

    // SAFETY: `created` was `Ok`, so `pi.hProcess` is an owned handle nothing else closes;
    // wrapping it transfers that ownership to the `OwnedHandle`, which closes it exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.0) };
    // SAFETY: the same, for the distinct thread handle `CreateProcessAsUserW` filled in.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.0) };
    Ok(Child {
        process,
        _thread: thread,
        pid: pi.dwProcessId,
    })
}

/// Open `path` for append as an inheritable handle (child stdout/stderr). The returned `HANDLE`
/// is owned by the caller — an ownership obligation, not a safety one.
pub(super) fn open_log_handle(path: &std::path::Path) -> Result<HANDLE> {
    let wpath = HSTRING::from(path.as_os_str());
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    // Append mask: `FILE_GENERIC_WRITE` minus `FILE_WRITE_DATA`, plus `FILE_APPEND_DATA`. Bare
    // `FILE_APPEND_DATA` alone produced a child handle that silently dropped writes.
    let access = (FILE_GENERIC_WRITE.0 & !FILE_WRITE_DATA.0) | FILE_APPEND_DATA.0;
    // SAFETY: `wpath` is the locally built NUL-terminated UTF-16 path and `sa` a correctly sized
    // local `SECURITY_ATTRIBUTES`; both outlive the call, and the result is checked by `?`.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wpath.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&sa),
            OPEN_ALWAYS,
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
    .context("CreateFileW(host.log)")?;
    Ok(h)
}
