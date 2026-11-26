//! The box library, and each seat's copy of it.
//!
//! The owner's host writes the library in the owner's home. [`refresh_box_library`] copies it
//! into `library/`, with every profile's layer, the plugins' grants and the firmware the
//! plugins staged. The door serves that copy to every client.
//!
//! [`refresh_seat_library`] copies it on into a seat's trust copy, with only the layer of the
//! profile that seat plays. The owner's row has no such copy: its host writes the library it
//! reads.
//!
//! The folder walk under both ([`sync_dir`], [`sync_each`], [`sync_files`]) opens each folder
//! through the one above it. It copies a plain file with a safe name and a size within its
//! limit; a link or an oversized entry is skipped.

use super::layout::{box_library_dir, io_err, trust_dir};
use super::safe_open;
use super::trust_copy::{open_copy, sync_file, unlink, Reader, MAX_TRUST_BYTES};
use crate::backend::BackendError;
use crate::model::SeatId;
use rustix::fs::{AtFlags, Mode};
use std::collections::HashSet;
use std::os::fd::OwnedFd;
use std::path::Path;

/// The library files a host reads from its library dir. Play stats stay with each host.
const LIBRARY_FILES: [&str; 6] = [
    "library.json",
    "library-hidden.json",
    "library-metadata.json",
    "library-overrides.json",
    "library-picks.json",
    "library-scanners.json",
];

/// One table per metadata source, a flat folder beside the library files.
const LIBRARY_METADATA: &str = "library-metadata";

/// A profile's own layer of the library: one folder per profile, named by its id.
const LIBRARY_PROFILES: &str = "library-profiles";
/// What a profile's folder holds. Nothing in them runs: `own-titles.json` names the titles its
/// seat keeps, and how each starts stays on the seat.
const PROFILE_FILES: [&str; 6] = [
    "library-hidden.json",
    "library-overrides.json",
    "library-picks.json",
    "library-scanners.json",
    "own-titles.json",
    "session-settings.json",
];

/// The owner's host's file that the door and every seat read beside the library: the display
/// policy, whose one writer is the owner's host. Copied with the library, into the same dir.
pub(super) const OWNER_POLICY: &str = "display-settings.json";

/// A big catalog with descriptions runs to tens of megabytes; past this it is not ours.
const MAX_LIBRARY_BYTES: u64 = 64 * 1024 * 1024;

/// The folder of the plugins' runner files. Only [`PLUGIN_GRANTS`] is read: its tokens stay.
pub(super) const PLUGIN_RUN: &str = "plugin-run";
/// The roots the owner granted each plugin: paths, never a secret.
pub(super) const PLUGIN_GRANTS: &str = "plugin-grants.json";

/// The plugins' own state. Only `<provider>/firmware/<platform>/<file>` is read: the rest holds
/// sign-ins.
const PLUGIN_STATE: &str = "plugin-state";
const FIRMWARE: &str = "firmware";
/// A PS3 update or a Switch firmware archive runs to hundreds of megabytes.
const MAX_FIRMWARE_BYTES: u64 = 1024 * 1024 * 1024;

/// Brings the box's library level with the owner's own in `home`, for `reader`, the door's
/// group. Returns whether anything changed. A config folder that isn't there, as in a home not
/// mounted yet, leaves the copy as it is: the door keeps serving the last library it had. The
/// display policy follows even when a library file can't; the first failure is the answer.
pub(super) fn refresh_box_library(
    home: &Path,
    state: &Path,
    reader: Reader,
) -> Result<bool, BackendError> {
    let io = |what: &str, e| io_err("box_library", what, e);
    let config = safe_open::dir(home).and_then(|at| safe_open::walk(at, &[".config", "punktfunk"]));
    let src = match config {
        Ok(Some(src)) => src,
        Ok(None) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io("open the owner's config folder", e)),
    };
    let (dst, reader) =
        open_copy(&box_library_dir(state), reader).map_err(|e| io("open the library copy", e))?;
    let library = sync_library(Some(&src), &dst, reader, &|_| true)
        .map_err(|e| io("copy the owner's library", e));
    let policy = sync_file(Some(&src), &dst, OWNER_POLICY, reader, MAX_TRUST_BYTES)
        .map_err(|e| io("copy the owner's display policy", e));
    Ok(library? | policy?)
}

