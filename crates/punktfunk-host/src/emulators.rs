//! Managed emulators: hermir, opened on this host's prefix. A plugin only asks; an install runs
//! on the operator's click and lands under `<prefix>/<id>/app`, the folder the plugin
//! is then granted, so its launch templates may point inside it.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

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

/// How long a launch waits for the client's first pad, and for every pad it claimed to be
/// built. A usbip DualSense takes about 0.4 s to enumerate.
const SETTLE_FIRST: Duration = Duration::from_millis(500);
const SETTLE_ALL: Duration = Duration::from_millis(1500);

/// The pads this host made for its players, in the order they appeared: seat 1 is the first.
/// The client's pads land just after the handshake, so this waits for the ones its session
/// claimed (`slots`, one bit each) to show up; Linux reads them off `/proc/bus/input/devices`.
/// A pad claimed but not built yet, or none at all, seats as the pad a session of `kind` makes.
pub fn session_players(kind: GamepadPref, slots: Option<&AtomicU16>) -> Vec<hermir::Player> {
    let claimed = || slots.map_or(0, |s| s.load(Ordering::Relaxed).count_ones() as usize);
    let start = Instant::now();
    let mut pads = Vec::new();
    loop {
        #[cfg(target_os = "linux")]
        if let Ok(text) = std::fs::read_to_string("/proc/bus/input/devices") {
            pads = virtual_pads(&text, Path::new("/sys"));
        }
        // Nothing to read the built pads from: the claim is all there is.
        #[cfg(not(target_os = "linux"))]
        let pads_len = claimed();
        #[cfg(target_os = "linux")]
        let pads_len = pads.len();
        let (want, waited) = (claimed(), start.elapsed());
        if slots.is_none()
            || (want > 0 && pads_len >= want)
            || (want == 0 && waited >= SETTLE_FIRST)
            || waited >= SETTLE_ALL
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    while pads.len() < claimed().max(1) {
        let index = u32::try_from(pads.len()).unwrap_or(u32::MAX);
        pads.push(hermir::PadRef {
            index,
            ..first_pad(kind)
        });
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

/// What hidapi reads off a HID pad, which SDL builds the pad's GUID from: the USB device's
/// strings and `bcdDevice` over usbip; for uhid, which has no USB parent, the HID name and 0.
#[derive(Debug, PartialEq)]
struct Hid {
    manufacturer: String,
    product: String,
    release: u16,
}

/// SDL's GUID for a pad its HIDAPI driver drives: USB bus, a CRC, the USB ids, hidapi's
/// release, then `h`. A Sony pad's CRC is of the name SDL renames it to; a Switch Pro is
/// renamed, then its GUID re-derived from hidapi's strings.
fn hidapi_guid(vendor: u16, product: u16, hid: &Hid) -> Option<String> {
    let name = sdl_name(vendor, product)?;
    let crc = match (vendor, hid.manufacturer.as_str()) {
        (0x054c, _) => crc16(name.as_bytes()),
        (_, "") => crc16(hid.product.as_bytes()),
        (_, m) => crc16(format!("{m} {}", hid.product).as_bytes()),
    };
    let words = [3, crc, vendor, 0, product, 0, hid.release];
    let mut guid: String = words
        .iter()
        .map(|w| format!("{:02x}{:02x}", w & 0xff, w >> 8))
        .collect();
    guid.push_str("6800");
    Some(guid)
}

/// CRC-16/ARC, what `SDL_crc16` computes.
fn crc16(data: &[u8]) -> u16 {
    data.iter().fold(0u16, |crc, &b| {
        (0..8).fold(crc ^ u16::from(b), |c, _| {
            if c & 1 == 1 {
                (c >> 1) ^ 0xA001
            } else {
                c >> 1
            }
        })
    })
}

/// What hidapi reads for the HID device under the input node at `sysfs`, `sys` being `/sys`:
/// `None` for a pad with no HID device, a uinput one SDL reads through evdev.
fn hid_of(sys: &Path, sysfs: &str) -> Option<Hid> {
    // <hid>/input/inputN; a usbip pad's USB device is two up from <hid>: interface, device.
    let hid = sys
        .join(sysfs.trim_start_matches('/'))
        .parent()?
        .parent()?
        .to_path_buf();
    let uevent = std::fs::read_to_string(hid.join("uevent")).ok()?;
    let name = uevent.lines().find_map(|l| l.strip_prefix("HID_NAME="))?;
    let usb = hid.parent()?.parent()?;
    let attr = |a: &str| {
        std::fs::read_to_string(usb.join(a))
            .map(|s| s.trim_end().to_string())
            .ok()
    };
    Some(match attr("bcdDevice") {
        Some(bcd) => Hid {
            manufacturer: attr("manufacturer").unwrap_or_default(),
            product: attr("product").unwrap_or_default(),
            release: u16::from_str_radix(&bcd, 16).unwrap_or(0),
        },
        None => Hid {
            manufacturer: String::new(),
            product: name.to_string(),
            release: 0,
        },
    })
}

/// The pad a session of `kind` makes first, as the kernel, hidapi and SDL name it. The kinds
/// without a row seat as an Xbox 360 pad.
fn first_pad(kind: GamepadPref) -> hermir::PadRef {
    #[cfg(target_os = "linux")]
    let usbip = pf_inject::dualsense_usbip::usbip_preferred();
    #[cfg(not(target_os = "linux"))]
    let usbip = false;
    let uhid = |name: &str| Hid {
        manufacturer: String::new(),
        product: name.into(),
        release: 0,
    };
    // hid-playstation sets 0x8000 on the version of the devices it drives.
    let (vendor, product, version, name, hid) = match kind {
        GamepadPref::XboxOne => (0x045e, 0x02ea, 0x0408, "Microsoft X-Box One S pad", None),
        GamepadPref::XboxElite => (
            0x045e,
            0x0b00,
            0x0511,
            "Microsoft X-Box One Elite 2 pad",
            None,
        ),
        GamepadPref::DualSense if usbip => (
            0x054c,
            0x0ce6,
            0x8111,
            "Sony Interactive Entertainment DualSense Wireless Controller",
            Some(Hid {
                manufacturer: "Sony Interactive Entertainment".into(),
                product: "DualSense Wireless Controller".into(),
                release: 0x0100,
            }),
        ),
        GamepadPref::DualSense => {
            let n = "Punktfunk DualSense 0";
            (0x054c, 0x0ce6, 0x8100, n, Some(uhid(n)))
        }
        GamepadPref::DualSenseEdge => {
            let n = "Punktfunk DualSense Edge 0";
            (0x054c, 0x0df2, 0x8100, n, Some(uhid(n)))
        }
        GamepadPref::DualShock4 => {
            let n = "Punktfunk DualShock 4 0";
            (0x054c, 0x09cc, 0x8100, n, Some(uhid(n)))
        }
        GamepadPref::SwitchPro => (
            0x057e,
            0x2009,
            0x0200,
            "Nintendo Switch Pro Controller",
            Some(uhid("Punktfunk Switch Pro Controller 0")),
        ),
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
        guid: hid.and_then(|h| hidapi_guid(vendor, product, &h)),
        gamepad_name: sdl_name(vendor, product).map(Into::into),
    }
}

/// The host's pads in a `/proc/bus/input/devices` listing, in event-node order: a virtual or
/// usbip-attached device of a pad vendor with a joystick node. A pad's motion sensors get one
/// too; the accelerometer property (bit 6) tells them apart.
/// `sys` is `/sys`, where each HID pad's hidapi strings are read.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn virtual_pads(text: &str, sys: &Path) -> Vec<hermir::PadRef> {
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
        let sysfs = field("S: Sysfs=").unwrap_or("");
        let ours = sysfs.starts_with("/devices/virtual/") || sysfs.contains("/vhci_hcd.");
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
                guid: hid_of(sys, sysfs).and_then(|h| hidapi_guid(vendor, product, &h)),
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
/// in seat order (steps of kind `players`; reverted after the game): see [`session_players`]
/// for `pad` and `slots`. `platform` is a catalog id or any of its aliases (RomM slug, ES-DE
/// folder, libretro name). Blocking: an installer may run.
pub fn prepare(
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
    pad: GamepadPref,
    slots: Option<&AtomicU16>,
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
        players: Some(session_players(pad, slots)),
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
/// What it did goes to the log. A launch waits for the client's pads, at most [`SETTLE_ALL`],
/// and never on the rest to succeed.
pub fn prepare_launch(library_id: &str, pad: GamepadPref, slots: Option<&AtomicU16>) {
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
    match prepare(&spec.value, platform, staged.as_deref(), pad, slots) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_pads_come_in_event_order_and_nothing_else_does() {
        let text = "I: Bus=0003 Vendor=045e Product=028e Version=0110\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs=/devices/virtual/input/input30\nH: Handlers=event12 js1\n\n\
I: Bus=0003 Vendor=045e Product=028e Version=0110\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs=/devices/virtual/input/input29\nH: Handlers=event11 js0\n\n\
I: Bus=0003 Vendor=045e Product=028e Version=0114\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs=/devices/pci0000:00/0000:00:14.0/usb1/1-3/input/input8\nH: Handlers=event4 js2\n\n\
I: Bus=0011 Vendor=0001 Product=0001 Version=ab41\nN: Name=\"AT Translated Set 2 keyboard\"\nS: Sysfs=/devices/platform/i8042/serio0/input/input1\nH: Handlers=kbd event1\n";
        let pads = virtual_pads(text, Path::new("/nonexistent"));
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
        assert!(virtual_pads("", Path::new("/nonexistent")).is_empty());
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
        let sys = tempfile::tempdir().unwrap();
        let usb = sys.path().join("devices/platform/vhci_hcd.0/usb9/9-1");
        let hid = usb.join("9-1:1.3/0003:054C:0CE6.0015");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::write(
            hid.join("uevent"),
            "HID_NAME=Sony Interactive Entertainment DualSense Wireless Controller\n",
        )
        .unwrap();
        std::fs::write(usb.join("bcdDevice"), "0100\n").unwrap();
        std::fs::write(usb.join("manufacturer"), "Sony Interactive Entertainment\n").unwrap();
        std::fs::write(usb.join("product"), "DualSense Wireless Controller\n").unwrap();
        let pads = virtual_pads(&text, sys.path());
        assert_eq!(pads.len(), 1, "{pads:?}");
        assert_eq!(
            pads[0].evdev.as_deref(),
            Some(Path::new("/dev/input/event10"))
        );
        // RPCS3 binds `<SDL name> <n>`; the GUID is what SDL 3.4 reported for this pad.
        assert_eq!(pads[0].sdl_name(), "DualSense Wireless Controller");
        assert_eq!(pads[0].sdl_guid(true), "030057564c050000e60c000000016800");
        assert_eq!(pads[0].sdl_guid(false), "030000004c050000e60c000000016800");
    }

    #[test]
    fn a_uhid_pad_has_no_usb_parent_so_hidapi_reads_its_hid_name_and_release_0() {
        let sys = tempfile::tempdir().unwrap();
        let hid = sys
            .path()
            .join("devices/virtual/misc/uhid/0003:057E:2009.0009");
        std::fs::create_dir_all(&hid).unwrap();
        std::fs::write(
            hid.join("uevent"),
            "HID_NAME=Punktfunk Switch Pro Controller 0\n",
        )
        .unwrap();
        let got = hid_of(
            sys.path(),
            "/devices/virtual/misc/uhid/0003:057E:2009.0009/input/input9",
        );
        let want = Hid {
            manufacturer: String::new(),
            product: "Punktfunk Switch Pro Controller 0".into(),
            release: 0,
        };
        assert_eq!(got.as_ref(), Some(&want));
        // A Switch Pro's CRC is of hidapi's product string, not of SDL's name for it.
        let crc = crc16(b"Punktfunk Switch Pro Controller 0");
        let guid = hidapi_guid(0x057e, 0x2009, &want).unwrap();
        assert_eq!(&guid[4..8], format!("{:02x}{:02x}", crc & 0xff, crc >> 8));
        assert_eq!(first_pad(GamepadPref::SwitchPro).guid, Some(guid));
        assert_eq!(hid_of(sys.path(), "/devices/virtual/input/input3"), None);
    }

    #[test]
    fn every_pad_the_session_claimed_gets_a_seat() {
        // No pad is built on a test box: both seats are the session's kind, in claim order.
        let seats = session_players(GamepadPref::DualSense, Some(&AtomicU16::new(0b101)));
        let got: Vec<_> = seats.iter().map(|p| (p.seat, p.pad.index)).collect();
        assert_eq!(got, [(1, 0), (2, 1)]);
        assert_eq!(seats[1].pad.sdl_name(), "DualSense Wireless Controller");
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
