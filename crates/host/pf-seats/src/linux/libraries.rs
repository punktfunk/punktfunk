//! The box owner's Steam libraries, shown to a seat in place.
//!
//! Every library the owner's Steam lists appears in the seat at `seats/<id>/libraries/<key>`.
//! Its `steamapps` arrives through an idmapped mount: on disk each file stays the owner's, in the
//! seat it is the seat user's, so the seat's Steam plays, updates and runs Proton from it with no
//! ACL. The seat's own `compatdata`, `shadercache`, `downloading` and `temp` under
//! `seats/<id>/private/<key>` cover the owner's, and the library's own path is inaccessible to the
//! unit, so the owner's prefixes never reach a seat.
//!
//! The supervisor holds no `CAP_SYS_ADMIN`, so the seat unit's `ExecStartPre=+` mounts each
//! library at a root-only stage under [`STAGE_ROOT`], through `nsenter` into PID 1's namespace:
//! systemd gives even a `+` command a namespace of its own once the unit has one. `BindPaths=`
//! then carries the stage, idmap and all, into the seat. The libraries are read at each start; a
//! drive the owner mounts later reaches the seat on its next start.
//!
//! The box's `seat_library_writable` off (`library_writable` in [`DEFAULTS`]) mounts them
//! read-only: a seat plays what is installed, and can't update, install or rewrite an
//! appmanifest there. The few files a game needs to write get private copies ([`spots`]).
//!
//! Every root the owner granted a plugin, a ROM folder say, appears read-only at its own path
//! ([`Rom`]), through the same stage and idmap, or a plain bind where the filesystem takes none.

use super::accounts::Passwd;
use super::shared::{self, make_dir};
use super::{err, run, safe_open};
use crate::backend::BackendError;
use crate::model::SeatId;
use crate::steam;
use std::collections::HashSet;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// What a seat keeps its own of in each library. Steam stages updates in `temp`.
pub(super) const PRIVATE: [&str; 4] = ["compatdata", "shadercache", "downloading", "temp"];

/// Root-only: each seat's idmapped mounts wait here for its unit to bind them.
const STAGE_ROOT: &str = "/run/punktfunk/libraries";

/// Filesystems that take an idmapped mount. A library elsewhere (exFAT, NTFS, FUSE) is skipped.
const IDMAP_FS: [&str; 4] = ["btrfs", "ext4", "xfs", "f2fs"];

/// `pf_host_config::seat_defaults::FILE`, which the box writes into its config dir.
const DEFAULTS: &str = "seat-defaults.json";

const LIST: &str = "libraryfolders.vdf";
const LIST_TEMP: &str = "libraryfolders.vdf.punktfunk";
const MAX_LIST_BYTES: u64 = 8 * 1024 * 1024;

/// One of the owner's libraries a seat sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Library {
    /// The owner's `steamapps`, canonical.
    pub steamapps: PathBuf,
    /// Its folder under the seat's: a hash of the path, so the seat's prefixes stay with it.
    pub key: String,
    /// What the seat writes in it while it is read-only; empty while it is writable.
    pub spots: Vec<Spot>,
}

/// A path under a read-only `steamapps` the seat gets a private copy of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Spot {
    /// Relative to `steamapps`.
    pub path: PathBuf,
    pub dir: bool,
}

/// A root the owner granted a plugin, which a seat sees read-only at its own path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Rom {
    /// Canonical.
    pub root: PathBuf,
    /// Its stage's name: a hash of the path, as a library's.
    pub key: String,
    /// `X-mount.idmap=` when its filesystem takes one. Without, a plain bind shows what the
    /// files' own modes let anyone read.
    pub idmap: Option<String>,
}

/// What a seat's unit needs to show the owner's libraries and granted roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Shares {
    pub nsenter: PathBuf,
    pub mount: PathBuf,
    pub umount: PathBuf,
    /// `X-mount.idmap=`: the owner's ids on disk, the seat's in the mount.
    pub idmap: String,
    /// The box's `seat_library_writable` is off.
    pub read_only: bool,
    pub libraries: Vec<Library>,
    pub roms: Vec<Rom>,
    /// The owner's home, canonical, when a root lies in it: an empty folder covers it in the
    /// seat, so the seat can walk to the root through a home that is closed to it.
    pub home: Option<PathBuf>,
}

fn libraries_dir(state: &Path, id: &SeatId) -> PathBuf {
    shared::seat_dir(state, id).join("libraries")
}

/// The library as the seat's Steam lists it.
fn root_dir(state: &Path, id: &SeatId, key: &str) -> PathBuf {
    libraries_dir(state, id).join(key)
}

fn private_dir(state: &Path, id: &SeatId, key: &str) -> PathBuf {
    shared::seat_dir(state, id).join("private").join(key)
}

fn stage_dir(id: &SeatId, key: &str) -> PathBuf {
    Path::new(STAGE_ROOT).join(id.as_str()).join(key)
}

fn key(steamapps: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let bytes = steamapps.as_os_str().as_bytes();
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
    hex::encode(&digest.as_ref()[..8])
}

/// libmount writes each triple into the mount's user namespace as given, and the kernel reads the
/// first id as the one on disk. mount(8) names the two columns the other way around.
fn idmap(owner: (u32, u32), seat: &Passwd) -> String {
    format!("u:{}:{}:1 g:{}:{}:1", owner.0, seat.uid, owner.1, seat.gid)
}

/// A path a unit file carries inside double quotes as written: no escape, specifier or variable,
/// and no `:`, which `BindPaths=` reads as the source/target split.
fn unit_safe(path: &str) -> bool {
    !path
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '%' | '$' | ':'))
}

/// The library at `path` as a seat would see it, with its `steamapps`' device and inode, or why
/// a seat can't.
fn check(
    path: &str,
    owner_uid: u32,
    fstype: &impl Fn(&Path) -> Option<String>,
) -> Result<(Library, (u64, u64)), &'static str> {
    let canonical = std::fs::canonicalize(path).map_err(|_| "not found")?;
    let steamapps = canonical.join("steamapps");
    if !steamapps.to_str().is_some_and(unit_safe) {
        return Err("its path has characters a unit file can't carry");
    }
    let meta = std::fs::symlink_metadata(&steamapps).map_err(|_| "no steamapps folder")?;
    if !meta.is_dir() {
        return Err("no steamapps folder");
    }
    if meta.uid() != owner_uid {
        return Err("its steamapps isn't the owner's");
    }
    if !fstype(&steamapps).is_some_and(|t| IDMAP_FS.contains(&t.as_str())) {
        return Err("its filesystem can't take an idmapped mount");
    }
    for name in PRIVATE {
        let private = std::fs::symlink_metadata(steamapps.join(name));
        if private.is_ok_and(|m| !m.is_dir()) {
            return Err("a folder a seat keeps its own of is a link or a file");
        }
    }
    let inode = (meta.dev(), meta.ino());
    Ok((
        Library {
            key: key(&steamapps),
            steamapps,
            spots: Vec::new(),
        },
        inode,
    ))
}

