//! Box-wide emulators: one archive build of a catalog emulator, installed under `emulators` in
//! the state directory for every account on the box. The [`USER`] account owns that folder and
//! runs each install, so no download runs as root, an AppImage that unpacks itself included. The
//! box owner and the seats read and run the copies and can't change them. Each account finds
//! them through hermir's `machine_prefix` and keeps its own config and saves in its home.
//!
//! The id is checked before it gets here ([`crate::ipc::valid_emulator_id`]).

use super::accounts::{self, Passwd};
use super::{err, layout, run, safe_open, STATE_DIR};
use crate::backend::BackendError;
use rustix::fs::Mode;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// The account that installs box-wide emulators. It has no login and runs nothing else.
pub const USER: &str = "punktfunk-emulators";
/// The verb `punktfunk-seats` runs as [`USER`] to install or update one emulator.
pub const INSTALL_VERB: &str = "install-emulator";
/// The verb `punktfunk-seats` runs as [`USER`] to remove one emulator.
pub const REMOVE_VERB: &str = "remove-emulator";

const CODE: &str = "emulator_failed";

/// Where box-wide emulators live: [`USER`]'s hermir prefix, and every account's
/// `machine_prefix`.
pub fn dir() -> PathBuf {
    Path::new(STATE_DIR).join("emulators")
}

/// Makes everything under `dir` this account's alone to change and every account's to read:
/// setuid, setgid, sticky, group-write and other-write go, read comes on, and execute where the
/// owner has it. An AppImage unpacks with the modes its maker stored, and shadPS4 and Xenia Canary
/// ship files that any account may write. hermir closes what it unpacks; the rule holds here
/// whatever hermir does. A link is never followed.
pub fn for_every_account(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let mode = meta.permissions().mode() & 0o7777;
        let open = match meta.is_dir() {
            true => 0o755,
            false if mode & 0o100 != 0 => (mode & 0o755) | 0o555,
            false => (mode & 0o644) | 0o444,
        };
        if open != mode {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(open))?;
        }
        if meta.is_dir() {
            for_every_account(&path)?;
        }
    }
    Ok(())
}

/// Installs `id` for every account, or updates it when it is there, and answers hermir's row for
/// the copy as JSON.
pub(super) fn install(id: &str) -> Result<String, BackendError> {
    let user = installer()?;
    refuse_running(Path::new("/proc"), &dir().join(id))?;
    as_installer(&user, INSTALL_VERB, id)
}

/// Removes the box-wide copy of `id`. Each account's config and saves are in its own home.
pub(super) fn remove(id: &str) -> Result<(), BackendError> {
    let user = installer()?;
    refuse_running(Path::new("/proc"), &dir().join(id))?;
    as_installer(&user, REMOVE_VERB, id).map(drop)
}

/// [`USER`], with [`dir`] made its own. An account of that name with root's uid is refused: an
/// install as it would run the download as root.
fn installer() -> Result<Passwd, BackendError> {
    let user = user()?;
    if user.uid == 0 {
        return Err(err("emulators_user", format!("user {USER} is root")));
    }
    own_dir(&user)?;
    Ok(user)
}

/// [`USER`], created as a system user with no login when missing.
fn user() -> Result<Passwd, BackendError> {
    Ok(match accounts::lookup(USER)? {
        Some(user) => user,
        None => {
            let home = dir();
            let home = home.to_string_lossy();
            let args = [
                "--system",
                "--user-group",
                "--no-create-home",
                "--home-dir",
                &home,
                "--shell",
                "/usr/sbin/nologin",
                USER,
            ];
            run("useradd_failed", "useradd", &args)?;
            accounts::lookup(USER)?
                .ok_or_else(|| err("user_missing", format!("user {USER} after useradd")))?
        }
    })
}

/// Makes [`dir`] a folder `user` owns, mode `0755`: every account reads and runs in it, and only
/// `user` writes. Root changes the folder through its handle and never looks inside it.
fn own_dir(user: &Passwd) -> Result<(), BackendError> {
    let io = |what: &str, e: std::io::Error| err("emulators_dir", format!("{what}: {e}"));
    let top =
        safe_open::dir(Path::new(STATE_DIR)).map_err(|e| io("open the state directory", e))?;
    let dir = layout::dir_in(&top, "emulators", false).map_err(|e| io("open emulators", e))?;
    std::os::unix::fs::fchown(&dir, Some(user.uid), Some(user.gid))
        .map_err(|e| io("own emulators", e))?;
    rustix::fs::fchmod(&dir, Mode::from_raw_mode(0o755))
        .map_err(|e| io("set the mode of emulators", e.into()))
}

/// `punktfunk-seats <verb> <id>` as `user`, which prints hermir's row. `setpriv` gives it the
/// user's ids and home and no PAM session, which this unit's sandbox refuses.
fn as_installer(user: &Passwd, verb: &str, id: &str) -> Result<String, BackendError> {
    let exe = std::env::current_exe().map_err(|e| err(CODE, format!("find this program: {e}")))?;
    let (uid, gid, exe) = (
        user.uid.to_string(),
        user.gid.to_string(),
        exe.to_string_lossy(),
    );
    let ids = [
        "--reuid",
        &uid,
        "--regid",
        &gid,
        "--clear-groups",
        "--reset-env",
    ];
    let args: Vec<&str> = ids.into_iter().chain([&*exe, verb, id]).collect();
    Ok(run(CODE, "setpriv", &args)?.trim().to_owned())
}

