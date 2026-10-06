//! Who this process is, and the two relaunches a SYSTEM parent can make on `winsta0\default`.
//! `as-user` is the launch the host will give its capture worker (`WTSQueryUserToken` → primary
//! token → `CreateProcessAsUserW`). `as-system` is the launch the service gives the host itself
//! (its own token, moved into the console session): the control that says whether WGC still
//! refuses SYSTEM. The child's output goes to a file; such a process has no console.

use std::ffi::c_void;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, SecurityImpersonation, SetTokenInformation,
    TokenElevation, TokenPrimary, TokenSessionId, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS,
    TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, INFINITE,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW,
};

use crate::win::{hr, log};

/// Print the token this process runs under: WGC activates for a user, not for SYSTEM, and the
/// worker gets the unelevated one.
pub fn identity() {
    let user = std::env::var("USERNAME").unwrap_or_default();
    let mut session = 0u32;
    // SAFETY: the pid is this process's own and `session` is a live local out-param.
    let _ = unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
    // SAFETY: no arguments; reads the console session id.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    log(
        "ident",
        format!(
            "user={user:?} session={session} console_session={console} elevated={}",
            elevated().map_or("unknown".into(), |e| e.to_string())
        ),
    );
}

fn elevated() -> Option<bool> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo-handle is always valid; `token` is a live out-param, closed below.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    let mut el = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    // SAFETY: `el` is a live local of exactly the size passed; `token` carries TOKEN_QUERY.
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut el).cast::<c_void>()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: `token` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(token) };
    got.ok().map(|()| el.TokenIsElevated != 0)
}

/// Quote one argument for a Windows command line. The probe's own arguments carry no quotes.
fn quoted(arg: &str) -> String {
    if arg.contains([' ', '\t']) {
        format!("\"{arg}\"")
    } else {
        arg.to_string()
    }
}

/// Run `wgc-probe <rest after -->` in the console session and return its exit code: as the
/// signed-in user, or with `system` as this process's own SYSTEM token moved into that session.
pub fn relaunch(args: &[String], system: bool) -> Result<i32, String> {
    identity();
    let (mut out, mut child) = (None, Vec::new());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = it.next().cloned(),
            "--" => {
                child = it.cloned().collect();
                break;
            }
            _ => return Err(format!("unexpected {a:?} before `--`")),
        }
    }
    let out = out.ok_or("needs --out <log>")?;
    if child.is_empty() {
        return Err("needs a command after `--`".into());
    }
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let line = std::iter::once(quoted(&exe.to_string_lossy()))
        .chain(child.iter().map(|a| quoted(a)))
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmd: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();

    // SAFETY: every handle below is checked before use and closed once; the buffers `cmd`,
    // `desktop` and the environment block outlive the `CreateProcessAsUserW` call that reads
    // them, and `si`/`pi` are live locals.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        let mut source = HANDLE::default();
        if system {
            hr(
                OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut source),
                "OpenProcessToken(self)",
            )?;
        } else {
            hr(
                WTSQueryUserToken(session, &mut source),
                "WTSQueryUserToken (needs SYSTEM and a signed-in console user)",
            )?;
        }
        let mut primary = HANDLE::default();
        let dup = DuplicateTokenEx(
            source,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        let _ = CloseHandle(source);
        hr(dup, "DuplicateTokenEx")?;
        if system {
            hr(
                SetTokenInformation(
                    primary,
                    TokenSessionId,
                    (&raw const session).cast::<c_void>(),
                    size_of::<u32>() as u32,
                ),
                "SetTokenInformation(TokenSessionId)",
            )?;
        }

        let mut env: *mut c_void = std::ptr::null_mut();
        hr(
            CreateEnvironmentBlock(&mut env, Some(primary), false),
            "CreateEnvironmentBlock",
        )?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let file = hr(
            CreateFileW(
                &HSTRING::from(out.as_str()),
                GENERIC_WRITE.0,
                FILE_SHARE_READ,
                Some(&sa),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            ),
            "CreateFileW(--out)",
        )?;
        let si = STARTUPINFOW {
            cb: size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdOutput: file,
            hStdError: file,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let made = CreateProcessAsUserW(
            Some(primary),
            None,
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env.cast_const()),
            PCWSTR::null(),
            &si,
            &mut pi,
        );
        let _ = DestroyEnvironmentBlock(env);
        let _ = CloseHandle(file);
        let _ = CloseHandle(primary);
        hr(made, "CreateProcessAsUserW")?;
        log(
            "relaunch",
            format!(
                "child_pid={} session={session} out={out:?} cmd={line:?}",
                pi.dwProcessId
            ),
        );
        let waited = WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 0u32;
        let exit = GetExitCodeProcess(pi.hProcess, &mut code);
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        if waited != WAIT_OBJECT_0 {
            return Err("the child's wait failed".into());
        }
        hr(exit, "GetExitCodeProcess")?;
        log("relaunch", format!("child_exit={code}"));
        Ok(code as i32)
    }
}
