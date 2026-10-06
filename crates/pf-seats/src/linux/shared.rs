//! What seats share and what each keeps: the games folder, the trust copy and the Steam clone.
//!
//! Under the box directory (`/var/lib/punktfunk`):
//!
//! - `games/steamapps/{common,compatdata,shadercache,downloading}` is one library every seat
//!   writes. `compatdata`, `shadercache` and `downloading` are covered per seat by bind mounts
//!   (`session.rs`), so Wine prefixes and download state never cross.
//! - `seats/<id>/` holds the seat user's home and those three private directories.
//! - `trust/<id>/` is a read-only copy of the box's identity, pairing store and profiles, so a
//!   seat user can read what it needs and nothing else in the box directory.
//! - `seats/hosts/<id>/mgmt-token` is the token the box presents to that seat host's loopback
//!   API. It belongs to the `punktfunk` user, not the seat group: every seat user is a member.

use super::accounts::Passwd;
use super::session::PRIVATE_DIRS;
use super::{err, run};
use crate::backend::BackendError;
use crate::model::SeatId;
use rustix::fs::OFlags;
use std::collections::HashSet;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Box files a seat host reads. The identity pair is required; the rest may not exist yet.
const TRUST_FILES: [&str; 5] = [
    "native-cert.pem",
    "native-key.pem",
    "punktfunk1-paired.json",
    "profiles.json",
    "display-settings.json",
];
const REQUIRED: [&str; 2] = ["native-cert.pem", "native-key.pem"];

/// Profile avatars, one flat directory beside `profiles.json`.
const AVATARS: &str = "profiles";

/// A trust file larger than this is not ours.
const MAX_TRUST_BYTES: u64 = 8 * 1024 * 1024;

/// Steam top-level entries a seat does not inherit: the owner's account, library and logs.
const CLONE_SKIP: &[&str] = &["config", "userdata", "appcache", "steamapps", "logs"];

/// Steam's launcher script. A clone carries it and a self-bootstrapped Steam does too, so its
/// presence says the seat already has an install.
const STEAM_MARKER: &str = ".local/share/Steam/steam.sh";

pub(super) fn seat_dir(box_dir: &Path, id: &SeatId) -> PathBuf {
    box_dir.join("seats").join(id.as_str())
}

pub(super) fn trust_dir(box_dir: &Path, id: &SeatId) -> PathBuf {
    box_dir.join("trust").join(id.as_str())
}

pub(super) fn games_dir(box_dir: &Path) -> PathBuf {
    box_dir.join("games")
}

/// The ledger's directory, `seats` under the box directory: the same path as
/// `persistence::default_root()` when the box directory is the config dir. Seat directories are
/// named by id, so they never collide with the ledger's files.
pub fn ledger_root(box_dir: &Path) -> PathBuf {
    box_dir.join("seats")
}

fn hosts_dir(box_dir: &Path) -> PathBuf {
    ledger_root(box_dir).join("hosts")
}

/// Where the box reads a seat host's management token.
pub(super) fn host_dir(box_dir: &Path, id: &SeatId) -> PathBuf {
    hosts_dir(box_dir).join(id.as_str())
}

/// The owner of a seat's token file: the `punktfunk` user's `(uid, gid)` when it exists, else
/// root, so the box host reads it as itself and no seat user can.
pub(super) fn token_owner(punktfunk: Option<&Passwd>) -> (u32, u32) {
    punktfunk.map_or((0, 0), |user| (user.uid, user.gid))
}

/// Writes `line` as the seat's token file, `0600` in a `0700` directory, both owned by `owner`.
pub(super) fn write_host_token(
    box_dir: &Path,
    id: &SeatId,
    line: &str,
    owner: (u32, u32),
) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("host_token", what, e);
    make_dir(&hosts_dir(box_dir), 0o711).map_err(|e| io("create the hosts directory", e))?;
    let dir = host_dir(box_dir, id);
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

fn io_err(code: &str, what: &str, error: std::io::Error) -> BackendError {
    err(code, format!("{what}: {error}"))
}

