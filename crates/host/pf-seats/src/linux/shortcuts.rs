//! The box owner's Steam shortcuts in a made seat's own Steam ([`shortcuts`]).
//!
//! A shortcut reaches a seat when the box's catalog lists it from a source left on, its program
//! lies in its start folder, and that folder is the owner's, lies deeper than directly in their
//! home, and passes the limits of a granted root ([`libraries::shareable`]). The seat sees the folder at
//! `/run/media/punktfunk/shortcuts/<key>` as an [`Overlay`]: a game that saves beside itself
//! keeps each person's saves apart, and the owner's folder never changes. A custom Proton tool such a shortcut maps comes the same way and
//! is linked into the seat's `compatibilitytools.d`. Launch options that still name the owner's
//! home after the move keep a shortcut on the owner's desktop: the seat can't see there.
//!
//! Root writes the seat's records through directory handles while its session is down: each
//! signed-in account's `shortcuts.vdf` and grid art, and the Proton rows of `config/config.vdf`.
//! The appids go into the box's [`shortcuts::SEAT_FILE`], for the door, and the seat's own copy,
//! which is also the ledger of what the box put there.

use super::accounts::Passwd;
use super::libraries::{self, Library, Overlay, VIEW};
use super::safe_open;
use super::shared::{self, Reader};
use crate::model::SeatId;
use crate::steam::shortcuts::{self, CompatRow, Fields};
use rustix::fs::AtFlags;
use rustix::io::Errno;
use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// A catalog with descriptions runs to tens of megabytes; past this it isn't the box's.
const MAX_CATALOG_BYTES: u64 = 64 * 1024 * 1024;
/// A `shortcuts.vdf`, a `config.vdf` or a piece of grid art; past this it isn't one.
const MAX_STEAM_FILE_BYTES: u64 = 16 * 1024 * 1024;
const TOOLS: &str = "compatibilitytools.d";

/// Seats starting together rewrite the box's one [`shortcuts::SEAT_FILE`] in turn.
static SEAT_FILE_LOCK: Mutex<()> = Mutex::new(());

/// What a seat gets of the owner's shortcuts.
#[derive(Default)]
struct Picked {
    /// Each entry with its paths moved to its view.
    entries: Vec<Fields>,
    /// Grid art by appid and file name.
    art: Vec<(u32, String, Vec<u8>)>,
    compat: Vec<CompatRow>,
    overlays: Vec<Overlay>,
    /// Each tool's link name in the seat's `compatibilitytools.d`, and its view.
    tools: Vec<(String, PathBuf)>,
}

/// The owner's shortcuts made seat `id` gets, written into `seat`'s Steam, and the overlays its
/// unit stages for them. `steam_root` is the owner's Steam, `libraries` what the seat sees of it,
/// `door` the reader of the box's library copy. A seat that is the owner gets nothing written.
pub(super) fn share(
    state: &Path,
    id: &SeatId,
    seat: &Passwd,
    owner: Option<&Passwd>,
    steam_root: Option<&Path>,
    libraries: &[Library],
    door: Option<Reader>,
) -> Vec<Overlay> {
    if owner.is_some_and(|owner| owner.uid == seat.uid) {
        return Vec::new();
    }
    let trust = safe_open::dir(&shared::trust_dir(state, id)).ok();
    let before = trust
        .as_ref()
        .and_then(|dir| read(dir, shortcuts::SEAT_FILE, MAX_STEAM_FILE_BYTES))
        .map_or_else(BTreeSet::new, |file| {
            shortcuts::shared_with(&file, id.as_str())
        });
    let picked = match (owner, steam_root) {
        (Some(owner), Some(root)) => pick(state, seat, owner, root, libraries),
        _ => Picked::default(),
    };
    let overlays: Vec<Overlay> = (picked.overlays.iter())
        .filter(|overlay| match prepare(state, id, seat, overlay) {
            Ok(()) => true,
            Err(error) => {
                let folder = overlay.root.display();
                tracing::warn!(seat = %id, %folder, %error, "shared shortcut folder not staged");
                false
            }
        })
        .cloned()
        .collect();
    let ours = place(seat, &picked, &before);
    publish(state, id, seat, trust.as_ref(), &ours, door);
    tracing::info!(seat = %id, shared = ours.len(), folders = overlays.len(),
        tools = picked.tools.len(), "owner's shortcuts shared with the seat");
    overlays
}

