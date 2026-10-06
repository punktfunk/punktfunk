//! Managed emulators: hermir, opened on this host's prefix. A plugin only asks; an install runs
//! on the operator's click and lands under `<prefix>/<id>/app`, the folder the plugin
//! is then granted, so its launch templates may point inside it.
use std::path::{Path, PathBuf};

use hermir::progress::Quiet;
use hermir::{Hermir, Installed, Options};

/// Where managed emulators live. `%ProgramData%\punktfunk\emulators` on Windows, so the SYSTEM
/// service installs and the player's session runs. The data dir on POSIX: a plugin is granted
/// the emulator's folder, and the runner shares nothing under the config dir.
pub fn prefix() -> PathBuf {
    #[cfg(windows)]
    let base = pf_paths::config_dir();
    #[cfg(not(windows))]
    let base = pf_paths::data_dir();
    base.join("emulators")
}

/// The emulator's own folder: exe, config and saves for a portable one. The path a plugin is
/// granted when its request is allowed, whether the copy lives here or is a Flatpak.
pub fn home_of(id: &str) -> PathBuf {
    prefix().join(id).join("app")
}

pub fn open() -> hermir::Result<Hermir> {
    Hermir::open(Options {
        prefix: Some(prefix()),
        ..Options::default()
    })
}

/// Whether this OS has an install channel for `id`; `NotInCatalog` for an unknown id.
pub fn offered(id: &str) -> hermir::Result<bool> {
    let h = open()?;
    let entry = h
        .catalog()
        .get(id)
        .ok_or_else(|| hermir::Error::NotInCatalog(id.into()))?;
    Ok(entry.channels.get(h.os()).is_some())
}

/// Installs (or reinstalls) `id` and makes its folder exist, so the grant that follows has a
/// directory to land on even when the copy itself is a Flatpak.
pub fn install(id: &str) -> hermir::Result<Installed> {
    let h = open()?;
    let row = h.emulator(id)?.install(&Quiet)?;
    let home = home_of(id);
    std::fs::create_dir_all(&home).map_err(|e| hermir::Error::Io {
        op: "create",
        path: home.clone(),
        source: e,
    })?;
    crate::plugins::open_for_players(&prefix().join(id));
    Ok(row)
}

pub fn remove(id: &str, purge: bool) -> hermir::Result<()> {
    open()?.emulator(id)?.remove(purge)
}

/// RetroArch's cores folder on this machine, from the best copy hermir knows; `None` without
/// a RetroArch. hermir creates it on the first core, so the grant that follows has a folder.
pub fn cores_dir() -> hermir::Result<Option<PathBuf>> {
    let h = open()?;
    Ok(h.emulator("retroarch")?
        .best()?
        .and_then(|i| i.config_root)
        .map(|root| root.join("cores")))
}

/// A libretro core from the buildbot into that folder.
pub fn install_core(core: &str) -> hermir::Result<PathBuf> {
    open()?.install_core(core, &Quiet)
}

/// Whether RetroArch's cores folder `dir` already holds `core`, under the name hermir gives it.
pub fn has_core(dir: &Path, core: &str) -> bool {
    dir.join(format!(
        "{core}_libretro.{}",
        std::env::consts::DLL_EXTENSION
    ))
    .is_file()
}

/// The pads this host made for its players, in the order they appeared: seat 1 is the first.
/// Linux reads them off `/proc/bus/input/devices` — a virtual device with a vendor the host's
/// pad backends present is one of ours. With none up yet (the client's pad frames arrive
/// after the launch), seat 1 is the pad the host will make, keyed by identity and index 0.
pub fn session_players() -> Vec<hermir::Player> {
    let mut pads = Vec::new();
    #[cfg(target_os = "linux")]
    if let Ok(text) = std::fs::read_to_string("/proc/bus/input/devices") {
        pads = virtual_pads(&text);
    }
    if pads.is_empty() {
        pads.push(hermir::PadRef::xbox360(0));
    }
    pads.into_iter()
        .zip(1u8..)
        .map(|(pad, seat)| hermir::Player { seat, pad })
        .collect()
}

/// Microsoft and Sony: the identities the host's uinput and uhid pads carry.
const PAD_VENDORS: [u16; 2] = [0x045e, 0x054c];