/// Refuses while a process in `proc` names a path in `folder` on its command line: an update
/// replaces the files it runs. The supervisor may not read another user's `exe` link, and a copy
/// under a shell or gamescope has its path after the first word.
fn refuse_running(proc: &Path, folder: &Path) -> Result<(), BackendError> {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return Ok(());
    };
    let names = |pid: &Path| {
        std::fs::read(pid.join("cmdline")).is_ok_and(|cmdline| {
            let mut words = cmdline.split(|b| *b == 0);
            words.any(|word| Path::new(OsStr::from_bytes(word)).starts_with(folder))
        })
    };
    if entries.flatten().any(|entry| names(&entry.path())) {
        return Err(err("emulator_running", "a copy of it is running"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copy someone plays holds its update back, under a shell too; another emulator's copy,
    /// or a program merely named like the folder, does not.
    #[test]
    fn a_running_copy_holds_its_change_back() {
        let proc = tempfile::tempdir().unwrap();
        let folder = Path::new("/var/lib/punktfunk-seats/emulators/rpcs3");
        let pid = |n: u32, cmdline: &[u8]| {
            let dir = proc.path().join(n.to_string());
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("cmdline"), cmdline).unwrap();
        };
        pid(
            10,
            b"/var/lib/punktfunk-seats/emulators/vita3k/app/AppRun\0--fullscreen\0",
        );
        pid(11, b"/var/lib/punktfunk-seats/emulators/rpcs3-old/AppRun\0");
        pid(12, b"");
        assert!(refuse_running(proc.path(), folder).is_ok());
        pid(
            13,
            b"/var/lib/punktfunk-seats/emulators/rpcs3/app/usr/bin/rpcs3\0--no-gui\0",
        );
        let refused = refuse_running(proc.path(), folder).unwrap_err();
        assert_eq!(refused.code, "emulator_running");
        let shell = Path::new("/var/lib/punktfunk-seats/emulators/shadps4");
        assert!(refuse_running(proc.path(), shell).is_ok());
        pid(
            14,
            b"/bin/sh\0/var/lib/punktfunk-seats/emulators/shadps4/app/AppRun\0game\0",
        );
        assert!(refuse_running(proc.path(), shell).is_err());
    }

    /// A copy comes out readable and runnable by every account, writable by its owner alone and
    /// never setuid, whatever modes its maker stored. A link's target outside it is not touched.
    #[test]
    fn a_copy_is_every_accounts_to_read_and_its_owners_to_change() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let (copy, outside) = (temp.path().join("copy"), temp.path().join("outside"));
        std::fs::create_dir_all(copy.join("usr/doc")).unwrap();
        let chmod = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        for (name, was) in [
            ("AppRun", 0o4750),
            ("usr/doc/copyright", 0o777),
            ("usr/key", 0o600),
        ] {
            std::fs::write(copy.join(name), "x").unwrap();
            chmod(&copy.join(name), was);
        }
        std::fs::write(&outside, "x").unwrap();
        chmod(&outside, 0o600);
        std::os::unix::fs::symlink(&outside, copy.join("link")).unwrap();
        chmod(&copy.join("usr/doc"), 0o2777);
        chmod(&copy.join("usr"), 0o700);
        for_every_account(&copy).unwrap();
        assert_eq!(mode(&copy.join("AppRun")), 0o755);
        assert_eq!(mode(&copy.join("usr/doc/copyright")), 0o755);
        assert_eq!(mode(&copy.join("usr/key")), 0o644);
        assert_eq!(mode(&copy.join("usr/doc")), 0o755);
        assert_eq!(mode(&copy.join("usr")), 0o755);
        assert_eq!(mode(&outside), 0o600);
    }

    /// An account of the installer's name that is root never installs: the folder stays as it is.
    #[test]
    fn an_installer_that_is_root_is_refused() {
        let _machine = super::super::fake::install(|program, _| match program {
            "getent" => (0, "punktfunk-emulators:x:0:0::/root:/bin/sh\n".into()),
            _ => (1, String::new()),
        });
        assert_eq!(installer().unwrap_err().code, "emulators_user");
        assert_eq!(
            super::super::fake::calls(),
            ["getent passwd punktfunk-emulators"]
        );
    }

    /// The installer account is made once, as a system user with no login whose home is the
    /// prefix, and an install runs as it with the row on stdout.
    #[test]
    fn an_install_runs_as_a_system_user_made_for_it() {
        let made = std::rc::Rc::new(std::cell::Cell::new(false));
        let seen = made.clone();
        let row = "punktfunk-emulators:x:961:961::/var/lib/punktfunk-seats/emulators:/sbin/nologin";
        let _machine = super::super::fake::install(move |program, _| match program {
            "useradd" => {
                seen.set(true);
                (0, String::new())
            }
            "getent" if seen.get() => (0, format!("{row}\n")),
            "setpriv" => (0, "{\"emulator\":\"rpcs3\"}\n".into()),
            _ => (2, String::new()),
        });
        let user = user().unwrap();
        assert_eq!((user.uid, user.gid), (961, 961));
        assert!(made.get());
        let installed = as_installer(&user, INSTALL_VERB, "rpcs3").unwrap();
        assert_eq!(installed, r#"{"emulator":"rpcs3"}"#);
        let calls = super::super::fake::calls();
        assert_eq!(calls[0], "getent passwd punktfunk-emulators");
        assert_eq!(
            calls[1],
            "useradd --system --user-group --no-create-home --home-dir \
             /var/lib/punktfunk-seats/emulators --shell /usr/sbin/nologin punktfunk-emulators"
        );
        assert_eq!(calls[2], "getent passwd punktfunk-emulators");
        let ids = "setpriv --reuid 961 --regid 961 --clear-groups --reset-env /";
        assert!(calls[3].starts_with(ids), "{}", calls[3]);
        assert!(
            calls[3].ends_with(" install-emulator rpcs3"),
            "{}",
            calls[3]
        );
    }
}
