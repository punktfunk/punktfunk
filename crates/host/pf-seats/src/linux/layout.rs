//! Where each thing lives under the supervisor's state directory, and how those folders are made.
//!
//! Everything is under [`super::STATE_DIR`], which only root can change:
//!
//! - `games/steamapps/{common,compatdata,shadercache,downloading}`: one Steam library that every
//!   seat writes. Each seat gets its own `compatdata`, `shadercache` and `downloading` mounted
//!   over the shared ones (`session.rs`), so Wine prefixes and downloads never mix.
//! - `seats/<id>/`: the seat user's home, those three private folders, and the box owner's
//!   libraries as the seat sees them (`libraries.rs`).
//! - `trust/<id>/`: the seat's trust copy ([`super::trust_copy`]).
//! - `library/`: the box library ([`super::library_copy`]).
//! - `seats/hosts/<id>/mgmt-token`: the token the box presents to that seat host's loopback
//!   API. It belongs to the `punktfunk` user and not to the seat group, because every seat user
//!   is in that group.
//!
//! The ACLs that keep the games folder and the save exchanges writable are in [`super::acl`].

use super::accounts::Passwd;
use super::acl::{
    acl_bytes, ACL_ACCESS, ACL_DEFAULT, ACL_GROUP_OBJ, ACL_MASK, ACL_NO_ID, ACL_OTHER, ACL_USER,
    ACL_USER_OBJ,
};
use super::library_copy::{PLUGIN_GRANTS, PLUGIN_RUN};
use super::session::PRIVATE_DIRS;
use super::{err, run, safe_open};
use crate::backend::BackendError;
use crate::model::SeatId;
use rustix::fs::{AtFlags, Mode, XattrFlags};
use rustix::io::Errno;
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};

pub(super) fn seat_dir(state: &Path, id: &SeatId) -> PathBuf {
    state.join("seats").join(id.as_str())
}

pub(super) fn trust_dir(state: &Path, id: &SeatId) -> PathBuf {
    state.join("trust").join(id.as_str())
}

pub(super) fn games_dir(state: &Path) -> PathBuf {
    state.join("games")
}

/// The box's library, the door's `PUNKTFUNK_LIBRARY_DIR` (`punktfunk-door.service`).
pub(super) fn box_library_dir(state: &Path) -> PathBuf {
    state.join("library")
}

/// The copy of the owner's plugin grants in the box's library.
pub(super) fn box_grants(state: &Path) -> PathBuf {
    box_library_dir(state).join(PLUGIN_RUN).join(PLUGIN_GRANTS)
}

/// The ledger's directory, `seats` under the state directory: `persistence::default_root()` on
/// Linux. Seat directories are named by id, so they never collide with the ledger's files.
pub fn ledger_root(state: &Path) -> PathBuf {
    state.join("seats")
}

fn hosts_dir(state: &Path) -> PathBuf {
    ledger_root(state).join("hosts")
}

/// Where the box reads a seat host's management token.
pub(super) fn host_dir(state: &Path, id: &SeatId) -> PathBuf {
    hosts_dir(state).join(id.as_str())
}

/// The owner of a seat's token file: the `punktfunk` user's `(uid, gid)` when it exists, else
/// root, so the box host reads it as itself and no seat user can.
pub(super) fn token_owner(punktfunk: Option<&Passwd>) -> (u32, u32) {
    punktfunk.map_or((0, 0), |user| (user.uid, user.gid))
}

/// Writes `line` as the seat's token file, `0600` in a `0700` directory, both owned by `owner`.
pub(super) fn write_host_token(
    state: &Path,
    id: &SeatId,
    line: &str,
    owner: (u32, u32),
) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("host_token", what, e);
    make_dir(&hosts_dir(state), 0o711).map_err(|e| io("create the hosts directory", e))?;
    let dir = host_dir(state, id);
    make_dir(&dir, 0o700).map_err(|e| io("create the token directory", e))?;
    std::os::unix::fs::chown(&dir, Some(owner.0), Some(owner.1))
        .map_err(|e| io("own the token directory", e))?;
    let file = dir.join("mgmt-token");
    let temp = dir.join("mgmt-token.tmp");
    let _ = std::fs::remove_file(&temp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| io("create the token file", e))?;
    out.write_all(line.as_bytes())
        .map_err(|e| io("write the token file", e))?;
    std::os::unix::fs::fchown(&out, Some(owner.0), Some(owner.1))
        .map_err(|e| io("own the token file", e))?;
    drop(out);
    std::fs::rename(temp, file).map_err(|e| io("replace the token file", e))
}

