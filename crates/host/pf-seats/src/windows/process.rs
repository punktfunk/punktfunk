//! Suspended launch and per-seat job containment.
//!
//! Every seat cycle owns one unnamed `KILL_ON_JOB_CLOSE` job for its keeper,
//! which starts suspended, enters the job and only then resumes. The quality gate
//! and host run in the seat's session and join the cycle's job too, so they end with the
//! service as well as when their [`ChildProcess`] drops and when the cycle logs the session
//! off. The RDP bootstrap travels through one inherited anonymous stdin pipe, never argv or
//! environment. A child inherits only the handles its [`Inherit`] list names, whatever another
//! seat's spawn has made inheritable meanwhile. Session children use a duplicated LocalSystem
//! primary token retargeted with `TokenSessionId` and the `winsta0\default` desktop. Host
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
    AssignProcessToJobObject, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CreateProcessW, DeleteProcThreadAttributeList, GetCurrentProcess,
    GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken, ResumeThread,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

pub(super) struct Job {
    handle: OwnedHandle,
}

impl Job {
    pub(super) fn new() -> WinResult<Self> {
        // The host's own children (games, Steam) stay out: they end with the session logoff,
        // which gives them a graceful exit that a job's kill does not.
        let handle = pf_paths_win::kill_on_close_job(
            JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        )
        .map_err(|error| io_error("job_create", "create the kill-on-close job", error))?;
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

    pub(super) fn terminate(&self) {
        // SAFETY: the process handle remains live; termination is idempotent after process exit.
        let _ = unsafe { TerminateProcess(HANDLE(self.process.as_raw_handle()), 1) };
    }
}

/// Start `punktfunk-seat-keeper.exe keep` suspended in `job`, hand it `bootstrap` on stdin,
/// then resume it. Its stderr is appended to `log`. No secret crosses argv or the environment.
pub(super) fn spawn_keeper(
    job: &Job,
    keeper: &Path,
    bootstrap: &RdpBootstrap,
    log: &Path,
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
    let log = open_log(log)?;

    let application = wide(keeper.as_os_str(), "keeper executable")?;
    let mut command_w = command_line(&[keeper.as_os_str().to_owned(), OsString::from("keep")])?;
    command_w.push(0);
    let (stdin, stdout, stderr) = (
        HANDLE(read_pipe.as_raw_handle()),
        HANDLE(nul.as_raw_handle()),
        HANDLE(log.as_raw_handle()),
    );
    let inherit = Inherit::new(&[stdin, stdout, stderr])?;
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: stdin,
            hStdOutput: stdout,
            hStdError: stderr,
            ..Default::default()
        },
        lpAttributeList: inherit.list(),
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: all pointers target live NUL-terminated buffers. `startup` is a STARTUPINFOEXW whose
    // `cb` says so, and its list, alive in `inherit`, names stdin, NUL and the log: the child
    // inherits those three and no other handle.
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_w.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            None,
            PCWSTR::null(),
            std::ptr::from_ref(&startup).cast::<STARTUPINFOW>(),
            &mut info,
        )
    }
    .map_err(|error| io_error("keeper_spawn", "CreateProcessW(seat keeper)", error))?;
    drop(inherit);
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
/// `log`, and in `job` when there is one. The returned handle terminates it on drop.
pub(super) fn spawn_in_session(
    job: Option<&Job>,
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
    let output = HANDLE(log.as_raw_handle());
    let inherit = Inherit::new(&[output])?;
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: output,
            hStdError: output,
            ..Default::default()
        },
        lpAttributeList: inherit.list(),
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: token is a live primary token for the target session and all buffers remain live.
    // `startup` is a STARTUPINFOEXW whose `cb` says so, and its list, alive in `inherit`, names
    // the log alone: the child inherits it as stdout and stderr, and no other handle.
    unsafe {
        CreateProcessAsUserW(
            Some(HANDLE(token.as_raw_handle())),
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_w.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW
                | EXTENDED_STARTUPINFO_PRESENT,
            Some(environment.as_ptr().cast::<c_void>()),
            PCWSTR(workdir_w.as_ptr()),
            std::ptr::from_ref(&startup).cast::<STARTUPINFOW>(),
            &mut info,
        )
    }
    .map_err(|error| io_error("process_spawn", "CreateProcessAsUserW failed", error))?;
    drop(inherit);
    finish_suspended_spawn(info, job)
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

/// A `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: the one set of handles a child inherits. Seats spawn
/// on threads of their own, so a handle another seat's spawn holds inheritable at that moment
/// would otherwise reach this child too. The list is deleted when this drops.
struct Inherit {
    /// The opaque list, which Windows lays out in place; `usize` keeps it pointer-aligned.
    list: Vec<usize>,
    /// What the list points at. Boxed, so its address holds while `Inherit` moves.
    handles: Box<[HANDLE]>,
}