/// The libraries at `paths` a seat can see, each once.
fn find(
    paths: &[String],
    owner_uid: u32,
    fstype: impl Fn(&Path) -> Option<String>,
) -> Vec<Library> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        match check(path, owner_uid, &fstype) {
            Ok((library, inode)) => {
                if seen.insert(inode) {
                    out.push(library);
                }
            }
            Err(reason) => {
                tracing::info!(library = %path, reason, "owner's library not shown to seats")
            }
        }
    }
    out
}

/// The folders in `dir` a unit file can name, sorted. A link is not a folder.
fn folders(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| unit_safe(name))
        .collect();
    out.sort();
    out
}

/// What a game still writes in a read-only `steamapps`, as found there: each Steam Linux
/// Runtime's `var` and each of its platforms' `files/.ref`, which pressure-vessel opens before a
/// native game starts, and each Proton's `dist.lock`. Read-only, either fails the launch with
/// EROFS. Links are never followed; a file that isn't there yet stays read-only.
fn spots(steamapps: &Path) -> Vec<Spot> {
    let found = |spot: &Spot| {
        let meta = std::fs::symlink_metadata(steamapps.join(&spot.path));
        meta.is_ok_and(|m| if spot.dir { m.is_dir() } else { m.is_file() })
    };
    let spot = |path: PathBuf, dir| Spot { path, dir };
    let mut out = Vec::new();
    for name in folders(&steamapps.join("common")) {
        let tool = Path::new("common").join(&name);
        if name.starts_with("SteamLinuxRuntime_") {
            out.push(spot(tool.join("var"), true));
            for platform in folders(&steamapps.join(&tool)) {
                let files = spot(tool.join(platform).join("files"), true);
                if found(&files) {
                    out.push(spot(files.path.join(".ref"), false));
                }
            }
        } else if name.starts_with("Proton") {
            out.push(spot(tool.join("dist.lock"), false));
        }
    }
    out.retain(found);
    out
}

/// The box's `seat_library_writable`, read from its [`DEFAULTS`] in `box_dir`. On when the box
/// wrote none, or none that names the setting. A file that can't be read, or a value that isn't
/// a bool, reads as off: this setting only ever narrows what a seat may do.
fn writable(box_dir: &Path) -> bool {
    let read = || -> std::io::Result<bool> {
        let path = box_dir.join(DEFAULTS);
        let Some((bytes, _)) = safe_open::read(&path, shared::MAX_TRUST_BYTES)? else {
            return Ok(true);
        };
        let defaults: serde_json::Value = serde_json::from_slice(&bytes)?;
        let object = defaults.as_object().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "not a JSON object")
        })?;
        Ok(object
            .get("library_writable")
            .is_none_or(|value| value.as_bool().unwrap_or(false)))
    };
    read().unwrap_or_else(|error| {
        tracing::warn!(%error, "seat defaults unreadable; the owner's libraries are read-only");
        false
    })
}

fn fstype(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    let found = run(
        "findmnt",
        "findmnt",
        &["-n", "-o", "FSTYPE", "--target", path],
    );
    Some(found.ok()?.trim().to_owned())
}

/// What seat `seat` is shown of the owner's: the libraries of the Steam at `steam_root`, read-only
/// when the box's defaults in `box_dir` say so, and every root `owner` granted a plugin. Their
/// folders are put in place under `state`. `None` without the mount tools.
pub(super) fn share(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    seat: &Passwd,
    steam_root: Option<&Path>,
    owner: Option<&Passwd>,
) -> Option<Shares> {
    let [Some(nsenter), Some(mount), Some(umount)] =
        ["nsenter", "mount", "umount"].map(super::which)
    else {
        tracing::warn!("nsenter, mount or umount not found; no library shown");
        return None;
    };
    let read_only = !writable(box_dir);
    let (idmap, libraries) = steam_root
        .and_then(|root| steam_libraries(state, id, seat, root, read_only))
        .unwrap_or_default();
    let (roms, home) = owner.map_or_else(Default::default, |owner| {
        granted_roms(state, id, seat, owner, &libraries)
    });
    Some(Shares {
        nsenter,
        mount,
        umount,
        idmap,
        read_only,
        libraries,
        roms,
        home,
    })
}

/// The libraries of the owner's Steam at `steam_root`, with the idmap they are mounted with.
/// `None` when there is nothing to show: a Steam that is root's or the seat's, or no list.
/// Root's is never mapped: a seat would own every root file in the library.
fn steam_libraries(
    state: &Path,
    id: &SeatId,
    seat: &Passwd,
    steam_root: &Path,
    read_only: bool,
) -> Option<(String, Vec<Library>)> {
    let meta = std::fs::metadata(steam_root).ok()?;
    let owner = (meta.uid(), meta.gid());
    if owner.0 == 0 || owner.1 == 0 || owner.0 == seat.uid {
        let steam = steam_root.display();
        tracing::info!(%steam, "steam is root's or the seat's; no library shown");
        return None;
    }
    let list = steam_root.join("steamapps").join(LIST);
    let vdf = match safe_open::read(&list, MAX_LIST_BYTES) {
        Ok(Some((bytes, _))) => String::from_utf8(bytes).ok()?,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "owner's steam library list unreadable");
            return None;
        }
    };
    let found = find(&steam::library_paths(&vdf), owner.0, fstype);
    let orphans = orphans(&shared::seat_dir(state, id).join("private"), &found);
    if !orphans.is_empty() {
        tracing::info!(seat = %id, ?orphans, "private folders of libraries no longer listed");
    }
    let libraries = found
        .into_iter()
        .map(|mut library| {
            if read_only {
                library.spots = spots(&library.steamapps);
            }
            library
        })
        .filter(|library| match prepare(state, id, seat, owner, library) {
            Ok(()) => true,
            Err(error) => {
                let library = library.steamapps.display();
                tracing::warn!(%library, %error, "owner's library not shown to the seat");
                false
            }
        })
        .collect();
    Some((idmap(owner, seat), libraries))
}

