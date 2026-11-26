//! A seat's Steam in its home: cloned from an install, linked, and given a library list.
//!
//! [`provision_steam`] makes a Steam under the seat's home, cloned from a [`SteamSource`] when
//! there is one, the links that name it ([`link_steam`]), and a library list that names that
//! Steam, then the shared games folder. It never fails the seat: without a clone Steam
//! downloads itself on first run, and without the list it offers games again.
//!
//! The clone runs as the source's user: root reading a user's tree would follow what that user
//! links. A Steam root another user owns is no source of a user's ([`steam_source_of`]).
//! [`STEAM_ROOTS`] lists where Steam installs itself under a home.

use super::accounts::{is_ordinary, Passwd};
use super::layout::{io_err, seat_dir, seat_folder};
use super::library_copy::names;
use super::{err, run, safe_open};
use crate::backend::BackendError;
use crate::model::SeatId;
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

/// Steam's launcher script. A clone carries it and a self-bootstrapped Steam does too, so its
/// presence says the seat already has an install.
const STEAM_MARKER: &str = ".local/share/Steam/steam.sh";

/// Where Steam installs itself under a home: its own link, the native folder, Debian's installer,
/// the Flatpak's home and data folders, and the Snap.
pub(super) const STEAM_ROOTS: [&str; 6] = [
    ".steam/steam",
    ".local/share/Steam",
    ".steam/debian-installation",
    ".var/app/com.valvesoftware.Steam/.local/share/Steam",
    ".var/app/com.valvesoftware.Steam/data/Steam",
    "snap/steam/common/.local/share/Steam",
];

/// The seat's Steam folders that get the library list: its own copy, and the folder Debian's
/// launcher installs into when `~/.steam/steam` names no install.
pub(super) const SEAT_STEAMS: [&str; 2] = [".local/share/Steam", ".steam/debian-installation"];

