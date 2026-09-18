//! Windows SCM service: a session-0 LocalSystem supervisor that launches two children.
//!
//! The streaming host must run **as SYSTEM in the interactive console session** (session 1+).
//! Capture of the secure (Winlogon/UAC/lock) desktop and `SendInput` both need SYSTEM; capture
//! and injection both need that session, which a plain session-0 service is not in. This process
//! never captures: it duplicates its LocalSystem token, retargets it to the active console
//! session, and `CreateProcessAsUserW`s the host there. The host captures the virtual display
//! in-process via IDD direct-push.
//!
//! The second child is the web management console (bun/Nitro on :47992), spawned plainly into
//! session 0 so a session switch does not tear it down.
//!
//! Subcommands: `run` (SCM binPath), `install`/`uninstall`, `start`/`stop`/`restart`/`status`.
//! Config: `%ProgramData%\punktfunk\host.env`. Logs: `%ProgramData%\punktfunk\logs\`.

use anyhow::{bail, Context, Result};
use std::ffi::{c_void, OsString};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    SECURITY_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_WRITE_DATA, OPEN_ALWAYS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess,
    OpenProcessToken, ResetEvent, ResumeThread, SetEvent, TerminateProcess, WaitForMultipleObjects,
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

/// SCM key under `HKLM\SYSTEM\CurrentControlSet\Services`. Do not rename.
const SERVICE_NAME: &str = "PunktfunkHost";
const SERVICE_DISPLAY: &str = "Punktfunk Host";
const SERVICE_DESCRIPTION: &str =
    "Low-latency desktop/game streaming host. Launches the ordinary host into the active console session.";

/// `PUNKTFUNK_HOST_CMD` when host.env names none. Only a host.env from before the setting has no
/// line; `service install` moves its GameStream into the settings store and writes `serve`.
const DEFAULT_HOST_CMD: &str = "serve --gamestream";

/// Manual-reset STOP and SESSION events. `OnceLock` so the SCM handler stays `'static` (`HANDLE` is
/// not `Send`); `OwnedHandle` for the process lifetime so the handler never signals a closed event.
static STOP_EVENT: OnceLock<OwnedHandle> = OnceLock::new();
static SESSION_EVENT: OnceLock<OwnedHandle> = OnceLock::new();

/// Borrow for `SetEvent`. `None` until `run_service` sets the events; the handler is registered after.
fn event_handle(ev: &OnceLock<OwnedHandle>) -> Option<HANDLE> {
    ev.get().map(|h| HANDLE(h.as_raw_handle()))
}

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("run") => run(),
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        Some("start") => sc(&["start", SERVICE_NAME]),
        Some("stop") => sc(&["stop", SERVICE_NAME]),
        Some("restart") => restart(),
        Some("status") => sc(&["query", SERVICE_NAME]),
        _ => {
            eprintln!(
                "punktfunk-host service — Windows service control\n\n\
                 USAGE:\n\
                 \x20   punktfunk-host service install [--gamestream=on|off] [--web-bind=ADDR]\n\
                 \x20                                      register the auto-start service + firewall rules\n\
                 \x20                                      (--gamestream sets the console's GameStream setting;\n\
                 \x20                                       --web-bind sets PUNKTFUNK_UI_BIND, the console's\n\
                 \x20                                       address: 127.0.0.1, 0.0.0.0, or one of yours)\n\
                 \x20   punktfunk-host service uninstall   stop + remove the service + firewall rules\n\
                 \x20   punktfunk-host service start       start the service now\n\
                 \x20   punktfunk-host service stop        stop the service\n\
                 \x20   punktfunk-host service restart     stop, wait for exit, start again\n\
                 \x20   punktfunk-host service status      query the service\n\n\
                 Config: %ProgramData%\\punktfunk\\host.env   Logs: %ProgramData%\\punktfunk\\logs\\"
            );
            Ok(())
        }
    }
}

pub fn service_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    // `create_secret_dir`, not `create_private_dir`: Users:(RX) inherited from the config dir would
    // let a local user pre-plant reparse points on SYSTEM log files. Logs carry webhook URLs.
    let _ = pf_paths::create_secret_dir(&dir);
    dir.join("service.log")
}

fn host_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    let _ = pf_paths::create_secret_dir(&dir);
    dir.join("host.log")
}

/// 10 MiB one-generation cap. Rotated at (re)open so a crash loop cannot grow logs without bound.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rename `path` → `path.old` when over [`LOG_ROTATE_BYTES`]. Call only before open: rename under
/// a live appender silently redirects writes into `.old`.
fn rotate_if_large(path: &std::path::Path) {
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

fn host_env_path() -> PathBuf {
    pf_paths::config_dir().join("host.env")
}

/// Load host.env into this process so the host child inherits `PUNKTFUNK_*` / `RUST_LOG`.
fn load_host_env() {
    let path = host_env_path();
    let Ok(contents) = std::fs::read_to_string(&path) else {
        tracing::info!(path = %path.display(), "no host.env (using defaults)");
        return;
    };
    let mut n = 0;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim().trim_matches('"'));
            // Allow-list matches `interactive::merged_env_block`. A planted host.env must not
            // override `SystemRoot` — `icacls_path` / the powershell warner resolve through it.
            // `PUNKTFUNK_HOST_CMD` still passes; a non-admin-owned host.env is rejected at install.
            // Credentials are excluded outright: they live in their own owner-only files, and an
            // environment copy is what every child process inherits.
            let secret = k.contains("TOKEN") || k.contains("PASSWORD");
            let allowed = (k.starts_with("PUNKTFUNK_") || k == "RUST_LOG") && !secret;
            if !k.is_empty() && allowed {
                // SAFETY: no other thread yet. The network-profile warner and the host child both
                // start after `load_host_env` returns, so nothing reads the environment concurrently.
                unsafe { std::env::set_var(k, v) };
                n += 1;
            } else if !k.is_empty() {
                tracing::warn!(key = %k, "host.env: ignoring non-allow-listed key");
            }
        }
    }
    tracing::info!(path = %path.display(), vars = n, "loaded host.env");
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn run() -> Result<()> {
    // Blocks until stop. The SCM then runs `service_main` on its own thread.
    windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| {
        anyhow::anyhow!(
            "service_dispatcher failed ({e}). `service run` is launched by the Service Control \
             Manager, not by hand — use `punktfunk-host service install` then `service start`."
        )
    })
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service exited with error: {e:#}");
    }
}