/// The virtual pads in a `/proc/bus/input/devices` listing, in event-node order.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn virtual_pads(text: &str) -> Vec<hermir::PadRef> {
    let mut out: Vec<(u32, hermir::PadRef)> = Vec::new();
    for block in text.split("\n\n") {
        let field = |tag: &str| {
            block
                .lines()
                .find_map(|l| l.strip_prefix(tag))
                .map(str::trim)
        };
        let Some(ids) = field("I:") else { continue };
        let hex = |key: &str| {
            ids.split_whitespace()
                .find_map(|kv| kv.strip_prefix(key))
                .and_then(|v| u16::from_str_radix(v, 16).ok())
        };
        let (Some(bus), Some(vendor), Some(product), Some(version)) = (
            hex("Bus="),
            hex("Vendor="),
            hex("Product="),
            hex("Version="),
        ) else {
            continue;
        };
        if !PAD_VENDORS.contains(&vendor)
            || !field("S: Sysfs=").is_some_and(|s| s.starts_with("/devices/virtual/"))
        {
            continue;
        }
        let name = field("N: Name=")
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        let event = field("H: Handlers=")
            .and_then(|h| {
                h.split_whitespace()
                    .find_map(|w| w.strip_prefix("event")?.parse::<u32>().ok())
            })
            .unwrap_or(u32::MAX);
        out.push((
            event,
            hermir::PadRef {
                name,
                bus,
                vendor,
                product,
                version,
                index: 0,
                evdev: (event != u32::MAX)
                    .then(|| PathBuf::from(format!("/dev/input/event{event}"))),
                guid: None,
                gamepad_name: None,
            },
        ));
    }
    out.sort_by_key(|(event, _)| *event);
    out.into_iter()
        .zip(0u32..)
        .map(|((_, mut pad), index)| {
            pad.index = index;
            pad
        })
        .collect()
}

/// Puts every emulator's own bindings back after a game: what `prepare` wrote for the
/// session's pads is gone again, file by file. Forced: an emulator rewrites its config on
/// exit, and that is no edit of the player's. Nothing outstanding is a quiet no-op.
pub fn revert_players() {
    let Ok(h) = open() else { return };
    revert_with(&h);
}

fn revert_with(h: &Hermir) {
    let reverted = match h.revert_all(true) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "player bindings not reverted");
            return;
        }
    };
    for (id, steps) in reverted {
        for s in steps {
            tracing::info!(emulator = %id, target = %s.target.display(), outcome = ?s.outcome, note = s.note.as_deref().unwrap_or(""), "player bindings reverted");
        }
    }
}

/// Every copy of `id` answers its first-run questions, gets `platform`'s firmware from
/// `firmware_dir` (the regular files in it, not below), and has the session's pads written
/// in seat order (steps of kind `players`; reverted after the game). `platform` is a catalog
/// id or any of its aliases (RomM slug, ES-DE folder, libretro name). Blocking: an installer
/// may run.
pub fn prepare(
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
) -> hermir::Result<Vec<(String, hermir::Prepared)>> {
    let h = open()?;
    // A game that ended without a lease reporting it (its session took the nested gamescope
    // down with it) still has its bindings out; the next launch is the latest they go back.
    revert_with(&h);
    let emulator = h.emulator(id)?;
    let platform = platform.map(|p| {
        h.catalog()
            .platforms()
            .iter()
            .find(|x| x.id == p || x.aliases.values().any(|a| a == p))
            .map_or_else(|| p.to_string(), |x| x.id.clone())
    });
    let platform = platform.as_deref();
    let firmware: Vec<PathBuf> = match firmware_dir {
        None => Vec::new(),
        Some(dir) => std::fs::read_dir(dir)
            .map_err(|e| hermir::Error::Io {
                op: "read",
                path: dir.to_path_buf(),
                source: e,
            })?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect(),
    };
    let players = hermir::Patch {
        players: Some(session_players()),
        ..Default::default()
    };
    emulator
        .copies()?
        .iter()
        .map(|copy| {
            let mut prepared = emulator.prepare(copy, platform, &firmware)?;
            prepared.steps.extend(emulator.apply(copy, &players)?.steps);
            Ok((copy.exe.to_string(), prepared))
        })
        .collect()
}

/// `platform` as the catalog spells it, from its id or any alias; as given when unknown.
fn platform_id(h: &Hermir, platform: &str) -> String {
    h.catalog()
        .find_platform(platform)
        .map_or_else(|| platform.to_string(), |p| p.id.clone())
}

/// What a library needs of the catalog: platforms and emulators, offered on this OS or not.
pub fn registry() -> hermir::Result<hermir::Registry> {
    Ok(open()?.registry())
}

