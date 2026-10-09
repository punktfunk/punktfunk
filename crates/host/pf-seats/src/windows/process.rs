//! Suspended launch and per-seat job containment.
//!
//! Every seat cycle owns one unnamed `KILL_ON_JOB_CLOSE` job for its keeper,
//! which starts suspended, enters the job and only then resumes. The quality gate
//! and host run in the seat's session, which a session-0 job cannot hold, so they
//! end when their [`ChildProcess`] drops and when the cycle logs the session off. The RDP
//! bootstrap travels through one inherited anonymous stdin pipe, never argv or
//! environment. Session children use a duplicated LocalSystem primary token
//! retargeted with `TokenSessionId` and the `winsta0\default` desktop. Host
//! environment construction uses an allow-listed system base plus exact seat
//! variables, so control credentials cannot leak through inheritance.

use crate::bootstrap::RdpBootstrap;
use crate::model::Seat;
use crate::windows::util::{backend_error, io_error, wide, WinResult};
use std::collections::BTreeMap;
use std::ffi::{c_void, OsStr, OsString};
use std::io::Write as _;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::Path;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    SetHandleInformation, GENERIC_WRITE, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
    SECURITY_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS,
    TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_ALWAYS, OPEN_EXISTING,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

pub(super) struct Job {
    handle: OwnedHandle,
}

impl Job {
    pub(super) fn new() -> WinResult<Self> {
        // SAFETY: null attributes and name request a fresh unnamed job with default security.
        let raw = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|error| io_error("job_create", "CreateJobObjectW failed", error))?;
        // SAFETY: `raw` is newly returned and has no owner; transfer it exactly once.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.0) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: handle is a live job and `limits` has the exact selected information type.
        unsafe {
            SetInformationJobObject(
                HANDLE(handle.as_raw_handle()),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|error| io_error("job_create", "SetInformationJobObject failed", error))?;
        Ok(Self { handle })
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

pub(super) struct ChildProcess {
    process: OwnedHandle,
    pid: u32,
    /// Outside any job: dropping the handle terminates the process.
    kill_on_drop: bool,
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        if self.kill_on_drop {
            self.terminate();
        }
    }
}

impl ChildProcess {
    pub(super) fn pid(&self) -> u32 {
        self.pid
    }

    pub(super) fn exit_code(&self) -> WinResult<Option<u32>> {
        // SAFETY: the process handle is owned by `self` and remains live during this zero-time wait.
        let wait = unsafe { WaitForSingleObject(HANDLE(self.process.as_raw_handle()), 0) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(io_error(
                "process_wait",
                "WaitForSingleObject failed",
                std::io::Error::last_os_error(),
            ));
        }
        let mut code = 0_u32;
        // SAFETY: the signalled process handle is live and `code` is a writable out-parameter.
        unsafe { GetExitCodeProcess(HANDLE(self.process.as_raw_handle()), &mut code) }
            .map_err(|error| io_error("process_wait", "GetExitCodeProcess failed", error))?;
        Ok(Some(code))
    }

    pub(super) fn wait(
        &self,
        timeout: Duration,
        mut cancelled: impl FnMut() -> bool,
    ) -> WinResult<Option<u32>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.exit_code()? {
                return Ok(Some(code));
            }
            if cancelled() {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub(super) fn terminate(&self) {
        // SAFETY: the process handle remains live; termination is idempotent after process exit.
        let _ = unsafe { TerminateProcess(HANDLE(self.process.as_raw_handle()), 1) };
    }
}

/// Start `punktfunk-seat-keeper.exe keep` suspended in `job`, hand it `bootstrap` on stdin,
/// then resume it. No secret crosses argv or the environment.
pub(super) fn spawn_keeper(
    job: &Job,
    keeper: &Path,
    bootstrap: &RdpBootstrap,
) -> WinResult<ChildProcess> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let mut read_raw = HANDLE::default();
    let mut write_raw = HANDLE::default();
    // SAFETY: both outputs are live and attributes requests inheritable handles with no descriptor.
    unsafe { CreatePipe(&mut read_raw, &mut write_raw, Some(&attributes), 0) }
        .map_err(|error| io_error("keeper_spawn", "CreatePipe failed", error))?;
    // SAFETY: each raw pipe handle is fresh and transferred into one owner immediately.
    let read_pipe = unsafe { OwnedHandle::from_raw_handle(read_raw.0) };
    // SAFETY: this is the distinct fresh write handle and is transferred once.
    let write_pipe = unsafe { OwnedHandle::from_raw_handle(write_raw.0) };
    // SAFETY: the write handle is live; clearing inheritance changes only its handle flags.
    unsafe {
        SetHandleInformation(
            HANDLE(write_pipe.as_raw_handle()),
            HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAGS(0),
        )
    }
    .map_err(|error| io_error("keeper_spawn", "SetHandleInformation failed", error))?;

