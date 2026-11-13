//! Launch processes as the signed-in user of the host process's WTS session.
//!
//! Store handlers, appx activation, hooks, and the tray need that user's token
//! so per-user registry and authentication state resolve for the same seat as
//! the streaming host. The host itself stays LocalSystem.
//!
//! [`ProcessIdToSessionId`] binds the launch to the host's session, then
//! `WTSQueryUserToken` and `CreateProcessAsUserW` target `winsta0\default`.
//! The SCM supervisor in [`crate::service`] separately selects the active
//! console session when it starts the ordinary host.

use anyhow::{bail, Context, Result};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows::core::{Owned, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf,
    SecurityImpersonation, TokenElevation, TokenLinkedToken, TokenPrimary, TOKEN_ALL_ACCESS,
    TOKEN_ELEVATION, TOKEN_LINKED_TOKEN,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, TerminateJobObject, JOB_OBJECT_LIMIT,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetExitCodeProcess, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Resolves one process through an injected WTS session query. A failed query
/// returns its error even if it wrote the out-parameter first.
fn query_process_session<E>(
    process_id: u32,
    query: impl FnOnce(u32, &mut u32) -> std::result::Result<(), E>,
) -> std::result::Result<u32, E> {
    let mut session = 0;
    query(process_id, &mut session)?;
    Ok(session)
}

/// The WTS session that contains this host process.
pub(super) fn current_process_session_id() -> Result<u32> {
    query_process_session(std::process::id(), |id, session| {
        // SAFETY: `id` names the live current process and `session` is a local out-parameter that
        // remains valid for this synchronous call.
        unsafe { ProcessIdToSessionId(id, session) }
    })
    .context("ProcessIdToSessionId(current host process)")
}

/// Where to start `cmdline`'s executable: the directory holding it.
///
/// A child must never inherit the host's cwd. The service starts the host in its
/// install directory, so an inherited cwd puts every launch under
/// `C:\Program Files\punktfunk` — which Ryujinx tests
/// (`Environment.CurrentDirectory`) and refuses to run under, and which sends a
/// game's relative asset loads into the host's folder.
///
/// The first token is read as `CreateProcess` reads it: quoted, else up to the
/// first space. `None` for a non-absolute token (`explorer.exe <uri>`,
/// `cmd.exe /c …`) or a directory that is gone — both keep the inherited cwd
/// rather than failing the spawn.
fn exe_dir(cmdline: &str) -> Option<PathBuf> {
    let rest = cmdline.trim_start();
    let exe = Path::new(match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split(' ').next()?,
    });
    if !exe.is_absolute() {
        return None;
    }
    let dir = exe.parent()?;
    dir.is_dir().then(|| dir.to_path_buf())
}

/// Spawns `cmdline` as the signed-in user of this process's WTS session on
/// `winsta0\default`. Returns the new process id.
///
/// Fire-and-forget: the child's handles close on return; the process keeps
/// running. Environment is the user's block plus this process's
/// `PUNKTFUNK_*` / `RUST_LOG` (see [`merged_env_block`]).
///
/// Needs SYSTEM (`WTSQueryUserToken` requires `SE_TCB`). Fails when this
/// process's session has no signed-in user. `workdir` defaults to the
/// executable's own directory ([`exe_dir`]) — never this process's.
pub fn spawn_as_current_session_user(cmdline: &str, workdir: Option<&Path>) -> Result<u32> {
    let (_process, _thread, pid) = launch(cmdline, workdir, PROCESS_CREATION_FLAGS(0), &[], false)?;
    Ok(pid)
}

/// A process tree [`spawn_tree_as_current_session_user`] started. It ends when this value drops
/// or this host exits: the job holding it is kill-on-close, and only this value holds the job.
pub struct SessionTree {
    process: OwnedHandle,
    job: OwnedHandle,
}

impl SessionTree {
    /// The first process's exit code, `None` while it runs.
    pub fn exit_code(&self) -> Option<u32> {
        let process = HANDLE(self.process.as_raw_handle());
        // SAFETY: a live process handle this value owns; a zero wait returns at once.
        if unsafe { WaitForSingleObject(process, 0) } != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0u32;
        // SAFETY: as above, and `code` is a live out-param.
        unsafe { GetExitCodeProcess(process, &mut code) }.ok()?;
        Some(code)
    }