fn run_service() -> Result<()> {
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
fn supervise(stop: HANDLE, session_ev: HANDLE) -> Result<()> {
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

fn wait_one(h: HANDLE, ms: u32) -> bool {
    // SAFETY: `&[h]` is a live one-element HANDLE slice the caller keeps open across the wait.
    // The binding derives the count from the slice length; the array is only read for this call.
    unsafe { WaitForMultipleObjects(&[h], false, ms) == WAIT_OBJECT_0 }
}

/// Index of the first signalled handle, or `None` on timeout.
fn wait_any(handles: &[HANDLE], ms: u32) -> Option<usize> {
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
fn make_job(limits: JOB_OBJECT_LIMIT) -> Result<OwnedHandle> {
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

struct Child {
    process: OwnedHandle,
    /// Closed on drop. The web-console spawn `ResumeThread`s it; host spawn never reads it.
    _thread: OwnedHandle,
    pid: u32,
}

/// Launch the host as SYSTEM into `session_id`'s interactive desktop.
unsafe fn spawn_host(
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
fn open_log_handle(path: &std::path::Path) -> Result<HANDLE> {
    let wpath: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
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

// Session-0 bun/Nitro on :47992 — not `spawn_host` (session retarget would tear it down on every
// switch). Own job, no `BREAKAWAY_OK`. Backoff 0.5 s → 60 s; a run ≥ `WEB_GOOD_RUN` resets it.
// Evidence: design/windows-web-console-lifecycle.md.

fn web_log_path() -> PathBuf {
    let dir = pf_paths::config_dir().join("logs");
    let _ = pf_paths::create_secret_dir(&dir);
    dir.join("web.log")
}

/// A run ≥ 60 s resets the consecutive-failure backoff.
const WEB_GOOD_RUN: Duration = Duration::from_secs(60);
const WEB_MAX_BACKOFF_MS: u64 = 60_000;

/// Console payload under the directory this exe runs from.
struct WebConfig {
    bun: PathBuf,
    server: PathBuf,
    web_dir: PathBuf,
}

/// Supervised web-console slot. Every host-loop wait goes through `wait`.
struct WebSlot {
    /// `None` when the payload is absent or `PUNKTFUNK_WEB_CONSOLE` opted out.
    cfg: Option<WebConfig>,
    child: Option<Child>,
    /// Kill-on-close, no breakaway. Created at first spawn; drop reaps bun and its children.
    job: Option<OwnedHandle>,
    spawned_at: Instant,
    /// Consecutive short-lived runs; spawn failures count. Drives the backoff.
    fast_exits: u32,
    next_spawn: Instant,
    /// Grace for `web-password` after the hard-gate files exist. Starting without it is safe:
    /// the console fail-closes until a password exists.
    password_deadline: Option<Instant>,
    /// One log line per wait episode, not one per poll.
    logged_wait: bool,
}

impl WebSlot {
    /// host.env is already loaded (`load_host_env` runs before `supervise`), so opt-out is an env check.
    fn new(exe: &Path) -> WebSlot {
        let app = exe.parent().unwrap_or(Path::new("."));
        let cfg = WebConfig {
            bun: app.join("bun").join("bun.exe"),
            server: app
                .join("web")
                .join(".output")
                .join("server")
                .join("index.mjs"),
            web_dir: app.join("web"),
        };
        let opted_out = std::env::var("PUNKTFUNK_WEB_CONSOLE").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            )
        });
        let cfg = if opted_out {
            tracing::info!(
                "web console disabled (PUNKTFUNK_WEB_CONSOLE in host.env) — not supervising it"
            );
            None
        } else if !cfg.bun.exists() || !cfg.server.exists() {
            // Host-only build. A payload appearing later comes from an installer, which restarts us.
            tracing::info!(
                bun = %cfg.bun.display(),
                server = %cfg.server.display(),
                "no web console payload — not supervising it"
            );
            None
        } else {
            Some(cfg)
        };
        let now = Instant::now();
        WebSlot {
            cfg,
            child: None,
            job: None,
            spawned_at: now,
            fast_exits: 0,
            next_spawn: now,
            password_deadline: None,
            logged_wait: false,
        }
    }

    /// Wait on `handles` for up to `ms`, (re)spawning the console and absorbing its exits.
    /// Same contract as `wait_any`.
    fn wait(&mut self, handles: &[HANDLE], ms: u32) -> Option<usize> {
        let deadline =
            (ms != INFINITE).then(|| Instant::now() + Duration::from_millis(u64::from(ms)));
        loop {
            let own_wake = self.converge();
            let mut set: Vec<HANDLE> = handles.to_vec();
            if let Some(c) = &self.child {
                set.push(HANDLE(c.process.as_raw_handle()));
            }
            let caller_ms = match deadline {
                None => INFINITE,
                Some(d) => d
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(u128::from(INFINITE)) as u32,
            };
            let wait_ms = caller_ms.min(own_wake.unwrap_or(INFINITE));
            match wait_any(&set, wait_ms) {
                Some(i) if i < handles.len() => return Some(i),
                Some(_) => self.on_child_exit(),
                None => {
                    if deadline.is_some_and(|d| Instant::now() >= d) {
                        return None;
                    }
                    // Gate poll / respawn due — loop into `converge`.
                }
            }
        }
    }

    /// Spawn if due and gated. `None` = no wake appointment (child running, or nothing to supervise).
    fn converge(&mut self) -> Option<u32> {
        let Some(cfg) = &self.cfg else { return None };
        if self.child.is_some() {
            return None;
        }
        let now = Instant::now();
        if now < self.next_spawn {
            return Some(
                (self.next_spawn - now)
                    .as_millis()
                    .min(u128::from(INFINITE)) as u32,
            );
        }
        // Installer stops us before touching files; still do not spawn into a half-written `{app}`.
        if !cfg.bun.exists() || !cfg.server.exists() {
            if !self.logged_wait {
                self.logged_wait = true;
                tracing::warn!(
                    bun = %cfg.bun.display(),
                    "web console payload missing — an install/uninstall in progress? waiting"
                );
            }
            return Some(60_000);
        }
        // Host writes mgmt-token at argument parse and cert/key after RSA keygen. Hold start
        // until all three exist.
        let data = pf_paths::config_dir();
        let token = data.join("mgmt-token");
        let cert = data.join("cert.pem");
        let key = data.join("key.pem");
        if !(token.exists() && cert.exists() && key.exists()) {
            if !self.logged_wait {
                self.logged_wait = true;
                tracing::info!(
                    "waiting for the host to write its mgmt token + identity cert before starting \
                     the web console"
                );
            }
            return Some(1_000);
        }
        // Soft gate: give `web setup` time to write the login password on a fresh install.
        let password = data.join("web-password");
        if !password.exists() {
            let deadline = *self
                .password_deadline
                .get_or_insert_with(|| now + Duration::from_secs(60));
            if now < deadline {
                return Some(1_000);
            }
            // Start anyway: the console fail-closes until a password exists; a respawn picks it up.
        }
        self.spawn(&data);
        self.child.is_none().then(|| {
            (self.next_spawn.saturating_duration_since(Instant::now()))
                .as_millis()
                .min(u128::from(INFINITE)) as u32
        })
    }

    /// One attempt. `data` is the config dir already used by `converge`'s gates, so env paths match.
    fn spawn(&mut self, data: &Path) {
        let Some(cfg) = &self.cfg else { return };
        // Lazy: a console-less box never creates a job.
        if self.job.is_none() {
            match make_job(JOB_OBJECT_LIMIT(0)) {
                Ok(j) => self.job = Some(j),
                Err(e) => {
                    tracing::error!("create web console job object: {e:#}");
                    self.schedule_retry();
                    return;
                }
            }
        }
        let job = HANDLE(self.job.as_ref().expect("just set").as_raw_handle());
        match spawn_web(cfg, data, job) {
            Ok(child) => {
                tracing::info!(
                    pid = child.pid,
                    "web console launched (https://<host-ip>:47992)"
                );
                self.spawned_at = Instant::now();
                self.password_deadline = None;
                self.logged_wait = false;
                self.child = Some(child);
            }
            Err(e) => {
                tracing::error!("web console did not launch: {e:#}");
                self.schedule_retry();
            }
        }
    }

    fn on_child_exit(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        let mut code: u32 = 0;
        // SAFETY: `child.process` is the live OwnedHandle of the just-signalled process (owned
        // until `child` drops at the end of this fn); `code` is a live local out-param.
        let _ = unsafe { GetExitCodeProcess(HANDLE(child.process.as_raw_handle()), &mut code) };
        let uptime = self.spawned_at.elapsed();
        if uptime >= WEB_GOOD_RUN {
            self.fast_exits = 0;
        }
        self.schedule_retry();
        tracing::warn!(
            pid = child.pid,
            exit_code = format!("{code:#x}"),
            uptime_secs = uptime.as_secs(),
            retry_in_ms = (self.next_spawn - Instant::now()).as_millis() as u64,
            "web console exited — relaunching"
        );
    }

    fn schedule_retry(&mut self) {
        self.fast_exits = self.fast_exits.saturating_add(1);
        let backoff_ms = (500u64 << (self.fast_exits - 1).min(7)).min(WEB_MAX_BACKOFF_MS);
        self.next_spawn = Instant::now() + Duration::from_millis(backoff_ms);
    }
}

/// One-line `KEY=VALUE` or a bare value — same as `mgmt_token::parse_token`.
fn read_env_file_value(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let line = contents.lines().find(|l| !l.trim().is_empty())?.trim();
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The console password line as the env name it must keep plus its value. `web-password` carries
/// `PUNKTFUNK_UI_PASSWORD_HASH` once the console has migrated it, and the clear-text key only
/// until then — forwarding either under the other's name hands the console a hash to compare as a
/// password. The value stays as written; the console strips the quotes the hash is stored in.
fn read_password_env(path: &Path) -> Option<(&'static str, String)> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut hash = None;
    let mut clear = None;
    for line in contents.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "PUNKTFUNK_UI_PASSWORD_HASH" => hash = Some(value.to_string()),
            "PUNKTFUNK_UI_PASSWORD" => clear = Some(value.to_string()),
            _ => {}
        }
    }
    // Clear text wins, the rule the console's own compare follows: writing one back beside a hash
    // is how a password is reset, and the console hashes it again on the next sign-in.
    clear
        .map(|v| ("PUNKTFUNK_UI_PASSWORD", v))
        .or_else(|| hash.map(|v| ("PUNKTFUNK_UI_PASSWORD_HASH", v)))
}

/// This process's env plus `overrides` (case-insensitive win) as a double-NUL UTF-16 block.
/// Same serialization as `interactive::merged_env_block`; the base is ours, not a user token.
fn env_block_with(overrides: &[(&str, String)]) -> Vec<u16> {
    let mut entries: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !overrides.iter().any(|(ok, _)| ok.eq_ignore_ascii_case(k)))
        .collect();
    entries.extend(overrides.iter().map(|(k, v)| (k.to_string(), v.clone())));
    // CreateProcess* requires the block sorted case-insensitively by name.
    entries.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in entries {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    block.push(0);
    block
}

/// bun serving the Nitro bundle in session 0, stdout → web.log, inside `job`. Env wiring
/// matches `scripts/punktfunk-web.service`.
fn spawn_web(cfg: &WebConfig, data: &Path, job: HANDLE) -> Result<Child> {
    // Env-over-file, same as `mgmt_token.rs`: a host.env override must reach both processes.
    let token = std::env::var("PUNKTFUNK_MGMT_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("mgmt-token")))
        .context("read mgmt-token")?;
    let pw_path = data.join("web-password");
    let password = std::env::var("PUNKTFUNK_UI_PASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| ("PUNKTFUNK_UI_PASSWORD", v))
        .or_else(|| read_password_env(&pw_path));
    // Env-over-file; `mgmt::publish_endpoint` rewrites the file each `serve`. Default 47990 is
    // last-resort — a Sunshine fork often owns that port as its web UI.
    let mgmt_url = std::env::var("PUNKTFUNK_MGMT_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| read_env_file_value(&data.join("mgmt-endpoint")))
        .unwrap_or_else(|| "https://127.0.0.1:47990".into());

    let mut overrides: Vec<(&str, String)> = vec![
        ("PORT", "47992".into()),
        // No HOST override: the bind is host.env's PUNKTFUNK_UI_BIND, which `load_host_env` has
        // already put in this process's environment, and the console defaults to loopback without
        // it. Overriding here would out-rank the file the operator edits.
        // Proxy hop to the host's loopback HTTPS mgmt API. Self-signed cert is accepted
        // per-request, never process-wide.
        ("PUNKTFUNK_MGMT_URL", mgmt_url),
        // Host identity cert; cookie is Secure. These names are the legacy pair — the console
        // prefers the native sibling (`web/nitro-entry/tls-paths.mjs`) when it exists.
        (
            "PUNKTFUNK_UI_TLS_CERT",
            data.join("cert.pem").to_string_lossy().into_owned(),
        ),
        (
            "PUNKTFUNK_UI_TLS_KEY",
            data.join("key.pem").to_string_lossy().into_owned(),
        ),
        ("PUNKTFUNK_UI_SECURE", "1".into()),
        ("PUNKTFUNK_MGMT_TOKEN", token),
        // The file the console rewrites when it turns a clear-text password into a salted hash.
        // It is reached through this name, not %ProgramData%, so the console needs no path rules.
        (
            "PUNKTFUNK_UI_PASSWORD_FILE",
            pw_path.to_string_lossy().into_owned(),
        ),
    ];
    if let Some((key, pw)) = password {
        overrides.push((key, pw));
    }
    let env = env_block_with(&overrides);

    let web_log = web_log_path();
    rotate_if_large(&web_log);
    let log = open_log_handle(&web_log)?;

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: log,
        hStdError: log,
        ..Default::default()
    };
    let mut cmd: Vec<u16> = format!("\"{}\" \"{}\"", cfg.bun.display(), cfg.server.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let cwd: Vec<u16> = cfg
        .web_dir
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut pi = PROCESS_INFORMATION::default();

    // CREATE_SUSPENDED: assign to the job before the first instruction or children escape it.
    // SAFETY: `cmd`, `cwd`, `env` and `si` (whose handles are the live inheritable `log`) are live,
    // NUL-terminated locals for the call (`env` doubly, per `env_block_with`); `pi` is a live
    // local out-param; no pointer is retained past the call.
    let created = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true, // inherit handles
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            Some(env.as_ptr() as *const c_void),
            PCWSTR(cwd.as_ptr()),
            &si,
            &mut pi,
        )
    };
    // SAFETY: `log` is live and owned here, closed exactly once and not used after — on success
    // the child holds its own inherited copy.
    let _ = unsafe { CloseHandle(log) };
    created.context("CreateProcessW(web console)")?;

    // Own the handles first so assignment failure still closes them via drop.
    // SAFETY: `created` was `Ok`, so `pi.hProcess` is an owned handle nothing else closes;
    // wrapping it transfers that ownership to the `OwnedHandle`, which closes it exactly once.
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess.0) };
    // SAFETY: the same, for the distinct thread handle `CreateProcessW` filled in.
    let thread = unsafe { OwnedHandle::from_raw_handle(pi.hThread.0) };
    let child = Child {
        process,
        _thread: thread,
        pid: pi.dwProcessId,
    };

    // Not best-effort: containment is the point of this job. Unassigned → do not run.
    // SAFETY: `job` is a live job object per this fn's contract; `child.process` is the live handle
    // of the still-suspended process just created.
    if let Err(e) = unsafe { AssignProcessToJobObject(job, HANDLE(child.process.as_raw_handle())) }
    {
        // SAFETY: `child.process` is live and owned (dropped at the end of this scope);
        // TerminateProcess only signals termination by handle. The process never ran (suspended).
        unsafe {
            let _ = TerminateProcess(HANDLE(child.process.as_raw_handle()), 1);
        }
        return Err(e).context("AssignProcessToJobObject(web console)");
    }
    // SAFETY: `child._thread` is the live primary-thread handle of the process just created; a
    // suspended primary thread is exactly what ResumeThread expects.
    let resumed = unsafe { ResumeThread(HANDLE(child._thread.as_raw_handle())) };
    if resumed == u32::MAX {
        // SAFETY: live owned handle; process never ran (still suspended). Tear it down.
        unsafe {
            let _ = TerminateProcess(HANDLE(child.process.as_raw_handle()), 1);
        }
        bail!("ResumeThread(web console) failed");
    }
    Ok(child)
}

fn install(args: &[String]) -> Result<()> {
    use windows_service::service::{
        ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl,
        ServiceFailureActions, ServiceFailureResetPeriod, ServiceInfo, ServiceStartType,
        ServiceType,
    };
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    // `None` = flag absent: the store keeps its value.
    let gamestream = match args.iter().find_map(|a| a.strip_prefix("--gamestream=")) {
        Some("on") => Some(true),
        Some("off") => Some(false),
        Some(v) => bail!("--gamestream must be 'on' or 'off' (got '{v}')"),
        None => None,
    };
    // `None` = flag absent: host.env keeps its bind.
    let mgmt_bind = match args.iter().find_map(|a| a.strip_prefix("--mgmt-bind=")) {
        Some(v) => match v.parse::<std::net::SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(_) => bail!("--mgmt-bind must be IP:PORT (got '{v}')"),
        },
        None => None,
    };
    // Where the web console listens. Address only — the port is the console's own.
    let web_bind = match args.iter().find_map(|a| a.strip_prefix("--web-bind=")) {
        Some(v) => match v.parse::<std::net::IpAddr>() {
            Ok(ip) => Some(ip),
            Err(_) => bail!("--web-bind must be an IP address (got '{v}')"),
        },
        None => None,
    };

    let exe = std::env::current_exe().context("current_exe")?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("open Service Control Manager (run from an elevated/Administrator prompt)")?;

    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(SERVICE_DISPLAY),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // None = LocalSystem
        account_password: None,
    };

    // Idempotent: create, or reconfigure if it already exists.
    match manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START) {
        Ok(svc) => {
            let _ = svc.set_description(SERVICE_DESCRIPTION);
            println!("Created service '{SERVICE_NAME}' (auto-start, LocalSystem).");
        }
        Err(windows_service::Error::Winapi(e))
            if e.raw_os_error() == Some(1073 /* ERROR_SERVICE_EXISTS */) =>
        {
            let svc = manager
                .open_service(SERVICE_NAME, ServiceAccess::CHANGE_CONFIG)
                .context("open existing service to reconfigure")?;
            svc.change_config(&info)
                .context("reconfigure existing service")?;
            let _ = svc.set_description(SERVICE_DESCRIPTION);
            println!("Reconfigured existing service '{SERVICE_NAME}'.");
        }
        Err(e) => return Err(e).context("create service"),
    }

    // Restart 1 s / 5 s / 60 s (SCM repeats the last action). Resets after a clean day. Fires
    // only on a crash, never a deliberate stop. Best-effort. Fresh open: restart needs SERVICE_START.
    let recovery = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::CHANGE_CONFIG | ServiceAccess::START,
        )
        .and_then(|svc| {
            svc.update_failure_actions(ServiceFailureActions {
                reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
                reboot_msg: None,
                command: None,
                actions: Some(vec![
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(1),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(5),
                    },
                    ServiceAction {
                        action_type: ServiceActionType::Restart,
                        delay: Duration::from_secs(60),
                    },
                ]),
            })
        });
    match recovery {
        Ok(()) => println!("Crash recovery: the SCM restarts the service at 1s/5s/60s."),
        Err(e) => eprintln!("warning: service recovery actions not set: {e}"),
    }

    ensure_default_host_env()?;
    apply_gamestream_choice(gamestream);
    // Before the rules below: the mgmt rule reads its port back from host.env.
    if let Some(addr) = mgmt_bind {
        set_host_env_line("PUNKTFUNK_MGMT_BIND", &addr.to_string())?;
    }
    if let Some(ip) = web_bind {
        set_host_env_line("PUNKTFUNK_UI_BIND", &ip.to_string())?;
    }
    // Remove prior rules first so an upgrade tightens scope instead of leaving a stale
    // all-profiles rule. Flag absent (upgrades) keeps the recorded choice.
    let allow_public = allow_public_network(args)?;
    set_fw_public_marker(allow_public);
    remove_firewall_rules();
    add_firewall_rules(allow_public);

    println!(
        "\nInstalled. Config: {}\nLogs:   {}\n\nStart now with:  punktfunk-host service start",
        host_env_path().display(),
        pf_paths::config_dir().join("logs").display()
    );
    Ok(())
}