    // SAFETY: NUL is a kernel device; attributes and returned handle stay valid as documented.
    let nul_raw = unsafe {
        CreateFileW(
            w!("NUL"),
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&attributes),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| io_error("keeper_spawn", "open NUL failed", error))?;
    // SAFETY: `nul_raw` is fresh and transferred to one owner.
    let nul = unsafe { OwnedHandle::from_raw_handle(nul_raw.0) };

    let application = wide(keeper.as_os_str(), "keeper executable")?;
    let mut command_w = command_line(&[keeper.as_os_str().to_owned(), OsString::from("keep")])?;
    command_w.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: HANDLE(read_pipe.as_raw_handle()),
        hStdOutput: HANDLE(nul.as_raw_handle()),
        hStdError: HANDLE(nul.as_raw_handle()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all pointers target live NUL-terminated buffers; only stdin/NUL are inheritable.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_w.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut info,
        )
    }
    .map_err(|error| io_error("keeper_spawn", "CreateProcessW(seat keeper)", error))?;
    finish_keeper_spawn(job, info, write_pipe, bootstrap)
}

fn finish_keeper_spawn(
    job: &Job,
    info: PROCESS_INFORMATION,
    write_pipe: OwnedHandle,
    bootstrap: &RdpBootstrap,
) -> WinResult<ChildProcess> {
    // SAFETY: successful CreateProcessW returned two distinct owned handles; transfer each once.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    // SAFETY: this is the distinct initial-thread handle and is transferred once.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread.0) };
    // SAFETY: job and suspended process handles are live; assignment completes synchronously.
    let assigned = unsafe { AssignProcessToJobObject(job.raw(), HANDLE(process.as_raw_handle())) };
    if let Err(error) = assigned {
        // SAFETY: the process is still suspended and its handle remains live.
        let _ = unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 1) };
        return Err(io_error(
            "job_assign",
            "assign RDP keeper to seat job",
            error,
        ));
    }
    let frame = bootstrap
        .encode()
        .map_err(|error| io_error("keeper_bootstrap", "encode RDP bootstrap", error))?;
    let mut writer = std::fs::File::from(write_pipe);
    if let Err(error) = writer.write_all(&frame) {
        // SAFETY: the process is still suspended and its handle remains live.
        let _ = unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 1) };
        return Err(io_error(
            "keeper_bootstrap",
            "write RDP keeper bootstrap",
            error,
        ));
    }
    drop(writer);
    // SAFETY: the initial thread is live and suspended exactly once by CREATE_SUSPENDED.
    let resumed = unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) };
    if resumed == u32::MAX {
        // SAFETY: resume failed while the process handle remains live; terminate before returning.
        let _ = unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 1) };
        return Err(io_error(
            "keeper_spawn",
            "ResumeThread(seat keeper)",
            std::io::Error::last_os_error(),
        ));
    }
    drop(thread);
    Ok(ChildProcess {
        process,
        pid: info.dwProcessId,
        kill_on_drop: false,
    })
}