impl Inherit {
    /// `handles` must be inheritable and distinct.
    fn new(handles: &[HANDLE]) -> WinResult<Self> {
        let mut bytes = 0_usize;
        // SAFETY: with no list, the call only writes the size one attribute needs to `bytes`, and
        // fails by design with ERROR_INSUFFICIENT_BUFFER.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut bytes) };
        let mut list = vec![0_usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        let raw = LPPROC_THREAD_ATTRIBUTE_LIST(list.as_mut_ptr().cast());
        // SAFETY: `list` holds at least `bytes` writable, pointer-aligned bytes and outlives the
        // list, which `Drop` deletes before the vector is freed.
        unsafe { InitializeProcThreadAttributeList(Some(raw), 1, None, &mut bytes) }
            .map_err(|error| io_error("process_spawn", "initialize the inherit list", error))?;
        let size = std::mem::size_of_val(handles);
        let inherit = Self {
            list,
            handles: handles.into(),
        };
        // SAFETY: the list was initialized above with room for one attribute. It keeps a pointer
        // to `handles`, a heap slice of `size` bytes that `inherit` owns as long as the list.
        unsafe {
            UpdateProcThreadAttribute(
                raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(inherit.handles.as_ptr().cast()),
                size,
                None,
                None,
            )
        }
        .map_err(|error| io_error("process_spawn", "name the inherited handles", error))?;
        Ok(inherit)
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.list.as_ptr().cast_mut().cast())
    }
}

impl Drop for Inherit {
    fn drop(&mut self) {
        // SAFETY: `new` initialized the list before any `Inherit` existed, and it is deleted once,
        // here, while its storage and the handles it names are still alive.
        unsafe {
            DeleteProcThreadAttributeList(LPPROC_THREAD_ATTRIBUTE_LIST(
                self.list.as_mut_ptr().cast(),
            ));
        }
    }
}