    /// Ends every process in the tree and waits up to `timeout` for the first to be gone, so
    /// what it held open is free.
    pub fn end(self, timeout: Duration) {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: live job and process handles this value owns; both calls are synchronous.
        unsafe {
            let _ = TerminateJobObject(HANDLE(self.job.as_raw_handle()), 1);
            let _ = WaitForSingleObject(HANDLE(self.process.as_raw_handle()), millis);
        }
    }
}

/// [`spawn_as_current_session_user`] with no window, in `workdir`, with `env` added, and every
/// process it starts in one job ([`SessionTree`]).
pub fn spawn_tree_as_current_session_user(
    cmdline: &str,
    workdir: &Path,
    env: &[(String, String)],
) -> Result<SessionTree> {
    let job = pf_paths_win::kill_on_close_job(JOB_OBJECT_LIMIT(0))
        .context("create the kill-on-close job")?;
    let flags = CREATE_SUSPENDED | CREATE_NO_WINDOW;
    let (process, thread, _) = launch(cmdline, Some(workdir), flags, env, false)?;
    let (child, main) = (*process, *thread);
    // Suspended until it is in the job: a child it started first would stay outside.
    // SAFETY: a live job handle and a live process handle with the rights its creator holds.
    let joined = unsafe { AssignProcessToJobObject(HANDLE(job.as_raw_handle()), child) };
    if let Err(e) = joined {
        // SAFETY: `process` is a live owned handle with the terminate right its creator holds.
        let _ = unsafe { TerminateProcess(child, 1) };
        return Err(e).context("AssignProcessToJobObject");
    }
    // SAFETY: the live handle of the suspended primary thread `launch` created.
    if unsafe { ResumeThread(main) } == u32::MAX {
        return Err(std::io::Error::last_os_error()).context("ResumeThread");
    }
    let raw: HANDLE = **std::mem::ManuallyDrop::new(process);
    // SAFETY: `process` gave its handle up above without closing it, so this closes it once.
    let process = unsafe { OwnedHandle::from_raw_handle(raw.0) };
    Ok(SessionTree { process, job })
}

/// [`spawn_as_current_session_user`] with `env` added, waited on for up to `timeout`. `admin`
/// launches on the user's administrator token ([`admin_session_user_token`]). The exit code,
/// or `None` when the process still ran at `timeout` and was ended.
pub fn run_as_current_session_user(
    cmdline: &str,
    env: &[(String, String)],
    admin: bool,
    timeout: Duration,
) -> Result<Option<u32>> {
    let (process, _, _) = launch(cmdline, None, PROCESS_CREATION_FLAGS(0), env, admin)?;
    let code = wait_exit(&process, timeout)?;
    if code.is_none() {
        // SAFETY: `process` is a live owned handle with the terminate right its creator holds.
        let _ = unsafe { TerminateProcess(*process, 1) };
    }
    Ok(code)
}

/// [`spawn_as_current_session_user`] for a console helper of our own: no window on the
/// user's desktop, and the caller learns the exit code. Errs when the helper is still
/// running after `timeout`; it keeps running on its own then.
pub fn run_hidden_as_current_session_user(cmdline: &str, timeout: Duration) -> Result<u32> {
    let (process, _, _) = launch(cmdline, None, CREATE_NO_WINDOW, &[], false)?;
    match wait_exit(&process, timeout)? {
        Some(code) => Ok(code),
        None => bail!("helper still running after {timeout:?}"),
    }
}

/// Waits up to `timeout` for `process`: its exit code, or `None` while it still runs.
fn wait_exit(process: &Owned<HANDLE>, timeout: Duration) -> Result<Option<u32>> {
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: `process` is a live owned handle for both calls and `code` a live out-param.
    let (waited, got, code) = unsafe {
        let waited = WaitForSingleObject(**process, millis);
        let mut code = 0u32;
        let got = GetExitCodeProcess(**process, &mut code);
        (waited, got, code)
    };
    if waited != WAIT_OBJECT_0 {
        return Ok(None);
    }
    got.context("GetExitCodeProcess")?;
    Ok(Some(code))
}