/// Stop, wait until Stopped, then start. A bare `sc stop && sc start` races: START fails with
/// "instance already running" while the old process winds down.
fn restart() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager (run elevated)")?;
    let svc = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::START,
        )
        .context("open service (run elevated)")?;
    // ERROR_SERVICE_NOT_ACTIVE means restart == start.
    let _ = svc.stop();
    wait_stopped(&svc)?;
    svc.start(&[] as &[&std::ffi::OsStr])
        .context("start service")?;
    println!("Restarted service '{SERVICE_NAME}'.");
    Ok(())
}

/// Poll until the SCM reports the service stopped, 30 s at most. A stop is asynchronous: a
/// delete or a start issued before this lands races the still-running host.
fn wait_stopped(svc: &windows_service::service::Service) -> Result<()> {
    use windows_service::service::ServiceState;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let state = svc.query_status().context("query service status")?;
        if state.current_state == ServiceState::Stopped {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("service did not stop within 30 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn uninstall() -> Result<()> {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("open Service Control Manager (run elevated)")?;
    let svc = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
        )
        .context("open service for delete")?;
    // ERROR_SERVICE_NOT_ACTIVE means there is nothing to wait for.
    let _ = svc.stop();
    wait_stopped(&svc)?;
    svc.delete().context("delete service")?;
    remove_firewall_rules();
    println!("Removed service '{SERVICE_NAME}' and its firewall rules.");
    Ok(())
}

/// Write a default `host.env` if none exists. Encoder default is `auto` (vendor GPU).
fn ensure_default_host_env() -> Result<()> {
    let path = host_env_path();
    // Non-admin-owned host.env was planted before this elevated install (`ProgramData` grants
    // Users add-subdirectory + CREATOR OWNER). Check before `create_private_dir` re-owns the
    // dir and erases that signal. Administrators-owned files from a prior install pass.
    let planted = pf_paths_win::planted_by_non_admin(&path);
    if planted {
        // Rename-aside is best-effort; the guarantee is the `!planted` skip overwrites anyway.
        match pf_paths_win::rename_aside(&path) {
            Ok(aside) => tracing::warn!(
                path = %path.display(), aside = %aside.display(),
                "host.env was owned by a non-admin account (planted before install) — renamed aside; writing the default"
            ),
            Err(e) => tracing::error!(
                error = %e, path = %path.display(),
                "host.env is non-admin-owned and could not be renamed aside — overwriting it with the default"
            ),
        }
    }
    // Harden the dir first, before the `exists()` check — not only in the create-file branch.
    // `ProgramData` grants Users add-subdirectory + CREATOR OWNER; a planted dir must still lock.
    // The error is the junction refusal: `write_secret_file` only rejects the FILE being a link,
    // so a junctioned parent would still redirect this write. Refuse the install step instead.
    if let Some(dir) = path.parent() {
        pf_paths::create_private_dir(dir)
            .with_context(|| format!("lock down {} before writing host.env", dir.display()))?;
    }
    if path.exists() && !planted {
        // Re-lock the file: an owner can rewrite the DACL it inherited. `planted` files fall
        // through and are overwritten even if the rename-aside failed.
        pf_paths::restrict_existing_secret_file(&path);
        name_web_console_bind(&path);
        return Ok(());
    }
    let default = "# punktfunk host configuration (read by the Windows service).\n\
        # KEY=VALUE per line; '#' comments. Restart the service after editing:\n\
        #   punktfunk-host service stop && punktfunk-host service start\n\
        \n\
        # Encode backend: auto (default) detects the GPU vendor — NVIDIA->nvenc, AMD->amf, Intel->qsv.\n\
        # Force one with nvenc | amf | qsv | mf (no software encoder: the driver encodes). amf/qsv need an FFmpeg-built\n\
        # host; mf is Media Foundation, any vendor's hardware encoder, 8-bit 4:2:0 only.\n\
        PUNKTFUNK_ENCODER=auto\n\
        PUNKTFUNK_VIDEO_SOURCE=virtual\n\
        # The virtual display is the bundled pf-vdisplay driver, which also encodes; there is no\n\
        # other capture path, and the secure desktop (UAC / lock / login) is always captured.\n\
        RUST_LOG=info\n\
        \n\
        # The host subcommand the service launches. GameStream (Moonlight) is a setting in the web\n\
        # console; `serve --gamestream` here would lock it on.\n\
        PUNKTFUNK_HOST_CMD=serve\n\
        \n\
        # The web management console (https://<this-PC>:47992) runs as a child of the service.\n\
        # Set to off to disable it:\n\
        # PUNKTFUNK_WEB_CONSOLE=off\n\
        \n\
        # Where that console listens: 0.0.0.0 (your network, never the internet), 127.0.0.1 (this\n\
        # PC only), or one address, e.g. a VPN interface. The plugin-UI origin on 47993 follows it.\n\
        PUNKTFUNK_UI_BIND=0.0.0.0\n\
        \n\
        # Force a specific render GPU by name substring (multi-GPU boxes only):\n\
        # PUNKTFUNK_RENDER_ADAPTER=4090\n\
        \n\
        # The name this host shows up under in Moonlight and the Punktfunk clients\n\
        # (default: the machine's own computer name):\n\
        # PUNKTFUNK_HOST_NAME=Living Room\n";
    // DACL-locked: host.env is the SYSTEM service's environment and launched command line.
    pf_paths::write_secret_file(&path, default.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    println!("Wrote default config: {}", path.display());
    Ok(())
}

/// Name the console's bind in a host.env that predates `PUNKTFUNK_UI_BIND`, once.
///
/// The value is the default the console already runs with, so nothing moves; the line is there so
/// the operator finds the setting. `--web-bind` (applied after this) changes it in the same run.
fn name_web_console_bind(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("PUNKTFUNK_UI_BIND="))
    {
        return;
    }
    let mut next = text;
    next.push_str(concat!(
        "\n# Where the web console listens: 0.0.0.0 (your network, never the internet), 127.0.0.1\n",
        "# (this PC only), or one address, e.g. a VPN interface.\n",
        "PUNKTFUNK_UI_BIND=0.0.0.0\n",
    ));
    match pf_paths::write_secret_file(path, next.as_bytes()) {
        Ok(()) => println!("PUNKTFUNK_UI_BIND=0.0.0.0 → {}", path.display()),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "name the console bind in host.env")
        }
    }
}