/// Creates `path` and its parents, then sets `mode` on `path` itself.
pub(super) fn make_dir(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// The directories every seat needs below the box directory, created if absent. `seats` and
/// `trust` are `0711`: a seat user walks to its own entry and can't list the others.
pub(super) fn ensure_layout(box_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(box_dir)?;
    make_dir(&ledger_root(box_dir), 0o711)?;
    make_dir(&hosts_dir(box_dir), 0o711)?;
    make_dir(&box_dir.join("trust"), 0o711)
}

/// The shared games library: group `punktfunk`, setgid, with a default ACL so a file one seat
/// creates is writable by the others.
pub(super) fn ensure_games(box_dir: &Path, gid: u32) -> Result<(), BackendError> {
    let games = games_dir(box_dir);
    let steamapps = games.join("steamapps");
    let mut dirs = vec![games, steamapps.clone()];
    for name in ["common", "compatdata", "shadercache", "downloading"] {
        dirs.push(steamapps.join(name));
    }
    for dir in &dirs {
        let io = |what: &str, e| io_err("games_dir", &format!("{what} {}", dir.display()), e);
        std::fs::create_dir_all(dir).map_err(|e| io("create", e))?;
        std::os::unix::fs::chown(dir, Some(0), Some(gid)).map_err(|e| io("own", e))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o2775))
            .map_err(|e| io("set the mode of", e))?;
    }
    let group = super::accounts::GAMES_GROUP;
    let mut args = vec!["-m".to_owned(), format!("g:{group}:rwX,d:g:{group}:rwX")];
    args.extend(dirs.iter().map(|d| d.to_string_lossy().into_owned()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("setfacl_failed", "setfacl", &args).map(drop)
}

/// Re-applies the ACL mask below the games folder. A file one seat creates with mode 0644 or 0755
/// gets a mask of `r--` or `r-x` that cuts the group entry, so the other seats can't write it.
///
/// Walks the whole tree each call: cheap for a normal library, slow past millions of files,
/// where an inotify watch on `steamapps` would fix only what changed.
pub(super) fn fix_acl(box_dir: &Path) -> Result<(), BackendError> {
    let games = games_dir(box_dir);
    run(
        "setfacl_failed",
        "setfacl",
        &["-R", "-m", "m::rwX", &games.to_string_lossy()],
    )
    .map(drop)
}

/// The seat's private directories, owned by its user and in place before its unit starts: a
/// missing bind source fails the unit with `226/NAMESPACE`.
pub(super) fn ensure_seat_dirs(
    box_dir: &Path,
    id: &SeatId,
    passwd: &Passwd,
) -> Result<(), BackendError> {
    for name in PRIVATE_DIRS {
        let dir = seat_dir(box_dir, id).join(name);
        let io = |what: &str, e| io_err("seat_dirs", &format!("{what} {}", dir.display()), e);
        make_dir(&dir, 0o700).map_err(|e| io("create", e))?;
        std::os::unix::fs::chown(&dir, Some(passwd.uid), Some(passwd.gid))
            .map_err(|e| io("own", e))?;
    }
    Ok(())
}

/// Removes the seat's directories, its trust copy and its token. Missing ones are fine.
pub(super) fn remove_seat_dirs(box_dir: &Path, id: &SeatId) -> Result<(), BackendError> {
    for dir in [
        seat_dir(box_dir, id),
        trust_dir(box_dir, id),
        host_dir(box_dir, id),
    ] {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err("seat_dirs", &format!("remove {}", dir.display()), e)),
        }
    }
    Ok(())
}

/// Whether the copy at `dst` is stale: absent, or a different size or modification time. Copies
/// keep the source's mtime, so an unchanged file compares equal.
fn needs_copy(src: (SystemTime, u64), dst: Option<(SystemTime, u64)>) -> bool {
    dst != Some(src)
}

/// Opens `path` as a regular file with one link, never through a symlink. `None` when absent.
fn open_regular(path: &Path) -> std::io::Result<Option<(std::fs::File, std::fs::Metadata)>> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() > 1 || meta.len() > MAX_TRUST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{} is not a plain single-link file within the size cap",
                path.display()
            ),
        ));
    }
    Ok(Some((file, meta)))
}

fn stamp(meta: &std::fs::Metadata) -> std::io::Result<(SystemTime, u64)> {
    Ok((meta.modified()?, meta.len()))
}

/// Who reads a trust copy. A seat user reads through the group, `root:gid 0640`. The owner's own
/// account may have a shared primary group, so it reads as the file's owner, `uid:gid 0600`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reader {
    uid: Option<u32>,
    gid: u32,
}

impl Reader {
    pub(super) fn group(gid: u32) -> Self {
        Self { uid: None, gid }
    }

    pub(super) fn person(uid: u32, gid: u32) -> Self {
        Self {
            uid: Some(uid),
            gid,
        }
    }

    fn file_mode(self) -> u32 {
        if self.uid.is_some() {
            0o600
        } else {
            0o640
        }
    }

    fn dir_mode(self) -> u32 {
        if self.uid.is_some() {
            0o700
        } else {
            0o750
        }
    }
}