/// Brings the box's library in a seat's trust copy level with [`box_library_dir`]: the seat's
/// `PUNKTFUNK_LIBRARY_DIR`. Of [`LIBRARY_PROFILES`] it holds only the folder of the profile the
/// door's `profiles.json` in `box_dir` puts on this seat ([`seat_profile`]). Never the owner's
/// row, whose host writes the library it reads.
pub(super) fn refresh_seat_library(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    reader: Reader,
) -> Result<bool, BackendError> {
    let io = |what: &str, e| io_err("trust_copy", what, e);
    let (dst, reader) =
        open_copy(&trust_dir(state, id), reader).map_err(|e| io("open the trust copy", e))?;
    let src = open_source(&box_library_dir(state)).map_err(|e| io("open the box library", e))?;
    let own = seat_profile(box_dir, id);
    sync_library(src.as_ref(), &dst, reader, &|p| own.as_deref() == Some(p))
        .map_err(|e| io("copy the box library", e))
}

/// The profile seat `id` plays: the first in `box_dir`'s `profiles.json` whose `os_account` is
/// `{"kind": "seat", "seat": id}`, as its host picks its own. Read from the door's file, never
/// the trust copy: a person owns theirs. A file [`safe_open`] refuses, one over
/// [`MAX_TRUST_BYTES`] or not `{"profiles": [...]}`, and an id that is not 12 lowercase hex,
/// name none.
fn seat_profile(box_dir: &Path, id: &SeatId) -> Option<String> {
    lock_holder(box_dir, id, false).flatten()
}

/// The profile whose lock seat `id` follows. It comes from the door's `profiles.json`, read as
/// [`seat_profile`] reads it.
///
/// - Without `owner`: the seat's own profile.
/// - With `owner`: the owner's profile, which is the first profile on the box's own session.
///   Every profile that shares that desktop opens with that profile's lock.
///
/// Returns `None` when the file can't be read, and `Some(None)` when no profile with a valid id
/// plays there.
pub(super) fn lock_holder(box_dir: &Path, id: &SeatId, owner: bool) -> Option<Option<String>> {
    #[derive(serde::Deserialize)]
    struct File {
        profiles: Vec<serde_json::Value>,
    }
    let (bytes, _) = safe_open::read(&box_dir.join("profiles.json"), MAX_TRUST_BYTES).ok()??;
    let file: File = serde_json::from_slice(&bytes).ok()?;
    let plays = |p: &&serde_json::Value| {
        let account = &p["os_account"];
        match owner {
            true => account["kind"] == "operator",
            false => account["kind"] == "seat" && account["seat"] == id.as_str(),
        }
    };
    let first = file.profiles.iter().find(plays);
    let id = first
        .and_then(|p| p["id"].as_str())
        .filter(|p| profile_id(p));
    Some(id.map(str::to_owned))
}

