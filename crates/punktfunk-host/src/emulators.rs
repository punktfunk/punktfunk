//! Managed emulators: hermir, opened on this host's prefix. A plugin only asks; an install runs
//! on the operator's click and lands under `<prefix>/<id>/app`, the folder the plugin
//! is then granted, so its launch templates may point inside it.
use std::path::{Path, PathBuf};

use hermir::progress::Quiet;
use hermir::{Hermir, Installed, Options};
use punktfunk_core::config::GamepadPref;

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
        #[cfg(windows)]
        runner: Box::new(AsPlayer),
        ..Options::default()
    })
}

/// Runs an emulator's own installer (RPCS3's firmware, a `.pkg`) as the player signed in to the
/// host's session. Every player can write the emulator's folder, so SYSTEM never starts what is
/// in it. The exit code is all that comes back.
#[cfg(windows)]
struct AsPlayer;

#[cfg(windows)]
impl hermir::Runner for AsPlayer {
    fn run(&self, program: &str, args: &[&str]) -> hermir::Result<hermir::Output> {
        /// Past this the installer keeps running on its own, and the step reads its marker.
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(4 * 3600);
        let cmdline = std::iter::once(program)
            .chain(args.iter().copied())
            .map(crate::library::win_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let code = crate::interactive::run_hidden_as_current_session_user(&cmdline, LIMIT)
            .map_err(|e| hermir::Error::Place {
                what: program.into(),
                why: format!("{e:#}"),
            })?;
        Ok(hermir::Output {
            ok: code == 0,
            stdout: String::new(),
            stderr: if code == 0 {
                String::new()
            } else {
                format!("exited with code {code}")
            },
        })
    }
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
/// Linux reads them off `/proc/bus/input/devices`. With none up yet (the client's pad frames
/// arrive after the launch), seat 1 is the first pad a session of `kind` makes, at index 0.
pub fn session_players(kind: GamepadPref) -> Vec<hermir::Player> {
    let mut pads = Vec::new();
    #[cfg(target_os = "linux")]
    if let Ok(text) = std::fs::read_to_string("/proc/bus/input/devices") {
        pads = virtual_pads(&text);
    }
    if pads.is_empty() {
        pads.push(first_pad(kind));
    }
    pads.into_iter()
        .zip(1u8..)
        .map(|(pad, seat)| hermir::Player { seat, pad })
        .collect()
}

/// Microsoft, Sony and Nintendo: the identities the host's uinput, uhid and usbip pads carry.
const PAD_VENDORS: [u16; 3] = [0x045e, 0x054c, 0x057e];

/// SDL 3's name for a pad its HIDAPI driver renames by USB id. `None` for the Xbox pads, which
/// hermir names itself, and anything else.
fn sdl_name(vendor: u16, product: u16) -> Option<&'static str> {
    Some(match (vendor, product) {
        (0x054c, 0x0ce6) => "DualSense Wireless Controller",
        (0x054c, 0x0df2) => "DualSense Edge Wireless Controller",
        (0x054c, 0x09cc) => "PS4 Controller",
        (0x057e, 0x2009) => "Nintendo Switch Pro Controller",
        _ => return None,
    })
}

/// The pad a session of `kind` makes first, as the kernel and SDL name it. The kinds without
/// a row seat as an Xbox 360 pad.
fn first_pad(kind: GamepadPref) -> hermir::PadRef {
    #[cfg(target_os = "linux")]
    let usbip = pf_inject::dualsense_usbip::usbip_preferred();
    #[cfg(not(target_os = "linux"))]
    let usbip = false;
    // hid-playstation sets 0x8000 on the version of the devices it drives.
    let (vendor, product, version, name) = match kind {
        GamepadPref::XboxOne => (0x045e, 0x02ea, 0x0408, "Microsoft X-Box One S pad"),
        GamepadPref::XboxElite => (0x045e, 0x0b00, 0x0511, "Microsoft X-Box One Elite 2 pad"),
        GamepadPref::DualSense if usbip => (
            0x054c,
            0x0ce6,
            0x8111,
            "Sony Interactive Entertainment DualSense Wireless Controller",
        ),
        GamepadPref::DualSense => (0x054c, 0x0ce6, 0x8100, "Punktfunk DualSense 0"),
        GamepadPref::DualSenseEdge => (0x054c, 0x0df2, 0x8100, "Punktfunk DualSense Edge 0"),
        GamepadPref::DualShock4 => (0x054c, 0x09cc, 0x8100, "Punktfunk DualShock 4 0"),
        GamepadPref::SwitchPro => (0x057e, 0x2009, 0x0200, "Nintendo Switch Pro Controller"),
        _ => return hermir::PadRef::xbox360(0),
    };
    hermir::PadRef {
        name: name.into(),
        bus: 3,
        vendor,
        product,
        version,
        index: 0,
        evdev: None,
        guid: None,
        gamepad_name: sdl_name(vendor, product).map(Into::into),
    }
}

/// The host's pads in a `/proc/bus/input/devices` listing, in event-node order: a virtual or
/// usbip-attached device of a pad vendor with a joystick node. A pad's motion sensors get one
/// too; the accelerometer property (bit 6) tells them apart.
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
        let handlers = field("H: Handlers=").unwrap_or("");
        let ours = field("S: Sysfs=")
            .is_some_and(|s| s.starts_with("/devices/virtual/") || s.contains("/vhci_hcd."));
        let sensors = field("B: PROP=")
            .and_then(|p| u64::from_str_radix(p, 16).ok())
            .is_some_and(|p| p & 1 << 6 != 0);
        if !PAD_VENDORS.contains(&vendor)
            || !ours
            || sensors
            || !handlers.split_whitespace().any(|h| h.starts_with("js"))
        {
            continue;
        }
        let name = field("N: Name=")
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        let event = handlers
            .split_whitespace()
            .find_map(|w| w.strip_prefix("event")?.parse::<u32>().ok())
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
                gamepad_name: sdl_name(vendor, product).map(Into::into),
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
/// in seat order (steps of kind `players`; reverted after the game), `pad` the kind they are.
/// `platform` is a catalog id or any of its aliases (RomM slug, ES-DE folder, libretro name).
/// Blocking: an installer may run.
pub fn prepare(
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
    pad: GamepadPref,
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
        players: Some(session_players(pad)),
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
/// firmware from the plugin's `firmware/<platform>` when it staged some, the session's pads
/// (`pad` the kind they are). What it did goes to the log; a launch never waits on it to succeed.
pub fn prepare_launch(library_id: &str, pad: GamepadPref) {
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
    match prepare(&spec.value, platform, staged.as_deref(), pad) {
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
    fn a_usbip_dualsense_is_one_pad_under_sdls_name() {
        // Read off a host with the usbip DualSense up: the pad, then its sensors, touchpad, jack.
        let dev =
            "S: Sysfs=/devices/platform/vhci_hcd.0/usb9/9-1/9-1:1.3/0003:054C:0CE6.0015/input";
        let ds = "I: Bus=0003 Vendor=054c Product=0ce6 Version=8111\nN: Name=\"Sony Interactive Entertainment DualSense Wireless Controller";
        let text = format!(
            "{ds}\"\n{dev}/input78\nH: Handlers=event10 js1 \nB: PROP=0\n\n\
             {ds} Motion Sensors\"\n{dev}/input79\nH: Handlers=event11 js2 \nB: PROP=40\n\n\
             {ds} Touchpad\"\n{dev}/input80\nH: Handlers=event12 mouse3 \nB: PROP=5\n\n\
             {ds} Headset Jack\"\n{dev}/input81\nH: Handlers=event13 \nB: PROP=0\n"
        );
        let pads = virtual_pads(&text);
        assert_eq!(pads.len(), 1, "{pads:?}");
        assert_eq!(
            pads[0].evdev.as_deref(),
            Some(Path::new("/dev/input/event10"))
        );
        // RPCS3 binds `<SDL name> <n>`.
        assert_eq!(pads[0].sdl_name(), "DualSense Wireless Controller");
    }

    #[test]
    fn a_session_with_no_pad_up_yet_seats_the_kind_it_will_make() {
        let seat = |kind| first_pad(kind).sdl_name();
        assert_eq!(
            seat(GamepadPref::DualSense),
            "DualSense Wireless Controller"
        );
        assert_eq!(
            seat(GamepadPref::SwitchPro),
            "Nintendo Switch Pro Controller"
        );
        assert_eq!(seat(GamepadPref::Xbox360), "Xbox 360 Controller");
        assert_eq!(seat(GamepadPref::SteamDeck), "Xbox 360 Controller");
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