/// Copies `src` to `dst` for `reader` when it differs, and removes `dst` when `src` is gone.
/// Returns whether `dst` changed.
fn sync_file(src: &Path, dst: &Path, reader: Reader) -> std::io::Result<bool> {
    let Some((mut file, meta)) = open_regular(src)? else {
        return match std::fs::remove_file(dst) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        };
    };
    let wanted = stamp(&meta)?;
    let current = std::fs::symlink_metadata(dst)
        .ok()
        .and_then(|m| stamp(&m).ok());
    if !needs_copy(wanted, current) {
        return Ok(false);
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.read_to_end(&mut bytes)?;
    let temp = dst.with_extension("tmp");
    let _ = std::fs::remove_file(&temp);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(reader.file_mode())
        .open(&temp)?;
    out.write_all(&bytes)?;
    out.set_permissions(std::fs::Permissions::from_mode(reader.file_mode()))?;
    std::os::unix::fs::fchown(&out, reader.uid, Some(reader.gid))?;
    out.set_modified(wanted.0)?;
    drop(out);
    std::fs::rename(temp, dst)?;
    Ok(true)
}

/// Brings the seat's trust copy level with the box. The identity pair must exist: a seat that
/// minted its own would strand every client's pin. Returns whether anything changed.
pub(super) fn refresh_trust(
    box_dir: &Path,
    id: &SeatId,
    reader: Reader,
) -> Result<bool, BackendError> {
    let dst = trust_dir(box_dir, id);
    let io = |what: &str, e| io_err("trust_copy", what, e);
    make_dir(&dst, reader.dir_mode()).map_err(|e| io("create the trust copy", e))?;
    std::os::unix::fs::chown(&dst, reader.uid, Some(reader.gid))
        .map_err(|e| io("own the trust copy", e))?;
    for name in REQUIRED {
        if !box_dir.join(name).is_file() {
            return Err(err(
                "trust_missing",
                format!("the box has no {name} yet; start its host once first"),
            ));
        }
    }
    let mut changed = false;
    for name in TRUST_FILES {
        changed |= sync_file(&box_dir.join(name), &dst.join(name), reader)
            .map_err(|e| io(&format!("copy {name}"), e))?;
    }
    changed |= sync_avatars(&box_dir.join(AVATARS), &dst.join(AVATARS), reader)
        .map_err(|e| io("copy the avatars", e))?;
    Ok(changed)
}

