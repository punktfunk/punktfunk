//! A Windows seat's Steam client, copied from the box's install into the seat's profile the first
//! time the seat's session is up. [`crate::steam`] says why a seat needs its own.
//!
//! Every usual way of opening Steam on a seat starts that copy under the seat's IPC name. The
//! box's shared Desktop and Start menu shortcuts go through `punktfunk-tray --steam`, which reads
//! the seat's `SeatSteam` key; the seat's `steam://` handler and autostart line name the copy.
//! The seat can read those keys but not change them. Steam started any other way is the box's,
//! whose start under the default name closes the Steam already running.

use crate::steam;
use crate::windows::accounts;
use crate::windows::enable::run;
use crate::windows::util::{io_error, wide, with_privilege, WinResult};
use pf_paths_win::SecurityAttributes;
use std::collections::HashSet;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows::core::{Interface as _, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegLoadKeyW, RegUnLoadKeyW, HKEY, HKEY_USERS as RAW_HKEY_USERS,
    KEY_ALL_ACCESS as RAW_ALL, REG_OPTION_NON_VOLATILE,
};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::Win32::UI::Shell::{
    FOLDERID_CommonPrograms, FOLDERID_PublicDesktop, IShellLinkW, SHGetKnownFolderPath, ShellLink,
    KF_FLAG_DEFAULT,
};
use winreg::enums::KEY_WOW64_64KEY;
use winreg::enums::{HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_ALL_ACCESS, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

/// Marks the `steam://` handler this module wrote, under the seat's classes key.
const HANDLER_MARKER: &str = "PunktfunkSeat";
/// The seat's copy and IPC name for `punktfunk-tray --steam`, under the seat's own hive.
const SEAT_STEAM_KEY: &str = r"Software\Punktfunk\SeatSteam";
/// The Seats key value keeping what the box's shared Steam shortcuts pointed at.
const KEPT_LINKS: &str = "SteamLinks";
const TRAY_EXE: &str = "punktfunk-tray.exe";

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

/// Where Steam registers its install machine-wide, as `InstallPath`.
const REGISTRATIONS: [&str; 2] = [r"SOFTWARE\WOW6432Node\Valve\Steam", r"SOFTWARE\Valve\Steam"];

/// The Steam registered machine-wide, when that isn't a seat's copy: a seat's copy rewrites
/// `InstallPath` to itself whenever it runs. Only for telling the operator where Steam is.
pub(super) fn registered_install() -> Option<PathBuf> {
    REGISTRATIONS
        .into_iter()
        .filter_map(|key| {
            RegKey::predef(HKEY_LOCAL_MACHINE)
                .open_subkey_with_flags(key, KEY_READ | KEY_WOW64_64KEY)
                .and_then(|key| key.get_value::<String, _>("InstallPath"))
                .ok()
        })
        .map(PathBuf::from)
        .filter(|dir| dir.join("steam.exe").is_file())
        .find(|dir| !seat_copy(dir))
}

/// Put the machine-wide registration back on the box's Steam where it names a seat's copy.
/// The box's Steam and other software find Steam by it. Called once no seat runs: a running
/// copy keeps the entry it wrote.
pub(super) fn restore_registration() {
    let Some(install) = box_install() else {
        return;
    };
    let install = install.to_string_lossy().into_owned();
    for name in REGISTRATIONS {
        let Ok(key) = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey_with_flags(name, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY)
        else {
            continue;
        };
        let registered = key.get_value::<String, _>("InstallPath");
        if !registered.is_ok_and(|dir| seat_copy(Path::new(&dir))) {
            continue;
        }
        match key.set_value("InstallPath", &install) {
            Ok(()) => tracing::info!(key = name, %install, "steam registration back on the box"),
            Err(error) => tracing::warn!(key = name, %error, "steam registration not restored"),
        }
    }
}

/// `<profiles>\<account>\Steam`: a Steam under a Windows profile, as every seat's copy is.
fn seat_copy(dir: &Path) -> bool {
    let profiles = std::env::var_os("SystemDrive")
        .map(|drive| PathBuf::from(format!(r"{}\Users", drive.to_string_lossy())))
        .unwrap_or_else(|| PathBuf::from(r"C:\Users"));
    dir.parent()
        .and_then(Path::parent)
        .is_some_and(|grand| eq_path(grand, &profiles))
}

fn eq_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

/// `<profile>\Steam\steam.exe`, the seat's copy whether or not it is made yet. `None` while the
/// account has no profile, which Windows makes at its first logon.
pub(super) fn seat_exe(account: &str) -> Option<PathBuf> {
    profile_dir(&accounts::sid_string(account)?)
        .map(|profile| profile.join("Steam").join("steam.exe"))
}

/// The profile folder of the account `sid` names, from its `ProfileList` entry. `None` while it
/// has none, which Windows makes at its first logon.
pub(super) fn profile_dir(sid: &str) -> Option<PathBuf> {
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
        .then_some(profile)
}

/// Undoes what [`ensure_launchers`] wrote for `sid`, a person's account a seat gives back: the
/// `SeatSteam` key, the `steam://` handler marked as ours, and an autostart line that names a
/// seat's IPC name. Steam writes its own line again at its next start with the setting on. An
/// account that never signed in has nothing to undo. Logs, never fails, on a hive that won't load.
pub(super) fn unwire(sid: &str) {
    let Some(profile) = profile_dir(sid) else {
        return;
    };
    let rid = sid.rsplit('-').next().unwrap_or(sid);
    let user = |hive: &RegKey| {
        delete_tree(hive, SEAT_STEAM_KEY);
        let run = hive.open_subkey_with_flags(RUN_KEY, KEY_READ | KEY_SET_VALUE);
        if let Ok(run) = run
            && run.get_value::<String, _>("Steam").is_ok_and(|line| {
                line.to_ascii_lowercase()
                    .contains("-master_ipc_name_override")
            })
            && let Err(error) = run.delete_value("Steam")
        {
            tracing::warn!(sid, %error, "seat steam autostart not removed");
        }
    };
    let classes = |hive: &RegKey| {
        let ours = hive
            .open_subkey("steam")
            .is_ok_and(|key| key.get_value::<u32, _>(HANDLER_MARKER).ok() == Some(1));
        if ours {
            delete_tree(hive, "steam");
        }
    };
    let edited = with_privilege("SeBackupPrivilege", || {
        with_privilege("SeRestorePrivilege", || {
            let mount = format!("pf-release-{rid}");
            edit_hive(sid, &mount, &profile.join("NTUSER.DAT"), user);
            edit_hive(
                &format!("{sid}_Classes"),
                &format!("{mount}_Classes"),
                &profile.join(r"AppData\Local\Microsoft\Windows\UsrClass.dat"),
                classes,
            );
        })
    });
    if let Err(error) = edited.and_then(|inner| inner) {
        tracing::warn!(sid, %error, "seat steam launchers not undone");
    }
}

/// The autostart key under a user's hive.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// How long a hive gets to unload after its account's logoff before [`edit_hive`] gives up.
const HIVE_WAIT: Duration = Duration::from_secs(5);

/// `edit` on the hive at `HKU\<loaded>`, which a session of its account holds, or else on `file`
/// loaded at `HKU\<mount>` for the edit. A hive still unloading after a logoff can't load yet,
/// so the load retries for [`HIVE_WAIT`]. Needs SeBackupPrivilege and SeRestorePrivilege on.
fn edit_hive(loaded: &str, mount: &str, file: &Path, edit: impl Fn(&RegKey)) {
    if !file.is_file() {
        return;
    }
    let users = RegKey::predef(HKEY_USERS);
    // A mount a crashed release left would keep the account's own sign-in off its hive.
    let _ = unload_hive(mount);
    let give_up = Instant::now() + HIVE_WAIT;
    loop {
        if let Ok(hive) = users.open_subkey_with_flags(loaded, KEY_ALL_ACCESS) {
            edit(&hive);
            return;
        }
        match load_hive(mount, file) {
            Ok(()) => {
                if let Ok(hive) = users.open_subkey_with_flags(mount, KEY_ALL_ACCESS) {
                    edit(&hive);
                }
                if let Err(error) = unload_hive(mount) {
                    tracing::warn!(mount, %error, "released account's hive not unloaded");
                }
                return;
            }
            Err(error) if Instant::now() >= give_up => {
                tracing::warn!(file = %file.display(), %error, "released account's hive not loaded");
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn load_hive(mount: &str, file: &Path) -> std::io::Result<()> {
    // SAFETY: both strings are owned HSTRINGs that outlive the call.
    unsafe {
        RegLoadKeyW(
            RAW_HKEY_USERS,
            &HSTRING::from(mount),
            &HSTRING::from(file.as_os_str()),
        )
    }
    .ok()
    .map_err(std::io::Error::other)
}

fn unload_hive(mount: &str) -> std::io::Result<()> {
    // SAFETY: `mount` is an owned HSTRING that outlives the call.
    unsafe { RegUnLoadKeyW(RAW_HKEY_USERS, &HSTRING::from(mount)) }
        .ok()
        .map_err(std::io::Error::other)
}

/// Deletes `path` under `hive` with its subkeys. One already gone is done.
fn delete_tree(hive: &RegKey, path: &str) {
    match hive.delete_subkey_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!(path, %error, "seat steam key not removed");
        }
        _ => {}
    }
}

/// The copy for `account`, signed in to `session`, when the box has Steam and the seat has none
/// yet, then its library list and its launchers. The files are written as that user: the profile
/// is theirs, and SYSTEM writing into a folder its user controls could be steered elsewhere
/// through a junction.
pub(super) fn ensure_copy(session: u32, account: &str, seat_id: &str) {
    let (Some(src), Some(exe)) = (box_install(), seat_exe(account)) else {
        return;
    };
    let Some(dst) = exe.parent().map(Path::to_path_buf) else {
        return;
    };
    let copied = exe.is_file();
    if copied {
        ensure_launchers(account, seat_id, &exe);
    }
    if !claim(account) {
        return;
    }
    let owned = account.to_owned();
    let seat_id = seat_id.to_owned();
    let spawned = std::thread::Builder::new()
        .name("seat-steam-copy".into())
        .spawn(move || {
            match as_user(session, |_| prepare(&src, &dst)) {
                Ok(Ok(())) if copied => {}
                Ok(Ok(())) => {
                    tracing::info!(account = %owned, dst = %dst.display(), "seat steam copied");
                    ensure_launchers(&owned, &seat_id, &exe);
                }
                Ok(Err(error)) => {
                    tracing::warn!(account = %owned, %error, "seat steam setup did not complete")
                }
                Err(error) => {
                    tracing::warn!(account = %owned, %error, "seat steam setup did not complete")
                }
            }
            release(&owned);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "seat steam setup did not start");
        release(account);
    }
}

/// Point every usual start of Steam on the seat at its copy `exe` under the seat's IPC name:
/// the seat's `SeatSteam` key, which the box's shared shortcuts read through the tray, its
/// autostart line, then its `steam://` handler, whose classes hive loads a moment after logon.
fn ensure_launchers(account: &str, seat_id: &str, exe: &Path) {
    let Some(sid) = accounts::sid_string(account) else {
        tracing::warn!(account, "seat account has no SID; steam launchers skipped");
        return;
    };
    let ipc = steam::ipc_name(seat_id);
    let exe = exe.to_string_lossy().into_owned();
    if let Err(error) = seat_steam_key(&sid, &exe, &ipc) {
        tracing::warn!(account, %error, "seat steam key not written");
    }
    if let Err(error) = autostart(&sid, &exe, &ipc) {
        tracing::warn!(account, %error, "seat steam autostart not rewritten");
    }
    let owned = account.to_owned();
    let spawned = std::thread::Builder::new()
        .name("seat-steam-url".into())
        .spawn(move || {
            if let Err(error) = url_handler(&sid, &exe, &ipc) {
                tracing::warn!(account = %owned, %error, "seat steam:// handler not written");
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "seat steam:// handler did not start");
    }
}

/// `HKU\<sid>\Software\Punktfunk\SeatSteam`: the copy and its IPC name, which
/// `punktfunk-tray --steam` starts when the seat opens one of the box's Steam shortcuts.
fn seat_steam_key(sid: &str, exe: &str, ipc: &str) -> std::io::Result<()> {
    let users = RegKey::predef(HKEY_USERS);
    let path = format!(r"{sid}\{SEAT_STEAM_KEY}");
    let current = users.open_subkey_with_flags(&path, KEY_READ).ok();
    let read = |name: &str| current.as_ref()?.get_value::<String, _>(name).ok();
    if read("Exe").as_deref() == Some(exe) && read("Ipc").as_deref() == Some(ipc) {
        return Ok(());
    }
    match users.delete_subkey_all(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    users.create_subkey(format!(r"{sid}\Software\Punktfunk"))?;
    create_read_only_key(&path, sid)?;
    let key = users.open_subkey_with_flags(&path, KEY_ALL_ACCESS)?;
    key.set_value("Exe", &exe)?;
    key.set_value("Ipc", &ipc)
}

/// How long [`url_handler`] waits for the seat's classes hive to load after its logon.
const CLASSES_WAIT: Duration = Duration::from_secs(30);

/// `steam://` for the seat, in its classes hive: the seat's copy under its IPC name. The key is
/// SYSTEM's and only readable to the seat, so its Steam can't point the links back at itself
/// without the name. Rewritten only when the command differs.
fn url_handler(sid: &str, exe: &str, ipc: &str) -> std::io::Result<()> {
    let users = RegKey::predef(HKEY_USERS);
    let classes = format!("{sid}_Classes");
    let deadline = Instant::now() + CLASSES_WAIT;
    while users.open_subkey_with_flags(&classes, KEY_READ).is_err() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::other("the seat's classes hive didn't load"));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let path = format!(r"{classes}\steam");
    let command = steam::seat_command(exe, ipc, r#"-- "%1""#);
    let current = users
        .open_subkey_with_flags(&path, KEY_READ)
        .ok()
        .filter(|key| key.get_value::<u32, _>(HANDLER_MARKER).ok() == Some(1))
        .and_then(|key| key.open_subkey(r"Shell\Open\Command").ok())
        .and_then(|key| key.get_value::<String, _>("").ok());
    if current.as_deref() == Some(command.as_str()) {
        return Ok(());
    }
    match users.delete_subkey_all(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    create_read_only_key(&path, sid)?;
    let key = users.open_subkey_with_flags(&path, KEY_ALL_ACCESS)?;
    key.set_value("", &"URL:steam protocol")?;
    key.set_value("URL Protocol", &"")?;
    key.set_value(HANDLER_MARKER, &1_u32)?;
    let (icon, _) = key.create_subkey("DefaultIcon")?;
    icon.set_value("", &format!("\"{exe}\",0"))?;
    let (open, _) = key.create_subkey(r"Shell\Open\Command")?;
    open.set_value("", &command)
}

/// `HKEY_USERS\<path>`, made by SYSTEM with a protected DACL its subkeys inherit: SYSTEM and
/// Administrators may change it, the user and packaged apps may only read it.
fn create_read_only_key(path: &str, sid: &str) -> std::io::Result<()> {
    let attributes = SecurityAttributes::new(&format!(
        "D:P(A;OICI;KA;;;SY)(A;OICI;KA;;;BA)(A;OICI;KR;;;{sid})(A;OICI;KR;;;AC)"
    ))
    .map_err(std::io::Error::other)?;
    let path = wide(path, "registry path").map_err(|error| std::io::Error::other(error.message))?;
    let mut key = HKEY::default();
    // SAFETY: `path` is NUL-terminated, `attributes` outlives the call, and
    // `key` is a live out-parameter.
    let created = unsafe {
        RegCreateKeyExW(
            RAW_HKEY_USERS,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            RAW_ALL,
            Some(attributes.as_ptr()),
            &mut key,
            None,
        )
    };
    created.ok().map_err(std::io::Error::other)?;
    // SAFETY: the create succeeded, so `key` is an open handle this frame alone owns.
    let _ = unsafe { RegCloseKey(key) };
    Ok(())
}

/// Steam's `Run` entry for the seat, when it has one, starting the copy under its IPC name.
fn autostart(sid: &str, exe: &str, ipc: &str) -> std::io::Result<()> {
    let run = match RegKey::predef(HKEY_USERS).open_subkey_with_flags(
        format!(r"{sid}\Software\Microsoft\Windows\CurrentVersion\Run"),
        KEY_READ | KEY_SET_VALUE,
    ) {
        Ok(run) => run,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let Ok(line) = run.get_value::<String, _>("Steam") else {
        return Ok(());
    };
    let fixed = steam::autostart_line(&line, exe, ipc);
    if fixed == line {
        return Ok(());
    }
    run.set_value("Steam", &fixed)
}

/// A known folder of SYSTEM's, such as the public Desktop.
fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` is a live known-folder GUID; the path is CoTaskMem the shell hands us, copied
    // out and freed once.
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let text = path.to_string().ok();
        CoTaskMemFree(Some(path.0 as *const _));
        text.map(PathBuf::from)
    }
}

/// The box's Steam shortcuts every user sees: the public Desktop's and the shared Start menu's.
fn box_shortcuts() -> Vec<PathBuf> {
    [
        known_folder(&FOLDERID_PublicDesktop).map(|dir| dir.join("Steam.lnk")),
        known_folder(&FOLDERID_CommonPrograms).map(|dir| dir.join(r"Steam\Steam.lnk")),
    ]
    .into_iter()
    .flatten()
    .filter(|path| path.is_file())
    .collect()
}

/// One shortcut's target, arguments, working folder and icon.
struct Link {
    target: String,
    args: String,
    dir: String,
    icon: (String, i32),
}

/// Point the box's shared Steam shortcuts at `punktfunk-tray --steam <their steam.exe>`, which
/// starts a seat's own copy on a seat and the box's Steam everywhere else. What each pointed at
/// is kept under the Seats key for [`unwrap_box_shortcuts`]. `temp` is a SYSTEM-only folder.
pub(super) fn wrap_box_shortcuts(host_path: &Path, temp: &Path) {
    let tray = host_path.with_file_name(TRAY_EXE);
    if !tray.is_file() {
        return;
    }
    let tray = tray.to_string_lossy().into_owned();
    with_com(|| {
        let mut kept = kept_links();
        for path in box_shortcuts() {
            let Some(link) = read_link(&path) else {
                continue;
            };
            if link.target.eq_ignore_ascii_case(&tray) {
                continue;
            }
            let wrapped = Link {
                args: format!("--steam \"{}\" {}", link.target, link.args)
                    .trim_end()
                    .to_owned(),
                target: tray.clone(),
                dir: link.dir.clone(),
                icon: if link.icon.0.is_empty() {
                    (link.target.clone(), 0)
                } else {
                    link.icon.clone()
                },
            };
            let path_text = path.to_string_lossy().into_owned();
            kept.retain(|(kept_path, _)| kept_path != &path_text);
            kept.push((path_text, link));
            if let Err(error) = keep_links(&kept).and_then(|()| replace_link(&path, &wrapped, temp))
            {
                tracing::warn!(path = %path.display(), %error, "box steam shortcut not rewired");
            }
        }
    });
}

/// Put the box's shared Steam shortcuts back as [`wrap_box_shortcuts`] found them.
pub(super) fn unwrap_box_shortcuts(temp: &Path) {
    with_com(|| {
        for (path, link) in kept_links() {
            let path = PathBuf::from(path);
            let wrapped = read_link(&path).is_some_and(|now| now.target.ends_with(TRAY_EXE));
            if wrapped && let Err(error) = replace_link(&path, &link, temp) {
                tracing::warn!(path = %path.display(), %error, "box steam shortcut not restored");
            }
        }
    });
}

/// The shortcuts the Seats key keeps, `path\ttarget\targs\tdir\ticon\tindex` per line.
fn kept_links() -> Vec<(String, Link)> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(super::SEATS_KEY, KEY_READ | KEY_WOW64_64KEY)
        .and_then(|key| key.get_value::<String, _>(KEPT_LINKS))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let (path, target, args, dir, icon) =
                (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
            let index = f.next()?.parse().ok()?;
            let link = Link {
                target: target.into(),
                args: args.into(),
                dir: dir.into(),
                icon: (icon.into(), index),
            };
            Some((path.to_owned(), link))
        })
        .collect()
}

fn keep_links(kept: &[(String, Link)]) -> std::io::Result<()> {
    let text = kept
        .iter()
        .map(|(path, l)| {
            format!(
                "{path}\t{}\t{}\t{}\t{}\t{}",
                l.target, l.args, l.dir, l.icon.0, l.icon.1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(super::SEATS_KEY, KEY_SET_VALUE | KEY_WOW64_64KEY)?
        .set_value(KEPT_LINKS, &text)
}

fn with_com(work: impl FnOnce()) {
    // SAFETY: COM is initialized for this thread and uninitialized below when this call did it;
    // every interface the work holds is released before that.
    let inited = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    work();
    if inited {
        // SAFETY: balances the successful CoInitializeEx above on this thread.
        unsafe { CoUninitialize() };
    }
}

fn read_link(path: &Path) -> Option<Link> {
    // SAFETY: COM is initialized on this thread; each buffer is a stack array whose length the
    // call is given, and the strings are copied out before it goes.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        link.cast::<IPersistFile>()
            .ok()?
            .Load(&HSTRING::from(path.as_os_str()), STGM_READ)
            .ok()?;
        let mut buf = [0u16; 1024];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let target = text(&buf);
        link.GetArguments(&mut buf).ok()?;
        let args = text(&buf);
        link.GetWorkingDirectory(&mut buf).ok()?;
        let dir = text(&buf);
        let mut index = 0;
        link.GetIconLocation(&mut buf, &mut index).ok()?;
        Some(Link {
            target,
            args,
            dir,
            icon: (text(&buf), index),
        })
    }
}

fn text(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Write `link` beside the seats' secrets, then move it over `path` and take the folder's ACL:
/// a move replaces the directory entry, so a hard link a user planted there points nowhere.
fn replace_link(path: &Path, link: &Link, temp: &Path) -> std::io::Result<()> {
    let staged = temp.join("steam.lnk");
    // SAFETY: COM is initialized on this thread; every argument is an owned HSTRING that lives
    // across its call.
    unsafe {
        let shell: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        shell.SetPath(&HSTRING::from(link.target.as_str()))?;
        shell.SetArguments(&HSTRING::from(link.args.as_str()))?;
        shell.SetWorkingDirectory(&HSTRING::from(link.dir.as_str()))?;
        shell.SetIconLocation(&HSTRING::from(link.icon.0.as_str()), link.icon.1)?;
        shell
            .cast::<IPersistFile>()?
            .Save(&HSTRING::from(staged.as_os_str()), true)?;
    }
    std::fs::rename(&staged, path)?;
    match run("icacls.exe", &[&path.to_string_lossy(), "/reset", "/Q"]) {
        Some(0) => Ok(()),
        code => Err(std::io::Error::other(format!(
            "icacls /reset exited {code:?}"
        ))),
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

/// `work` on this thread as the user signed in to `session`, given that user's token. Call it
/// only on a thread of its own: a failed revert leaves the thread impersonating.
fn as_user<T>(session: u32, work: impl FnOnce(HANDLE) -> T) -> WinResult<T> {
    let mut token = HANDLE::default();
    // SAFETY: `session` is a plain id and `token` a live out-param; SYSTEM holds SE_TCB.
    unsafe { WTSQueryUserToken(session, &mut token) }
        .map_err(|error| io_error("seat_user", "WTSQueryUserToken failed", error))?;
    // SAFETY: the query succeeded, so this frame alone owns the fresh token handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let raw = HANDLE(token.as_raw_handle());
    // SAFETY: `token` is live; this thread reverts below before anything else runs on it.
    unsafe { ImpersonateLoggedOnUser(raw) }
        .map_err(|error| io_error("seat_user", "ImpersonateLoggedOnUser failed", error))?;
    let out = work(raw);
    // SAFETY: ends the impersonation this thread began above.
    unsafe { RevertToSelf() }
        .map_err(|error| io_error("seat_user", "RevertToSelf failed", error))?;
    Ok(out)
}

/// The copy when the seat has none, then its library lists. A list that can't be written leaves
/// the seat its own library only.
fn prepare(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !dst.join("steam.exe").is_file() {
        steam::copy_install(src, dst)?;
    }
    if let Err(error) = list_libraries(src, dst) {
        tracing::warn!(%error, "seat steam library list not written");
    }
    Ok(())
}

/// Names the box's library folders in both of the copy's lists, after the copy itself. Steam
/// reads them at sign-in and writes them back, so a change made under a running Steam lasts
/// only until that write.
fn list_libraries(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut shared = ["config", "steamapps"]
        .into_iter()
        .find_map(|dir| std::fs::read_to_string(src.join(dir).join("libraryfolders.vdf")).ok())
        .map(|vdf| steam::library_paths(&vdf))
        .unwrap_or_default();
    if shared.is_empty() {
        shared.push(src.to_string_lossy().into_owned());
    }
    let own = dst.to_string_lossy();
    for dir in ["config", "steamapps"] {
        let file = dst.join(dir).join("libraryfolders.vdf");
        let had = std::fs::read_to_string(&file).ok();
        let list = steam::seat_library_list(had.as_deref(), &own, &shared);
        if had.as_deref() != Some(list.as_str()) {
            std::fs::create_dir_all(dst.join(dir))?;
            std::fs::write(file, list)?;
        }
    }
    Ok(())
}