/// The owner's shortcuts the box's catalog lists from a source left on, from each account of the
/// owner's Steam at `root`, the first of an appid kept, that seat `seat` can play.
fn pick(state: &Path, seat: &Passwd, owner: &Passwd, root: &Path, libraries: &[Library]) -> Picked {
    let box_library = safe_open::dir(&shared::box_library_dir(state)).ok();
    let catalog = |name, max| (box_library.as_ref()).and_then(|dir| read(dir, name, max));
    let off = shortcuts::off_sources(
        &catalog("library-scanners.json", MAX_STEAM_FILE_BYTES).unwrap_or_default(),
    );
    let listed = shortcuts::listed(
        &catalog("library.json", MAX_CATALOG_BYTES).unwrap_or_default(),
        &off,
    );
    let mut picked = Picked::default();
    if listed.is_empty() {
        return picked;
    }
    let steam = match owned_dir(root, owner.uid) {
        Ok(steam) => steam,
        Err(error) => {
            tracing::warn!(steam = %root.display(), %error, "owner's steam unreadable; no shortcut shared");
            return picked;
        }
    };
    let skip = |entry: &Fields, reason: &str| {
        let name = shortcuts::text(entry, "AppName").unwrap_or_default();
        tracing::info!(shortcut = %name, reason, "owner's shortcut not shared with seats");
    };
    let home = std::fs::canonicalize(&owner.home).unwrap_or_else(|_| owner.home.clone());
    // Each listed entry with its art, its folder as stored, and that folder once it passed those limits.
    let mut found = Vec::new();
    let mut seen = BTreeSet::new();
    for config in account_configs(&steam) {
        let Some(entries) = read_owned(&config, "shortcuts.vdf", owner.uid)
            .and_then(|bytes| shortcuts::parse(&bytes))
        else {
            continue;
        };
        let grid = safe_open::dir_at(&config, "grid").ok().flatten();
        for entry in entries {
            let Some(id) = shortcuts::appid(&entry).filter(|id| listed.contains(id)) else {
                continue;
            };
            if !seen.insert(id) {
                continue;
            }
            let Some(stored) = shortcuts::folder(&entry) else {
                skip(&entry, "it starts in no absolute folder");
                continue;
            };
            let folder = match libraries::shareable(Path::new(&stored), state, owner, libraries) {
                Ok(folder) if in_home_itself(&folder, &home) => {
                    skip(&entry, "it starts in a folder directly in the owner's home");
                    continue;
                }
                Ok(folder) if owns(&folder, owner.uid) => folder,
                Ok(_) => {
                    skip(&entry, "its folder isn't the owner's");
                    continue;
                }
                Err(reason) => {
                    skip(&entry, reason);
                    continue;
                }
            };
            let art: Vec<(u32, String, Vec<u8>)> = (grid.as_ref().into_iter())
                .flat_map(|grid| {
                    shortcuts::art_names(id).filter_map(|name| {
                        let bytes = read_owned(grid, &name, owner.uid)?;
                        Some((id, name, bytes))
                    })
                })
                .collect();
            found.push((entry, art, stored, folder));
        }
    }
    let roots = outermost(found.iter().map(|(.., folder)| folder.clone()).collect());
    let idmap = libraries::idmap((owner.uid, owner.gid), seat);
    for (entry, art, stored, folder) in found {
        let Some(root) = roots.iter().find(|root| folder.starts_with(root)) else {
            continue;
        };
        let key = libraries::key(root);
        let view = Path::new(VIEW).join("shortcuts").join(&key);
        let inside = folder.strip_prefix(root).unwrap_or(Path::new(""));
        let to = view.join(inside);
        let Some(moved) = (to.to_str()).and_then(|to| shortcuts::moved(&entry, &stored, to)) else {
            skip(&entry, "its program isn't in its folder");
            continue;
        };
        if names_home(&moved, [owner.home.as_path(), home.as_path()]) {
            skip(&entry, "its launch options name the owner's home");
            continue;
        }
        if !picked.overlays.iter().any(|o| o.key == key) {
            picked.overlays.push(Overlay {
                root: root.clone(),
                key,
                view,
                owner: owner.uid,
                idmap: idmap.clone(),
            });
        }
        picked.art.extend(art);
        picked.entries.push(moved);
    }
    let ids = picked.entries.iter().filter_map(shortcuts::appid).collect();
    let config = (safe_open::dir_at(&steam, "config").ok().flatten())
        .and_then(|dir| read_owned(&dir, "config.vdf", owner.uid));
    picked.compat =
        shortcuts::compat_rows(&String::from_utf8_lossy(&config.unwrap_or_default()), &ids);
    tools(state, owner, root, &steam, libraries, &idmap, &mut picked);
    picked
}