/// Mirrors the flat avatar directory: copies each file, drops copies whose source is gone.
fn sync_avatars(src: &Path, dst: &Path, reader: Reader) -> std::io::Result<bool> {
    let mut wanted = HashSet::new();
    let mut changed = false;
    if src.is_dir() {
        make_dir(dst, reader.dir_mode())?;
        std::os::unix::fs::chown(dst, reader.uid, Some(reader.gid))?;
        for entry in std::fs::read_dir(src)? {
            let name = entry?.file_name();
            let Some(text) = name.to_str().filter(|n| safe_name(n)) else {
                continue;
            };
            // A link or oversized entry is skipped, not copied and not fatal.
            if let Ok(did) = sync_file(&src.join(text), &dst.join(text), reader) {
                changed |= did;
                wanted.insert(text.to_owned());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(dst) {
        for entry in entries.flatten() {
            let keep = entry
                .file_name()
                .to_str()
                .is_some_and(|n| wanted.contains(n));
            if !keep {
                let _ = std::fs::remove_file(entry.path());
                changed = true;
            }
        }
    }
    if !src.is_dir() {
        let _ = std::fs::remove_dir(dst);
    }
    Ok(changed)
}

/// A file name that is one plain path component: letters, digits, `.`, `_` and `-`.
fn safe_name(name: &str) -> bool {
    !matches!(name, "." | "..")
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Top-level Steam entries the seat does not get.
fn excluded_from_clone(name: &str) -> bool {
    CLONE_SKIP.contains(&name) || name.ends_with(".vdf")
}

/// The box owner's Steam install: `~/.steam/steam`, else `~/.local/share/Steam`.
fn steam_root_of(home: &Path) -> Option<PathBuf> {
    let linked = home.join(".steam/steam");
    if linked.is_dir() {
        return Some(std::fs::canonicalize(&linked).unwrap_or(linked));
    }
    let data = home.join(".local/share/Steam");
    data.is_dir().then_some(data)
}

/// The Steam a new seat is cloned from: the lowest-uid ordinary user that has one. A box with
/// several Steam users picks the first; `--steam-source` names another.
pub(super) fn find_steam_source(users: &[Passwd]) -> Option<PathBuf> {
    let mut ordinary: Vec<&Passwd> = users
        .iter()
        .filter(|u| (1000..60000).contains(&u.uid) && !u.gecos.starts_with("punktfunk-seat="))
        .collect();
    ordinary.sort_by_key(|u| u.uid);
    ordinary.into_iter().find_map(|u| steam_root_of(&u.home))
}

/// A Steam under the seat's home, cloned from `source` when there is one, and a library list that
/// names the shared games folder first. Never fails the seat: without a clone Steam downloads
/// itself on first run, and without the list it offers games again.
pub(super) fn provision_steam(box_dir: &Path, passwd: &Passwd, source: Option<&Path>) {
    let steam = passwd.home.join(".local/share/Steam");
    if !passwd.home.join(STEAM_MARKER).exists() {
        match source {
            Some(src) => {
                if let Err(error) = clone_install(src, &steam) {
                    let _ = std::fs::remove_dir_all(&steam);
                    tracing::warn!(%error, "steam clone did not complete; the seat downloads its own");
                }
            }
            None => tracing::info!("no steam install to clone; the seat downloads its own"),
        }
    }
    if let Err(error) = write_library_folders(box_dir, &steam) {
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

/// `cp -a --reflink=auto` of every non-skipped top-level entry. A filesystem without reflinks
/// makes a real copy, which takes a while but leaves the seat a working Steam.
fn clone_install(src: &Path, dst: &Path) -> Result<(), BackendError> {
    let mut args = vec!["-a".to_owned(), "--reflink=auto".to_owned()];
    let entries = std::fs::read_dir(src).map_err(|e| io_err("steam_clone", "read steam", e))?;
    for entry in entries.flatten() {
        if !excluded_from_clone(&entry.file_name().to_string_lossy()) {
            args.push(entry.path().to_string_lossy().into_owned());
        }
    }
    if args.len() == 2 {
        return Err(err(
            "steam_clone",
            "the steam install holds nothing to clone",
        ));
    }
    std::fs::create_dir_all(dst).map_err(|e| io_err("steam_clone", "create the seat steam", e))?;
    args.push(dst.to_string_lossy().into_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run("steam_clone", "cp", &args).map(drop)
}

/// The smallest `libraryfolders.vdf` Steam accepts: one numbered entry per folder.
fn library_folders_vdf(paths: &[String]) -> String {
    let mut out = String::from("\"libraryfolders\"\n{\n");
    for (i, path) in paths.iter().enumerate() {
        out.push_str(&format!(
            "\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{path}\"\n\t}}\n"
        ));
    }
    out.push_str("}\n");
    out
}

/// Lists the shared games folder first, then the seat's own Steam. Written once; Steam owns the
/// file afterwards.
fn write_library_folders(box_dir: &Path, steam: &Path) -> std::io::Result<()> {
    let dir = steam.join("steamapps");
    let file = dir.join("libraryfolders.vdf");
    if file.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir)?;
    let folders = [
        games_dir(box_dir).to_string_lossy().into_owned(),
        steam.to_string_lossy().into_owned(),
    ];
    std::fs::write(file, library_folders_vdf(&folders))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    fn own_gid(dir: &Path) -> u32 {
        std::fs::metadata(dir).unwrap().gid()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn box_with_identity(dir: &Path) {
        std::fs::write(dir.join("native-cert.pem"), "cert").unwrap();
        std::fs::write(dir.join("native-key.pem"), "key").unwrap();
    }

    #[test]
    fn the_account_and_the_library_stay_with_the_owner() {
        for skipped in [
            "config",
            "userdata",
            "appcache",
            "steamapps",
            "logs",
            "local.vdf",
            "loginusers.vdf",
        ] {
            assert!(excluded_from_clone(skipped), "{skipped}");
        }
        for kept in [
            "ubuntu12_32",
            "steam.sh",
            "linux64",
            "package",
            "compatibilitytools.d",
        ] {
            assert!(!excluded_from_clone(kept), "{kept}");
        }
    }

    #[test]
    fn the_library_list_names_the_shared_folder_first() {
        let list = library_folders_vdf(&["/g".to_owned(), "/home/s/.local/share/Steam".to_owned()]);
        assert_eq!(
            list,
            "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/g\"\n\t}\n\
             \t\"1\"\n\t{\n\t\t\"path\"\t\t\"/home/s/.local/share/Steam\"\n\t}\n}\n"
        );
    }

    #[test]
    fn a_copy_is_made_only_for_a_missing_or_different_file() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        assert!(needs_copy((t, 5), None));
        assert!(needs_copy((t, 5), Some((t, 6))));
        assert!(needs_copy(
            (t, 5),
            Some((t + std::time::Duration::from_secs(1), 5))
        ));
        assert!(!needs_copy((t, 5), Some((t, 5))));
    }

    #[test]
    fn the_trust_copy_follows_the_box_and_is_group_readable_only() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let gid = own_gid(box_dir);
        assert_eq!(
            refresh_trust(box_dir, &id(), Reader::group(gid))
                .unwrap_err()
                .code,
            "trust_missing",
            "no identity, no seat"
        );
        box_with_identity(box_dir);
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        std::fs::create_dir(box_dir.join("profiles")).unwrap();
        std::fs::write(box_dir.join("profiles/p1.png"), "png").unwrap();
        std::fs::write(box_dir.join("mgmt-token"), "secret").unwrap();

        assert!(refresh_trust(box_dir, &id(), Reader::group(gid)).unwrap());
        let trust = trust_dir(box_dir, &id());
        assert_eq!(
            std::fs::read_to_string(trust.join("native-key.pem")).unwrap(),
            "key"
        );
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles/p1.png")).unwrap(),
            "png"
        );
        assert_eq!(mode(&trust), 0o750);
        assert_eq!(mode(&trust.join("native-key.pem")), 0o640);
        assert!(
            !trust.join("mgmt-token").exists(),
            "only the listed files cross"
        );
        assert!(
            !refresh_trust(box_dir, &id(), Reader::group(gid)).unwrap(),
            "unchanged is a no-op"
        );

        // A changed file, a new avatar and a removed avatar all follow.
        std::fs::write(box_dir.join("profiles.json"), "{\"a\":1}").unwrap();
        std::fs::write(box_dir.join("profiles/p2.png"), "png2").unwrap();
        std::fs::remove_file(box_dir.join("profiles/p1.png")).unwrap();
        assert!(refresh_trust(box_dir, &id(), Reader::group(gid)).unwrap());
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles.json")).unwrap(),
            "{\"a\":1}"
        );
        assert!(trust.join("profiles/p2.png").exists());
        assert!(!trust.join("profiles/p1.png").exists());

        // A file the box drops is dropped from the copy.
        std::fs::remove_file(box_dir.join("profiles.json")).unwrap();
        assert!(refresh_trust(box_dir, &id(), Reader::group(gid)).unwrap());
        assert!(!trust.join("profiles.json").exists());
    }

    /// The owner's copy is theirs alone: owned by the account, closed to every group, because a
    /// person's primary group may be one everybody on the box shares.
    #[test]
    fn the_owner_s_trust_copy_is_private_to_the_account() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        box_with_identity(box_dir);
        std::fs::create_dir(box_dir.join("profiles")).unwrap();
        std::fs::write(box_dir.join("profiles/a.png"), "png").unwrap();
        let me = std::fs::metadata(box_dir).unwrap();
        let reader = Reader::person(me.uid(), me.gid());
        assert!(refresh_trust(box_dir, &id(), reader).is_ok());
        let trust = trust_dir(box_dir, &id());
        assert_eq!(mode(&trust), 0o700);
        assert_eq!(mode(&trust.join("native-key.pem")), 0o600);
        assert_eq!(mode(&trust.join("profiles")), 0o700);
        assert_eq!(mode(&trust.join("profiles/a.png")), 0o600);
    }

    /// A link in the box directory is never followed into the copy.
    #[test]
    fn a_symlink_in_the_box_is_not_copied() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        box_with_identity(box_dir);
        let secret = box_dir.join("elsewhere");
        std::fs::write(&secret, "root only").unwrap();
        std::os::unix::fs::symlink(&secret, box_dir.join("profiles.json")).unwrap();
        let gid = own_gid(box_dir);
        let error = refresh_trust(box_dir, &id(), Reader::group(gid)).unwrap_err();
        assert_eq!(error.code, "trust_copy");
        assert!(!trust_dir(box_dir, &id()).join("profiles.json").exists());
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

    #[test]
    fn the_ledger_directory_is_the_default_root() {
        assert_eq!(
            ledger_root(&pf_paths::config_dir()),
            crate::persistence::default_root()
        );
    }

    #[test]
    fn the_steam_source_is_the_lowest_ordinary_user_with_a_steam() {
        let temp = tempfile::tempdir().unwrap();
        let user = |name: &str, uid: u32, gecos: &str, with_steam: bool| {
            let home = temp.path().join(name);
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
        assert_eq!(
            find_steam_source(&users),
            Some(temp.path().join("early/.local/share/Steam"))
        );
        assert_eq!(find_steam_source(&users[..2]), None);
    }
}