pub(super) fn io_err(code: &str, what: &str, error: std::io::Error) -> BackendError {
    err(code, format!("{what}: {error}"))
}

/// Creates `path` and its parents, then sets `mode` on `path` itself.
pub(super) fn make_dir(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The state directory and the directories every seat needs below it, created `0711` if absent:
/// a seat user walks to its own entry and can't list the others. Each must be a directory this
/// process owns and no group or other may write; `games`, which every seat writes, only no other.
pub(super) fn ensure_layout(state: &Path) -> std::io::Result<()> {
    for dir in [
        state.to_path_buf(),
        ledger_root(state),
        hosts_dir(state),
        state.join("trust"),
    ] {
        match std::fs::DirBuilder::new().mode(0o711).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        check_owned(&dir, 0o022)?;
    }
    match check_owned(&games_dir(state), 0o002) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        checked => checked,
    }
}

/// Refuses `path` unless it is a directory, not a link, owned by this process's uid, with none
/// of the `forbidden` mode bits set.
fn check_owned(path: &Path, forbidden: u32) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    let ours = meta.uid() == rustix::process::geteuid().as_raw();
    if !meta.is_dir() || !ours || meta.mode() & forbidden != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a directory only the supervisor's uid can change",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Prepares the shared games library. It has the group `punktfunk-games`, is setgid, and has a
/// default ACL, so a file that one seat creates is writable by the other seats.
///
/// Every seat writes in this tree, so root trusts no path in it. Each folder is opened through
/// the folder above it, without following a link, and is changed through its own handle.
/// `setfacl` gets each folder as `/proc/<pid>/fd/<n>`, which names that folder whatever its
/// path holds.
///
/// - `games` is sticky, so a seat can't replace `steamapps`. It is closed to other users: only
///   the group reads the tree.
/// - A link or a file that a seat left where a bind target belongs is removed.
///
/// This runs at each seat start. A unit's `BindPaths=` names the targets, and a planted link
/// would send the mount somewhere else.
pub(super) fn ensure_games(state: &Path, gid: u32) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("games_dir", what, e);
    let top = safe_open::dir(state).map_err(|e| io("open the state directory", e))?;
    let games = dir_in(&top, "games", false).map_err(|e| io("open games", e))?;
    let steamapps = dir_in(&games, "steamapps", true).map_err(|e| io("open games/steamapps", e))?;
    let mut dirs = Vec::new();
    for name in ["common", "compatdata", "shadercache", "downloading"] {
        dirs.push(dir_in(&steamapps, name, true).map_err(|e| io(&format!("open {name}"), e))?);
    }
    dirs.extend([steamapps, games]);
    for (i, dir) in dirs.iter().enumerate() {
        let sticky = i + 1 == dirs.len();
        let mode = if sticky { 0o3770 } else { 0o2775 };
        std::os::unix::fs::fchown(dir, Some(0), Some(gid)).map_err(|e| io("own a folder", e))?;
        rustix::fs::fchmod(dir, Mode::from_raw_mode(mode as _))
            .map_err(|e| io("set the mode of a folder", e.into()))?;
    }
    let group = super::accounts::GAMES_GROUP;
    let pid = std::process::id();
    let mut args = vec!["-m".to_owned(), format!("g:{group}:rwX,d:g:{group}:rwX")];
    args.extend(
        dirs.iter()
            .map(|d| format!("/proc/{pid}/fd/{}", d.as_raw_fd())),
    );
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("setfacl_failed", "setfacl", &args).map(drop)
}