/// Start `executable` in session `session_id` as LocalSystem, its stdout and stderr appended to
/// `log`. It runs outside the cycle's job, which lives in session 0; the returned handle
/// terminates it on drop.
pub(super) fn spawn_in_session(
    session_id: u32,
    executable: &Path,
    arguments: &[OsString],
    environment: &[u16],
    workdir: &Path,
    log: &Path,
) -> WinResult<ChildProcess> {
    let token = session_system_token(session_id)?;
    let application = wide(executable.as_os_str(), "child executable")?;
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(executable.as_os_str().to_owned());
    argv.extend_from_slice(arguments);
    let mut command_w = command_line(&argv)?;
    command_w.push(0);
    let workdir_w = wide(workdir.as_os_str(), "child working directory")?;
    let mut desktop = wide("winsta0\\default", "child desktop")?;
    let log = open_log(log)?;
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        dwFlags: STARTF_USESTDHANDLES,
        hStdOutput: HANDLE(log.as_raw_handle()),
        hStdError: HANDLE(log.as_raw_handle()),
        ..Default::default()
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: token is a live primary token for the target session; all buffers remain live, and
    // the log handle is the inheritable one the child's stdout and stderr name.
    unsafe {
        CreateProcessAsUserW(
            Some(HANDLE(token.as_raw_handle())),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_w.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(environment.as_ptr().cast::<c_void>()),
            PCWSTR(workdir_w.as_ptr()),
            &startup,
            &mut info,
        )
    }
    .map_err(|error| io_error("process_spawn", "CreateProcessAsUserW failed", error))?;
    finish_suspended_spawn(info)
}

/// A log past this size moves to `<log>.old` at the next spawn, so a crash loop can't fill the disk.
const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// `path` opened for appending through an inheritable handle, rotated first when large.
fn open_log(path: &Path) -> WinResult<OwnedHandle> {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= LOG_ROTATE_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = std::fs::rename(path, Path::new(&old));
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let path_w = wide(path.as_os_str(), "seat log")?;
    // Append mask: bare `FILE_APPEND_DATA` gives a child handle that drops its writes.
    let access = (FILE_GENERIC_WRITE.0 & !FILE_WRITE_DATA.0) | FILE_APPEND_DATA.0;
    // SAFETY: `path_w` is NUL-terminated and `attributes` sized; both outlive the call.
    let raw = unsafe {
        CreateFileW(
            PCWSTR(path_w.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            Some(&attributes),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| io_error("seat_log", "open the seat log", error))?;
    // SAFETY: `raw` is a fresh handle with no other owner.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw.0) })
}

fn finish_suspended_spawn(info: PROCESS_INFORMATION) -> WinResult<ChildProcess> {
    // SAFETY: successful process creation returned two distinct owned handles; transfer each once.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    // SAFETY: this is the distinct initial-thread handle and is transferred once.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread.0) };
    // SAFETY: the initial thread is live and suspended exactly once by CREATE_SUSPENDED.
    if unsafe { ResumeThread(HANDLE(thread.as_raw_handle())) } == u32::MAX {
        // SAFETY: process handle remains live and the process must not escape suspended.
        let _ = unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 1) };
        return Err(io_error(
            "process_spawn",
            "ResumeThread failed",
            std::io::Error::last_os_error(),
        ));
    }
    drop(thread);
    Ok(ChildProcess {
        process,
        pid: info.dwProcessId,
        kill_on_drop: true,
    })
}