/// Runs `f` on the session user's `TEMP`, as that user ([`as_session_user`]). A file there stays
/// readable to a process launched as them; SYSTEM's own temp does not. The path comes from the
/// user's environment, so the file work must not carry this host's rights.
pub fn in_session_user_temp<R>(f: impl FnOnce(&Path) -> R) -> Result<R> {
    let temp = session_user_var("TEMP").context("the session user has no TEMP")?;
    as_session_user(|| f(Path::new(&temp)))
}

/// Variable `name` of the session user's own environment (`USERPROFILE`, `USERNAME`).
pub(crate) fn session_user_var(name: &str) -> Option<String> {
    let primary = session_user_token().ok()?;
    user_env_block(&primary, &[])
        .split(|&u| u == 0)
        .map(String::from_utf16_lossy)
        .find_map(|e| {
            let (k, v) = e.split_once('=')?;
            k.eq_ignore_ascii_case(name).then(|| v.to_string())
        })
}

/// Runs `f` on this thread as the session user: a file it opens is checked against their rights,
/// never this host's. A process `f` starts is still this host's.
pub fn as_session_user<R>(f: impl FnOnce() -> R) -> Result<R> {
    let primary = session_user_token()?;
    // SAFETY: `primary` is the live token above; `_revert` ends the impersonation on every path.
    unsafe { ImpersonateLoggedOnUser(*primary) }.context("ImpersonateLoggedOnUser")?;
    let _revert = Revert;
    Ok(f())
}

/// Ends this thread's impersonation when dropped.
struct Revert;

impl Drop for Revert {
    fn drop(&mut self) {
        // SAFETY: ends the impersonation this thread began; without one it changes nothing.
        let _ = unsafe { RevertToSelf() };
    }
}

