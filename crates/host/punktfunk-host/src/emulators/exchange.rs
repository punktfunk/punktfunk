//! A seat's saves, for the host with the plugins (`?profile=` on the save routes).
//!
//! On Windows that host reads the seat account's copies itself: a [`Seat`] is the account's
//! profile folder, hermir's home for them. A Linux owner's host can't reach a seat's home, so the
//! seat host keeps its exchange folder (`pf_seats::linux::saves_dir`) filled. At a launch's hold,
//! before it waits, it exports every unit of the title's emulator and platform to `out/` and lists
//! them, with the platform's firmware, in `out/units.json`. Once the hold is released it puts back
//! what the owner's host left in `in/`, then empties it. When the game exits it exports what
//! changed. The owner's host answers from there. A seat's ROM roots are read-only, so a seat saves
//! nothing beside a game, and `game` is not asked there.

use std::path::{Path, PathBuf};

/// A seat as the host with the plugins reaches its saves: on Windows its account's profile
/// folder, on Linux its exchange folder.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub(crate) struct Seat(PathBuf);

/// The seat `id` names, when this host reaches its saves. Blocking: Windows reads the ledger.
pub(crate) fn seat(id: &str) -> Option<Seat> {
    let id = pf_seats::model::SeatId::parse(id).ok()?;
    #[cfg(windows)]
    {
        let ledger = crate::seats::ledger();
        let (row, _) = ledger.seat(id.as_str())?;
        pf_seats::windows::account_profile(&row.account).map(Seat)
    }
    #[cfg(target_os = "linux")]
    {
        let dir = pf_seats::linux::saves_dir(&id);
        dir.is_dir().then_some(Seat(dir))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = id;
        None
    }
}

/// [`super::units`] of `seat`.
pub(crate) fn units(
    seat: &Seat,
    id: &str,
    platform: &str,
    game: Option<&Path>,
) -> hermir::Result<Vec<hermir::SaveUnit>> {
    #[cfg(target_os = "linux")]
    {
        let _ = game;
        Ok(linux::exported(&seat.0, id, platform)
            .map(|e| e.units.into_iter().map(|u| u.unit).collect())
            .unwrap_or_default())
    }
    #[cfg(not(target_os = "linux"))]
    super::units(Some(seat.0.as_path()), id, platform, game)
}

/// [`super::export_units`] of `seat`. On Linux the files are the seat's last export, and a unit
/// it didn't export is left out.
pub(crate) fn export_units(
    seat: &Seat,
    id: &str,
    platform: &str,
    game: Option<&Path>,
    units: &[(hermir::SaveKind, String)],
    out: &Path,
) -> hermir::Result<Vec<(hermir::SaveKind, hermir::ExportedUnit)>> {
    #[cfg(target_os = "linux")]
    {
        let _ = game;
        linux::copy_out(&seat.0, id, platform, units, out)
    }
    #[cfg(not(target_os = "linux"))]
    super::export_units(Some(seat.0.as_path()), id, platform, game, units, out)
}

/// [`super::import_unit`] into `seat`. On Linux the unit waits in `in/` for the seat host, which
/// puts it back before its next game starts.
#[allow(clippy::too_many_arguments)] // `super::import_unit`'s, with the seat for the account
pub(crate) fn import_unit(
    seat: &Seat,
    id: &str,
    platform: &str,
    game: Option<&Path>,
    kinds: &[hermir::SaveKind],
    name: &str,
    from: &Path,
    others: &[String],
) -> hermir::Result<hermir::PrepareStep> {
    #[cfg(target_os = "linux")]
    {
        let _ = game;
        linux::hand_in(&seat.0, id, platform, kinds, name, from, others)
    }
    #[cfg(not(target_os = "linux"))]
    super::import_unit(
        Some(seat.0.as_path()),
        id,
        platform,
        game,
        kinds,
        name,
        from,
        others,
    )
}