fn finish_suspended_spawn(info: PROCESS_INFORMATION, job: Option<&Job>) -> WinResult<ChildProcess> {
    // SAFETY: successful process creation returned two distinct owned handles; transfer each once.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    // SAFETY: this is the distinct initial-thread handle and is transferred once.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread.0) };
    if let Some(job) = job {
        // SAFETY: job and suspended process handles are live; assignment completes synchronously.
        let assigned =
            unsafe { AssignProcessToJobObject(job.raw(), HANDLE(process.as_raw_handle())) };
        // A refusal leaves the handle's drop and the logoff to end the child.
        if let Err(error) = assigned {
            tracing::warn!(%error, "seat child not in the cycle's job");
        }
    }
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
/// so an isolate it made is never restored and leaves the session with no lit display. A row on a
/// person's own account gets `PUNKTFUNK_SEAT_OWN_ACCOUNT=1`, as a Linux one does.
pub(super) fn seat_environment(
    host_root: &Path,
    seat: &Seat,
    steam: Option<&Path>,
    isolate: bool,
) -> WinResult<Vec<u16>> {
    let mut entries: BTreeMap<String, OsString> = BTreeMap::new();
    // `CommonProgramFiles(x86)` locates Steam's streaming audio INFs, which mint a seat's sound.
    for name in [
        "ComSpec",
        "CommonProgramFiles",
        "CommonProgramFiles(x86)",
        "CommonProgramW6432",
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
        // Only the box advertises; a client reaches a seat through its redirect.
        ("PUNKTFUNK_MDNS", OsString::from("0")),
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
    if seat.adopted_sid.is_some() {
        entries.insert("PUNKTFUNK_SEAT_OWN_ACCOUNT".into(), OsString::from("1"));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::{util, wts};
    use std::time::{Duration, Instant};

    /// Only a row on a person's own account carries the marker its host reads.
    #[test]
    fn only_an_adopted_row_s_host_is_marked_as_a_person_s() {
        let mut ledger = crate::model::Ledger::new();
        let request = |name: &str| crate::model::CreateSeat {
            name: name.into(),
            account: name.into(),
            autostart: false,
        };
        let made = ledger.allocate(request("pf_seat1")).unwrap();
        let sid = "S-1-5-21-1-2-3-1001".to_owned();
        let adopted = ledger.allocate_adopted(request("Ben"), sid).unwrap();
        let marked = |seat: &Seat| {
            let block = seat_environment(Path::new(r"C:\seat"), seat, None, true).unwrap();
            String::from_utf16_lossy(&block)
                .split('\0')
                .any(|entry| entry == "PUNKTFUNK_SEAT_OWN_ACCOUNT=1")
        };
        assert!(marked(&adopted));
        assert!(!marked(&made));
    }

    /// A child holds the inheritable handle its list names, and not one the list leaves out.
    #[test]
    fn a_child_inherits_only_the_handles_its_list_names() {
        use windows::Win32::Foundation::{
            CompareObjectHandles, DuplicateHandle, DUPLICATE_SAME_ACCESS,
        };
        use windows::Win32::System::Threading::CreateEventW;
        let inheritable = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            bInheritHandle: true.into(),
            ..Default::default()
        };
        let event = || {
            // SAFETY: an unnamed event whose handle the returned owner closes.
            let raw = unsafe { CreateEventW(Some(&inheritable), true, false, None) }.unwrap();
            // SAFETY: `raw` is fresh and has no other owner.
            unsafe { OwnedHandle::from_raw_handle(raw.0) }
        };
        let (listed, stray) = (event(), event());
        let inherit = Inherit::new(&[HANDLE(listed.as_raw_handle())]).unwrap();
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                ..Default::default()
            },
            lpAttributeList: inherit.list(),
        };
        let cmd = wide(pf_paths::system32("cmd.exe"), "cmd").unwrap();
        let mut info = PROCESS_INFORMATION::default();
        // SAFETY: as in `spawn_keeper`; the child stays suspended and is ended below.
        unsafe {
            CreateProcessW(
                PCWSTR(cmd.as_ptr()),
                None,
                None,
                None,
                true,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                None,
                PCWSTR::null(),
                std::ptr::from_ref(&startup).cast::<STARTUPINFOW>(),
                &mut info,
            )
        }
        .unwrap();
        // SAFETY: the creation returned two handles this test owns from here.
        let (child, _thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(info.hProcess.0),
                OwnedHandle::from_raw_handle(info.hThread.0),
            )
        };
        // An inherited handle keeps its value in the child, and names the same object there.
        let held = |handle: &OwnedHandle| {
            let mut copy = HANDLE::default();
            // SAFETY: both process handles are live; `copy` receives a handle owned below.
            let copied = unsafe {
                DuplicateHandle(
                    HANDLE(child.as_raw_handle()),
                    HANDLE(handle.as_raw_handle()),
                    GetCurrentProcess(),
                    &mut copy,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            copied.is_ok_and(|()| {
                // SAFETY: the duplicate is fresh and closed when this owner drops.
                let copy = unsafe { OwnedHandle::from_raw_handle(copy.0) };
                // SAFETY: both handles are live for the call.
                unsafe {
                    CompareObjectHandles(
                        HANDLE(copy.as_raw_handle()),
                        HANDLE(handle.as_raw_handle()),
                    )
                }
                .as_bool()
            })
        };
        let (got_listed, got_stray) = (held(&listed), held(&stray));
        // SAFETY: the child is still suspended and its handle live.
        let _ = unsafe { TerminateProcess(HANDLE(child.as_raw_handle()), 1) };
        assert!(got_listed, "the listed handle is inherited");
        assert!(!got_stray, "a handle left out of the list is not");
    }

    /// A child in another session ends when its job closes, as it must when the service dies.
    /// It needs a LocalSystem allowed to start a process in the console session, which a CI
    /// runner's service context is not, so it runs on demand with `--ignored`.
    #[test]
    #[ignore = "needs LocalSystem that may start a process in the console session"]
    fn a_job_ends_a_child_in_another_session() {
        if !util::is_local_system().unwrap_or(false) {
            eprintln!("skipped: not LocalSystem");
            return;
        }
        let Some(session) = wts::enumerate()
            .unwrap()
            .into_iter()
            .find(|session| session.console)
        else {
            eprintln!("skipped: no console session");
            return;
        };
        let root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let system32 = Path::new(&root).join("System32");
        let mut environment: Vec<u16> = OsStr::new("SystemRoot=")
            .encode_wide()
            .chain(OsStr::new(&root).encode_wide())
            .collect();
        environment.extend([0, 0]);
        let temp = tempfile::tempdir().unwrap();

        let job = Job::new().unwrap();
        let child = spawn_in_session(
            Some(&job),
            session.id,
            &system32.join("PING.EXE"),
            &[
                OsString::from("-n"),
                OsString::from("120"),
                OsString::from("127.0.0.1"),
            ],
            &environment,
            &system32,
            &temp.path().join("child.log"),
        )
        .unwrap();
        assert_eq!(child.exit_code().unwrap(), None, "the child runs");
        eprintln!("child {} runs in session {}", child.pid(), session.id);

        drop(job);
        let give_up = Instant::now() + Duration::from_secs(5);
        while child.exit_code().unwrap().is_none() {
            assert!(Instant::now() < give_up, "the job left its child running");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
