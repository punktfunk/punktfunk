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
/// session's pads is gone again, file by file. Nothing outstanding is a quiet no-op.
pub fn revert_players() {
    let Ok(h) = open() else { return };
    revert_with(&h);
}

fn revert_with(h: &Hermir) {
    for (id, steps) in h.revert_all_players() {
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
    let players = session_players();
    Ok(emulator
        .copies()?
        .iter()
        .map(|copy| {
            let mut prepared = emulator.prepare(copy, platform, &firmware);
            prepared
                .steps
                .extend(emulator.apply_players(copy, &players).steps);
            (copy.exe.to_string(), prepared)
        })
        .collect())
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