/// [`super::firmware_status`] of `seat`. `None` on Linux while the seat host hasn't checked
/// that emulator and platform.
pub(crate) fn firmware_status(
    seat: &Seat,
    id: &str,
    platform: &str,
) -> Option<hermir::Result<Option<hermir::FirmwareStatus>>> {
    #[cfg(target_os = "linux")]
    {
        linux::exported(&seat.0, id, platform).map(|e| Ok(e.firmware))
    }
    #[cfg(not(target_os = "linux"))]
    Some(super::firmware_status(Some(seat.0.as_path()), id, platform))
}

/// Before a seat's launch hold: the title's units into `out/`, listed in `out/units.json`.
pub(crate) fn before_hold(app: Option<&str>) {
    #[cfg(target_os = "linux")]
    if let Some((dir, emulator, platform)) = linux::here(app) {
        let out = dir.join("out");
        linux::empty(&out);
        linux::export(&out, &emulator, &platform, None);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

/// Once a seat's launch hold is released: each unit the owner's host left in `in/`, put back.
pub(crate) fn after_hold(app: Option<&str>) {
    #[cfg(target_os = "linux")]
    if let Some((dir, emulator, platform)) = linux::here(app) {
        linux::take_in(&dir.join("in"), &emulator, &platform);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

/// Before a seat reports its game exited: the units that changed, exported again.
pub(crate) fn after_exit(app: Option<&str>) {
    #[cfg(target_os = "linux")]
    if let Some((dir, emulator, platform)) = linux::here(app) {
        let out = dir.join("out");
        let before = linux::read_json(&out.join(linux::UNITS), None);
        linux::export(&out, &emulator, &platform, before);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = app;
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::io::{Read as _, Write as _};
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    /// `out/units.json`: what the seat host last exported, for one emulator and platform.
    #[derive(Serialize, Deserialize)]
    pub(super) struct Exported {
        pub emulator: String,
        /// The catalog's id.
        pub platform: String,
        pub units: Vec<Unit>,
        pub firmware: Option<hermir::FirmwareStatus>,
    }

    #[derive(Clone, Serialize, Deserialize)]
    pub(super) struct Unit {
        pub unit: hermir::SaveUnit,
        /// Its file in `out/`, as hermir wrote it.
        pub export: hermir::ExportedUnit,
    }

    /// `in/<name>.import.json`: how the seat host puts back the unit in `in/<name>`.
    #[derive(Serialize, Deserialize)]
    struct Import {
        emulator: String,
        platform: String,
        kinds: Vec<hermir::SaveKind>,
        others: Vec<String>,
    }

    pub(super) const UNITS: &str = "units.json";
    const IMPORT: &str = ".import.json";
    /// The most of a seat's JSON the owner's host reads: a seat host is a guest.
    const JSON_LIMIT: u64 = 4 << 20;
    /// The largest save file that crosses: a PS3 save folder's tar runs to hundreds of MB.
    const FILE_LIMIT: u64 = 1 << 30;

    /// This host's exchange folder and the title's emulator and platform, for an `emulator`
    /// entry on a contract seat.
    pub(super) fn here(app: Option<&str>) -> Option<(PathBuf, String, String)> {
        if !pf_paths::seat::role().follows_contract() {
            return None;
        }
        let id = pf_paths::seat::seat_id().ok()??;
        let dir = pf_seats::linux::saves_dir(&pf_seats::model::SeatId::parse(id).ok()?);
        let entry = crate::library::entry_for_library_id(app?)?;
        let spec = entry.launch.as_ref().filter(|s| s.kind == "emulator")?;
        let platform = spec.args.iter().flatten().find(|a| a.name == "platform")?;
        let platform = platform_id(&platform.value);
        dir.is_dir().then(|| (dir, spec.value.clone(), platform))
    }

    /// `platform` as the catalog spells it, from its id or any alias; as given when unknown.
    fn platform_id(platform: &str) -> String {
        hermir::Catalog::embedded()
            .ok()
            .and_then(|c| c.find_platform(platform).map(|p| p.id.clone()))
            .unwrap_or_else(|| platform.to_string())
    }

    /// Exports every unit of `emulator` and `platform` into `out` and writes `units.json`. A
    /// unit `before` lists with the same stamp keeps its file.
    pub(super) fn export(out: &Path, emulator: &str, platform: &str, before: Option<Exported>) {
        let exported = (|| {
            let h = super::super::open()?;
            let handle = h.emulator(emulator)?;
            let Some(install) = handle.best()? else {
                return Ok(None);
            };
            let before = before
                .filter(|b| b.emulator == emulator && b.platform == platform)
                .map(|b| b.units)
                .unwrap_or_default();
            let mut units = Vec::new();
            for unit in handle.units(&install, platform, None)? {
                let stamp =
                    |u: &hermir::SaveUnit| (u.kind, u.name.clone(), u.size, u.files, u.modified);
                let kept = before.iter().find(|k| stamp(&k.unit) == stamp(&unit));
                if let Some(kept) = kept.filter(|k| k.export.file.is_file()) {
                    units.push(kept.clone());
                    continue;
                }
                match handle.export_unit(&install, platform, None, unit.kind, &unit.name, out) {
                    Ok(export) => units.push(Unit { unit, export }),
                    Err(error) => {
                        tracing::warn!(%error, unit = %unit.name, "seat save not exported")
                    }
                }
            }
            let firmware = handle.firmware_status(&install, platform).ok().flatten();
            hermir::Result::Ok(Some(Exported {
                emulator: emulator.to_string(),
                platform: platform.to_string(),
                units,
                firmware,
            }))
        })();
        match exported {
            Ok(Some(exported)) => {
                if let Err(error) = write_json(&out.join(UNITS), &exported) {
                    tracing::warn!(%error, "seat saves not listed for the owner's host");
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, emulator, "seat saves not exported"),
        }
    }

    /// Puts back each unit in `input` the owner's host left for `emulator` and `platform`, then
    /// empties it. One for another title goes unused.
    pub(super) fn take_in(input: &Path, emulator: &str, platform: &str) {
        let Ok(entries) = std::fs::read_dir(input) else {
            return;
        };
        for entry in entries.flatten() {
            let file = entry.file_name();
            let Some(name) = file.to_str().and_then(|f| f.strip_suffix(IMPORT)) else {
                continue;
            };
            let Some(import) = read_json::<Import>(&entry.path(), None) else {
                continue;
            };
            if import.emulator != emulator || import.platform != platform {
                continue;
            }
            let from = input.join(name);
            match super::super::import_unit(
                None,
                emulator,
                platform,
                None,
                &import.kinds,
                name,
                &from,
                &import.others,
            ) {
                Ok(step) => {
                    tracing::info!(unit = name, outcome = ?step.outcome, note = step.note.as_deref().unwrap_or(""), "seat save put back")
                }
                Err(error) => tracing::warn!(%error, unit = name, "seat save not put back"),
            }
        }
        empty(input);
    }

    /// Removes everything in `dir`, never following a link.
    pub(super) fn empty(dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let gone = if entry.file_type().is_ok_and(|t| t.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(error) = gone {
                tracing::debug!(%error, path = %path.display(), "exchange entry not removed");
            }
        }
    }

    /// The seat's last export for `emulator` and `platform`, as the owner's host reads it.
    pub(super) fn exported(dir: &Path, emulator: &str, platform: &str) -> Option<Exported> {
        let out = dir.join("out");
        let seat = std::fs::symlink_metadata(&out).ok()?.uid();
        read_json::<Exported>(&out.join(UNITS), Some(seat))
            .filter(|e| e.emulator == emulator && e.platform == platform_id(platform))
    }

    /// Copies the named units' files from the seat's `out/` into `to`.
    pub(super) fn copy_out(
        dir: &Path,
        emulator: &str,
        platform: &str,
        units: &[(hermir::SaveKind, String)],
        to: &Path,
    ) -> hermir::Result<Vec<(hermir::SaveKind, hermir::ExportedUnit)>> {
        let out = dir.join("out");
        let seat = std::fs::symlink_metadata(&out)
            .map_err(|e| io("read", &out, e))?
            .uid();
        let Some(exported) = exported(dir, emulator, platform) else {
            return Ok(Vec::new());
        };
        let mut copied = Vec::new();
        for (kind, name) in units {
            let Some(unit) = exported
                .units
                .iter()
                .find(|u| u.unit.kind == *kind && u.unit.name == *name)
            else {
                continue;
            };
            let Some(file) = unit.export.file.file_name() else {
                continue;
            };
            let Some(mut source) = open_seat(&out.join(file), Some(seat)) else {
                tracing::debug!(unit = %name, "the seat's file for a unit isn't a plain file of its own");
                continue;
            };
            let target = to.join(file);
            let mut sink = create_new(&target)?;
            std::io::copy(&mut (&mut source).take(FILE_LIMIT), &mut sink)
                .map_err(|e| io("write", &target, e))?;
            copied.push((
                *kind,
                hermir::ExportedUnit {
                    name: name.clone(),
                    file: target,
                    size: unit.export.size,
                    md5: unit.export.md5.clone(),
                },
            ));
        }
        Ok(copied)
    }

    /// `from` into the seat's `in/` as `name`, with what the seat host needs to put it back.
    pub(super) fn hand_in(
        dir: &Path,
        emulator: &str,
        platform: &str,
        kinds: &[hermir::SaveKind],
        name: &str,
        from: &Path,
        others: &[String],
    ) -> hermir::Result<hermir::PrepareStep> {
        let plain = !name.is_empty()
            && name.len() <= 200
            && !name.starts_with('.')
            && !name.ends_with(IMPORT)
            && !name.contains(['/', '\0']);
        if !plain {
            return Err(hermir::Error::Invalid(format!(
                "{name:?} isn't a file name"
            )));
        }
        let input = dir.join("in");
        let target = input.join(name);
        let mut source = std::fs::File::open(from).map_err(|e| io("open", from, e))?;
        std::io::copy(
            &mut (&mut source).take(FILE_LIMIT),
            &mut create_new(&target)?,
        )
        .map_err(|e| io("write", &target, e))?;
        let import = Import {
            emulator: emulator.to_string(),
            platform: platform_id(platform),
            kinds: kinds.to_vec(),
            others: others.to_vec(),
        };
        let manifest = input.join(format!("{name}{IMPORT}"));
        let json = serde_json::to_vec(&import).unwrap_or_default();
        create_new(&manifest)?
            .write_all(&json)
            .map_err(|e| io("write", &manifest, e))?;
        Ok(hermir::PrepareStep {
            kind: "save".into(),
            target,
            outcome: hermir::StepOutcome::Applied,
            note: Some("The seat puts it back before its next game starts.".into()),
        })
    }

    /// `path` made afresh, never through a link a seat left there.
    fn create_new(path: &Path) -> hermir::Result<std::fs::File> {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io("replace", path, e)),
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| io("create", path, e))
    }

    /// A plain file, opened without following a link or waiting on a pipe; with `owner`, only
    /// one that uid owns.
    fn open_seat(path: &Path, owner: Option<u32>) -> Option<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let meta = file.metadata().ok()?;
        (meta.is_file() && owner.is_none_or(|uid| meta.uid() == uid)).then_some(file)
    }

    /// The JSON at `path`, read as [`open_seat`] opens it, at most [`JSON_LIMIT`] of it.
    pub(super) fn read_json<T: serde::de::DeserializeOwned>(
        path: &Path,
        owner: Option<u32>,
    ) -> Option<T> {
        let mut bytes = Vec::new();
        open_seat(path, owner)?
            .take(JSON_LIMIT)
            .read_to_end(&mut bytes)
            .ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// `value` to `path` whole: written beside it and renamed over it, so a reader never sees
    /// half of it.
    fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
        let staged = path.with_extension("json.new");
        let _ = std::fs::remove_file(&staged);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        file.write_all(&serde_json::to_vec(value).map_err(std::io::Error::other)?)?;
        std::fs::rename(&staged, path)
    }

    fn io(op: &'static str, path: &Path, source: std::io::Error) -> hermir::Error {
        hermir::Error::Io {
            op,
            path: path.to_path_buf(),
            source,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn unit(name: &str, file: &Path) -> Unit {
            Unit {
                unit: hermir::SaveUnit {
                    kind: hermir::SaveKind::Memcard,
                    name: name.into(),
                    shared: true,
                    written_at_start: false,
                    size: 8,
                    files: 1,
                    modified: 1,
                    path: PathBuf::from("/seat/home/Mcd001.ps2"),
                },
                export: hermir::ExportedUnit {
                    name: name.into(),
                    file: file.to_path_buf(),
                    size: 4,
                    md5: "ab".into(),
                },
            }
        }

        /// What a seat exported reaches the owner's side for that emulator and platform alone,
        /// spelled by id or alias; its files copy out, and a link the seat planted never does.
        #[test]
        fn a_seat_s_export_crosses_for_its_own_title() {
            let temp = tempfile::tempdir().unwrap();
            let (out, to) = (temp.path().join("out"), temp.path().join("plugin"));
            std::fs::create_dir_all(&out).unwrap();
            std::fs::create_dir_all(&to).unwrap();
            std::fs::write(out.join("Mcd001.ps2"), "save").unwrap();
            std::os::unix::fs::symlink("/etc/passwd", out.join("Mcd002.ps2")).unwrap();
            let exported = Exported {
                emulator: "pcsx2".into(),
                platform: platform_id("ps2"),
                units: vec![
                    unit("Mcd001.ps2", &out.join("Mcd001.ps2")),
                    unit("Mcd002.ps2", &out.join("Mcd002.ps2")),
                ],
                firmware: None,
            };
            write_json(&out.join(UNITS), &exported).unwrap();

            let dir = temp.path();
            assert!(exported_ok(dir, "pcsx2", "ps2"));
            assert!(!exported_ok(dir, "dolphin", "ps2"));
            assert!(!exported_ok(dir, "pcsx2", "gc"));
            let kind = hermir::SaveKind::Memcard;
            let copied =
                copy_out(dir, "pcsx2", "ps2", &[(kind, "Mcd001.ps2".into())], &to).unwrap();
            assert_eq!(copied[0].1.md5, "ab");
            assert_eq!(
                std::fs::read_to_string(to.join("Mcd001.ps2")).unwrap(),
                "save"
            );
            let planted = copy_out(dir, "pcsx2", "ps2", &[(kind, "Mcd002.ps2".into())], &to);
            assert!(planted.unwrap().is_empty());
            assert!(!to.join("Mcd002.ps2").exists());
        }

        fn exported_ok(dir: &Path, emulator: &str, platform: &str) -> bool {
            exported(dir, emulator, platform).is_some()
        }

        /// A unit handed in lands in `in/` beside how to put it back, replacing a link the seat
        /// left there rather than writing through it; a name that is a path is refused.
        #[test]
        fn a_handed_in_unit_waits_in_the_seat_s_in_folder() {
            let temp = tempfile::tempdir().unwrap();
            let input = temp.path().join("in");
            std::fs::create_dir_all(&input).unwrap();
            let from = temp.path().join("from.ps2");
            std::fs::write(&from, "server's").unwrap();
            let victim = temp.path().join("victim");
            std::fs::write(&victim, "mine").unwrap();
            std::os::unix::fs::symlink(&victim, input.join("Mcd001.ps2")).unwrap();
            let kinds = [hermir::SaveKind::Memcard];
            let step = hand_in(
                temp.path(),
                "pcsx2",
                "ps2",
                &kinds,
                "Mcd001.ps2",
                &from,
                &[],
            )
            .unwrap();
            assert_eq!(step.outcome, hermir::StepOutcome::Applied);
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "mine");
            assert_eq!(
                std::fs::read_to_string(input.join("Mcd001.ps2")).unwrap(),
                "server's"
            );
            let import: Import = read_json(&input.join("Mcd001.ps2.import.json"), None).unwrap();
            assert_eq!(
                (import.emulator.as_str(), import.platform),
                ("pcsx2", platform_id("ps2"))
            );
            assert!(hand_in(temp.path(), "pcsx2", "ps2", &kinds, "../x", &from, &[]).is_err());
            empty(&input);
            assert_eq!(std::fs::read_dir(&input).unwrap().count(), 0);
        }
    }
}