/// The libretro cores each copy of `id` has, by exe; empty for every emulator but RetroArch.
pub fn cores(h: &Hermir, install: &hermir::Install) -> Vec<String> {
    h.emulator(&install.emulator)
        .map(|e| e.cores(install))
        .unwrap_or_default()
}

/// Runs `f` on the best copy of `id`: managed, else the first found. `Invalid` without one.
fn on_best<T>(
    id: &str,
    f: impl FnOnce(&Hermir, &hermir::EmulatorHandle<'_>, &hermir::Install) -> hermir::Result<T>,
) -> hermir::Result<T> {
    let h = open()?;
    let emulator = h.emulator(id)?;
    let install = emulator
        .best()?
        .ok_or_else(|| hermir::Error::Invalid(format!("no copy of {id} on this host")))?;
    f(&h, &emulator, &install)
}

/// The save units of `platform` on the best copy; `game` is the game's folder.
pub fn units(
    id: &str,
    platform: &str,
    game: Option<&Path>,
) -> hermir::Result<Vec<hermir::SaveUnit>> {
    on_best(id, |h, e, i| e.units(i, &platform_id(h, platform), game))
}

/// Each named unit written into `out`, a tar for a folder.
pub fn export_units(
    id: &str,
    platform: &str,
    game: Option<&Path>,
    units: &[(hermir::SaveKind, String)],
    out: &Path,
) -> hermir::Result<Vec<(hermir::SaveKind, hermir::ExportedUnit)>> {
    std::fs::create_dir_all(out).map_err(|e| hermir::Error::Io {
        op: "create",
        path: out.to_path_buf(),
        source: e,
    })?;
    on_best(id, |h, e, i| {
        let platform = platform_id(h, platform);
        units
            .iter()
            .map(|(kind, name)| Ok((*kind, e.export_unit(i, &platform, game, *kind, name, out)?)))
            .collect()
    })
}

/// One unit put back from the file `from`, into the first of `kinds` with a place for it.
pub fn import_unit(
    id: &str,
    platform: &str,
    game: Option<&Path>,
    kinds: &[hermir::SaveKind],
    name: &str,
    from: &Path,
    others: &[String],
) -> hermir::Result<hermir::PrepareStep> {
    on_best(id, |h, e, i| {
        e.import_unit(
            i,
            &platform_id(h, platform),
            game,
            kinds,
            name,
            from,
            others,
        )
    })
}

/// A game's update or DLC files, installed the way the emulator takes them.
pub fn install_content(
    id: &str,
    platform: &str,
    kind: &str,
    files: &[PathBuf],
) -> hermir::Result<Vec<hermir::ContentStep>> {
    on_best(id, |h, e, i| {
        e.install_content(i, &platform_id(h, platform), kind, files)
    })
}

/// Whether the best copy has `platform`'s firmware; `None` when the platform needs none.
pub fn firmware_status(id: &str, platform: &str) -> hermir::Result<Option<hermir::FirmwareStatus>> {
    on_best(id, |h, e, i| {
        e.firmware_status(i, &platform_id(h, platform))
    })
}

/// The command that starts `file` in the best copy of `id`, fullscreen.
pub fn launch_spec(
    id: &str,
    platform: &str,
    file: &Path,
    core: Option<&str>,
) -> hermir::Result<hermir::LaunchSpec> {
    on_best(id, |h, e, i| {
        e.launch(
            i,
            &hermir::LaunchRequest {
                file: Some(file.to_path_buf()),
                platform: Some(platform_id(h, platform)),
                fullscreen: Some(true),
                core: core.map(str::to_string),
                ..Default::default()
            },
        )
    })
}

/// Points hermir at the operator's own copy of `id`; `keep: false` forgets it.
pub fn adopt(id: &str, exe: &Path, keep: bool) -> hermir::Result<Option<hermir::Install>> {
    open()?.emulator(id)?.adopt(exe, keep)
}

/// The folder whose read grant lets a plugin move emulator saves: hermir's own backups of
/// the saves it replaced. The grant is the operator's one yes for every emulator.
pub fn saves_grant() -> PathBuf {
    prefix().join(".save-backups")
}

/// Whether `id` names an emulator in hermir's catalog. Read once: the catalog is compiled in.
pub fn in_catalog(id: &str) -> bool {
    static IDS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    IDS.get_or_init(|| {
        hermir::Catalog::embedded()
            .map(|c| c.entries().iter().map(|e| e.id.clone()).collect())
            .unwrap_or_default()
    })
    .iter()
    .any(|known| known == id)
}

/// Before an `emulator` entry starts: every copy past its first-run questions, the platform's
/// firmware from the plugin's `firmware/<platform>` when it staged some, the session's pads.
/// What it did goes to the log; a launch never waits on it to succeed.
pub fn prepare_launch(library_id: &str) {
    let Some(entry) = crate::library::entry_for_library_id(library_id) else {
        return;
    };
    let Some(spec) = entry.launch.as_ref().filter(|s| s.kind == "emulator") else {
        return;
    };
    let platform = spec
        .args
        .iter()
        .flatten()
        .find(|a| a.name == "platform")
        .map(|a| a.value.as_str());
    let staged = entry
        .provider
        .as_deref()
        .zip(platform)
        .and_then(|(provider, platform)| staged_firmware(provider, platform));
    match prepare(&spec.value, platform, staged.as_deref()) {
        Ok(copies) => {
            for (exe, prepared) in copies {
                for s in prepared.steps {
                    tracing::info!(emulator = %spec.value, %exe, kind = %s.kind, target = %s.target.display(), outcome = ?s.outcome, note = s.note.as_deref().unwrap_or(""), "emulator prepared for the launch");
                }
            }
        }
        Err(e) => {
            tracing::warn!(emulator = %spec.value, error = %e, "emulator not prepared for the launch")
        }
    }
}

/// `<plugin-state>/<provider>/firmware/<platform>` when it is a folder inside that plugin's own
/// state: a link out of it would hand the emulator files the plugin could never write.
fn staged_firmware(provider: &str, platform: &str) -> Option<PathBuf> {
    let own = pf_paths::config_dir()
        .join("plugin-state")
        .join(provider)
        .canonicalize()
        .ok()?;
    let dir = own.join("firmware").join(platform).canonicalize().ok()?;
    (dir.is_dir() && dir.starts_with(&own)).then_some(dir)
}

/// A core name as the buildbot spells it: `snes9x`, `mupen64plus_next`.
pub fn valid_core(core: &str) -> bool {
    !core.is_empty()
        && core.len() <= 64
        && core.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The grant a plugin's request asks for: read-only, on the emulator's folder.
pub fn grant_target(id: &str) -> (PathBuf, bool) {
    (home_of(id), false)
}

/// True when `path` is an emulator home under the prefix.
pub fn is_home(path: &Path) -> bool {
    path.starts_with(prefix())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_pads_come_in_event_order_and_nothing_else_does() {
        let text = "I: Bus=0003 Vendor=045e Product=028e Version=0110\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs=/devices/virtual/input/input30\nH: Handlers=event12 js1\n\n\
I: Bus=0003 Vendor=045e Product=028e Version=0110\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs=/devices/virtual/input/input29\nH: Handlers=event11 js0\n\n\
I: Bus=0003 Vendor=045e Product=028e Version=0114\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs=/devices/pci0000:00/0000:00:14.0/usb1/1-3/input/input8\nH: Handlers=event4 js2\n\n\
I: Bus=0011 Vendor=0001 Product=0001 Version=ab41\nN: Name=\"AT Translated Set 2 keyboard\"\nS: Sysfs=/devices/platform/i8042/serio0/input/input1\nH: Handlers=kbd event1\n";
        let pads = virtual_pads(text);
        assert_eq!(pads.len(), 2);
        assert_eq!(pads[0].index, 0);
        assert_eq!(
            pads[0].evdev.as_deref(),
            Some(Path::new("/dev/input/event11"))
        );
        assert_eq!(
            pads[1].evdev.as_deref(),
            Some(Path::new("/dev/input/event12"))
        );
        assert_eq!(pads[0].sdl_guid(true), "030081b85e0400008e02000010010000");
        assert!(virtual_pads("").is_empty());
    }

    #[test]
    fn a_core_counts_as_present_only_under_the_name_retroarch_loads() {
        let dir = tempfile::tempdir().unwrap();
        let ext = std::env::consts::DLL_EXTENSION;
        std::fs::write(dir.path().join(format!("snes9x_libretro.{ext}")), b"").unwrap();
        std::fs::write(dir.path().join("mgba_libretro.zip"), b"").unwrap();
        assert!(has_core(dir.path(), "snes9x"));
        assert!(!has_core(dir.path(), "mgba"));
        assert!(!has_core(dir.path(), "ppsspp"));
    }
}