fn session_system_token(session_id: u32) -> WinResult<OwnedHandle> {
    let mut process_token = HANDLE::default();
    // SAFETY: current-process pseudo-handle stays valid; output receives one owned token.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE
                | TOKEN_QUERY
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ADJUST_SESSIONID,
            &mut process_token,
        )
    }
    .map_err(|error| io_error("process_token", "OpenProcessToken failed", error))?;
    // SAFETY: this fresh token handle has no other owner and is transferred once.
    let process_token = unsafe { OwnedHandle::from_raw_handle(process_token.0) };
    let mut primary = HANDLE::default();
    // SAFETY: source token is live and output receives a distinct owned primary token.
    unsafe {
        DuplicateTokenEx(
            HANDLE(process_token.as_raw_handle()),
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    }
    .map_err(|error| io_error("process_token", "DuplicateTokenEx failed", error))?;
    // SAFETY: this fresh primary token has no other owner and is transferred once.
    let primary = unsafe { OwnedHandle::from_raw_handle(primary.0) };
    // SAFETY: primary is live; the value pointer and exact u32 size match TokenSessionId.
    unsafe {
        SetTokenInformation(
            HANDLE(primary.as_raw_handle()),
            TokenSessionId,
            std::ptr::from_ref(&session_id).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .map_err(|error| {
        io_error(
            "process_token",
            "SetTokenInformation(TokenSessionId) failed",
            error,
        )
    })?;
    Ok(primary)
}

/// `steam` is the seat's own `steam.exe` ([`super::steam::seat_exe`]), set when the box has Steam.
/// `isolate` false extends the virtual display instead: the quality gate exits inside its linger,
/// so an isolate it made is never restored and leaves the session with no lit display.
pub(super) fn seat_environment(
    host_root: &Path,
    seat: &Seat,
    steam: Option<&Path>,
    isolate: bool,
) -> WinResult<Vec<u16>> {
    let mut entries: BTreeMap<String, OsString> = BTreeMap::new();
    for name in [
        "ComSpec",
        "NUMBER_OF_PROCESSORS",
        "PATH",
        "PATHEXT",
        "PROCESSOR_ARCHITECTURE",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "SystemDrive",
        "SystemRoot",
        "TEMP",
        "TMP",
        "WINDIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            entries.insert(name.to_ascii_uppercase(), value);
        }
    }
    let rust_log = std::env::var_os("RUST_LOG").unwrap_or_else(|| OsString::from("punktfunk=info"));
    entries.insert("RUST_LOG".into(), rust_log);
    let values = [
        ("PUNKTFUNK_CONFIG_DIR", host_root.as_os_str().to_owned()),
        // The box's identity, pairing, profiles and overlays, read only: the service's own
        // config dir is the box's.
        (
            "PUNKTFUNK_TRUST_DIR",
            pf_paths::config_dir().into_os_string(),
        ),
        // The box's catalog, read only: a Windows seat runs no plugins of its own.
        (
            "PUNKTFUNK_LIBRARY_DIR",
            pf_paths::config_dir().into_os_string(),
        ),
        ("PUNKTFUNK_PAIRING", OsString::from("refused")),
        ("PUNKTFUNK_SEAT_ID", OsString::from(seat.id.as_str())),
        ("PUNKTFUNK_SEAT_SESSION", OsString::from("1")),
        (
            "PUNKTFUNK_SEAT_DISPLAY_SLOT",
            OsString::from(seat.display_slot.to_string()),
        ),
        (
            "PUNKTFUNK_NATIVE_PORT",
            OsString::from(seat.native_port.to_string()),
        ),
        (
            "PUNKTFUNK_MGMT_BIND",
            OsString::from(format!("127.0.0.1:{}", seat.mgmt_port)),
        ),
        ("PUNKTFUNK_HOST_NAME", OsString::from(&seat.name)),
        ("PUNKTFUNK_GAMESTREAM", OsString::from("0")),
        (
            "PUNKTFUNK_AUDIO_OUTPUT_MODE",
            OsString::from("follow_default"),
        ),
    ];
    for (name, value) in values {
        entries.insert(name.into(), value);
    }
    if let Some(exe) = steam {
        entries.insert("PUNKTFUNK_SEAT_STEAM".into(), exe.as_os_str().to_owned());
    }
    if !isolate {
        entries.insert("PUNKTFUNK_NO_ISOLATE".into(), OsString::from("1"));
    }
    let mut block = Vec::new();
    for (name, value) in entries {
        if value.encode_wide().any(|unit| unit == 0) {
            return Err(backend_error(
                "process_environment",
                format!("environment value for {name} contains NUL"),
            ));
        }
        block.extend(name.encode_utf16());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

fn command_line(arguments: &[OsString]) -> WinResult<Vec<u16>> {
    if arguments.is_empty() {
        return Err(backend_error(
            "process_spawn",
            "Windows command line needs an executable",
        ));
    }
    let mut output = Vec::new();
    for (index, argument) in arguments.iter().enumerate() {
        if index != 0 {
            output.push(b' ' as u16);
        }
        append_quoted(&mut output, argument)?;
    }
    Ok(output)
}

fn append_quoted(output: &mut Vec<u16>, value: &OsStr) -> WinResult<()> {
    let units: Vec<u16> = value.encode_wide().collect();
    if units.contains(&0) {
        return Err(backend_error(
            "process_spawn",
            "Windows argument contains an embedded NUL",
        ));
    }
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
    if !needs_quotes {
        output.extend_from_slice(&units);
        return Ok(());
    }
    output.push(b'"' as u16);
    let mut slashes = 0_usize;
    for unit in units {
        if unit == b'\\' as u16 {
            slashes += 1;
        } else if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
            output.push(unit);
            slashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            output.push(unit);
            slashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
    Ok(())
}