/// The owner's custom Proton tools that a row of `picked` maps, each as an overlay at
/// `/run/media/punktfunk/tools/<key>`. A Valve Proton is a Steam app in a shared library, and a
/// system tool every account has.
fn tools(
    state: &Path,
    owner: &Passwd,
    root: &Path,
    steam: &OwnedFd,
    libraries: &[Library],
    idmap: &str,
    picked: &mut Picked,
) {
    let wanted: BTreeSet<&str> = picked.compat.iter().map(|row| row.tool.as_str()).collect();
    let Some(dir) = (!wanted.is_empty())
        .then(|| safe_open::dir_at(steam, TOOLS).ok().flatten())
        .flatten()
    else {
        return;
    };
    let mut found = Vec::new();
    for name in shared::names(&dir).unwrap_or_default() {
        let declared = (safe_open::dir_at(&dir, &name).ok().flatten())
            .and_then(|tool| read_owned(&tool, "compatibilitytool.vdf", owner.uid))
            .map(|vdf| shortcuts::tool_names(&String::from_utf8_lossy(&vdf)))
            .unwrap_or_default();
        if !declared.iter().any(|tool| wanted.contains(tool.as_str())) {
            continue;
        }
        let path = root.join(TOOLS).join(&name);
        match libraries::shareable(&path, state, owner, libraries) {
            Ok(tool) if owns(&tool, owner.uid) => found.push((name, tool)),
            Ok(_) => tracing::info!(tool = %name, "owner's proton tool isn't theirs; not shared"),
            Err(reason) => tracing::info!(tool = %name, reason, "owner's proton tool not shared"),
        }
    }
    for (name, tool) in found {
        let key = libraries::key(&tool);
        let view = Path::new(VIEW).join("tools").join(&key);
        picked.tools.push((name, view.clone()));
        picked.overlays.push(Overlay {
            root: tool,
            key,
            view,
            owner: owner.uid,
            idmap: idmap.to_owned(),
        });
    }
}

/// Whether `entry`'s launch options name a path in the owner's home, by any of `homes`.
fn names_home<'a>(entry: &Fields, homes: impl IntoIterator<Item = &'a Path>) -> bool {
    let options = shortcuts::text(entry, "LaunchOptions").unwrap_or_default();
    homes.into_iter().any(|home| {
        let home = home.to_string_lossy();
        !home.is_empty() && home != "/" && options.contains(&format!("{home}/"))
    })
}

/// `folder` sits directly in the owner's `home`: Downloads, Desktop, a Games folder of every game.
/// A seat reads all of a shared folder as its own, so only a game's own folder deeper down goes.
fn in_home_itself(folder: &Path, home: &Path) -> bool {
    folder.parent() == Some(home)
}

/// `folders`, each once, none inside another.
fn outermost(mut folders: Vec<PathBuf>) -> Vec<PathBuf> {
    folders.sort();
    let mut kept: Vec<PathBuf> = Vec::new();
    for folder in folders {
        if !kept.iter().any(|outer| folder.starts_with(outer)) {
            kept.push(folder);
        }
    }
    kept
}

/// Overlay `overlay`'s stage, its lower layer's, and the seat's upper and work folders. The
/// upper is the seat's; every folder above it is root's, so the seat can't move it.
fn prepare(state: &Path, id: &SeatId, seat: &Passwd, overlay: &Overlay) -> std::io::Result<()> {
    libraries::make_stage(id, &overlay.key)?;
    let stage = libraries::stage_dir(id, &overlay.key);
    std::fs::create_dir_all(format!("{}.lower", stage.display()))?;
    let dir = libraries::overlay_dir(state, id, &overlay.key);
    if let Some(overlays) = dir.parent() {
        shared::make_dir(overlays, 0o711)?;
    }
    shared::make_dir(&dir, 0o711)?;
    let upper = dir.join("upper");
    if std::fs::symlink_metadata(&upper).is_err() {
        std::fs::create_dir(&upper)?;
        std::os::unix::fs::chown(&upper, Some(seat.uid), Some(seat.gid))?;
    }
    shared::make_dir(&dir.join("work"), 0o700)
}