/// `name` in `at`, created if absent, as a handle. A link or file there is never followed. With
/// `replace` it is removed and a folder made in its place; without, the open fails. `games`
/// is opened without: only root can have linked it, onto a bigger disk say.
pub(super) fn dir_in(at: &OwnedFd, name: &str, replace: bool) -> std::io::Result<OwnedFd> {
    let make = || match rustix::fs::mkdirat(at, name, Mode::from_raw_mode(0o2775 as _)) {
        Ok(()) | Err(Errno::EXIST) => Ok(()),
        Err(e) => Err(e),
    };
    make()?;
    let opened = match safe_open::dir_at(at, name) {
        Err(e)
            if replace && matches!(Errno::from_io_error(&e), Some(Errno::NOTDIR | Errno::LOOP)) =>
        {
            rustix::fs::unlinkat(at, name, AtFlags::empty())?;
            make()?;
            safe_open::dir_at(at, name)?
        }
        opened => opened?,
    };
    opened.ok_or_else(|| std::io::ErrorKind::NotFound.into())
}

/// Where a person's host keeps its identity, beside the row's other files and not in their home:
/// the person's `0700`, so nothing of the host's settings lands next to theirs.
pub(super) fn person_config_dir(state: &Path, id: &SeatId) -> PathBuf {
    seat_dir(state, id).join("config")
}

/// A person's row directory, `0711` as a seat's is, and inside it their host's [`person_config_dir`],
/// owned by them. Nothing is made in their home. What is in either stays.
pub(super) fn ensure_person_dirs(
    state: &Path,
    id: &SeatId,
    passwd: &Passwd,
) -> Result<(), BackendError> {
    let io =
        |what: &str, path: &Path, e| io_err("seat_dirs", &format!("{what} {}", path.display()), e);
    let dir = seat_dir(state, id);
    make_dir(&dir, 0o711).map_err(|e| io("create", &dir, e))?;
    let config = person_config_dir(state, id);
    std::fs::create_dir_all(&config).map_err(|e| io("create", &config, e))?;
    std::os::unix::fs::chown(&config, Some(passwd.uid), Some(passwd.gid))
        .map_err(|e| io("own", &config, e))?;
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| io("set the mode of", &config, e))
}

/// Makes the seat's private directories, owned by the seat's user. They must exist before the
/// seat's unit starts: a missing bind source fails the unit with `226/NAMESPACE`.
///
/// Then makes `saves/out` and `saves/in`, where the seat's saves cross to and from the owner's
/// plugins. Both belong to the seat's user, mode `0770`, with an ACL entry and a default ACL
/// entry for `owner`, the box owner's uid. So no other seat reaches them.
///
/// Whatever is already in any of these directories stays.
pub(super) fn ensure_seat_dirs(
    state: &Path,
    id: &SeatId,
    passwd: &Passwd,
    owner: Option<u32>,
) -> Result<(), BackendError> {
    let saves = seat_dir(state, id).join("saves");
    let private = PRIVATE_DIRS.map(|name| (seat_dir(state, id).join(name), 0o700));
    let exchange = ["out", "in"].map(|name| (saves.join(name), 0o770));
    for (dir, mode) in private.into_iter().chain(exchange.clone()) {
        let io = |what: &str, e| io_err("seat_dirs", &format!("{what} {}", dir.display()), e);
        std::fs::create_dir_all(&dir).map_err(|e| io("create", e))?;
        std::os::unix::fs::chown(&dir, Some(passwd.uid), Some(passwd.gid))
            .map_err(|e| io("own", e))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
            .map_err(|e| io("set the mode of", e))?;
    }
    std::fs::set_permissions(&saves, std::fs::Permissions::from_mode(0o711)).map_err(|e| {
        io_err(
            "seat_dirs",
            &format!("set the mode of {}", saves.display()),
            e,
        )
    })?;
    for (dir, _) in &exchange {
        let set = |name, default| {
            let acl = exchange_acl(passwd.uid, owner, default);
            rustix::fs::lsetxattr(dir, name, &acl, XattrFlags::empty())
        };
        // A filesystem without ACLs leaves the folder the seat's alone, which is no reason to
        // keep the seat from starting.
        if let Err(error) = set(ACL_ACCESS, false).and_then(|()| set(ACL_DEFAULT, true)) {
            tracing::warn!(%error, dir = %dir.display(), "save exchange not opened to the box owner");
        }
    }
    Ok(())
}

