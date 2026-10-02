//! The self-extractor (D2/D3): a packed exe unpacks runtime + payload into a fresh admin-only
//! dir and re-runs itself from there, where the WinAppSDK DLLs sit beside it. The child gets
//! `PUNKTFUNK_SETUP_ROOT` plus the same argv; the parent waits and returns the child's exit
//! code, so winget and a terminal see one process with one result.
//!
//! Elevated (the host installer), the dir is created with an explicit SDDL — owner
//! Administrators, SYSTEM + Administrators only, protected, inherited — so it passes the
//! host's `ensure_admin_only_source` by construction: driver staging and the password temp
//! file are served from it. Unelevated (the per-user client, M4) it is a plain dir under
//! LOCALAPPDATA. Cleanup is left to a later run: a process cannot delete the directory it
//! runs from.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use punktfunk_setup::platform::windows::sys;

use crate::{overlay, payload};

pub const ROOT_ENV: &str = "PUNKTFUNK_SETUP_ROOT";

/// `Some(exit)` when this process was the packed outer exe and has run the extracted copy.
/// A plain wizard build (no footer) or the extracted child itself gets `None`.
pub fn relaunch_if_packed() -> Option<ExitCode> {
    if std::env::var_os(ROOT_ENV).is_some() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let data = std::fs::read(&exe).ok()?;
    let payload = overlay::extract(&data).ok()?;
    // Console first: the child's AttachConsole(ATTACH_PARENT_PROCESS) binds OURS.
    let console = sys::attach_parent_console();
    Some(match extract_and_run(&exe, &data, payload) {
        Ok(code) => code,
        Err(e) => {
            use std::io::Write;
            if let Some(mut c) = console {
                let _ = writeln!(c, "  xx {e}");
            }
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    })
}

// The one raw spawn outside `SystemRunner`: this is the self-extractor re-running itself,
// not a plan step — no demo or test fake can stand in for it, which is what the fence guards.
#[allow(clippy::disallowed_methods)]
fn extract_and_run(exe: &Path, data: &[u8], payload: &[u8]) -> Result<ExitCode, String> {
    let root = fresh_root()?;
    payload::extract(payload, &root)?;
    // Windows inherits the DACL, never the owner: the unpacked `staging\*` dirs are owned by
    // whoever ran setup, and the host's driver leg rejects a source not owned by SYSTEM /
    // Administrators. Best-effort — the per-user client root cannot and need not be re-owned.
    #[cfg(windows)]
    {
        // Absolute: `CreateProcess` searches the cwd before `%PATH%`, and setup runs elevated.
        // The target is `root`, the extract dir — never `SystemRoot`.
        let _ = std::process::Command::new(sys::system32("icacls.exe"))
            .arg(&root)
            .args(["/setowner", "*S-1-5-32-544", "/T", "/C", "/Q"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // The stub is the bytes before the overlay — the same wizard, minus payload and
    // signature. Written beside the runtime under the launch name, so `unins000.exe` keeps
    // its D6 meaning in the child.
    let stub_len = payload.as_ptr() as usize - data.as_ptr() as usize;
    let name = exe.file_name().ok_or("the running exe has no file name")?;
    let child = root.join("runtime").join(name);
    std::fs::write(&child, &data[..stub_len]).map_err(|e| format!("{}: {e}", child.display()))?;
    let status = std::process::Command::new(&child)
        .args(std::env::args_os().skip(1))
        .env(ROOT_ENV, &root)
        .status()
        .map_err(|e| format!("couldn't start {}: {e}", child.display()))?;
    // The child is gone, so its exe is deletable: nothing of the ~300 MB extract stays under
    // %ProgramData% (the /LOG file lives elsewhere). Best effort — a locked file is not a
    // failed install.
    let _ = std::fs::remove_dir_all(&root);
    // A crash status (NTSTATUS) is negative as an i32; clamped, it would read as success.
    Ok(match status.code().map(u8::try_from) {
        Some(Ok(code)) => ExitCode::from(code),
        _ => ExitCode::FAILURE,
    })
}

/// Elevated: `%ProgramData%\punktfunk\setup\<pid>-<hex>`, protected before anything lands in
/// it. Unelevated: the same layout under `%LOCALAPPDATA%` — the client has no driver staging
/// and nothing there needs the admin-only guarantee.
fn fresh_root() -> Result<PathBuf, String> {
    let elevated = elevated();
    let data = std::env::var_os(if elevated {
        "ProgramData"
    } else {
        "LOCALAPPDATA"
    })
    .map(PathBuf::from)
    .ok_or("neither ProgramData nor LOCALAPPDATA is set")?;
    let base = data.join("punktfunk").join("setup");
    if elevated {
        // Re-own and lock both ancestors first. `%ProgramData%` lets any account create
        // `punktfunk\setup` ahead of the first install and keep FILE_DELETE_CHILD over it,
        // which renames a protected root away mid-extract. Junctions are refused.
        for dir in [base.parent().unwrap_or(&base), base.as_path()] {
            pf_paths::create_private_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            // Hardening is not proof: the host's own check on what is actually there, so a
            // grant the reset missed refuses the install instead of staging into it.
            pf_paths_win::ensure_admin_only_source(dir)
                .map_err(|e| format!("{} isn't admin-only after hardening: {e}", dir.display()))?;
        }
    } else {
        std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let root = base.join(format!("{}-{}", std::process::id(), sys::random_hex(4)?));
    if elevated {
        protected_dir(&root)?;
    } else {
        std::fs::create_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    }
    Ok(root)
}

/// Whether this process runs with an elevated token — the host bin's manifest asks for one,
/// the client twin's does not.
fn elevated() -> bool {
    use windows::Win32::handleapi::CloseHandle;
    use windows::Win32::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::securitybaseapi::GetTokenInformation;
    use windows::Win32::{TokenElevation, HANDLE, TOKEN_ELEVATION, TOKEN_QUERY};

    let mut token = HANDLE::default();
    // SAFETY: plain token query — open our own process token, read one fixed-size struct
    // into a stack local of exactly that size, close the handle on every path.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY as u32, &mut token)
            .ok()
            .is_err()
        {
            return false;
        }
        let mut info = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let read = GetTokenInformation(
            token,
            TokenElevation,
            Some(std::ptr::from_mut(&mut info).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .ok()
        .is_ok();
        let _ = CloseHandle(token);
        read && info.TokenIsElevated != 0
    }
}

/// Owner Administrators; SYSTEM and Administrators full control, inherited; nothing else,
/// and protected from the parent's ACL. Every trustee with write access is privileged, which
/// is exactly what `ensure_admin_only_source` checks.
fn protected_dir(path: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::fileapi::CreateDirectoryW;
    use windows::Win32::sddl::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::winbase::LocalFree;
    use windows::Win32::{HANDLE, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    const SDDL: &str = "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL string outlives the call; `sd` is the single LocalAlloc'd descriptor
    // the conversion returns, handed to CreateDirectoryW by pointer and LocalFree'd after.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(SDDL),
            SDDL_REVISION_1 as u32,
            &mut sd,
            None,
        )
        .ok()
        .map_err(|e| format!("security descriptor: {e}"))?;
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let created = CreateDirectoryW(&HSTRING::from(path.as_os_str()), Some(&attrs)).ok();
        // HLOCAL is a HANDLE alias at this rev, so the alias cannot construct.
        LocalFree(HANDLE(sd.0));
        created.map_err(|e| format!("create {}: {e}", path.display()))
    }
}