/// Record the GameStream choice in the settings store, where the console reads and changes it.
///
/// A host command of `serve --gamestream`, or none (which ran that), becomes `serve` plus a stored
/// `true`: the flag would lock the console's toggle. The store is written before host.env, so a
/// failed write never turns GameStream off. A custom command stays. Best-effort.
fn apply_gamestream_choice(choice: Option<bool>) {
    let path = host_env_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!(
            "warning: {} not read, so the GameStream choice is unapplied",
            path.display()
        );
        return;
    };
    let (store, host_env, pinned) = gamestream_migration(&text, choice);
    if let Some(on) = store {
        let patch = serde_json::Map::from_iter([("gamestream".into(), on.into())]);
        if let Err(e) = pf_host_config::save(&patch) {
            eprintln!("warning: the GameStream choice was not saved: {e}");
            return;
        }
        println!(
            "GameStream (Moonlight) compatibility: {}",
            if on { "on" } else { "off" }
        );
    }
    if let Some(next) = host_env {
        // `write_secret_file` re-asserts the SYSTEM/Administrators DACL.
        match pf_paths::write_secret_file(&path, next.as_bytes()) {
            Ok(()) => println!("PUNKTFUNK_HOST_CMD=serve → {}", path.display()),
            Err(e) => eprintln!("warning: {} not written: {e}", path.display()),
        }
    }
    if pinned && choice == Some(false) {
        println!("host.env's PUNKTFUNK_HOST_CMD passes --gamestream, which keeps GameStream on");
    }
}