/// The roots `owner` granted a plugin, as the box's library copy lists them, that seat `seat`
/// is shown, each with its stage in place; and the owner's home when one lies in it.
fn granted_roms(
    state: &Path,
    id: &SeatId,
    seat: &Passwd,
    owner: &Passwd,
    libraries: &[Library],
) -> (Vec<Rom>, Option<PathBuf>) {
    let text = match safe_open::read(&shared::box_grants(state), shared::MAX_TRUST_BYTES) {
        Ok(Some((bytes, _))) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(None) => return (Vec::new(), None),
        Err(error) => {
            tracing::warn!(%error, "owner's plugin grants unreadable; no root shown");
            return (Vec::new(), None);
        }
    };
    let home = std::fs::canonicalize(&owner.home).unwrap_or_else(|_| owner.home.clone());
    let state = std::fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf());
    let fence = Fence {
        home: &home,
        state: &state,
        libraries,
    };
    let reads = |root: &Path| owner_reads(owner, root);
    let idmap = idmap((owner.uid, owner.gid), seat);
    let roms: Vec<Rom> = roms(&granted(&text, &owner.home), &fence, reads, fstype, &idmap)
        .into_iter()
        .filter(|rom| {
            let staged = rom.idmap.is_none() || make_stage(id, &rom.key).is_ok();
            if !staged {
                tracing::warn!(root = %rom.root.display(), "granted root's stage not made");
            }
            staged
        })
        .collect();
    let home = Some(home).filter(|home| roms.iter().any(|rom| rom.root.starts_with(home)));
    (roms, home)
}

/// Every plugin's granted paths in `grants`, the text of `plugin-grants.json`, write grants too:
/// `{id: {grants: [{path}]}}`, or the older `{id: [path]}`. `~/` is the owner's `home`; a path
/// that isn't absolute is left out.
fn granted(grants: &str, home: &Path) -> Vec<PathBuf> {
    use serde_json::Value;
    let Ok(Value::Object(plugins)) = serde_json::from_str::<Value>(grants) else {
        tracing::warn!("owner's plugin grants aren't a JSON object; no root shown");
        return Vec::new();
    };
    let spelled = plugins.values().flat_map(|entry| match entry {
        Value::Array(paths) => paths.iter().filter_map(Value::as_str).collect(),
        Value::Object(access) => access
            .get("grants")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|grant| grant.get("path")?.as_str())
            .collect(),
        _ => Vec::new(),
    });
    spelled
        .map(|path| match path.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(path),
        })
        .filter(|path| path.is_absolute())
        .collect()
}

/// What a granted root must keep clear of: the owner's home and the supervisor's state, both
/// canonical, and the libraries a seat sees already.
struct Fence<'a> {
    home: &'a Path,
    state: &'a Path,
    libraries: &'a [Library],
}

/// The root at `path` as a seat would see it, or why a seat can't. The owner's grants are
/// theirs to edit, so the supervisor shows only a folder the owner can read (`reads`) as
/// themselves, never one that holds their home or the seats' own folders.
fn rom_root(
    path: &Path,
    fence: &Fence<'_>,
    reads: &impl Fn(&Path) -> bool,
) -> Result<PathBuf, &'static str> {
    let root = std::fs::canonicalize(path).map_err(|_| "not found")?;
    if !root.is_dir() {
        return Err("not a folder");
    }
    if !root.to_str().is_some_and(unit_safe) {
        return Err("its path has characters a unit file can't carry");
    }
    if fence.home.starts_with(&root)
        || fence.state.starts_with(&root)
        || root.starts_with(fence.state)
    {
        return Err("it holds the owner's home or the seats' own folders");
    }
    if fence
        .libraries
        .iter()
        .any(|l| root.starts_with(&l.steamapps))
    {
        return Err("it is in a Steam library the seat sees already");
    }
    if !reads(&root) {
        return Err("the owner can't read it");
    }
    Ok(root)
}

/// The roots at `paths` a seat can see, each once and none inside another, idmapped by `idmap`
/// where `fstype` takes one.
fn roms(
    paths: &[PathBuf],
    fence: &Fence<'_>,
    reads: impl Fn(&Path) -> bool,
    fstype: impl Fn(&Path) -> Option<String>,
    idmap: &str,
) -> Vec<Rom> {
    let mut roots: Vec<PathBuf> = paths
        .iter()
        .filter_map(|path| match rom_root(path, fence, &reads) {
            Ok(root) => Some(root),
            Err(reason) => {
                tracing::info!(root = %path.display(), reason, "granted root not shown to seats");
                None
            }
        })
        .collect();
    // Sorted, a folder comes before what is inside it.
    roots.sort();
    let mut kept: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !kept.iter().any(|outer| root.starts_with(outer)) {
            kept.push(root);
        }
    }
    kept.into_iter()
        .map(|root| Rom {
            key: key(&root),
            idmap: fstype(&root)
                .filter(|t| IDMAP_FS.contains(&t.as_str()))
                .map(|_| idmap.to_owned()),
            root,
        })
        .collect()
}

/// Whether `owner` may list and enter `root` as themselves, with their own groups.
fn owner_reads(owner: &Passwd, root: &Path) -> bool {
    let (uid, gid) = (owner.uid.to_string(), owner.gid.to_string());
    let Some(root) = root.to_str() else {
        return false;
    };
    let args = [
        "--reuid",
        &uid,
        "--regid",
        &gid,
        "--init-groups",
        "test",
        "-r",
        root,
        "-a",
        "-x",
        root,
    ];
    run("rom_root", "setpriv", &args).is_ok()
}

/// The keys under `private` that none of `libraries` carries: prefixes and caches of a library
/// the owner's Steam no longer lists, or whose drive is away. They are kept, never deleted.
fn orphans(private: &Path, libraries: &[Library]) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(private)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|key| libraries.iter().all(|library| &library.key != key))
        .collect();
    out.sort();
    out
}

/// Removes `id`'s stage folders, the mount points its unit mounted the libraries at. `rmdir`
/// only: a mount still up fails with `EBUSY` and stays, so no library is emptied through one.
pub(super) fn remove_stage(id: &SeatId) {
    remove_empty_dirs(&Path::new(STAGE_ROOT).join(id.as_str()));
}

/// `root` and the folders directly in it, each only if empty.
fn remove_empty_dirs(root: &Path) {
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let _ = std::fs::remove_dir(entry.path());
    }
    let _ = std::fs::remove_dir(root);
}