/// A profile id as the host mints it: 12 lowercase hex.
fn profile_id(name: &str) -> bool {
    name.len() == 12 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Removes the box's library: the owner's row is gone, and so is the door it was for.
pub(super) fn remove_box_library(state: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(box_library_dir(state)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The library files, the metadata folder, the folders of the profiles `profile` admits, the
/// plugins' grants and the firmware they staged, from `src` into `dst`, for `reader`. An item
/// that can't be copied keeps its last good copy and the rest still follow; the first failure is
/// the answer.
fn sync_library(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    reader: Reader,
    profile: &dyn Fn(&str) -> bool,
) -> std::io::Result<bool> {
    let mut tally = Tally::default();
    for name in LIBRARY_FILES {
        tally.note(sync_file(src, dst, name, reader, MAX_LIBRARY_BYTES));
    }
    let metadata = sync_flat_dir(src, dst, LIBRARY_METADATA, reader, MAX_LIBRARY_BYTES);
    tally.note(metadata);
    tally.note(sync_profiles(src, dst, reader, profile));
    tally.note(sub(src, PLUGIN_RUN).and_then(|run| {
        sync_dir(run.as_ref(), dst, PLUGIN_RUN, reader, &|run, copy| {
            sync_file(run, copy, PLUGIN_GRANTS, reader, MAX_TRUST_BYTES)
        })
    }));
    tally.note(sync_firmware(src, dst, reader));
    tally.answer()
}

/// Whether any step changed its copy, and the first step that failed.
#[derive(Default)]
struct Tally {
    changed: bool,
    first: Option<std::io::Error>,
}

impl Tally {
    fn note(&mut self, step: std::io::Result<bool>) {
        match step {
            Ok(did) => self.changed |= did,
            Err(e) => {
                self.first.get_or_insert(e);
            }
        }
    }

    fn answer(self) -> std::io::Result<bool> {
        self.first.map_or(Ok(self.changed), Err)
    }
}

/// [`LIBRARY_PROFILES`] from `src` into `dst`: the folder of each profile id `keep` admits,
/// holding [`PROFILE_FILES`] alone, and every other folder in `dst` emptied and removed. A name
/// that is no profile id, or a link or file where a folder belongs, counts as absent. A folder
/// on its way out loses the temp a cut-short copy left too.
fn sync_profiles(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    reader: Reader,
    keep: &dyn Fn(&str) -> bool,
) -> std::io::Result<bool> {
    let files = |src: Option<&OwnedFd>, copy: &OwnedFd| {
        let mut tally = Tally::default();
        for name in PROFILE_FILES {
            tally.note(sync_file(src, copy, name, reader, MAX_LIBRARY_BYTES));
            if src.is_none() {
                tally.note(unlink(copy, &format!("{name}.tmp")));
            }
        }
        tally.answer()
    };
    let profiles = |src: Option<&OwnedFd>, copy: &OwnedFd| {
        let mut found = folders(src, |dir| Ok(Some(dir)))?;
        found.retain(|(name, _)| profile_id(name) && keep(name));
        sync_each(found, copy, reader, &files)
    };
    let src = sub(src, LIBRARY_PROFILES)?;
    sync_dir(src.as_ref(), dst, LIBRARY_PROFILES, reader, &profiles)
}

/// `plugin-state/<provider>/firmware/<platform>/<file>` from `src` into `dst`: what a plugin
/// staged for an emulator. Each folder is opened through the one above it and only those are;
/// a provider's other entries, and anything deeper than a platform's files, are never read.
fn sync_firmware(src: Option<&OwnedFd>, dst: &OwnedFd, reader: Reader) -> std::io::Result<bool> {
    let platform =
        |src: Option<&OwnedFd>, copy: &OwnedFd| sync_files(src, copy, reader, MAX_FIRMWARE_BYTES);
    let firmware = |src: Option<&OwnedFd>, copy: &OwnedFd| {
        sync_each(folders(src, |dir| Ok(Some(dir)))?, copy, reader, &platform)
    };
    let provider =
        |src: Option<&OwnedFd>, copy: &OwnedFd| sync_dir(src, copy, FIRMWARE, reader, &firmware);
    let state = |src: Option<&OwnedFd>, copy: &OwnedFd| {
        let found = folders(src, |dir| safe_open::dir_at(&dir, FIRMWARE))?;
        sync_each(found, copy, reader, &provider)
    };
    let src = sub(src, PLUGIN_STATE)?;
    sync_dir(src.as_ref(), dst, PLUGIN_STATE, reader, &state)
}

/// What a copied folder is filled with: its source's handle, `None` once the source is gone,
/// and the copy's.
type Fill<'a> = &'a dyn Fn(Option<&OwnedFd>, &OwnedFd) -> std::io::Result<bool>;

/// `name` in `src`, a folder and not a link. `None` when either is absent.
fn sub(src: Option<&OwnedFd>, name: &str) -> std::io::Result<Option<OwnedFd>> {
    match src {
        Some(src) => safe_open::dir_at(src, name),
        None => Ok(None),
    }
}

/// The folders in `src` with a safe name, each as `open` takes it on from its handle. An entry
/// that is no folder, or that `open` refuses or finds nothing in, is left out.
fn folders(
    src: Option<&OwnedFd>,
    open: impl Fn(OwnedFd) -> std::io::Result<Option<OwnedFd>>,
) -> std::io::Result<Vec<(String, OwnedFd)>> {
    let Some(src) = src else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for name in names(src)?.into_iter().filter(|n| safe_name(n)) {
        if let Ok(Some(dir)) = safe_open::dir_at(src, &name)
            && let Ok(Some(next)) = open(dir)
        {
            out.push((name, next));
        }
    }
    Ok(out)
}

/// Every folder in `dst` not in `found` emptied and removed, then each of `found` copied into
/// `dst` through `fill`. A folder that fails keeps what it has and stops none after it; the
/// first failure is the answer.
fn sync_each(
    found: Vec<(String, OwnedFd)>,
    dst: &OwnedFd,
    reader: Reader,
    fill: Fill<'_>,
) -> std::io::Result<bool> {
    let mut tally = Tally::default();
    for name in names(dst)? {
        if found.iter().all(|(kept, _)| *kept != name) {
            tally.note(sync_dir(None, dst, &name, reader, fill));
        }
    }
    for (name, src) in &found {
        tally.note(sync_dir(Some(src), dst, name, reader, fill));
    }
    tally.answer()
}

/// The folder `name` in `dst` level with `src`, its source already opened: made for `reader`
/// and filled through `fill`, or, with no source, emptied through `fill` and removed. A folder
/// that `reader` refuses ([`Reader::claim`]) is an error, and stays as it is.
fn sync_dir(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    name: &str,
    reader: Reader,
    fill: Fill<'_>,
) -> std::io::Result<bool> {
    if src.is_some() {
        let mode = Mode::from_raw_mode(reader.dir_mode() as _);
        match rustix::fs::mkdirat(dst, name, mode) {
            Ok(()) => {}
            Err(e) if e == rustix::io::Errno::EXIST => {}
            Err(e) => return Err(e.into()),
        }
    }
    let Some(copies) = safe_open::dir_at(dst, name)? else {
        return Ok(false);
    };
    reader.claim(&copies, name)?;
    if src.is_some() {
        reader.hand(&copies, true)?;
    }
    let changed = fill(src, &copies)?;
    if src.is_none() {
        let _ = rustix::fs::unlinkat(dst, name, AtFlags::REMOVEDIR);
    }
    Ok(changed)
}

/// A directory a copy is taken from, `None` when it doesn't exist.
pub(super) fn open_source(dir: &Path) -> std::io::Result<Option<OwnedFd>> {
    match safe_open::dir(dir) {
        Ok(src) => Ok(Some(src)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// The names in `dir`, without `.` and `..`; a name that is not UTF-8 is left out.
pub(super) fn names(dir: &OwnedFd) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir)? {
        if let Ok(name) = entry?.file_name().to_str()
            && !matches!(name, "." | "..")
        {
            out.push(name.to_owned());
        }
    }
    Ok(out)
}

/// Mirrors the flat directory `dir` of `src` into `dst` ([`sync_files`]). A link where the
/// directory should be is refused.
pub(super) fn sync_flat_dir(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    dir: &str,
    reader: Reader,
    max: u64,
) -> std::io::Result<bool> {
    let src = sub(src, dir)?;
    sync_dir(src.as_ref(), dst, dir, reader, &|src, copies| {
        sync_files(src, copies, reader, max)
    })
}

/// Copies each file of `src` with a safe name and at most `max` bytes into `copies`, and drops
/// every copy whose source is gone or isn't such a file.
fn sync_files(
    src: Option<&OwnedFd>,
    copies: &OwnedFd,
    reader: Reader,
    max: u64,
) -> std::io::Result<bool> {
    let mut wanted = HashSet::new();
    let mut changed = false;
    if let Some(src) = src {
        for name in names(src)?.into_iter().filter(|n| safe_name(n)) {
            // A link, a second link or an oversized entry is skipped, not copied and not fatal.
            if let Ok(did) = sync_file(Some(src), copies, &name, reader, max) {
                changed |= did;
                wanted.insert(name);
            }
        }
    }
    for name in names(copies)? {
        if !wanted.contains(&name) {
            changed |= unlink(copies, &name)?;
        }
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::linux::accounts::Passwd;
    use crate::linux::layout::{ensure_person_dirs, person_config_dir};
    use crate::linux::trust_copy::tests::{id, keeps_acls, mode, own_gid};
    use crate::linux::trust_copy::{copy_person_pin, refresh_trust, AVATARS, TRUST_FILES};
    use std::os::unix::fs::MetadataExt as _;

    /// The box's library follows the owner's own files and reaches a seat's trust copy, minus
    /// play stats and anything else in the owner's config folder.
    #[test]
    fn the_box_library_follows_the_owner_into_each_seat() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state) = (temp.path().join("home"), temp.path().join("state"));
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(config.join("library-metadata")).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(config.join("library.json"), r#"{"entries":[]}"#).unwrap();
        std::fs::write(config.join("library-metadata/sgdb.json"), "{}").unwrap();
        std::fs::write(config.join("library-stats.json"), "{}").unwrap();
        std::fs::write(config.join("mgmt-token"), "secret").unwrap();
        let (run, romm) = (config.join("plugin-run"), config.join("plugin-state/romm"));
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("plugin-grants.json"), r#"{"romm":{"grants":[]}}"#).unwrap();
        std::fs::write(run.join("plugin-tokens.json"), "secret").unwrap();
        for dir in ["firmware/ps2/deeper", "firmware/psx", "cache"] {
            std::fs::create_dir_all(romm.join(dir)).unwrap();
        }
        std::fs::write(romm.join("firmware/ps2/scph39001.bin"), "bios").unwrap();
        std::fs::write(romm.join("firmware/ps2/deeper/x.bin"), "too deep").unwrap();
        std::fs::write(romm.join("firmware/psx/scph5501.bin"), "bios").unwrap();
        std::fs::write(romm.join("firmware/token.json"), "secret").unwrap();
        std::fs::write(romm.join("token.json"), "secret").unwrap();
        std::fs::write(romm.join("cache/x"), "x").unwrap();
        std::fs::create_dir_all(config.join("plugin-state/steam")).unwrap();
        std::fs::write(config.join("plugin-state/steam/key"), "secret").unwrap();
        std::fs::write(config.join("plugin-state/runner.refused"), "x").unwrap();
        let reader = Reader::group(own_gid(&home));

        assert!(refresh_box_library(&home, &state, reader).unwrap());
        let copy = box_library_dir(&state);
        assert_eq!(mode(&copy), 0o750);
        assert_eq!(mode(&copy.join("library.json")), 0o640);
        assert!(copy.join("library-metadata/sgdb.json").exists());
        for kept in [
            "library-stats.json",
            "mgmt-token",
            "plugin-run/plugin-tokens.json",
        ] {
            assert!(!copy.join(kept).exists(), "{kept} stays the owner's");
        }
        assert!(
            !refresh_box_library(&home, &state, reader).unwrap(),
            "unchanged is a no-op"
        );

        assert!(refresh_seat_library(&home, &state, &id(), reader).unwrap());
        let trust = trust_dir(&state, &id());
        assert!(trust.join("library.json").exists());
        assert!(trust.join("library-metadata/sgdb.json").exists());
        let state_copy = trust.join("plugin-state");
        for copied in [copy.clone(), trust.clone()] {
            assert!(copied.join("plugin-run/plugin-grants.json").exists());
            assert!(!copied.join("plugin-run/plugin-tokens.json").exists());
            let fw = copied.join("plugin-state/romm/firmware");
            assert_eq!(
                std::fs::read_to_string(fw.join("ps2/scph39001.bin")).unwrap(),
                "bios"
            );
            assert_eq!(mode(&fw.join("ps2/scph39001.bin")), 0o640);
            assert!(fw.join("psx/scph5501.bin").exists());
        }
        // Only `<provider>/firmware/<platform>/<file>` crosses: no token beside the firmware or
        // above it, no cache, no deeper folder, no provider without firmware.
        let mut crossed: Vec<String> = Vec::new();
        let mut walk = vec![state_copy.clone()];
        while let Some(dir) = walk.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let rel = path.strip_prefix(&state_copy).unwrap();
                crossed.push(rel.to_string_lossy().into_owned());
                if path.is_dir() {
                    walk.push(path);
                }
            }
        }
        crossed.sort();
        assert_eq!(
            crossed,
            [
                "romm",
                "romm/firmware",
                "romm/firmware/ps2",
                "romm/firmware/ps2/scph39001.bin",
                "romm/firmware/psx",
                "romm/firmware/psx/scph5501.bin",
            ]
        );

        // A file the owner drops leaves both copies, and so does a platform or the grants.
        std::fs::remove_file(config.join("library-metadata/sgdb.json")).unwrap();
        std::fs::remove_dir_all(romm.join("firmware/psx")).unwrap();
        std::fs::remove_file(run.join("plugin-grants.json")).unwrap();
        assert!(refresh_box_library(&home, &state, reader).unwrap());
        assert!(refresh_seat_library(&home, &state, &id(), reader).unwrap());
        assert!(!trust.join("library-metadata/sgdb.json").exists());
        assert!(!trust.join("plugin-state/romm/firmware/psx").exists());
        assert!(!trust.join("plugin-run/plugin-grants.json").exists());

        // A platform the plugin links elsewhere is not followed, and its copy goes.
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("shadow"), "root only").unwrap();
        std::fs::remove_dir_all(romm.join("firmware/ps2")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, romm.join("firmware/ps2")).unwrap();
        assert!(refresh_box_library(&home, &state, reader).unwrap());
        assert!(!copy.join("plugin-state/romm/firmware/ps2").exists());

        remove_box_library(&state).unwrap();
        assert!(!copy.exists());
        assert!(refresh_seat_library(&home, &state, &id(), reader).unwrap());
        assert!(!trust.join("library.json").exists());
        assert!(!state_copy.exists());
    }

    pub(in crate::linux) const ANNA: &str = "a1a1a1a1a1a1";
    const BEN: &str = "b2b2b2b2b2b2";
    pub(in crate::linux) const OTHER_SEAT: &str = "fedcba9876543210fedcba9876543210";

    /// The door's `profiles.json`: the owner, Anna on `anna`'s seat, Ben on `ben`'s.
    pub(in crate::linux) fn profiles_on(box_dir: &Path, anna: &str, ben: &str) {
        let file = serde_json::json!({"version": 1, "profiles": [
            {"id": "c3c3c3c3c3c3", "display_name": "Owner", "os_account": {"kind": "operator"}},
            {"id": ANNA, "display_name": "Anna", "os_account": {"kind": "seat", "seat": anna}},
            {"id": BEN, "display_name": "Ben", "os_account": {"kind": "seat", "seat": ben}},
        ]});
        std::fs::write(box_dir.join("profiles.json"), file.to_string()).unwrap();
    }

    /// The names in `dir`, sorted; none when it is absent.
    fn listing(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The door's copy holds every profile's layer; a seat's trust copy holds its own profile's
    /// alone and drops a folder that stops being its own.
    #[test]
    fn a_seat_gets_its_own_profile_s_layer_and_no_other() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state, box_dir) = (
            temp.path().join("home"),
            temp.path().join("state"),
            temp.path().join("box"),
        );
        let layer = home.join(".config/punktfunk").join(LIBRARY_PROFILES);
        for dir in [
            layer.join(ANNA),
            layer.join(BEN),
            state.clone(),
            box_dir.clone(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(layer.join(ANNA).join("library-hidden.json"), "[1]").unwrap();
        std::fs::write(layer.join(ANNA).join("session-settings.json"), "{}").unwrap();
        std::fs::write(layer.join(ANNA).join("own-titles.json"), "{}").unwrap();
        std::fs::write(layer.join(ANNA).join("notes.txt"), "not a store").unwrap();
        std::fs::write(layer.join(BEN).join("library-picks.json"), "{}").unwrap();
        profiles_on(&box_dir, id().as_str(), OTHER_SEAT);
        let door = Reader::group(own_gid(&home));
        assert!(refresh_box_library(&home, &state, door).unwrap());
        let copy = box_library_dir(&state).join(LIBRARY_PROFILES);
        assert_eq!(listing(&copy), [ANNA, BEN]);
        assert_eq!(mode(&copy.join(ANNA)), 0o750);
        assert_eq!(mode(&copy.join(ANNA).join("library-hidden.json")), 0o640);
        assert_eq!(
            listing(&copy.join(ANNA)),
            [
                "library-hidden.json",
                "own-titles.json",
                "session-settings.json"
            ],
            "the stores alone"
        );

        let me = std::fs::metadata(&home).unwrap();
        let seat = Reader::person(me.uid(), me.gid());
        let seat_copy = |box_dir: &Path| refresh_seat_library(box_dir, &state, &id(), seat);
        assert!(seat_copy(&box_dir).unwrap());
        let trust = trust_dir(&state, &id()).join(LIBRARY_PROFILES);
        assert_eq!(listing(&trust), [ANNA]);
        // With ACLs root owns the copy, and the account reads it through its entry.
        let (folder, file) = if keeps_acls(&state) {
            (0o750, 0o640)
        } else {
            (0o700, 0o600)
        };
        assert_eq!(mode(&trust.join(ANNA)), folder);
        assert_eq!(
            std::fs::read_to_string(trust.join(ANNA).join("library-hidden.json")).unwrap(),
            "[1]"
        );
        assert_eq!(mode(&trust.join(ANNA).join("library-hidden.json")), file);
        assert_eq!(mode(&trust.join(ANNA).join("own-titles.json")), file);
        assert!(!seat_copy(&box_dir).unwrap(), "unchanged is a no-op");

        // The seat becomes Ben's: Anna's folder goes, with what a cut-short copy left.
        std::fs::write(trust.join(ANNA).join("library-picks.json.tmp"), "{").unwrap();
        profiles_on(&box_dir, OTHER_SEAT, id().as_str());
        assert!(seat_copy(&box_dir).unwrap());
        assert_eq!(listing(&trust), [BEN]);

        // No profile plays the seat, or the door's file is gone: no profile's folder.
        profiles_on(&box_dir, OTHER_SEAT, OTHER_SEAT);
        assert!(seat_copy(&box_dir).unwrap());
        assert!(listing(&trust).is_empty());
        profiles_on(&box_dir, id().as_str(), OTHER_SEAT);
        assert!(seat_copy(&box_dir).unwrap());
        assert!(seat_copy(&temp.path().join("gone")).unwrap());
        assert!(listing(&trust).is_empty());

        // The owner's host drops Anna's layer: it leaves the door's copy, then the seat's.
        assert!(seat_copy(&box_dir).unwrap());
        std::fs::remove_dir_all(layer.join(ANNA)).unwrap();
        assert!(refresh_box_library(&home, &state, door).unwrap());
        assert_eq!(listing(&copy), [BEN]);
        assert!(seat_copy(&box_dir).unwrap());
        assert!(listing(&trust).is_empty());
    }

    /// These never cross into the copy: a link, a file where a folder belongs, a name that is not
    /// a profile id, and a store that is a link, a folder or over its size cap. The rest of the
    /// layer is still copied.
    #[test]
    fn hostile_profile_entries_are_not_copied() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state, box_dir) = (
            temp.path().join("home"),
            temp.path().join("state"),
            temp.path().join("box"),
        );
        let layer = home.join(".config/punktfunk").join(LIBRARY_PROFILES);
        let (anna, secret) = (layer.join(ANNA), temp.path().join("secret"));
        for dir in [&anna, &state, &box_dir, &secret] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(secret.join("library-hidden.json"), "root only").unwrap();
        std::os::unix::fs::symlink(
            secret.join("library-hidden.json"),
            anna.join("library-hidden.json"),
        )
        .unwrap();
        std::fs::create_dir(anna.join("library-overrides.json")).unwrap();
        let big = std::fs::File::create(anna.join("library-picks.json")).unwrap();
        big.set_len(MAX_LIBRARY_BYTES + 1).unwrap();
        std::fs::write(anna.join("library-scanners.json"), "{}").unwrap();
        std::os::unix::fs::symlink(&secret, layer.join(BEN)).unwrap();
        std::fs::write(layer.join("d4d4d4d4d4d4"), "a file").unwrap();
        for name in ["A1A1A1A1A1A1", "notaprofile", "a1a1a1a1a1a1a1"] {
            std::fs::create_dir(layer.join(name)).unwrap();
            std::fs::write(layer.join(name).join("library-hidden.json"), "[]").unwrap();
        }
        let door = Reader::group(own_gid(&home));
        let error = refresh_box_library(&home, &state, door).unwrap_err();
        assert_eq!(error.code, "box_library");
        let copy = box_library_dir(&state).join(LIBRARY_PROFILES);
        assert_eq!(listing(&copy), [ANNA]);
        assert_eq!(listing(&copy.join(ANNA)), ["library-scanners.json"]);

        profiles_on(&box_dir, id().as_str(), OTHER_SEAT);
        assert!(refresh_seat_library(&box_dir, &state, &id(), door).unwrap());
        let trust = trust_dir(&state, &id()).join(LIBRARY_PROFILES);
        assert_eq!(listing(&trust.join(ANNA)), ["library-scanners.json"]);

        // A linked layer folder is refused whole: the copies keep what they had.
        std::fs::remove_dir_all(&layer).unwrap();
        std::os::unix::fs::symlink(&secret, &layer).unwrap();
        assert!(refresh_box_library(&home, &state, door).is_err());
        assert_eq!(listing(&copy), [ANNA]);
    }

    /// A seat's profile is the one whose account names that seat. The owner's row, a light seat,
    /// an id that is no profile id, and a door file that is a link or no JSON name none.
    #[test]
    fn a_seat_s_profile_comes_from_the_door_s_file_alone() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        assert_eq!(seat_profile(box_dir, &id()), None, "no file");
        profiles_on(box_dir, id().as_str(), OTHER_SEAT);
        assert_eq!(seat_profile(box_dir, &id()).as_deref(), Some(ANNA));
        let other = SeatId::parse(OTHER_SEAT).unwrap();
        assert_eq!(seat_profile(box_dir, &other).as_deref(), Some(BEN));

        let file = |rows: serde_json::Value| {
            let file = serde_json::json!({ "profiles": rows });
            std::fs::write(box_dir.join("profiles.json"), file.to_string()).unwrap();
            seat_profile(box_dir, &id())
        };
        let seat = serde_json::json!({"kind": "seat", "seat": id().as_str()});
        for bad in [
            "../a1a1a1a1a",
            "A1A1A1A1A1A1",
            "a1a1a1a1a1a1/..",
            "operator",
            "",
        ] {
            let rows = serde_json::json!([{"id": bad, "os_account": seat}]);
            assert_eq!(file(rows), None, "{bad}");
        }
        let light = serde_json::json!([{"id": ANNA, "os_account": {"kind": "seat"}}]);
        assert_eq!(file(light), None, "a light seat");
        let owner = serde_json::json!([{"id": ANNA, "os_account": {"kind": "operator"}}]);
        assert_eq!(file(owner), None, "the owner's");

        std::fs::write(box_dir.join("profiles.json"), "not json").unwrap();
        assert_eq!(seat_profile(box_dir, &id()), None);
        let real = temp.path().join("real.json");
        profiles_on(box_dir, id().as_str(), OTHER_SEAT);
        std::fs::rename(box_dir.join("profiles.json"), &real).unwrap();
        std::os::unix::fs::symlink(&real, box_dir.join("profiles.json")).unwrap();
        assert_eq!(seat_profile(box_dir, &id()), None, "a link");
    }

    /// The owner's display policy travels with the library: into the copy the door reads, and
    /// from there into a seat's trust copy, never from the door's own directory.
    #[test]
    fn the_owner_s_display_policy_reaches_the_door_and_the_seats() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state, box_dir) = (
            temp.path().join("home"),
            temp.path().join("state"),
            temp.path().join("box"),
        );
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        std::fs::write(config.join("library.json"), "{}").unwrap();
        std::fs::write(
            config.join("display-settings.json"),
            r#"{"preset":"shared"}"#,
        )
        .unwrap();
        std::fs::write(
            box_dir.join("display-settings.json"),
            r#"{"preset":"door"}"#,
        )
        .unwrap();
        let reader = Reader::group(own_gid(&home));
        assert!(refresh_box_library(&home, &state, reader).unwrap());
        let copy = box_library_dir(&state).join("display-settings.json");
        assert_eq!(
            std::fs::read_to_string(&copy).unwrap(),
            r#"{"preset":"shared"}"#
        );

        assert!(refresh_trust(&box_dir, &state, &id(), reader).unwrap());
        let trust = trust_dir(&state, &id()).join("display-settings.json");
        assert_eq!(
            std::fs::read_to_string(&trust).unwrap(),
            r#"{"preset":"shared"}"#
        );
    }

    /// An owner's config folder that isn't there, as in a home not mounted yet, keeps the copy
    /// the door serves; a link among the owner's files is refused and not followed.
    #[test]
    fn the_box_library_survives_a_missing_home_and_refuses_a_link() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state) = (temp.path().join("home"), temp.path().join("state"));
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(config.join("library.json"), "{}").unwrap();
        let reader = Reader::group(own_gid(&home));
        assert!(refresh_box_library(&home, &state, reader).unwrap());

        let gone = temp.path().join("gone");
        assert!(!refresh_box_library(&gone, &state, reader).unwrap());
        assert!(box_library_dir(&state).join("library.json").exists());

        let secret = temp.path().join("secret");
        std::fs::write(&secret, "root only").unwrap();
        std::fs::remove_file(config.join("library.json")).unwrap();
        std::os::unix::fs::symlink(&secret, config.join("library.json")).unwrap();
        let error = refresh_box_library(&home, &state, reader).unwrap_err();
        assert_eq!(error.code, "box_library");
        assert_ne!(
            std::fs::read_to_string(box_library_dir(&state).join("library.json")).unwrap(),
            "root only"
        );
    }

    /// A library file the copy refuses keeps its last good copy and stops none after it: the
    /// later files, the plugins' grants and the display policy still follow.
    #[test]
    fn a_bad_library_file_keeps_its_copy_and_the_rest_follow() {
        let temp = tempfile::tempdir().unwrap();
        let (home, state) = (temp.path().join("home"), temp.path().join("state"));
        let config = home.join(".config/punktfunk");
        let run = config.join("plugin-run");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(config.join("library.json"), "{}").unwrap();
        std::fs::write(run.join("plugin-grants.json"), "[1]").unwrap();
        let reader = Reader::group(own_gid(&home));
        assert!(refresh_box_library(&home, &state, reader).unwrap());

        // A second link makes the first library file unreadable to the copy.
        std::fs::hard_link(config.join("library.json"), temp.path().join("backup")).unwrap();
        std::fs::write(config.join("library-picks.json"), "[]").unwrap();
        std::fs::write(run.join("plugin-grants.json"), "{}").unwrap();
        std::fs::write(config.join("display-settings.json"), "{}").unwrap();
        let error = refresh_box_library(&home, &state, reader).unwrap_err();
        assert_eq!(error.code, "box_library");
        let copy = box_library_dir(&state);
        let read = |name: &str| std::fs::read_to_string(copy.join(name)).unwrap();
        assert_eq!(read("library.json"), "{}");
        assert_eq!(read("library-picks.json"), "[]");
        assert_eq!(read("plugin-run/plugin-grants.json"), "{}");
        assert_eq!(read("display-settings.json"), "{}");
    }

    /// The person token stays the person's: no copy list names it, and a person's row hands the
    /// box the certificate alone from the folder that holds it.
    #[test]
    fn no_copy_takes_the_person_token() {
        let token = pf_paths::seat::PERSON_TOKEN;
        let named = TRUST_FILES
            .iter()
            .chain(&LIBRARY_FILES)
            .chain(&PROFILE_FILES)
            .chain(&[
                AVATARS,
                LIBRARY_METADATA,
                LIBRARY_PROFILES,
                OWNER_POLICY,
                PLUGIN_RUN,
                PLUGIN_GRANTS,
                PLUGIN_STATE,
                FIRMWARE,
            ]);
        for name in named {
            assert!(!name.contains(token), "{name}");
        }
        let temp = tempfile::tempdir().unwrap();
        let (state, pin) = (temp.path().join("state"), temp.path().join("pin"));
        // The folder the token lands in is the person's, and closed to the door's user.
        let me = std::fs::metadata(temp.path()).unwrap();
        let person = Passwd {
            name: "jane".into(),
            uid: me.uid(),
            gid: me.gid(),
            gecos: String::new(),
            home: temp.path().join("home"),
        };
        ensure_person_dirs(&state, &id(), &person).unwrap();
        let config = person_config_dir(&state, &id());
        let made = std::fs::metadata(&config).unwrap();
        assert_eq!((made.uid(), mode(&config)), (me.uid(), 0o700));
        std::fs::write(config.join("native-cert.pem"), "pem").unwrap();
        std::fs::write(config.join(token), "secret").unwrap();
        copy_person_pin(&state, &id(), &pin).unwrap();
        let copied: Vec<_> = std::fs::read_dir(&pin)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(copied, ["native-cert.pem"]);
    }
}