/// `(store value, host.env rewrite, custom command passes --gamestream)` for `service install`.
fn gamestream_migration(text: &str, choice: Option<bool>) -> (Option<bool>, Option<String>, bool) {
    let cmd = text
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix("PUNKTFUNK_HOST_CMD="))
        .map(|v| v.trim().trim_matches('"'));
    match cmd {
        None | Some("serve --gamestream") => (
            choice.or(Some(true)),
            Some(with_env_line(text, "PUNKTFUNK_HOST_CMD", "serve")),
            false,
        ),
        Some(cmd) => (
            choice,
            None,
            cmd.split_whitespace()
                .any(|a| a == "--gamestream" || a == "--moonlight"),
        ),
    }
}

/// `text` with its last live `key=` line set to `value`, else one appended. Last wins, as
/// [`load_host_env`] and [`mgmt_port`] read it.
fn with_env_line(text: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let line = format!("{key}={value}");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    match lines
        .iter()
        .rposition(|l| l.trim_start().starts_with(&prefix))
    {
        Some(i) => lines[i] = line,
        None => lines.push(line),
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Write `key=value` into the existing host.env, keeping every other line.
fn set_host_env_line(key: &str, value: &str) -> Result<()> {
    let path = host_env_path();
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    // `write_secret_file` re-asserts the SYSTEM/Administrators DACL.
    pf_paths::write_secret_file(&path, with_env_line(&text, key, value).as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    println!("{key}={value} → {}", path.display());
    Ok(())
}

/// `netsh` `profile=` for inbound rules. Default Domain+Private; `allow_public` is all profiles.
/// Shared with the web-console rule in `install.rs`.
pub(crate) fn firewall_profile_arg(allow_public: bool) -> &'static str {
    if allow_public {
        "profile=any"
    } else {
        "profile=domain,private"
    }
}

/// Public-network firewall scope. Tri-state, like `--gamestream=on|off`:
/// - `--allow-public-network` or `=on` → opt-in (bare form kept for existing scripts)
/// - `=off` → opt-out
/// - absent → the previous install's marker (so a silent upgrade does not reset the checkbox)
///
/// A typo (`=of`) must not fall through to the marker: the marker may be `true`, and a mistyped
/// opt-out would leave Public open. No marker on a first install → Domain+Private.
pub(crate) fn allow_public_network(args: &[String]) -> Result<bool> {
    for a in args {
        if let Some(v) = a.strip_prefix("--allow-public-network") {
            return match v {
                "" | "=on" => Ok(true),
                "=off" => Ok(false),
                _ => bail!(
                    "--allow-public-network must be 'on' or 'off' (got '{}')",
                    v.trim_start_matches('=')
                ),
            };
        }
    }
    Ok(fw_public_marker().exists())
}

/// `netsh advfirewall firewall add rule` for one inbound allow.
///
/// A port-only `dir=in action=allow` admits any process that binds first (high ports need no
/// elevation) and suppresses the Windows prompt. Name `program` so the ports are ours only.
/// Keep `ports` too: both is tighter. Dropped only for [`add_data_plane_firewall_rule`]
/// (ephemeral per session). `program: None` is the old any-program rule — a looser rule still
/// streams; no rule is a black screen.
pub(crate) fn fw_add_rule_args(
    name: &str,
    proto: &str,
    ports: Option<&str>,
    program: Option<&std::path::Path>,
    profile: &str,
) -> Vec<String> {
    let mut args: Vec<String> = ["advfirewall", "firewall", "add", "rule"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(format!("name={name}"));
    args.push("dir=in".into());
    args.push("action=allow".into());
    args.push(format!("protocol={proto}"));
    if let Some(p) = ports {
        args.push(format!("localport={p}"));
    }
    if let Some(exe) = program {
        args.push(format!("program={}", exe.display()));
    }
    args.push(profile.to_string());
    args
}

pub(crate) fn run_netsh(args: &[String]) -> bool {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_quiet("netsh", &borrowed)
}

/// The mgmt port `serve` binds: host.env's last `PUNKTFUNK_MGMT_BIND`, else 47990. A blank or
/// bad value keeps 47990; the host treats blank as unset and refuses to start on a bad one.
fn mgmt_port(host_env: &str) -> u16 {
    host_env
        .lines()
        .rev()
        .filter_map(|l| l.trim().split_once('='))
        .find(|(k, _)| k.trim() == "PUNKTFUNK_MGMT_BIND")
        .and_then(|(_, v)| {
            v.trim()
                .trim_matches('"')
                .parse::<std::net::SocketAddr>()
                .ok()
        })
        .map_or(crate::mgmt::DEFAULT_PORT, |a| a.port())
}

/// Inbound streaming + mgmt rules. Best-effort; never fails the install. Scoped by
/// [`firewall_profile_arg`] and this executable ([`fw_add_rule_args`]). The mgmt port is
/// deliberate: `serve` binds mgmt/library to all interfaces; off-loopback
/// `mgmt::require_auth` is read-only to a paired client cert, so opening it adds no admin surface.
fn add_firewall_rules(allow_public: bool) {
    let profile = firewall_profile_arg(allow_public);
    // Resolved once, shared with the data-plane rule. `service install` remove-then-adds on
    // every upgrade, so a moved install cannot leave a stale path.
    let exe = match std::env::current_exe() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!(
                "warning: could not resolve the host executable path ({e}) — the rules below stay \
                 open to any program on those ports, and the per-session data-plane rule is skipped"
            );
            None
        }
    };
    // Mgmt/library on host.env's port (LAN read-only, paired-cert); `--mgmt-bind` lands there
    // first. GameStream 47984/47989/48010, 47998-48010; native 9777; mDNS 5353.
    let mgmt = mgmt_port(&std::fs::read_to_string(host_env_path()).unwrap_or_default());
    let tcp = format!("47984,47989,48010,{mgmt}");
    let rules = [
        ("TCP", "TCP", tcp.as_str()),
        ("UDP", "UDP", "47998-48010,9777,5353"),
    ];
    for (suffix, proto, ports) in rules {
        let name = format!("Punktfunk {suffix}");
        let ok = run_netsh(&fw_add_rule_args(
            &name,
            proto,
            Some(ports),
            exe.as_deref(),
            profile,
        ));
        if ok {
            let scope = match &exe {
                Some(p) => format!(" for {}", p.display()),
                None => String::new(),
            };
            println!("Firewall rule added: {name} ({ports}{scope}) [{profile}]");
        } else {
            eprintln!("warning: firewall rule '{name}' not added (add it manually if needed)");
        }
    }
    add_data_plane_firewall_rule(profile, exe.as_deref());
    // Print only when scoping actually happened: with no exe path the rules are still wide open.
    if exe.is_some() {
        println!(
            "Note: these rules are scoped to the punktfunk host executable, so they no longer open \
             those ports to every program on this machine. Another mDNS/GameStream application \
             that relied on punktfunk's rules to be reachable now needs a rule of its own."
        );
    }
    if !allow_public {
        println!(
            "Note: streaming ports are open on Private/Domain networks only. On a network Windows \
             classifies as Public, clients won't connect — set that network to Private, or reinstall \
             with the 'Allow connections on Public networks' option."
        );
    }
}

const FW_DATA_PLANE_RULE: &str = "Punktfunk UDP (data plane)";

/// Inbound UDP for the host executable at any local port.
///
/// The media data plane binds `0.0.0.0:0` per session (reported in Welcome), so no `localport=`
/// rule can cover it. Without this, the client's hole-punch (`PUNCH_MAGIC`) never opens the
/// return path: control stays healthy, picture is black. Program-scoped, not a pinned port —
/// covers whatever the session picks and does not collide with Sunshine/Apollo's 47998-48010.
///
/// `exe: None` skips the rule rather than widening it: a program-less "any inbound UDP" rule
/// is an open host, not a looser version of this.
fn add_data_plane_firewall_rule(profile: &str, exe: Option<&std::path::Path>) {
    let Some(exe) = exe else {
        eprintln!(
            "warning: no host executable path — skipping the data-plane firewall rule; streams may \
             show a black picture behind a healthy connection on networks that need the client's \
             hole-punch to open the path"
        );
        return;
    };
    let ok = run_netsh(&fw_add_rule_args(
        FW_DATA_PLANE_RULE,
        "UDP",
        None,
        Some(exe),
        profile,
    ));
    if ok {
        println!(
            "Firewall rule added: {FW_DATA_PLANE_RULE} (any UDP port for {}) [{profile}]",
            exe.display()
        );
    } else {
        eprintln!(
            "warning: could not add firewall rule '{FW_DATA_PLANE_RULE}' — the per-session video \
             data port stays closed to inbound, so the client's hole-punch cannot reach it"
        );
    }
}

fn remove_firewall_rules() {
    let _ = run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={FW_DATA_PLANE_RULE}"),
        ],
    );
    for suffix in ["TCP", "UDP"] {
        // netsh matches rule names case-insensitively, so this also reaps the old lowercase names.
        let name = format!("Punktfunk {suffix}");
        let _ = run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ],
        );
    }
}