/// Every directory a library's binds name, and the seat's copy of each of its [`Spot`]s, before
/// the unit starts: a missing one fails it with `226/NAMESPACE`, and none can be made inside the
/// idmapped mount, where root has no id.
fn prepare(
    state: &Path,
    id: &SeatId,
    seat: &Passwd,
    owner: (u32, u32),
    library: &Library,
) -> Result<(), BackendError> {
    let io = |what: &str, e: std::io::Error| err("library_dirs", format!("{what}: {e}"));
    let missing: Vec<String> = PRIVATE
        .iter()
        .map(|name| library.steamapps.join(name))
        .filter(|path| !path.exists())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    if !missing.is_empty() {
        // As the owner: root walking the owner's tree would follow what the owner links.
        let (uid, gid) = (owner.0.to_string(), owner.1.to_string());
        let mut args = vec![
            "--reuid",
            &uid,
            "--regid",
            &gid,
            "--clear-groups",
            "mkdir",
            "--",
        ];
        args.extend(missing.iter().map(String::as_str));
        run("library_dirs", "setpriv", &args)?;
    }
    make_dir(&libraries_dir(state, id), 0o711).map_err(|e| io("create the libraries", e))?;
    let root = root_dir(state, id, &library.key);
    let mount_point = root.join("steamapps");
    std::fs::create_dir_all(&root).map_err(|e| io("create the library", e))?;
    // The mount point is made while the folder is root's alone; the sticky bit then keeps the
    // seat, which writes beside it, from swapping it for a link.
    match std::fs::symlink_metadata(&mount_point) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(err(
                "library_dirs",
                "the library's mount point isn't a folder",
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| io("close the library", e))?;
            std::fs::create_dir(&mount_point).map_err(|e| io("create the mount point", e))?;
        }
        Err(e) => return Err(io("read the mount point", e)),
    }
    std::os::unix::fs::chown(&root, Some(0), Some(seat.gid))
        .map_err(|e| io("own the library", e))?;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o1770))
        .map_err(|e| io("open the library to the seat", e))?;
    let private = private_dir(state, id, &library.key);
    make_dir(&shared::seat_dir(state, id).join("private"), 0o711)
        .and_then(|()| make_dir(&private, 0o711))
        .map_err(|e| io("create the private folders", e))?;
    for name in PRIVATE {
        let dir = private.join(name);
        make_dir(&dir, 0o700).map_err(|e| io("create a private folder", e))?;
        std::os::unix::fs::chown(&dir, Some(seat.uid), Some(seat.gid))
            .map_err(|e| io("own a private folder", e))?;
    }
    make_spots(&private, &library.spots, seat).map_err(|e| io("create a private copy", e))?;
    make_stage(id, &library.key).map_err(|e| io("create the stage", e))
}

/// `key`'s stage for seat `id`, under the root-only [`STAGE_ROOT`].
fn make_stage(id: &SeatId, key: &str) -> std::io::Result<()> {
    make_dir(Path::new(STAGE_ROOT), 0o700)?;
    std::fs::create_dir_all(stage_dir(id, key))
}

/// The seat's own copy of each spot under `private`, empty and the seat user's. Root makes the
/// folders on the way; a copy already there is kept.
fn make_spots(private: &Path, spots: &[Spot], seat: &Passwd) -> std::io::Result<()> {
    for spot in spots {
        let path = private.join(&spot.path);
        if spot.dir {
            make_dir(&path, 0o700)?;
            std::os::unix::fs::chown(&path, Some(seat.uid), Some(seat.gid))?;
        } else {
            std::fs::create_dir_all(path.parent().unwrap_or(private))?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .mode(0o644)
                .open(&path)?;
            std::os::unix::fs::fchown(&file, Some(seat.uid), Some(seat.gid))?;
        }
    }
    Ok(())
}

/// The drop-in lines that show `shares` to seat `id`: mount each library at its stage before the
/// unit starts, read-only if the box says so, and unmount it after; hide its own path, bind the
/// stage, the seat's private folders and its spots' copies over it, and keep the owner's library
/// list there read-only. A mount that fails leaves the seat an empty library, not a failed start.
/// Then each granted root, read-only at its own path ([`rom_lines`]).
pub(super) fn unit_lines(state: &Path, id: &SeatId, shares: &Shares) -> String {
    let host = format!("{} -t 1 -m --", shares.nsenter.display());
    let ro = if shares.read_only { "ro," } else { "" };
    let mut out = String::new();
    for library in &shares.libraries {
        let stage = stage_dir(id, &library.key);
        let target = root_dir(state, id, &library.key).join("steamapps");
        let private = private_dir(state, id, &library.key);
        let (stage, owner) = (stage.display(), library.steamapps.display());
        out.push_str(&format!(
            "ExecStartPre=-+{host} {} --bind -o \"{ro}nosuid,nodev,X-mount.idmap={}\" \"{owner}\" {stage}\n\
             ExecStopPost=-+{host} {} -l {stage}\n\
             InaccessiblePaths=\"-{owner}\"\n\
             BindPaths={stage}:{}\n",
            shares.mount.display(),
            shares.idmap,
            shares.umount.display(),
            target.display(),
        ));
        for name in PRIVATE {
            out.push_str(&format!(
                "BindPaths={}:{}\n",
                private.join(name).display(),
                target.join(name).display()
            ));
        }
        // Each side quoted on its own: a spot's path may hold a space.
        for spot in &library.spots {
            out.push_str(&format!(
                "BindPaths=\"{}\":\"{}\"\n",
                private.join(&spot.path).display(),
                target.join(&spot.path).display()
            ));
        }
        let list = target.join(LIST);
        out.push_str(&format!(
            "BindReadOnlyPaths=-{stage}/{LIST}:{}\n",
            list.display()
        ));
    }
    out.push_str(&rom_lines(id, shares, &host));
    out
}

/// Each granted root read-only at its own path: through its idmapped stage, mounted as a
/// library's is, or as a plain bind. An empty folder covers the owner's home first when a root
/// lies in it; systemd mounts a parent before what lies inside it, as under `/run/media`.
fn rom_lines(id: &SeatId, shares: &Shares, host: &str) -> String {
    let mut out = String::new();
    if let Some(home) = &shares.home {
        out.push_str(&format!("TemporaryFileSystem=\"{}:ro\"\n", home.display()));
    }
    for rom in &shares.roms {
        let root = rom.root.display();
        let Some(idmap) = &rom.idmap else {
            out.push_str(&format!("BindReadOnlyPaths=-\"{root}\":\"{root}\"\n"));
            continue;
        };
        let stage = stage_dir(id, &rom.key);
        let stage = stage.display();
        out.push_str(&format!(
            "ExecStartPre=-+{host} {} --bind -o \"ro,nosuid,nodev,X-mount.idmap={idmap}\" \"{root}\" {stage}\n\
             ExecStopPost=-+{host} {} -l {stage}\n\
             BindReadOnlyPaths={stage}:\"{root}\"\n",
            shares.mount.display(),
            shares.umount.display(),
        ));
    }
    out
}