/// `~/.steam/steam` and `~/.steam/root` naming the seat's own Steam, once it has one, where the
/// seat has neither, as `steam.sh` makes them. A launcher with another default install, as
/// Debian's, then runs that copy. Through descriptors: no link the seat user placed is followed.
pub(super) fn link_steam(seat: &Passwd) -> std::io::Result<()> {
    if !seat.home.join(STEAM_MARKER).exists() {
        return Ok(());
    }
    let steam = seat.home.join(".local/share/Steam");
    let home = safe_open::dir(&seat.home)?;
    let dot = match safe_open::dir_at(&home, ".steam")? {
        Some(dot) => dot,
        None => {
            rustix::fs::mkdirat(&home, ".steam", Mode::from_raw_mode(0o755 as _))?;
            let dot = safe_open::dir_at(&home, ".steam")?.ok_or(Errno::NOENT)?;
            std::os::unix::fs::fchown(&dot, Some(seat.uid), Some(seat.gid))?;
            dot
        }
    };
    let owner = rustix::process::Uid::from_raw(seat.uid);
    let group = rustix::process::Gid::from_raw(seat.gid);
    for name in ["steam", "root"] {
        match rustix::fs::symlinkat(&steam, &dot, name) {
            Ok(()) => {}
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
        rustix::fs::chownat(
            &dot,
            name,
            Some(owner),
            Some(group),
            AtFlags::SYMLINK_NOFOLLOW,
        )?;
    }
    Ok(())
}

/// The Steam install under `home` that `owns` admits: of [`STEAM_ROOTS`], the one whose library
/// list Steam wrote last, so an old native install beside the Flatpak in use loses to it. A root
/// with no list counts oldest, and the first listed wins a tie.
fn steam_root_of(home: &Path, owns: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let written = |root: &Path| {
        let list = root.join("steamapps").join("libraryfolders.vdf");
        std::fs::metadata(list)
            .and_then(|meta| meta.modified())
            .ok()
    };
    let mut best: Option<(Option<std::time::SystemTime>, PathBuf)> = None;
    for root in STEAM_ROOTS.iter().map(|root| home.join(root)) {
        let Ok(root) = std::fs::canonicalize(&root) else {
            continue;
        };
        if !root.is_dir() || !owns(&root) {
            continue;
        }
        let at = written(&root);
        if best.as_ref().is_none_or(|(best, _)| at > *best) {
            best = Some((at, root));
        }
    }
    best.map(|(_, root)| root)
}

/// A Steam install to clone and the user it is read as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SteamSource {
    pub root: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

impl SteamSource {
    /// The install at `root`, read as whoever owns it: `--steam-source`, which the operator names.
    pub(super) fn owned(root: PathBuf) -> Option<Self> {
        let meta = std::fs::metadata(&root).ok()?;
        Some(Self {
            root,
            uid: meta.uid(),
            gid: meta.gid(),
        })
    }
}

/// `user`'s Steam install, read as `user`. `~/.steam/steam` is the user's to point anywhere, so a
/// root another user owns is no source of theirs: root would read and mount that user's libraries.
pub(super) fn steam_source_of(user: &Passwd) -> Option<SteamSource> {
    owned_source_of(user, stat_owner)
}

/// The Steam a new seat is cloned from when no owner row names one: the lowest-uid ordinary user
/// that has one, read as that user. `--steam-source` names another.
pub(super) fn find_steam_source(users: &[Passwd]) -> Option<SteamSource> {
    find_source(users, stat_owner)
}

fn stat_owner(root: &Path) -> Option<u32> {
    std::fs::metadata(root).ok().map(|meta| meta.uid())
}

/// [`steam_source_of`] with `owner` naming who owns a Steam root.
fn owned_source_of(user: &Passwd, owner: impl Fn(&Path) -> Option<u32>) -> Option<SteamSource> {
    let root = steam_root_of(&user.home, |root| owner(root) == Some(user.uid))?;
    Some(SteamSource {
        root,
        uid: user.uid,
        gid: user.gid,
    })
}

/// [`find_steam_source`] with `owner` naming who owns a Steam root.
fn find_source(users: &[Passwd], owner: impl Fn(&Path) -> Option<u32>) -> Option<SteamSource> {
    let mut ordinary: Vec<&Passwd> = users.iter().filter(|u| is_ordinary(u)).collect();
    ordinary.sort_by_key(|u| u.uid);
    ordinary
        .into_iter()
        .find_map(|u| owned_source_of(u, &owner))
}

/// A Steam under the seat's home, cloned from `source` when there is one, the links that name it
/// ([`link_steam`]), and a library list that names that Steam, then the shared games folder.
/// Never fails the seat: without a clone Steam downloads itself on first run, and without the
/// list it offers games again.
pub(super) fn provision_steam(
    state: &Path,
    id: &SeatId,
    passwd: &Passwd,
    source: Option<&SteamSource>,
) {
    let steam = passwd.home.join(".local/share/Steam");
    if !passwd.home.join(STEAM_MARKER).exists() {
        match source {
            Some(src) => {
                let stage = seat_dir(state, id).join("steam-clone");
                if let Err(error) = clone_install(src, &stage, &steam, passwd) {
                    let _ = std::fs::remove_dir_all(&stage);
                    let _ = std::fs::remove_dir_all(&steam);
                    tracing::warn!(%error, "steam clone did not complete; the seat downloads its own");
                }
            }
            None => tracing::info!("no steam install to clone; the seat downloads its own"),
        }
    }
    if let Err(error) = link_steam(passwd) {
        tracing::warn!(%error, "seat's steam links not made");
    }
    if let Err(error) = write_library_folders(passwd) {
        tracing::warn!(%error, "steam library list not written");
    }
    let local = passwd.home.join(".local");
    let owner = format!("{}:{}", passwd.uid, passwd.gid);
    if let Err(error) = run(
        "chown_failed",
        "chown",
        &["-R", &owner, &local.to_string_lossy()],
    ) {
        tracing::warn!(%error, "steam files not handed to the seat user");
    }
}

/// `cp -a --reflink=auto` of every non-skipped top-level entry, run as the source's user: root
/// reading a user's tree would follow what that user links. The copy lands in `stage`, which only
/// root and that user reach, then goes to the seat's user and moves to `dst`. A filesystem without
/// reflinks makes a real copy, which takes a while but leaves the seat a working Steam.
fn clone_install(
    src: &SteamSource,
    stage: &Path,
    dst: &Path,
    seat: &Passwd,
) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("steam_clone", what, e);
    let names = clone_names(src).map_err(|e| io("read steam", e))?;
    if names.is_empty() {
        return Err(err(
            "steam_clone",
            "the steam install holds nothing to clone",
        ));
    }
    let _ = std::fs::remove_dir_all(stage);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(stage)
        .map_err(|e| io("create the stage", e))?;
    std::os::unix::fs::chown(stage, Some(src.uid), Some(src.gid))
        .map_err(|e| io("own the stage", e))?;
    let (uid, gid) = (src.uid.to_string(), src.gid.to_string());
    let mut args = vec![
        "--reuid".to_owned(),
        uid,
        "--regid".to_owned(),
        gid,
        "--clear-groups".to_owned(),
        "cp".to_owned(),
        "-a".to_owned(),
        "--reflink=auto".to_owned(),
        "--".to_owned(),
    ];
    args.extend(
        names
            .iter()
            .map(|name| src.root.join(name).to_string_lossy().into_owned()),
    );
    args.push(stage.to_string_lossy().into_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("steam_clone", "setpriv", &args)?;
    let owner = format!("{}:{}", seat.uid, seat.gid);
    run(
        "chown_failed",
        "chown",
        &["-R", &owner, &stage.to_string_lossy()],
    )?;
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io("create the seat steam", e))?;
    }
    let _ = std::fs::remove_dir_all(dst);
    std::fs::rename(stage, dst).map_err(|e| io("move the copy into place", e))
}

