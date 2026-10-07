//! A Windows seat's Steam client, copied from the box's install into the seat's profile the first
//! time the seat's session is up. [`crate::steam`] says why a seat needs its own.

use crate::steam;
use crate::windows::accounts;
use crate::windows::util::{io_error, WinResult};
use std::collections::HashSet;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
use winreg::RegKey;

/// Accounts a copy is running for, so a seat restarting mid-copy doesn't start a second.
static COPYING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// The box's install, where the console host's launches find it.
pub(super) fn box_install() -> Option<PathBuf> {
    ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|dir| PathBuf::from(dir).join("Steam"))
        .find(|dir| dir.join("steam.exe").is_file())
}

/// `<profile>\Steam\steam.exe`, the seat's copy whether or not it is made yet. `None` while the
/// account has no profile, which Windows makes at its first logon.
pub(super) fn seat_exe(account: &str) -> Option<PathBuf> {
    let sid = accounts::sid_string(account)?;
    let profile: String = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}"),
            KEY_READ | KEY_WOW64_64KEY,
        )
        .ok()?
        .get_value("ProfileImagePath")
        .ok()?;
    // A local account's profile path is literal; an unexpanded one is not ours to guess.
    let profile = PathBuf::from(profile);
    (profile.is_absolute() && !profile.as_os_str().to_string_lossy().contains('%'))
        .then(|| profile.join("Steam").join("steam.exe"))
}

/// Starts the copy for `account`, signed in to `session`, when the box has Steam and the seat
/// has none yet. The copy runs as that user: the profile is theirs, and SYSTEM writing into a
/// folder its user controls could be steered elsewhere through a junction.
pub(super) fn ensure_copy(session: u32, account: &str) {
    let (Some(src), Some(exe)) = (box_install(), seat_exe(account)) else {
        return;
    };
    let Some(dst) = exe.parent().map(Path::to_path_buf) else {
        return;
    };
    if exe.is_file() || !claim(account) {
        return;
    }
    let owned = account.to_owned();
    let spawned = std::thread::Builder::new()
        .name("seat-steam-copy".into())
        .spawn(move || {
            match copy_as_user(session, &src, &dst) {
                Ok(()) => {
                    tracing::info!(account = %owned, dst = %dst.display(), "seat steam copied")
                }
                Err(error) => {
                    tracing::warn!(account = %owned, %error, "seat steam copy did not complete")
                }
            }
            release(&owned);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "seat steam copy did not start");
        release(account);
    }
}

fn claim(account: &str) -> bool {
    COPYING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashSet::new)
        .insert(account.to_owned())
}

fn release(account: &str) {
    if let Some(set) = COPYING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        set.remove(account);
    }
}

fn copy_as_user(session: u32, src: &Path, dst: &Path) -> WinResult<()> {
    let mut token = HANDLE::default();
    // SAFETY: `session` is a plain id and `token` a live out-param; SYSTEM holds SE_TCB.
    unsafe { WTSQueryUserToken(session, &mut token) }
        .map_err(|error| io_error("steam_copy", "WTSQueryUserToken failed", error))?;
    // SAFETY: the query succeeded, so this frame alone owns the fresh token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    // SAFETY: `token` is live; this thread reverts below before anything else runs on it.
    unsafe { ImpersonateLoggedOnUser(HANDLE(token.as_raw_handle())) }
        .map_err(|error| io_error("steam_copy", "ImpersonateLoggedOnUser failed", error))?;
    let copied = copy_with_library(src, dst);
    // SAFETY: ends the impersonation this thread began above.
    unsafe { RevertToSelf() }
        .map_err(|error| io_error("steam_copy", "RevertToSelf failed", error))?;
    copied.map_err(|error| io_error("steam_copy", "copy the box's steam", error))
}

/// The copy, then a library list that names the box's folders first and the copy last. Written
/// once; Steam owns the files afterwards.
fn copy_with_library(src: &Path, dst: &Path) -> std::io::Result<()> {
    steam::copy_install(src, dst)?;
    let mut folders = ["config", "steamapps"]
        .into_iter()
        .find_map(|dir| std::fs::read_to_string(src.join(dir).join("libraryfolders.vdf")).ok())
        .map(|vdf| steam::library_paths(&vdf))
        .unwrap_or_default();
    if folders.is_empty() {
        folders.push(src.to_string_lossy().into_owned());
    }
    folders.push(dst.to_string_lossy().into_owned());
    let vdf = steam::library_folders_vdf(&folders);
    for dir in ["config", "steamapps"] {
        let file = dst.join(dir).join("libraryfolders.vdf");
        if !file.exists() {
            std::fs::create_dir_all(dst.join(dir))?;
            std::fs::write(file, &vdf)?;
        }
    }
    Ok(())
}