/// Presence means `--allow-public-network` was chosen; suppresses the startup Public warning.
fn fw_public_marker() -> std::path::PathBuf {
    pf_paths::config_dir().join("fw-allow-public")
}

fn set_fw_public_marker(allow_public: bool) {
    let path = fw_public_marker();
    if allow_public {
        let _ = std::fs::write(&path, b"1\n");
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

/// Any active connection classified Public? `None` if `Get-NetConnectionProfile` cannot answer.
fn active_network_is_public() -> Option<bool> {
    // Full System32 path: CreateProcess searches the launching EXE's directory first, so a
    // planted `powershell.exe` next to the host would run as SYSTEM.
    let ps = crate::install::sys32(r"WindowsPowerShell\v1.0\powershell.exe");
    let out = std::process::Command::new(&ps)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-NetConnectionProfile).NetworkCategory",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.lines().any(|l| l.trim().eq_ignore_ascii_case("Public")))
}

/// Warn when the network is Public and the operator did not opt in. Own thread: must not delay
/// the host.
fn warn_if_public_network() {
    if fw_public_marker().exists() {
        return;
    }
    if active_network_is_public() == Some(true) {
        tracing::warn!(
            "this machine's current network is classified Public (an untrusted-network profile), so \
             punktfunk's streaming ports are firewalled off here and clients on this network can't \
             reach the host. Fix: set the network to Private (Windows Settings > Network > \
             properties) — or, only for a network you trust, reinstall with the 'Allow connections \
             on Public networks' option."
        );
    }
}

/// `sc.exe` with output passed through (start/stop/status).
///
/// Absolute: `CreateProcess` searches the cwd before `%PATH%`, and this runs elevated.
fn sc(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(crate::install::resolve_tool("sc"))
        .args(args)
        .status()
        .context("run sc.exe")?;
    if !status.success() {
        bail!("sc {} failed ({status})", args.join(" "));
    }
    Ok(())
}

/// System32 path for a bare tool: `service install` runs elevated.
fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(crate::install::resolve_tool(cmd))
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Boot-loop rollback after a host update. A fresh intent plus a crash-looping child that *is*
/// the intent's target means the just-installed host does not stay up. Re-run the cached
/// previous installer once per intent, Authenticode-checked. This process is not in the
/// kill-on-close job, so the installer survives the service stop it is about to perform.
/// Evidence: `host-update-from-web-console.md`.
fn maybe_boot_loop_rollback(restarts: u32, attempted: &mut bool) {
    if *attempted || restarts < 3 {
        return;
    }
    let intent_path = crate::update::jobs::intent_path();
    let Some(intent) = crate::update::jobs::read_intent(&intent_path) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Stale intent: no rollback. A boot-looping *old* binary is not this update; reconcile owns it.
    if now.saturating_sub(intent.started_unix) > 30 * 60 || env!("PUNKTFUNK_VERSION") != intent.to {
        return;
    }
    *attempted = true;

    let updates = pf_paths::config_dir().join("updates");
    let previous = std::fs::read_dir(&updates)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    n.starts_with("punktfunk-host-setup-")
                        && n.ends_with(".exe")
                        && !n.contains(intent.to.as_str())
                })
                .unwrap_or(false)
        })
        .max_by_key(|p| {
            p.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        });
    let Some(previous) = previous else {
        tracing::error!(
            to = %intent.to,
            "updated host is crash-looping and no cached previous installer exists — \
             leaving the intent for reconcile; manual reinstall required"
        );
        return;
    };
    // The re-check is signature only, so the directory must still be admin-only: a planted
    // `updates\` would otherwise make a self-signed exe the rollback target.
    if let Some(dir) = previous.parent() {
        if let Err(e) = crate::install::ensure_admin_only_source(dir) {
            tracing::error!(dir = %dir.display(), error = %format!("{e:#}"), "not rolling back");
            return;
        }
    }
    if let Err(e) = crate::update::windows::verify_authenticode(&previous, &[], None) {
        tracing::error!(
            installer = %previous.display(),
            error = %e,
            "cached previous installer fails its signature check — not rolling back"
        );
        return;
    }

    let log = pf_paths::config_dir()
        .join("logs")
        .join(format!("update-rollback-from-{}.log", intent.to));
    let record = crate::update::jobs::ResultRecord {
        ok: false,
        from: intent.from.clone(),
        to: intent.to.clone(),
        finished_unix: now,
        stage: Some("rolled-back".into()),
        error: Some(format!(
            "the updated host crash-looped after install; rolled back via {}",
            previous.display()
        )),
        log_path: Some(log.display().to_string()),
        staged: false,
    };
    let _ = crate::update::jobs::write_json_atomic(&crate::update::jobs::result_path(), &record);
    // Delete the intent first: the incoming host must boot clean, and a missing intent is the one-shot.
    let _ = std::fs::remove_file(&intent_path);

    tracing::warn!(
        failed_version = %intent.to,
        back_to = %previous.display(),
        "updated host is crash-looping — rolling back via the cached previous installer"
    );
    match std::process::Command::new(&previous)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"])
        .arg(format!("/LOG={}", log.display()))
        .spawn()
    {
        // Detached: it stops this service and reinstalls the previous version.
        Ok(child) => drop(child),
        Err(e) => tracing::error!(error = %e, "rollback installer did not spawn"),
    }
}