/// A save exchange's ACL: its user and own group `rwx`, others nothing, and the box owner
/// `owner` `rwx`. The `default` one also names the seat, so a file either makes there stays
/// readable by both. Without an owner, an earlier owner's entry goes.
fn exchange_acl(seat: u32, owner: Option<u32>, default: bool) -> Vec<u8> {
    let mut users: Vec<u32> = owner.filter(|&o| o != seat).into_iter().collect();
    if default {
        users.push(seat);
        users.sort_unstable();
    }
    let mut entries = vec![(ACL_USER_OBJ, 7, ACL_NO_ID)];
    entries.extend(users.iter().map(|&uid| (ACL_USER, 7, uid)));
    entries.push((ACL_GROUP_OBJ, 7, ACL_NO_ID));
    if !users.is_empty() {
        entries.push((ACL_MASK, 7, ACL_NO_ID));
    }
    entries.push((ACL_OTHER, 0, ACL_NO_ID));
    acl_bytes(&entries)
}

/// Removes the seat's directories, its trust copy, its token and its library stage. Missing ones
/// are fine.
pub(super) fn remove_seat_dirs(state: &Path, id: &SeatId) -> Result<(), BackendError> {
    super::libraries::remove_stage(id);
    for dir in [
        seat_dir(state, id),
        trust_dir(state, id),
        host_dir(state, id),
    ] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("seat_dirs", &format!("remove {}", dir.display()), e)),
        }
    }
    Ok(())
}