/// `picked` in each of the seat's Steam installs. The appids the box has in any account.
fn place(seat: &Passwd, picked: &Picked, before: &BTreeSet<u32>) -> BTreeSet<u32> {
    let mut ours = BTreeSet::new();
    for root in shared::SEAT_STEAMS {
        let names: Vec<&str> = root.split('/').collect();
        let steam = safe_open::dir(&seat.home).and_then(|home| safe_open::walk(home, &names));
        let placed = match steam {
            Ok(Some(steam)) => place_in(&steam, seat, picked, before, &mut ours),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = placed {
            tracing::warn!(account = %seat.name, root, %error, "seat's steam shortcuts not written");
        }
    }
    ours
}

/// `picked` in the seat's Steam at `steam`: each signed-in account's shortcuts and art, the
/// Proton rows, and the tools' links. Adds the appids the box has there to `ours`.
fn place_in(
    steam: &OwnedFd,
    seat: &Passwd,
    picked: &Picked,
    before: &BTreeSet<u32>,
    ours: &mut BTreeSet<u32>,
) -> std::io::Result<()> {
    let reader = Reader::person(seat.uid, seat.gid);
    let mut here = BTreeSet::new();
    if let Some(userdata) = safe_open::dir_at(steam, "userdata")? {
        for account in shared::names(&userdata)? {
            let config = safe_open::dir_at(&userdata, &account)?
                .map(|account| safe_open::walk(account, &["config"]))
                .transpose()?
                .flatten();
            let Some(config) = config else { continue };
            let had = read(&config, "shortcuts.vdf", MAX_STEAM_FILE_BYTES);
            let Some(merged) = shortcuts::merge(had.as_deref(), &picked.entries, before) else {
                tracing::warn!(account = %seat.name, steam_account = %account,
                    "seat's shortcuts.vdf unreadable; left as it is");
                continue;
            };
            shared::put_file(&config, "shortcuts.vdf", &merged.file, reader)?;
            let art = picked
                .art
                .iter()
                .filter(|(id, ..)| merged.ours.contains(id));
            let mut art = art.peekable();
            if art.peek().is_some() {
                let grid = shared::seat_folder(&config, "grid", seat)?;
                for (_, name, bytes) in art {
                    shared::put_file(&grid, name, bytes, reader)?;
                }
            }
            here.extend(merged.ours);
        }
    }
    let rows: Vec<CompatRow> = (picked.compat.iter())
        .filter(|row| here.contains(&row.appid))
        .cloned()
        .collect();
    if let Some(config) = safe_open::dir_at(steam, "config")?
        && let Some(text) = read(&config, "config.vdf", MAX_STEAM_FILE_BYTES)
    {
        let text = String::from_utf8_lossy(&text);
        if let Some(merged) = shortcuts::with_compat_rows(&text, &rows)
            && merged != text
        {
            shared::put_file(&config, "config.vdf", merged.as_bytes(), reader)?;
        }
    }
    link_tools(steam, seat, &picked.tools)?;
    ours.extend(here);
    Ok(())
}

/// Links each of `tools` into the seat's `compatibilitytools.d` where the seat has no folder of
/// that name, and removes the box's links to a tool no shared shortcut maps any more.
fn link_tools(steam: &OwnedFd, seat: &Passwd, tools: &[(String, PathBuf)]) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let ours = Path::new(VIEW).join("tools");
    let dir = match safe_open::dir_at(steam, TOOLS)? {
        Some(dir) => dir,
        None if tools.is_empty() => return Ok(()),
        None => shared::seat_folder(steam, TOOLS, seat)?,
    };
    for name in shared::names(&dir)? {
        let Ok(target) = rustix::fs::readlinkat(&dir, name.as_str(), Vec::new()) else {
            continue;
        };
        let target = Path::new(std::ffi::OsStr::from_bytes(target.as_bytes()));
        let wanted = tools.iter().any(|(n, view)| *n == name && view == target);
        if target.starts_with(&ours) && !wanted {
            rustix::fs::unlinkat(&dir, name.as_str(), AtFlags::empty())?;
        }
    }
    let (uid, gid) = (
        rustix::fs::Uid::from_raw(seat.uid),
        rustix::fs::Gid::from_raw(seat.gid),
    );
    for (name, view) in tools {
        match rustix::fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => continue,
            Err(Errno::NOENT) => {}
            Err(e) => return Err(e.into()),
        }
        rustix::fs::symlinkat(view, &dir, name.as_str())?;
        let flags = AtFlags::SYMLINK_NOFOLLOW;
        rustix::fs::chownat(&dir, name.as_str(), Some(uid), Some(gid), flags)?;
    }
    Ok(())
}