#[cfg(test)]
mod firewall_tests {
    use super::*;
    use std::path::Path;

    /// Fixed-port rules carry both `program=` and `localport=`. Dropping `program=` still streams;
    /// any unprivileged process can then bind those ports without a Windows prompt.
    #[test]
    fn fixed_port_rules_are_scoped_to_the_program_and_the_ports() {
        let exe = Path::new(r"C:\Program Files\Punktfunk\punktfunk-host.exe");
        let args = fw_add_rule_args(
            "Punktfunk UDP",
            "UDP",
            Some("47998-48010,9777,5353"),
            Some(exe),
            "profile=domain,private",
        );
        assert!(args.contains(&format!("program={}", exe.display())));
        assert!(args.contains(&"localport=47998-48010,9777,5353".to_string()));
        assert!(args.contains(&"dir=in".to_string()));
        assert!(args.contains(&"action=allow".to_string()));
        assert!(args.contains(&"profile=domain,private".to_string()));
        assert_eq!(&args[..4], &["advfirewall", "firewall", "add", "rule"]);
    }

    /// Data plane has no port (`0.0.0.0:0` per session). A program-less "any inbound UDP" rule
    /// is an open host, not a looser version of this.
    #[test]
    fn the_data_plane_rule_has_a_program_but_no_port() {
        let exe = Path::new(r"C:\Program Files\Punktfunk\punktfunk-host.exe");
        let args = fw_add_rule_args(FW_DATA_PLANE_RULE, "UDP", None, Some(exe), "profile=any");
        assert!(args.contains(&format!("program={}", exe.display())));
        assert!(
            !args.iter().any(|a| a.starts_with("localport=")),
            "the per-session data port is ephemeral — pinning one would close the others"
        );
    }