/// The top-level entries of `src` a seat inherits, all but [`crate::steam::skipped`]. Its folder
/// must be its user's own: one it links elsewhere would have root list a directory that user
/// can't.
fn clone_names(src: &SteamSource) -> std::io::Result<Vec<String>> {
    let dir = safe_open::dir(&src.root)?;
    if rustix::fs::fstat(&dir)?.st_uid != src.uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} isn't its user's own", src.root.display()),
        ));
    }
    let mut out: Vec<String> = names(&dir)?
        .into_iter()
        .filter(|name| !crate::steam::skipped(name))
        .collect();
    out.sort();
    Ok(out)
}

/// Writes the seat's first Steam library list: its own Steam, then the shared games folder as
/// the seat sees it. Steam keeps entry `0` for its own install and drops a folder listed there
/// at sign-in. Steam owns the file afterwards, and [`super::libraries::register`] adds the games
/// folder if it goes.
///
/// The folder was copied from another user's Steam, so root goes down by handle and creates the
/// file only where nothing is. A link at any step is never followed or written through.
fn write_library_folders(seat: &Passwd) -> std::io::Result<()> {
    let steam = seat.home.join(SEAT_STEAMS[0]);
    let mut dir = safe_open::dir(&seat.home)?;
    for name in SEAT_STEAMS[0].split('/').chain(["steamapps"]) {
        dir = seat_folder(&dir, name, seat)?;
    }
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mode = Mode::from_raw_mode(0o644);
    let file = match rustix::fs::openat(&dir, "libraryfolders.vdf", create, mode) {
        Ok(file) => file,
        Err(Errno::EXIST) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let folders = [
        steam.to_string_lossy().into_owned(),
        super::libraries::games_view()
            .to_string_lossy()
            .into_owned(),
    ];
    let mut file = std::fs::File::from(file);
    file.write_all(crate::steam::library_folders_vdf(&folders).as_bytes())?;
    std::os::unix::fs::fchown(&file, Some(seat.uid), Some(seat.gid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clone leaves the owner's sign-in with the owner: the account, the library, Steam
    /// Guard's machine token, the caches and the seats' library folders.
    #[test]
    fn the_account_and_the_library_stay_with_the_owner() {
        for skipped in [
            "config",
            "userdata",
            "appcache",
            "steamapps",
            "logs",
            "dumps",
            "depotcache",
            "ssfn1234567890",
            "clientregistry.blob",
            "local.vdf",
            "loginusers.vdf",
            ".punktfunk-seats",
        ] {
            assert!(crate::steam::skipped(skipped), "{skipped}");
        }
        for kept in [
            "ubuntu12_32",
            "steam.sh",
            "linux64",
            "package",
            "compatibilitytools.d",
        ] {
            assert!(!crate::steam::skipped(kept), "{kept}");
        }
    }

    /// A new seat's list names its own Steam at entry 0, the games folder's view after it, and a
    /// list Steam already wrote stays.
    #[test]
    fn a_new_seat_lists_its_own_steam_first() {
        let temp = tempfile::tempdir().unwrap();
        let seat = seat_at(&temp.path().join("home"));
        std::fs::create_dir(&seat.home).unwrap();
        let steam = seat.home.join(".local/share/Steam");
        write_library_folders(&seat).unwrap();
        let file = steam.join("steamapps/libraryfolders.vdf");
        let listed = crate::steam::library_paths(&std::fs::read_to_string(&file).unwrap());
        assert_eq!(
            listed,
            [
                steam.to_string_lossy().into_owned(),
                "/run/media/punktfunk/games".to_owned(),
            ]
        );
        std::fs::write(&file, "steam's own").unwrap();
        write_library_folders(&seat).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "steam's own");
    }

    /// The first library list is written only where nothing is. A link planted in the cloned
    /// folder is not written through, and a linked `steamapps` is refused.
    #[test]
    fn the_first_library_list_never_goes_through_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        let seat = seat_at(&temp.path().join("home"));
        let apps = seat.home.join(".local/share/Steam/steamapps");
        std::fs::create_dir_all(&apps).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let list = apps.join("libraryfolders.vdf");
        std::os::unix::fs::symlink(outside.join("planted"), &list).unwrap();
        write_library_folders(&seat).unwrap();
        assert!(!outside.join("planted").exists(), "written through a link");
        std::fs::remove_dir_all(&apps).unwrap();
        std::os::unix::fs::symlink(&outside, &apps).unwrap();
        assert!(write_library_folders(&seat).is_err());
        let made = std::fs::read_dir(&outside).unwrap().count();
        assert_eq!(made, 0, "nothing made through a linked folder");
    }

    /// A seat whose home is `home`, on the account the tests run as.
    fn seat_at(home: &Path) -> Passwd {
        Passwd {
            name: "pf-seat-1".into(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            gecos: String::new(),
            home: home.into(),
        }
    }

    #[test]
    fn a_clone_lists_what_a_seat_inherits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for dir in ["ubuntu12_32", "dumps", "steamapps"] {
            std::fs::create_dir(root.join(dir)).unwrap();
        }
        for file in ["steam.sh", "ssfn42", "clientregistry.blob", "registry.vdf"] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        let meta = std::fs::metadata(&root).unwrap();
        let source = SteamSource {
            root,
            uid: meta.uid(),
            gid: meta.gid(),
        };
        assert_eq!(clone_names(&source).unwrap(), ["steam.sh", "ubuntu12_32"]);
    }

    #[test]
    fn the_steam_source_is_the_lowest_ordinary_user_with_a_steam() {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let user = |name: &str, uid: u32, gecos: &str, with_steam: bool| {
            let home = base.join(name);
            if with_steam {
                std::fs::create_dir_all(home.join(".local/share/Steam")).unwrap();
            }
            Passwd {
                name: name.into(),
                uid,
                gid: uid,
                gecos: gecos.into(),
                home,
            }
        };
        let users = [
            user("root", 0, "", true),
            user("seat", 975, "punktfunk-seat=0123", true),
            user("late", 1002, "", true),
            user("bare", 1000, "", false),
            user("early", 1001, "", true),
        ];
        // Whoever's home holds the root owns it, as on disk.
        let owner = |root: &Path| {
            let home = users.iter().find(|u| root.starts_with(&u.home));
            home.map(|u| u.uid)
        };
        let early = SteamSource {
            root: base.join("early/.local/share/Steam"),
            uid: 1001,
            gid: 1001,
        };
        assert_eq!(find_source(&users, owner), Some(early.clone()));
        assert_eq!(find_source(&users[..2], owner), None);
        // The owner's row names its user, whose uid is not the lowest.
        assert_eq!(
            owned_source_of(&users[2], owner).map(|source| source.uid),
            Some(1002)
        );
        assert_eq!(
            owned_source_of(&users[3], owner),
            None,
            "an owner without a Steam"
        );

        // The lowest user links their Steam to another user's: not their source, so the next
        // user with a Steam of their own is. The owner's row gets the same answer.
        let linked = users[3].home.join(".steam");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(&early.root, linked.join("steam")).unwrap();
        assert_eq!(find_source(&users, owner), Some(early));
        assert_eq!(owned_source_of(&users[3], owner), None);
    }

    /// Of the Steam folders under a home, the one whose list Steam wrote last is the owner's: an
    /// old native install beside the Flatpak in use loses to it. A root the user doesn't own is
    /// passed over.
    #[test]
    fn the_owner_s_steam_is_the_install_that_wrote_its_list_last() {
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        let list = |root: &str, age: u64| {
            let steamapps = home.join(root).join("steamapps");
            std::fs::create_dir_all(&steamapps).unwrap();
            let file = std::fs::File::create(steamapps.join("libraryfolders.vdf")).unwrap();
            let at = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            file.set_modified(at).unwrap();
        };
        let all = |_: &Path| true;
        assert_eq!(steam_root_of(&home, all), None, "no Steam");
        list(".local/share/Steam", 3600);
        assert_eq!(
            steam_root_of(&home, all),
            Some(home.join(".local/share/Steam"))
        );
        let flatpak = ".var/app/com.valvesoftware.Steam/data/Steam";
        list(flatpak, 60);
        assert_eq!(steam_root_of(&home, all), Some(home.join(flatpak)));
        let native = |root: &Path| !root.starts_with(home.join(".var"));
        assert_eq!(
            steam_root_of(&home, native),
            Some(home.join(".local/share/Steam"))
        );
    }

    /// A seat with its own Steam gets the links a launcher follows to it. A link it has stays, and
    /// a `.steam` that is itself a link is refused.
    #[test]
    fn a_seat_s_own_steam_gets_the_links_launchers_follow() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let seat = Passwd {
            name: "pf-seat-1".into(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            gecos: String::new(),
            home: home.clone(),
        };
        link_steam(&seat).unwrap();
        assert!(!home.join(".steam").exists(), "no install, no links");
        let own = home.join(".local/share/Steam");
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(home.join(STEAM_MARKER), "").unwrap();
        link_steam(&seat).unwrap();
        assert_eq!(std::fs::read_link(home.join(".steam/steam")).unwrap(), own);
        assert_eq!(std::fs::read_link(home.join(".steam/root")).unwrap(), own);
        std::fs::remove_file(home.join(".steam/root")).unwrap();
        std::os::unix::fs::symlink("/elsewhere", home.join(".steam/root")).unwrap();
        link_steam(&seat).unwrap();
        let kept = std::fs::read_link(home.join(".steam/root")).unwrap();
        assert_eq!(kept, Path::new("/elsewhere"));
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::remove_dir_all(home.join(".steam")).unwrap();
        std::os::unix::fs::symlink(&outside, home.join(".steam")).unwrap();
        assert!(link_steam(&seat).is_err());
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "nothing made through it"
        );
    }
}