/// The seat's appids into the box's library copy, beside every other seat's row, and into its
/// own copy alone.
fn publish(
    state: &Path,
    id: &SeatId,
    seat: &Passwd,
    trust: Option<&OwnedFd>,
    ours: &BTreeSet<u32>,
    door: Option<Reader>,
) {
    let name = shortcuts::SEAT_FILE;
    if let Some(door) = door
        && let Ok(dir) = safe_open::dir(&shared::box_library_dir(state))
    {
        let _turn = SEAT_FILE_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let had = read(&dir, name, MAX_STEAM_FILE_BYTES);
        let file = shortcuts::with_seat(had.as_deref(), id.as_str(), ours);
        if let Err(error) = shared::put_file(&dir, name, &file, door) {
            tracing::warn!(seat = %id, %error, "box shortcut list not written");
        }
    }
    if let Some(dir) = trust {
        let file = shortcuts::with_seat(None, id.as_str(), ours);
        if let Err(error) = shared::put_file(dir, name, &file, Reader::group(seat.gid)) {
            tracing::warn!(seat = %id, %error, "seat's shortcut list not written");
        }
    }
}

/// `path`, a folder of `uid`'s. Root reads below it one folder at a time, so a link the owner
/// placed inside is never followed.
fn owned_dir(path: &Path, uid: u32) -> std::io::Result<OwnedFd> {
    let dir = safe_open::dir(path)?;
    if rustix::fs::fstat(&dir)?.st_uid != uid {
        return Err(std::io::Error::other("it isn't the owner's"));
    }
    Ok(dir)
}

/// Each account's `config` folder in the Steam at `steam`.
fn account_configs(steam: &OwnedFd) -> Vec<OwnedFd> {
    let Ok(Some(userdata)) = safe_open::dir_at(steam, "userdata") else {
        return Vec::new();
    };
    (shared::names(&userdata).unwrap_or_default().iter())
        .filter_map(|account| safe_open::dir_at(&userdata, account).ok().flatten())
        .filter_map(|account| safe_open::walk(account, &["config"]).ok().flatten())
        .collect()
}

fn read(dir: &OwnedFd, name: &str, max: u64) -> Option<Vec<u8>> {
    Some(safe_open::read_at(dir, name, max).ok()??.0)
}

/// `name` in `dir` when it is `uid`'s.
fn read_owned(dir: &OwnedFd, name: &str, uid: u32) -> Option<Vec<u8>> {
    let (bytes, meta) = safe_open::read_at(dir, name, MAX_STEAM_FILE_BYTES).ok()??;
    (meta.uid() == uid).then_some(bytes)
}

fn owns(path: &Path, uid: u32) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.uid() == uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_directly_in_the_home_stays_home() {
        let home = Path::new("/home/u");
        assert!(in_home_itself(Path::new("/home/u/Downloads"), home));
        assert!(in_home_itself(Path::new("/home/u/Games"), home));
        assert!(!in_home_itself(Path::new("/home/u/Games/Celeste"), home));
        assert!(!in_home_itself(Path::new("/home/u"), home));
        assert!(!in_home_itself(Path::new("/mnt/games/Celeste"), home));
    }

    #[test]
    fn a_folder_inside_another_shares_its_overlay() {
        let kept = outermost(vec![
            PathBuf::from("/g/A/bin"),
            PathBuf::from("/g/B"),
            PathBuf::from("/g/A"),
            PathBuf::from("/g/AB"),
        ]);
        assert_eq!(
            kept,
            [Path::new("/g/A"), Path::new("/g/AB"), Path::new("/g/B")]
        );
    }

    /// Options that still reach into the owner's home after the move keep the shortcut there.
    #[test]
    fn options_naming_the_owner_s_home_keep_a_shortcut_home() {
        let s = |v: &str| shortcuts::Value::Str(v.as_bytes().to_vec());
        let with = |options: &str| -> Fields { vec![(b"LaunchOptions".to_vec(), s(options))] };
        let homes = [Path::new("/home/o"), Path::new("/var/home/o")];
        assert!(names_home(
            &with("-L '/var/home/o/Emulation/roms/x.sfc'"),
            homes
        ));
        assert!(names_home(&with("--cfg /home/o/.config/x"), homes));
        assert!(!names_home(
            &with("--data /run/media/punktfunk/shortcuts/00aa/x"),
            homes
        ));
        assert!(!names_home(&with("/home/other/x /home/o2/x"), homes));
        assert!(!names_home(&Vec::new(), homes));
    }
}