/// A primary token for the signed-in user of this process's WTS session. Needs SYSTEM.
pub(super) fn session_user_token() -> Result<Owned<HANDLE>> {
    let session = current_process_session_id()?;
    let mut user_token = HANDLE::default();
    // SAFETY: `session` is a plain id and `user_token` a live local out-param.
    unsafe { WTSQueryUserToken(session, &mut user_token) }.context(
        "WTSQueryUserToken (host must be SYSTEM; its WTS session needs a signed-in user)",
    )?;
    // SAFETY: the query succeeded, so `user_token` is a token this frame alone owns.
    let user_token = unsafe { Owned::new(user_token) };

    let mut primary = HANDLE::default();
    // SAFETY: `user_token` is the live token just opened; `primary` is a live local out-param.
    unsafe {
        DuplicateTokenEx(
            *user_token,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    }
    .context("DuplicateTokenEx(TokenPrimary)")?;
    // SAFETY: the duplicate succeeded, so `primary` is a second token this frame alone owns.
    Ok(unsafe { Owned::new(primary) })
}

/// The session user's full administrator token, with no UAC prompt: the session token when it
/// is already elevated, else the token UAC links behind it. A standard user has neither.
fn admin_session_user_token() -> Result<Owned<HANDLE>> {
    let token: Owned<HANDLE> = session_user_token()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: `token` is live; `elevation` is a writable local of the length passed.
    unsafe {
        GetTokenInformation(
            *token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .context("GetTokenInformation(TokenElevation)")?;
    if elevation.TokenIsElevated != 0 {
        return Ok(token);
    }
    let mut linked = TOKEN_LINKED_TOKEN::default();
    // SAFETY: as above, for `linked`.
    unsafe {
        GetTokenInformation(
            *token,
            TokenLinkedToken,
            Some((&raw mut linked).cast()),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut len,
        )
    }
    .context("the signed-in user isn't an administrator")?;
    // SAFETY: the query succeeded, so `LinkedToken` is a handle this frame alone owns. SYSTEM
    // holds SE_TCB, so it is a primary token.
    Ok(unsafe { Owned::new(linked.LinkedToken) })
}

/// `primary`'s environment block with this host's settings and `extra` overlaid
/// ([`merged_env_block`]).
pub(super) fn user_env_block(primary: &Owned<HANDLE>, extra: &[(String, String)]) -> Vec<u16> {
    let mut env_block: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `env_block` is a live local out-param and `primary` a live token; on success
    // the call stores an owned block pointer, destroyed exactly once below.
    let _ = unsafe { CreateEnvironmentBlock(&mut env_block, Some(**primary), false) };
    // SAFETY: `env_block` is either still null (the call above failed) or the double-null-terminated
    // UTF-16 block `CreateEnvironmentBlock` just wrote — exactly the two states the helper accepts.
    let merged_env = unsafe { merged_env_block(env_block as *const u16, true, extra) };
    if !env_block.is_null() {
        // SAFETY: `env_block` is the live block from the call above, destroyed exactly once and not
        // read after — `merged_env` owns its own copy of the parsed entries.
        let _ = unsafe { DestroyEnvironmentBlock(env_block) };
    }
    merged_env
}

/// The `CreateProcessAsUserW` core the launchers share. `extra` joins the creation flags, `env`
/// joins the user's environment, and `admin` picks [`admin_session_user_token`].
/// Returns the child's process and primary thread handles, and its pid.
fn launch(
    cmdline: &str,
    workdir: Option<&Path>,
    extra: PROCESS_CREATION_FLAGS,
    env: &[(String, String)],
    admin: bool,
) -> Result<(Owned<HANDLE>, Owned<HANDLE>, u32)> {
    let primary = match admin {
        true => admin_session_user_token()?,
        false => session_user_token()?,
    };
    let merged_env = user_env_block(&primary, env);

    // The target user's interactive desktop is not inherited from the LocalSystem caller.
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };

    let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let workdir = workdir.map(Path::to_path_buf).or_else(|| exe_dir(cmdline));
    let workdir_w: Option<HSTRING> = workdir.map(|d| HSTRING::from(d.as_os_str()));
    let cwd = match &workdir_w {
        Some(w) => PCWSTR(w.as_ptr()),
        None => PCWSTR::null(),
    };

    let mut pi = PROCESS_INFORMATION::default();
    // The streaming host sits in a kill-on-close job that permits breakaway. A detached user
    // process must outlive that host; retry inside the job when policy refuses breakaway.
    let mut flags = CREATE_UNICODE_ENVIRONMENT | CREATE_BREAKAWAY_FROM_JOB | extra;
    let created = loop {
        // SAFETY: `primary` is the live primary token; `cmd`, `desktop` (via `si.lpDesktop`),
        // `workdir_w` (via `cwd`) and `merged_env` are locals that outlive the call, each
        // NUL-terminated as the API requires — `merged_env` doubly so, per `merged_env_block`.
        // `pi` is a live local out-param, and the API retains none of these pointers.
        let r = unsafe {
            CreateProcessAsUserW(
                Some(*primary),
                None,
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false, // no inherit: fire-and-forget; no stdio relay
                flags,
                Some(merged_env.as_ptr() as *const core::ffi::c_void),
                cwd,
                &si,
                &mut pi,
            )
        };
        if r.is_ok() || !flags.contains(CREATE_BREAKAWAY_FROM_JOB) {
            break r;
        }
        tracing::debug!("breakaway launch refused ({r:?}) — retrying inside the job");
        flags &= !CREATE_BREAKAWAY_FROM_JOB;
    };
    created.context("CreateProcessAsUserW (current-session user launch)")?;
    // SAFETY: the launch succeeded, so both handles in `pi` are ours alone; closing either never
    // ends the child.
    let (process, thread) = unsafe { (Owned::new(pi.hProcess), Owned::new(pi.hThread)) };
    Ok((process, thread, pi.dwProcessId))
}

/// UTF-16, double-null-terminated block for `CREATE_UNICODE_ENVIRONMENT`:
/// the target session's `user_block` (`CreateEnvironmentBlock`) with this
/// process's `PUNKTFUNK_*` and `RUST_LOG` overlaid, so the child inherits
/// host settings rather than the target shell's, then `extra` (a hook's
/// `PF_*` event). Shared with [`crate::service`]. `strip_secrets` drops
/// `*TOKEN*`/`*PASSWORD*` keys: a session-user child (game, hook, tray) must
/// not receive the admin token an operator set through host.env.
///
/// # Safety
/// `user_block` must be null or a valid pointer to a UTF-16,
/// double-null-terminated environment block, readable for its whole length.
pub(crate) unsafe fn merged_env_block(
    user_block: *const u16,
    strip_secrets: bool,
    extra: &[(String, String)],
) -> Vec<u16> {
    let mut entries: Vec<String> = Vec::new();
    if !user_block.is_null() {
        let mut p = user_block;
        loop {
            let mut len = 0isize;
            // SAFETY: per this fn's contract `p` is in a readable double-null-terminated
            // block. `len` only advances over non-NUL units already read, so
            // `p.offset(len)` stays in the current entry. The empty entry stops the
            // outer loop before `p` can pass the block's end.
            while unsafe { *p.offset(len) } != 0 {
                len += 1;
            }
            if len == 0 {
                break; // empty entry: end of block
            }
            // SAFETY: `p` is readable for `len` non-NUL UTF-16 units, just scanned above, and the
            // slice is consumed before `p` moves.
            let slice = unsafe { std::slice::from_raw_parts(p, len as usize) };
            entries.push(String::from_utf16_lossy(slice));
            // SAFETY: `len` is the entry length and unit `len` is its NUL, so this lands on the next
            // entry — at worst the trailing empty one, which is still inside the block.
            p = unsafe { p.offset(len + 1) };
        }
    }
    let is_ours = |k: &str| k.starts_with("PUNKTFUNK_") || k == "RUST_LOG";
    let is_secret = |k: &str| k.contains("TOKEN") || k.contains("PASSWORD");
    entries.retain(|e| !is_ours(e.split('=').next().unwrap_or("")));
    for (k, v) in std::env::vars().filter(|(k, _)| is_ours(k)) {
        if strip_secrets && is_secret(&k) {
            continue;
        }
        entries.push(format!("{k}={v}"));
    }
    entries.extend(extra.iter().map(|(k, v)| format!("{k}={v}")));
    let mut block: Vec<u16> = Vec::new();
    for e in entries {
        block.extend(e.encode_utf16());
        block.push(0);
    }
    // An empty block is still two NULs; one would send CreateProcess reading past the end.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

#[cfg(test)]
mod tests {
    use super::{exe_dir, merged_env_block, query_process_session};

    #[test]
    fn a_hook_s_event_joins_the_environment_block() {
        let extra = [("PF_EVENT_KIND".to_string(), "stream.started".to_string())];
        // SAFETY: a null block is one the contract accepts.
        let block = unsafe { merged_env_block(std::ptr::null(), true, &extra) };
        assert!(block.ends_with(&[0, 0]), "double-NUL terminated");
        let entries: Vec<String> = block
            .split(|&u| u == 0)
            .filter(|e| !e.is_empty())
            .map(String::from_utf16_lossy)
            .collect();
        assert_eq!(
            entries.last().map(String::as_str),
            Some("PF_EVENT_KIND=stream.started")
        );
    }

    #[test]
    fn launch_directory_comes_from_the_executable_path() {
        let exe = std::env::temp_dir().join("emu.exe");
        let dir = exe.parent().expect("temp dir has a parent").to_path_buf();

        // Quoted (every store recipe and plugin template) and bare, both with arguments.
        let quoted = format!("\"{}\" \"a rom.nsp\"", exe.display());
        assert_eq!(exe_dir(&quoted), Some(dir.clone()));
        assert_eq!(exe_dir(&format!("{} rom.nsp", exe.display())), Some(dir));

        // Hand-offs name no path, so the cwd is the OS default and does not matter.
        assert_eq!(exe_dir("explorer.exe \"steam://rungameid/70\""), None);
        assert_eq!(exe_dir("cmd.exe /c start game"), None);

        // A directory that is gone must not fail the spawn.
        let missing = std::env::temp_dir().join("pf-no-such-dir").join("emu.exe");
        assert_eq!(exe_dir(&format!("\"{}\"", missing.display())), None);
    }

    #[test]
    fn process_session_query_uses_requested_process() {
        let session = query_process_session(41, |process_id, out| {
            assert_eq!(process_id, 41);
            *out = 7;
            Ok::<(), ()>(())
        });
        assert_eq!(session, Ok(7));
    }

    #[test]
    fn failed_process_session_query_preserves_error() {
        let session = query_process_session(41, |_, out| {
            *out = 7;
            Err("query failed")
        });
        assert_eq!(session, Err("query failed"));
    }
}