/// Folder `name` in `at`, made the seat's when absent. A link there is refused.
pub(super) fn seat_folder(at: &OwnedFd, name: &str, seat: &Passwd) -> std::io::Result<OwnedFd> {
    let made = match rustix::fs::mkdirat(at, name, Mode::from_raw_mode(0o755)) {
        Ok(()) => true,
        Err(Errno::EXIST) => false,
        Err(e) => return Err(e.into()),
    };
    let dir = safe_open::dir_at(at, name)?.ok_or(std::io::ErrorKind::NotFound)?;
    if made {
        std::os::unix::fs::fchown(&dir, Some(seat.uid), Some(seat.gid))?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::trust_copy::tests::{id, mode};

    /// A seat that plants a link or a file where a bind target belongs gets a folder there, and
    /// what the link led to stays as it was.
    #[test]
    fn a_planted_bind_target_is_replaced_by_a_folder() {
        let temp = tempfile::tempdir().unwrap();
        let (steamapps, outside) = (temp.path().join("steamapps"), temp.path().join("outside"));
        std::fs::create_dir(&steamapps).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, steamapps.join("compatdata")).unwrap();
        std::fs::write(steamapps.join("shadercache"), "x").unwrap();
        std::fs::create_dir(steamapps.join("downloading")).unwrap();
        let at = safe_open::dir(&steamapps).unwrap();
        for name in ["compatdata", "shadercache", "downloading", "common"] {
            dir_in(&at, name, true).unwrap();
            let kind = std::fs::symlink_metadata(steamapps.join(name)).unwrap();
            assert!(kind.is_dir(), "{name}");
        }
        assert!(outside.join("keep").exists());

        // Without `replace` a link stays where an admin put it, and the open fails.
        std::os::unix::fs::symlink(&outside, steamapps.join("moved")).unwrap();
        assert!(dir_in(&at, "moved", false).is_err());
        assert!(std::fs::symlink_metadata(steamapps.join("moved"))
            .unwrap()
            .is_symlink());
    }

    #[test]
    fn the_token_belongs_to_the_punktfunk_user_when_there_is_one() {
        let user = Passwd {
            name: "punktfunk".into(),
            uid: 972,
            gid: 969,
            gecos: String::new(),
            home: PathBuf::from("/var/lib/punktfunk"),
        };
        assert_eq!(token_owner(Some(&user)), (972, 969));
        assert_eq!(token_owner(None), (0, 0));
    }

    /// The token file is private to its owner and carries the line it was given; a rewrite
    /// replaces it whole; removing the seat takes the directory with it.
    #[test]
    fn a_seat_token_is_private_and_removed_with_the_seat() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let me = std::fs::metadata(box_dir).unwrap();
        let owner = (me.uid(), me.gid());
        write_host_token(box_dir, &id(), "PUNKTFUNK_MGMT_TOKEN=aa\n", owner).unwrap();
        write_host_token(box_dir, &id(), "PUNKTFUNK_MGMT_TOKEN=bb\n", owner).unwrap();
        let dir = host_dir(box_dir, &id());
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&hosts_dir(box_dir)), 0o711);
        assert_eq!(mode(&dir.join("mgmt-token")), 0o600);
        assert_eq!(
            std::fs::read_to_string(dir.join("mgmt-token")).unwrap(),
            "PUNKTFUNK_MGMT_TOKEN=bb\n"
        );
        assert!(!dir.join("mgmt-token.tmp").exists());
        remove_seat_dirs(box_dir, &id()).unwrap();
        assert!(!dir.exists());
    }

    /// The save exchange folders are the seat's own with the box owner's ACL entries, keep what
    /// is in them across starts, and go with the seat.
    #[test]
    fn a_seat_s_save_exchange_is_kept_until_the_seat_goes() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path();
        let me = std::fs::metadata(state).unwrap();
        let seat = Passwd {
            name: "pf-seat-1".into(),
            uid: me.uid(),
            gid: me.gid(),
            gecos: String::new(),
            home: state.join("home"),
        };
        ensure_seat_dirs(state, &id(), &seat, Some(4242)).unwrap();
        let saves = seat_dir(state, &id()).join("saves");
        assert_eq!(mode(&saves), 0o711);
        for name in ["out", "in"] {
            let dir = saves.join(name);
            assert_eq!(mode(&dir), 0o770, "{name}");
            let mut buf = vec![0u8; 256];
            let Ok(len) = rustix::fs::getxattr(&dir, ACL_DEFAULT, &mut buf[..]) else {
                eprintln!("no POSIX ACLs under {}: skipped", state.display());
                continue;
            };
            assert_eq!(
                &buf[..len],
                exchange_acl(me.uid(), Some(4242), true),
                "{name}"
            );
            let len = rustix::fs::getxattr(&dir, ACL_ACCESS, &mut buf[..]).unwrap();
            assert_eq!(
                &buf[..len],
                exchange_acl(me.uid(), Some(4242), false),
                "{name}"
            );
        }
        assert_eq!(mode(&seat_dir(state, &id()).join("compatdata")), 0o700);
        std::fs::write(saves.join("out/slot.ps2"), "save").unwrap();
        ensure_seat_dirs(state, &id(), &seat, None).unwrap();
        assert!(saves.join("out/slot.ps2").exists());
        remove_seat_dirs(state, &id()).unwrap();
        assert!(!saves.exists());
    }

    /// The owner reaches the exchange and a new file in it; the seat is named in the default
    /// ACL alone, uids in order; with no owner the access ACL is the bare mode.
    #[test]
    fn the_exchange_acl_names_the_seat_and_the_owner() {
        let user = |uid: u32| (ACL_USER, 7, uid);
        let base = |named: &[(u16, u16, u32)]| {
            let mut e = vec![(ACL_USER_OBJ, 7, ACL_NO_ID)];
            e.extend_from_slice(named);
            e.push((ACL_GROUP_OBJ, 7, ACL_NO_ID));
            if !named.is_empty() {
                e.push((ACL_MASK, 7, ACL_NO_ID));
            }
            e.push((ACL_OTHER, 0, ACL_NO_ID));
            acl_bytes(&e)
        };
        assert_eq!(exchange_acl(975, Some(1000), false), base(&[user(1000)]));
        assert_eq!(
            exchange_acl(1975, Some(1000), true),
            base(&[user(1000), user(1975)])
        );
        assert_eq!(exchange_acl(975, None, false), base(&[]));
        assert_eq!(exchange_acl(1000, Some(1000), false), base(&[]));
    }

    /// The door reads a seat host's token below `persistence::default_root()`.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_ledger_directory_is_the_default_root() {
        assert_eq!(
            ledger_root(Path::new(super::super::STATE_DIR)),
            crate::persistence::default_root()
        );
    }

    /// The supervisor refuses a ledger root another uid owns or may write. Giving a directory
    /// away needs root, so elsewhere this checks nothing.
    #[test]
    fn a_ledger_root_with_another_owner_is_refused() {
        if !rustix::process::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = ledger_root(temp.path());
        ensure_layout(temp.path()).unwrap();
        std::os::unix::fs::chown(&root, Some(1000), None).unwrap();
        assert!(ensure_layout(temp.path()).is_err(), "owned by another uid");
        std::os::unix::fs::chown(&root, Some(0), None).unwrap();
        ensure_layout(temp.path()).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_layout(temp.path()).is_err(), "writable by others");
    }
}