    /// Missing executable → port-only rule, not no rule. A looser rule still streams; none is black.
    #[test]
    fn a_missing_program_falls_back_to_the_port_only_rule() {
        let args = fw_add_rule_args("Punktfunk TCP", "TCP", Some("47990"), None, "profile=any");
        assert!(!args.iter().any(|a| a.starts_with("program=")));
        assert!(args.contains(&"localport=47990".to_string()));
    }

    /// A host moved off Sunshine's 47990 opens the port it binds; clients learn it in-band.
    #[test]
    fn the_mgmt_rule_follows_the_host_env_bind() {
        assert_eq!(mgmt_port(""), 47990);
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\r\n"), 47991);
        assert_eq!(mgmt_port("# PUNKTFUNK_MGMT_BIND=0.0.0.0:48123\n"), 47990);
        assert_eq!(
            mgmt_port("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\nPUNKTFUNK_MGMT_BIND=\n"),
            47990
        );
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=\"[::]:48123\"\n"), 48123);
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=nonsense\n"), 47990);
    }

    /// `--mgmt-bind` writes the line the mgmt rule reads back, whatever host.env already holds.
    #[test]
    fn the_mgmt_bind_flag_lands_where_the_rule_reads() {
        let set =
            |text: &str| mgmt_port(&with_env_line(text, "PUNKTFUNK_MGMT_BIND", "0.0.0.0:47991"));
        assert_eq!(set(""), 47991);
        assert_eq!(
            set("RUST_LOG=info\n# PUNKTFUNK_MGMT_BIND=0.0.0.0:1\n"),
            47991
        );
        assert_eq!(
            set("PUNKTFUNK_MGMT_BIND=0.0.0.0:1\nPUNKTFUNK_MGMT_BIND=0.0.0.0:2\n"),
            47991
        );
        assert_eq!(
            with_env_line("A=1\n# K=0\nK=2\n", "K", "3"),
            "A=1\n# K=0\nK=3\n"
        );
    }

    /// The installer's `--gamestream` moves to the store without turning GameStream off.
    #[test]
    fn gamestream_moves_from_the_host_command_to_the_store() {
        let legacy = "# PUNKTFUNK_HOST_CMD=serve\nPUNKTFUNK_HOST_CMD=serve --gamestream\n";
        let (store, text, pinned) = gamestream_migration(legacy, None);
        assert_eq!(store, Some(true));
        assert_eq!(
            text.as_deref(),
            Some("# PUNKTFUNK_HOST_CMD=serve\nPUNKTFUNK_HOST_CMD=serve\n")
        );
        assert!(!pinned);
        // No line ran `serve --gamestream`.
        let (store, text, _) = gamestream_migration("RUST_LOG=info\n", None);
        assert_eq!(store, Some(true));
        assert_eq!(
            text.as_deref(),
            Some("RUST_LOG=info\nPUNKTFUNK_HOST_CMD=serve\n")
        );
        assert_eq!(gamestream_migration(legacy, Some(false)).0, Some(false));
        // `serve` keeps whatever the store holds unless the installer chose.
        assert_eq!(
            gamestream_migration("PUNKTFUNK_HOST_CMD=serve\n", None),
            (None, None, false)
        );
        assert_eq!(
            gamestream_migration("PUNKTFUNK_HOST_CMD=serve\n", Some(true)),
            (Some(true), None, false)
        );
        assert_eq!(
            gamestream_migration(
                "PUNKTFUNK_HOST_CMD=serve --gamestream --open\n",
                Some(false)
            ),
            (Some(false), None, true)
        );
    }
}