/// Lists the shared games folder and `libraries` in the seat's Steam and drops the ones it no
/// longer has, and any folder under `/run/media`, which a seat never sees. Steam's own entries
/// stay as written, and a folder missing from the list goes last: Steam reads entry `0` as its
/// own install and drops a folder listed there. Never fails the start: an unlisted library is
/// one the seat's Steam doesn't offer.
pub(super) fn register(state: &Path, id: &SeatId, seat: &Passwd, libraries: &[Library]) {
    let ours = libraries_dir(state, id);
    let games = shared::games_dir(state).to_string_lossy().into_owned();
    let wanted: Vec<String> = std::iter::once(games)
        .chain(
            libraries
                .iter()
                .map(|l| root_dir(state, id, &l.key).to_string_lossy().into_owned()),
        )
        .collect();
    let managed =
        |path: &str| Path::new(path).starts_with(&ours) || path.starts_with("/run/media/");
    // Steam writes both copies and reads the one under `config`.
    for dir in ["steamapps", "config"] {
        let merge = |vdf: &str| steam::merge_library_folders(vdf, managed, &wanted);
        if let Err(error) = merge_list(&seat.home, dir, seat, merge) {
            tracing::warn!(%error, dir, "seat's steam library list not updated");
        }
    }
}

/// Rewrites `.local/share/Steam/<dir>/libraryfolders.vdf` under `home` through `merge`. Root
/// writes in a tree the seat user owns, so it goes through [`safe_open`]: no link on the way is
/// followed, a FIFO there doesn't block, and the new file is the seat's. An absent file stays
/// absent: Steam writes its own on first run.
pub(super) fn merge_list(
    home: &Path,
    dir: &str,
    seat: &Passwd,
    merge: impl Fn(&str) -> Option<String>,
) -> std::io::Result<()> {
    let path = [".local", "share", "Steam", dir];
    let Some(at) = safe_open::walk(safe_open::dir(home)?, &path)? else {
        return Ok(());
    };
    match safe_open::read_at(&at, LIST, MAX_LIST_BYTES)? {
        Some((bytes, meta)) => rewrite(&at, &bytes, &meta, seat, merge),
        None => Ok(()),
    }
}

