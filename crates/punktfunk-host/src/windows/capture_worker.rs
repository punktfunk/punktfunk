//! Start the capture worker: the process that captures a monitor this host did not create.
//!
//! Windows Graphics Capture does not activate for SYSTEM, so the worker runs with the token
//! of the signed-in user of this host's session, on `winsta0\default`. It is handed three pipe
//! ends and nothing else of this process: the handle list names exactly those, so no other
//! inheritable handle of a SYSTEM process crosses into a user's. Unlike a game launch it
//! does not break away from the service's kill-on-close job, so it never outlives the host.
//! Evidence: `design/windows-wgc-capture.md` §4.2, §4.13.

use anyhow::{Context, Result};
use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    UpdateProcThreadAttribute, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

const WORKER_EXE: &str = "punktfunk-capture-worker.exe";

/// The worker's image: beside this one, where the installer puts both. The directory is one
/// the user cannot write, which is what makes running it with their token safe.
fn worker_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .context("locate the host executable")?
        .with_file_name(WORKER_EXE);
    anyhow::ensure!(exe.is_file(), "{} is not installed", exe.display());
    Ok(exe)
}

/// One anonymous pipe as `(read, write)`, both ends inheritable.
fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: true.into(),
    };
    let (mut read, mut write) = (HANDLE::default(), HANDLE::default());
    // SAFETY: both out-params are live locals and `sa` outlives the call.
    unsafe { CreatePipe(&mut read, &mut write, Some(&sa), 0) }.context("CreatePipe")?;
    // SAFETY: the call succeeded, so both handles are new and this frame alone owns them.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        )
    })
}

/// Keep `end` out of the child: this process's side of a pipe.
fn keep(end: &OwnedHandle) -> Result<()> {
    // SAFETY: `end` is a live handle this process owns.
    unsafe {
        SetHandleInformation(
            HANDLE(end.as_raw_handle()),
            HANDLE_FLAG_INHERIT.0,
            HANDLE_FLAGS(0),
        )
    }
    .context("SetHandleInformation")
}

/// Start a capture worker as the signed-in user of this process's session. Fails when nobody
/// is signed in: there is then no desktop a worker could capture.
pub(crate) fn spawn() -> Result<pf_capture::WorkerProcess> {
    let exe = worker_exe()?;
    let primary = super::interactive::session_user_token()?;
    let env = super::interactive::user_env_block(&primary);

    // (child reads, host writes), (host reads, child writes), (host reads, child's stderr).
    let (requests_child, requests) = pipe()?;
    let (replies, replies_child) = pipe()?;
    let (log, log_child) = pipe()?;
    for ours in [&requests, &replies, &log] {
        keep(ours)?;
    }
    let raw = |h: &OwnedHandle| HANDLE(h.as_raw_handle());
    let inherited = [raw(&requests_child), raw(&replies_child), raw(&log_child)];

    let mut bytes = 0usize;
    // SAFETY: the sizing call; it reports the bytes needed through `bytes` and fails by design.
    let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut bytes) };
    let mut list_buf = vec![0u8; bytes];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(list_buf.as_mut_ptr().cast());
    // SAFETY: `list_buf` is `bytes` long, as the sizing call asked, and outlives the list.
    unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut bytes) }
        .context("InitializeProcThreadAttributeList")?;
    // SAFETY: `list` is initialised; `inherited` is a live array of live handles that outlives
    // the process creation below, and the size is its own.
    let listed = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(inherited.as_ptr().cast()),
            size_of_val(&inherited),
            None,
            None,
        )
    };

    // The user's interactive desktop is not inherited from this LocalSystem caller.
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.lpDesktop = PWSTR(desktop.as_mut_ptr());
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdError = raw(&log_child);
    si.lpAttributeList = list;
    let mut cmd: Vec<u16> = format!(
        "\"{}\" --control {},{}",
        exe.display(),
        raw(&requests_child).0 as usize,
        raw(&replies_child).0 as usize
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect();
    let cwd = exe.parent().map(|d| HSTRING::from(d.as_os_str()));
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `primary` is the live primary token; `cmd`, `desktop`, `cwd` and `env` are
    // NUL-terminated locals that outlive the call (`env` doubly so); `si` is a whole
    // STARTUPINFOEXW whose list is initialised above; `pi` is a live out-param.
    let created = listed.and_then(|()| unsafe {
        CreateProcessAsUserW(
            Some(*primary),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
            Some(env.as_ptr().cast()),
            cwd.as_ref().map_or(PCWSTR::null(), |d| PCWSTR(d.as_ptr())),
            &si.StartupInfo,
            &mut pi,
        )
    });
    // SAFETY: `list` was initialised above and is not used after this.
    unsafe { DeleteProcThreadAttributeList(list) };
    created.context("CreateProcessAsUserW (capture worker)")?;
    // SAFETY: the launch succeeded, so both handles in `pi` are this frame's alone.
    let (process, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(pi.hProcess.0),
            OwnedHandle::from_raw_handle(pi.hThread.0),
        )
    };
    tracing::info!(pid = pi.dwProcessId, exe = %exe.display(), "capture worker started");
    // The child's ends close here as they drop; the child holds its own copies.
    Ok(pf_capture::WorkerProcess {
        process,
        pid: pi.dwProcessId,
        requests: File::from(requests),
        replies: File::from(replies),
        log: File::from(log),
    })
}
