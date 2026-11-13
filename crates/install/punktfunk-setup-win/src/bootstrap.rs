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
        let _ = std::process::Command::new(pf_paths::system32("icacls.exe"))
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
/// it, after what another account put under `%ProgramData%\punktfunk` has gone aside.
/// Unelevated: the same layout under `%LOCALAPPDATA%` — the client has no driver staging
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
        // `%ProgramData%` lets any account create `punktfunk\setup` ahead of the first install,
        // own what it puts there and keep FILE_DELETE_CHILD over the extract root. The host
        // trusts a file Administrators own, so a folder another account owns goes aside whole,
        // and one that was open is locked and cleared, never re-owned.
        let parent = base.parent().unwrap_or(&base);
        let refused = |e: std::io::Error| {
            pf_l10n::tools::setup_win_data_dir_failed(parent.display(), e.to_string())
        };
        if parent.symlink_metadata().is_ok() {
            pf_paths_win::set_aside_if_planted(parent).map_err(refused)?;
        }
        pf_paths::create_private_dir(parent).map_err(refused)?;
        pf_paths::create_private_dir(&base).map_err(|e| format!("{}: {e}", base.display()))?;
        // Hardening is not proof: the host's own check on what is actually there, so a
        // grant the lock missed refuses the install instead of staging into it.
        pf_paths_win::ensure_admin_only_source(&base)
            .map_err(|e| format!("{} isn't admin-only after hardening: {e}", base.display()))?;
    } else {
        std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    }
    let root = base.join(format!("{}-{}", std::process::id(), sys::random_hex(4)?));
    if elevated {
        protected_dir(&root, pf_paths_win::ADMIN_ONLY)?;
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

/// Create `path` with `sddl` as its descriptor from the first instant, so nothing lands in it
/// under the parent's ACL. Fails if `path` exists.
fn protected_dir(path: &Path, sddl: &str) -> Result<(), String> {
    pf_paths_win::create_dir_with(path, sddl).map_err(|e| format!("create {}: {e}", path.display()))
}

/// Give `dir` the DACL `sddl` describes (`grants`: its ACEs as icacls spells them): a missing
/// dir is born with it, one that exists takes it in place of what it inherited. A link, or a
/// dir another account owns, is refused. A dir others could write in until now then loses every
/// file and every folder they own ([`pf_paths_win::clear_open_tree`]). It holds
/// [`pf_paths_win::UNPROVEN`] from its lock until that is done, so a run that fails halfway
/// leaves the dir locked and the next run finishes the clear.
// A real-box step: demo and test runs neither extract nor deploy a payload.
#[allow(clippy::disallowed_methods)]
pub(crate) fn lock_dir(dir: &Path, sddl: &str, grants: &[&str]) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let was_open = match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
            }
            protected_dir(dir, sddl)?;
            false
        }
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
        Ok(md) if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 => {
            return Err(format!("a link is in place of {}", dir.display()));
        }
        Ok(_) if pf_paths_win::is_admin_owned(dir) != Some(true) => {
            return Err(format!("another account owns {}", dir.display()));
        }
        Ok(_) => {
            let was_open = pf_paths_win::ensure_admin_only_source(dir).is_err();
            let mut icacls = std::process::Command::new(pf_paths::system32("icacls.exe"));
            icacls.arg(dir).arg("/inheritance:r");
            for grant in grants {
                icacls.args(["/grant:r", grant]);
            }
            let status = icacls
                .arg("/Q")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if !status.is_ok_and(|s| s.success()) {
                return Err(format!(
                    "the access list of {} didn't change",
                    dir.display()
                ));
            }
            was_open
        }
    };
    pf_paths_win::ensure_admin_only_source(dir).map_err(|e| e.to_string())?;
    let unproven = dir.join(pf_paths_win::UNPROVEN);
    if was_open || std::fs::symlink_metadata(&unproven).is_ok() {
        pf_paths_win::mark_unproven(&unproven).map_err(|e| e.to_string())?;
        pf_paths_win::clear_open_tree(dir, &unproven).map_err(|e| e.to_string())?;
        std::fs::remove_file(&unproven)
            .map_err(|e| format!("remove {}: {e}", unproven.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Elevated: an open folder is locked and emptied of files; one a process holds without
    /// share-delete is refused and stays locked with its marker, and the next run clears it; a
    /// healthy re-run walks nothing. Not elevated, the folder this test owns is refused untouched.
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn an_open_install_dir_is_cleared_now_or_by_the_next_run() {
        use std::os::windows::fs::OpenOptionsExt;
        const GRANTS: [&str; 2] = ["*S-1-5-18:(OI)(CI)(F)", "*S-1-5-32-544:(OI)(CI)(F)"];
        let lock = |dir: &std::path::Path| super::lock_dir(dir, pf_paths_win::ADMIN_ONLY, &GRANTS);
        let base = std::env::temp_dir().join(format!("pf-setup-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("app");
        std::fs::create_dir_all(&app).unwrap();
        // Users may modify below `base`, as a volume root lets them.
        let granted = std::process::Command::new(pf_paths::system32("icacls.exe"))
            .arg(&base)
            .args(["/grant", "*S-1-5-32-545:(OI)(CI)(M)", "/Q"])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(granted.success());
        std::fs::write(app.join("version.dll"), "x").unwrap();
        if pf_paths_win::is_admin_owned(&app) != Some(true) {
            assert!(lock(&app).is_err());
            assert!(app.join("version.dll").exists());
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1) // FILE_SHARE_READ: no rename while this is open
            .open(app.join("version.dll"))
            .unwrap();
        let marker = app.join(pf_paths_win::UNPROVEN);
        assert!(lock(&app).is_err());
        assert!(
            pf_paths_win::ensure_admin_only_source(&app).is_ok(),
            "a failed clear reopened the folder"
        );
        assert!(marker.is_file(), "nothing tells the next run");
        drop(held);
        lock(&app).unwrap();
        assert!(!app.join("version.dll").exists());
        assert!(app.join("version.dll.untrusted").exists());
        assert!(!marker.exists(), "a cleared folder keeps its marker");

        std::fs::write(app.join("punktfunk-host.exe"), "x").unwrap();
        lock(&app).unwrap();
        assert!(
            app.join("punktfunk-host.exe").exists(),
            "a healthy dir was walked"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