fn rewrite(
    dir: &OwnedFd,
    bytes: &[u8],
    meta: &std::fs::Metadata,
    seat: &Passwd,
    merge: impl Fn(&str) -> Option<String>,
) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags};
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("not text"))?;
    let merged = merge(text).ok_or_else(|| invalid("not a steam library list"))?;
    if merged == text {
        return Ok(());
    }
    let _ = rustix::fs::unlinkat(dir, LIST_TEMP, AtFlags::empty());
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let temp = rustix::fs::openat(dir, LIST_TEMP, create, Mode::RUSR | Mode::WUSR)?;
    let mut temp = std::fs::File::from(temp);
    temp.write_all(merged.as_bytes())?;
    temp.set_permissions(std::fs::Permissions::from_mode(meta.mode() & 0o777))?;
    std::os::unix::fs::fchown(&temp, Some(seat.uid), Some(seat.gid))?;
    drop(temp);
    rustix::fs::renameat(dir, LIST_TEMP, dir, LIST)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    /// A seat user that is whoever owns `home`, so the test can hand files to it.
    fn me(home: &Path) -> Passwd {
        let meta = std::fs::metadata(home).unwrap();
        Passwd {
            name: "pf-seat-1".into(),
            uid: meta.uid(),
            gid: meta.gid(),
            gecos: String::new(),
            home: home.to_path_buf(),
        }
    }

    fn btrfs(_: &Path) -> Option<String> {
        Some("btrfs".into())
    }

    #[test]
    fn the_idmap_puts_the_owner_s_ids_on_disk_first() {
        let seat = Passwd {
            uid: 987,
            gid: 956,
            ..me(Path::new("/"))
        };
        assert_eq!(idmap((1000, 1000), &seat), "u:1000:987:1 g:1000:956:1");
    }

    #[test]
    fn a_library_s_key_follows_its_path() {
        let a = key(Path::new("/run/media/bazzite/QEMU HARDDISK/steamapps"));
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            a,
            key(Path::new("/run/media/bazzite/QEMU HARDDISK/steamapps"))
        );
        assert_ne!(
            a,
            key(Path::new("/run/media/bazzite/QEMU HARDDISK1/steamapps"))
        );
    }

    /// Only a library a seat can use is shown, and each reason to skip one is named.
    #[test]
    fn a_library_is_shown_only_when_a_seat_can_use_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let uid = std::fs::metadata(&root).unwrap().uid();
        let library = |name: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(dir.join("steamapps/compatdata")).unwrap();
            dir.to_string_lossy().into_owned()
        };
        let reason = |path: &str, owner: u32, fs: &str| {
            let fs = fs.to_owned();
            check(path, owner, &|_: &Path| Some(fs.clone())).map(|(l, _)| l)
        };

        let good = library("QEMU HARDDISK");
        let shown = reason(&good, uid, "btrfs").unwrap();
        assert_eq!(shown.steamapps, root.join("QEMU HARDDISK/steamapps"));
        assert_eq!(reason(&good, uid, "ext4"), Ok(shown.clone()));

        let missing = root.join("QEMU HARDDISK1").to_string_lossy().into_owned();
        assert_eq!(reason(&missing, uid, "btrfs"), Err("not found"));
        assert_eq!(
            reason(&good, uid + 1, "btrfs"),
            Err("its steamapps isn't the owner's")
        );
        for fs in ["exfat", "ntfs3", "fuseblk", "vfat"] {
            assert_eq!(
                reason(&good, uid, fs),
                Err("its filesystem can't take an idmapped mount"),
                "{fs}"
            );
        }

        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        let empty = empty.to_string_lossy().into_owned();
        assert_eq!(reason(&empty, uid, "btrfs"), Err("no steamapps folder"));
        let linked = root.join("linked");
        std::fs::create_dir(&linked).unwrap();
        let target = root.join("QEMU HARDDISK/steamapps");
        std::os::unix::fs::symlink(target, linked.join("steamapps")).unwrap();
        let linked = linked.to_string_lossy().into_owned();
        assert_eq!(reason(&linked, uid, "btrfs"), Err("no steamapps folder"));

        let quoted = library("say \"hi\"");
        assert_eq!(
            reason(&quoted, uid, "btrfs"),
            Err("its path has characters a unit file can't carry")
        );
        let split = library("a:b");
        assert_eq!(
            reason(&split, uid, "btrfs"),
            Err("its path has characters a unit file can't carry"),
            "a `:` would split a BindPaths= line"
        );
        let prefixes_elsewhere = library("prefixes elsewhere");
        let compatdata = root.join("prefixes elsewhere/steamapps/compatdata");
        std::fs::remove_dir(&compatdata).unwrap();
        std::os::unix::fs::symlink(&root, &compatdata).unwrap();
        assert_eq!(
            reason(&prefixes_elsewhere, uid, "btrfs"),
            Err("a folder a seat keeps its own of is a link or a file")
        );
    }

    /// A seat's private folders for a library it no longer sees are named, not touched; the
    /// stage goes empty folder by empty folder, and a folder holding anything stays.
    #[test]
    fn a_library_the_seat_lost_is_named_and_its_stage_goes() {
        let temp = tempfile::tempdir().unwrap();
        let private = temp.path().join("private");
        for key in ["00aa", "11bb", "22cc"] {
            std::fs::create_dir_all(private.join(key).join("compatdata")).unwrap();
        }
        let library = |key: &str| Library {
            steamapps: PathBuf::from("/x/steamapps"),
            key: key.into(),
            spots: Vec::new(),
        };
        assert_eq!(orphans(&private, &[library("11bb")]), ["00aa", "22cc"]);
        assert!(orphans(&temp.path().join("gone"), &[]).is_empty());
        assert!(private.join("00aa/compatdata").exists());

        let stage = temp.path().join("stage");
        std::fs::create_dir_all(stage.join("00aa")).unwrap();
        std::fs::create_dir_all(stage.join("11bb")).unwrap();
        std::fs::write(stage.join("11bb/mounted"), "x").unwrap();
        remove_empty_dirs(&stage);
        assert!(!stage.join("00aa").exists());
        assert!(stage.join("11bb/mounted").exists(), "not recursive");
        remove_empty_dirs(&temp.path().join("gone"));
    }

    #[test]
    fn a_library_listed_twice_is_shown_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join("drive/steamapps")).unwrap();
        std::os::unix::fs::symlink(root.join("drive"), root.join("alias")).unwrap();
        let uid = std::fs::metadata(&root).unwrap().uid();
        let paths = ["drive", "alias", "gone"].map(|p| root.join(p).to_string_lossy().into_owned());
        let found = find(&paths, uid, btrfs);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].steamapps, root.join("drive/steamapps"));
    }

    fn shares(read_only: bool, spots: Vec<Spot>) -> Shares {
        Shares {
            nsenter: PathBuf::from("/usr/bin/nsenter"),
            mount: PathBuf::from("/usr/bin/mount"),
            umount: PathBuf::from("/usr/bin/umount"),
            idmap: "u:1000:987:1 g:1000:956:1".into(),
            read_only,
            libraries: vec![Library {
                steamapps: PathBuf::from("/run/media/bazzite/QEMU HARDDISK/steamapps"),
                key: "00aa".into(),
                spots,
            }],
            roms: Vec::new(),
            home: None,
        }
    }

    /// Both shapes of the grants file give their paths, write grants and `~/` ones included;
    /// a denial, a relative path and what isn't a grant give none.
    #[test]
    fn every_plugin_s_granted_paths_are_read() {
        let text = r#"{
            "romm": {"grants": [
                {"path": "/mnt/roms", "write": false, "at": "", "by": "cli"},
                {"path": "~/ROMs", "write": true, "at": "", "by": "form"}
            ], "denied": ["/etc"]},
            "old": ["/srv/psx", "relative/x"],
            "odd": 7
        }"#;
        let mut paths = granted(text, Path::new("/home/enrico"));
        paths.sort();
        assert_eq!(
            paths,
            ["/home/enrico/ROMs", "/mnt/roms", "/srv/psx"].map(PathBuf::from)
        );
        assert!(granted("[]", Path::new("/h")).is_empty());
        assert!(granted("{", Path::new("/h")).is_empty());
    }

    /// A granted root is shown once, outermost, only where a seat may see it, and idmapped only
    /// on a filesystem that takes it. Each reason to skip one is named.
    #[test]
    fn a_granted_root_is_shown_only_where_a_seat_may_see_it() {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let dir = |rel: &str| {
            let path = base.join(rel);
            std::fs::create_dir_all(&path).unwrap();
            path
        };
        let (home, state) = (dir("home/enrico"), dir("state"));
        let steamapps = dir("drive/steamapps");
        let libraries = [Library {
            steamapps: steamapps.clone(),
            key: "00aa".into(),
            spots: Vec::new(),
        }];
        let fence = Fence {
            home: &home,
            state: &state,
            libraries: &libraries,
        };
        let closed = dir("closed");
        let reads = |root: &Path| root != closed;
        let reason = |path: &Path| rom_root(path, &fence, &reads);

        assert_eq!(reason(&dir("home/enrico/ROMs")), Ok(home.join("ROMs")));
        assert_eq!(
            reason(&dir("drive/Emulation/roms")),
            Ok(base.join("drive/Emulation/roms"))
        );
        assert_eq!(reason(&base.join("gone")), Err("not found"));
        std::fs::write(base.join("file"), "x").unwrap();
        assert_eq!(reason(&base.join("file")), Err("not a folder"));
        assert_eq!(
            reason(&dir("a:b")),
            Err("its path has characters a unit file can't carry")
        );
        for holds in [
            home.clone(),
            base.join("home"),
            base.clone(),
            state.clone(),
            dir("state/games"),
        ] {
            assert_eq!(
                reason(&holds),
                Err("it holds the owner's home or the seats' own folders"),
                "{}",
                holds.display()
            );
        }
        assert_eq!(
            reason(&dir("drive/steamapps/common/roms")),
            Err("it is in a Steam library the seat sees already")
        );
        assert_eq!(reason(&closed), Err("the owner can't read it"));

        let card = dir("card/roms");
        let paths = [
            dir("mnt/roms/psx"),
            dir("mnt/roms"),
            home.join("ROMs"),
            card.clone(),
            closed.clone(),
            base.join("mnt/roms/../roms"),
        ];
        let fstype = |path: &Path| Some(if path == card { "exfat" } else { "ext4" }.to_owned());
        let shown = roms(&paths, &fence, reads, fstype, "u:1000:987:1 g:1000:956:1");
        let idmap = Some("u:1000:987:1 g:1000:956:1".to_owned());
        let rom = |root: PathBuf, idmap: Option<String>| Rom {
            key: key(&root),
            root,
            idmap,
        };
        assert_eq!(
            shown,
            [
                rom(card, None),
                rom(home.join("ROMs"), idmap.clone()),
                rom(base.join("mnt/roms"), idmap),
            ]
        );
    }

    /// An idmapped root is mounted read-only at its stage and bound at its own path, a plain
    /// one bound to itself; an empty folder covers the owner's home a root lies in. Paths may
    /// hold a space.
    #[test]
    fn the_drop_in_shows_each_granted_root_at_its_own_path() {
        let shown = Shares {
            libraries: Vec::new(),
            roms: vec![
                Rom {
                    root: PathBuf::from("/home/enrico/My ROMs"),
                    key: "11bb".into(),
                    idmap: Some("u:1000:987:1 g:1000:956:1".into()),
                },
                Rom {
                    root: PathBuf::from("/run/media/enrico/SD Card/roms"),
                    key: "22cc".into(),
                    idmap: None,
                },
            ],
            home: Some(PathBuf::from("/home/enrico")),
            ..shares(false, Vec::new())
        };
        let text = unit_lines(Path::new("/b"), &id(), &shown);
        let stage = "/run/punktfunk/libraries/0123456789abcdef0123456789abcdef/11bb";
        assert_eq!(
            text,
            format!(
                "TemporaryFileSystem=\"/home/enrico:ro\"\n\
                 ExecStartPre=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/mount --bind \
                 -o \"ro,nosuid,nodev,X-mount.idmap=u:1000:987:1 g:1000:956:1\" \
                 \"/home/enrico/My ROMs\" {stage}\n\
                 ExecStopPost=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/umount -l {stage}\n\
                 BindReadOnlyPaths={stage}:\"/home/enrico/My ROMs\"\n\
                 BindReadOnlyPaths=-\"/run/media/enrico/SD Card/roms\":\"/run/media/enrico/SD Card/roms\"\n"
            )
        );
        let without = Shares {
            roms: Vec::new(),
            home: None,
            ..shown
        };
        assert!(unit_lines(Path::new("/b"), &id(), &without).is_empty());
    }

    /// The unit mounts each library idmapped at its stage, hides the owner's own path, binds the
    /// stage and the seat's private folders into the seat, and keeps the owner's list read-only.
    /// `/run/media` stays hidden.
    #[test]
    fn the_drop_in_binds_each_library_with_private_folders() {
        let text = super::super::session::render_binds(
            Path::new("/b"),
            &id(),
            "pf-seat-1",
            Some(&shares(false, Vec::new())),
        );
        let seat = "/b/seats/0123456789abcdef0123456789abcdef";
        let stage = "/run/punktfunk/libraries/0123456789abcdef0123456789abcdef/00aa";
        let owner = "/run/media/bazzite/QEMU HARDDISK/steamapps";
        let mut expected = format!(
            "[Service]\n\
             TemporaryFileSystem=/run/media:ro\n\
             BindPaths={seat}/compatdata:/b/games/steamapps/compatdata\n\
             BindPaths={seat}/shadercache:/b/games/steamapps/shadercache\n\
             BindPaths={seat}/downloading:/b/games/steamapps/downloading\n\
             ExecStartPre=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/mount --bind \
             -o \"nosuid,nodev,X-mount.idmap=u:1000:987:1 g:1000:956:1\" \"{owner}\" {stage}\n\
             ExecStopPost=-+/usr/bin/nsenter -t 1 -m -- /usr/bin/umount -l {stage}\n\
             InaccessiblePaths=\"-{owner}\"\n\
             BindPaths={stage}:{seat}/libraries/00aa/steamapps\n"
        );
        for name in ["compatdata", "shadercache", "downloading", "temp"] {
            expected.push_str(&format!(
                "BindPaths={seat}/private/00aa/{name}:{seat}/libraries/00aa/steamapps/{name}\n"
            ));
        }
        expected.push_str(&format!(
            "BindReadOnlyPaths=-{stage}/libraryfolders.vdf:\
             {seat}/libraries/00aa/steamapps/libraryfolders.vdf\n\
             BindPaths=-/dev/punktfunk/uinput-pf-seat-1:/dev/uinput\n"
        ));
        assert_eq!(text, expected);
    }

    /// Read-only, the mount gains `ro` and each spot gets the seat's copy; nothing else moves.
    #[test]
    fn a_read_only_library_mounts_ro_and_binds_each_spot() {
        let spots = [
            "common/Proton 10.0/dist.lock",
            "common/SteamLinuxRuntime_sniper/var",
        ];
        let shares_spots = spots
            .iter()
            .map(|path| Spot {
                path: PathBuf::from(path),
                dir: path.ends_with("var"),
            })
            .collect();
        let state = Path::new("/b");
        let writable = unit_lines(state, &id(), &shares(false, Vec::new()));
        let read_only = unit_lines(state, &id(), &shares(true, shares_spots));
        let seat = "/b/seats/0123456789abcdef0123456789abcdef";
        let mut binds = String::new();
        for spot in spots {
            binds.push_str(&format!(
                "BindPaths=\"{seat}/private/00aa/{spot}\":\
                 \"{seat}/libraries/00aa/steamapps/{spot}\"\n"
            ));
        }
        let expected = writable
            .replace("-o \"nosuid,", "-o \"ro,nosuid,")
            .replace("BindReadOnlyPaths=", &format!("{binds}BindReadOnlyPaths="));
        assert_eq!(read_only, expected);
    }

    /// Each runtime's `var` and platform `.ref`, and each Proton's `dist.lock`, as they exist;
    /// no link is followed. The seat's copies are its own and keep what it wrote.
    #[test]
    fn the_spots_are_what_a_game_writes_in_a_library() {
        let temp = tempfile::tempdir().unwrap();
        let steamapps = temp.path().join("steamapps");
        let common = steamapps.join("common");
        for dir in [
            "SteamLinuxRuntime_sniper/var",
            "SteamLinuxRuntime_sniper/sniper_platform_1/files",
            "SteamLinuxRuntime_sniper/sniper_platform_2/files",
            "SteamLinuxRuntime_soldier/soldier_platform_1/files",
            "SteamLinuxRuntime_linkedfiles/platform",
            "Proton 10.0",
            "Proton - Experimental",
            "Proton linked lock",
            "Some Game/var",
        ] {
            std::fs::create_dir_all(common.join(dir)).unwrap();
        }
        for file in [
            "SteamLinuxRuntime_sniper/sniper_platform_1/files/.ref",
            "SteamLinuxRuntime_soldier/soldier_platform_1/files/.ref",
            "Proton 10.0/dist.lock",
        ] {
            std::fs::write(common.join(file), "").unwrap();
        }
        let link = |target: &str, at: &str| {
            std::os::unix::fs::symlink(common.join(target), common.join(at)).unwrap()
        };
        link("SteamLinuxRuntime_sniper", "SteamLinuxRuntime_linked");
        link("Proton 10.0", "Proton linked");
        link(
            "SteamLinuxRuntime_sniper/sniper_platform_1/files",
            "SteamLinuxRuntime_linkedfiles/platform/files",
        );
        link("Proton 10.0/dist.lock", "Proton linked lock/dist.lock");

        let found = spots(&steamapps);
        let want = [
            ("common/Proton 10.0/dist.lock", false),
            ("common/SteamLinuxRuntime_sniper/var", true),
            (
                "common/SteamLinuxRuntime_sniper/sniper_platform_1/files/.ref",
                false,
            ),
            (
                "common/SteamLinuxRuntime_soldier/soldier_platform_1/files/.ref",
                false,
            ),
        ]
        .map(|(path, dir)| Spot {
            path: PathBuf::from(path),
            dir,
        });
        assert_eq!(found, want);

        let private = temp.path().join("private");
        let seat = me(temp.path());
        make_spots(&private, &found, &seat).unwrap();
        let lock = private.join("common/Proton 10.0/dist.lock");
        assert!(private.join("common/SteamLinuxRuntime_sniper/var").is_dir());
        assert_eq!(std::fs::metadata(&lock).unwrap().uid(), seat.uid);
        std::fs::write(&lock, "held").unwrap();
        make_spots(&private, &found, &seat).unwrap();
        assert_eq!(std::fs::read_to_string(&lock).unwrap(), "held");
    }

    /// The box's Seat default decides; without one the libraries stay writable, and a file that
    /// can't be read as the setting turns them read-only.
    #[test]
    fn the_box_s_seat_default_makes_the_libraries_read_only() {
        let temp = tempfile::tempdir().unwrap();
        assert!(writable(temp.path()));
        let path = temp.path().join(pf_host_config::seat_defaults::FILE);
        for library_writable in [false, true] {
            let defaults = pf_host_config::seat_defaults::SeatDefaults {
                end_on_game_exit: true,
                max_mode: None,
                library_writable,
                dualsense_usbip: false,
                browser: false,
                browser_origins: Vec::new(),
                background_input: false,
            };
            std::fs::write(&path, defaults.to_json().to_string()).unwrap();
            assert_eq!(writable(temp.path()), library_writable);
        }
        std::fs::write(&path, "{}").unwrap();
        assert!(writable(temp.path()), "no setting named");
        for unreadable in ["", "{", "[]", r#"{"library_writable":"yes"}"#, "null"] {
            std::fs::write(&path, unreadable).unwrap();
            assert!(!writable(temp.path()), "{unreadable:?}");
        }
        std::fs::write(&path, r#"{"library_writable":null}"#).unwrap();
        assert!(!writable(temp.path()), "a value that isn't a bool");
    }

    fn steam_file(home: &Path, dir: &str) -> PathBuf {
        home.join(".local/share/Steam").join(dir).join(LIST)
    }

    /// Both of the seat's lists gain the shared games folder and the box's libraries after
    /// Steam's own entry, and lose a stale drive; an absent copy stays absent.
    #[test]
    fn the_seat_s_lists_name_the_box_s_libraries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().to_path_buf();
        let own = home
            .join(".local/share/Steam")
            .to_string_lossy()
            .into_owned();
        let before = steam::library_folders_vdf(&[
            own.clone(),
            "/run/media/system/QEMU_HARDDISK".to_owned(),
        ]);
        for dir in ["steamapps", "config"] {
            std::fs::create_dir_all(steam_file(&home, dir).parent().unwrap()).unwrap();
            std::fs::write(steam_file(&home, dir), &before).unwrap();
        }
        let seat = me(&home);
        let library = Library {
            steamapps: PathBuf::from("/x/steamapps"),
            key: "00aa".into(),
            spots: Vec::new(),
        };
        register(
            Path::new("/b"),
            &id(),
            &seat,
            std::slice::from_ref(&library),
        );
        let after = steam::library_folders_vdf(&[
            own.clone(),
            "/b/games".to_owned(),
            "/b/seats/0123456789abcdef0123456789abcdef/libraries/00aa".to_owned(),
        ]);
        for dir in ["steamapps", "config"] {
            assert_eq!(
                std::fs::read_to_string(steam_file(&home, dir)).unwrap(),
                after
            );
        }

        std::fs::remove_file(steam_file(&home, "config")).unwrap();
        register(Path::new("/b"), &id(), &seat, &[]);
        assert!(!steam_file(&home, "config").exists());
        let text = std::fs::read_to_string(steam_file(&home, "steamapps")).unwrap();
        assert_eq!(steam::library_paths(&text), [own.as_str(), "/b/games"]);

        // Steam wrote its own list back without the games folder, which it drops at entry 0.
        std::fs::write(
            steam_file(&home, "steamapps"),
            steam::library_folders_vdf(std::slice::from_ref(&own)),
        )
        .unwrap();
        register(Path::new("/b"), &id(), &seat, &[]);
        let text = std::fs::read_to_string(steam_file(&home, "steamapps")).unwrap();
        assert_eq!(steam::library_paths(&text), [own.as_str(), "/b/games"]);
    }

    /// Root never follows a link the seat planted on the way to its lists.
    #[test]
    fn a_link_in_the_seat_s_tree_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let elsewhere = temp.path().join("elsewhere");
        let target = elsewhere.join("steamapps").join(LIST);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "\"libraryfolders\"\n{\n}\n").unwrap();
        std::fs::create_dir_all(home.join(".local/share")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, home.join(".local/share/Steam")).unwrap();
        let seat = me(temp.path());
        let merge = |_: &str| Some("changed".to_owned());
        assert!(merge_list(&home, "steamapps", &seat, merge).is_err());

        std::fs::remove_file(home.join(".local/share/Steam")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/Steam/steamapps")).unwrap();
        std::os::unix::fs::symlink(&target, steam_file(&home, "steamapps")).unwrap();
        assert!(merge_list(&home, "steamapps", &seat, merge).is_err());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "\"libraryfolders\"\n{\n}\n"
        );
    }

    /// A FIFO for a seat's library list or the box's Seat defaults answers at once.
    #[test]
    fn a_fifo_in_a_tree_root_reads_does_not_block() {
        use super::super::safe_open::tests::{fifo, within};
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(steam_file(&home, "steamapps").parent().unwrap()).unwrap();
        fifo(&steam_file(&home, "steamapps"));
        let seat = me(temp.path());
        let h = home.clone();
        let merge = |_: &str| Some("changed".to_owned());
        let merged = within(move || merge_list(&h, "steamapps", &seat, merge).is_ok());
        assert_eq!(
            merged,
            Some(false),
            "a library list FIFO is refused at once"
        );
        fifo(&temp.path().join(DEFAULTS));
        let b = temp.path().to_path_buf();
        assert_eq!(
            within(move || writable(&b)),
            Some(false),
            "unreadable reads as read-only"
        );
    }
}
