//! Managed emulators: hermir, opened on this host's prefix. A plugin only asks; an install runs
//! on the operator's click and lands under `<prefix>/<id>/app`, the folder the plugin
//! is then granted, so its launch templates may point inside it.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::session_launch::Pads;
use hermir::{Hermir, Installed, Options};
use pf_seats::ipc::{Command, CommandResult};
use punktfunk_core::config::GamepadPref;

/// Where managed emulators live. `%ProgramData%\punktfunk\emulators` on Windows, so the SYSTEM
/// service installs and the player's session runs; a Windows seat reads the box's copies there.
/// The data dir on POSIX: a plugin is granted the emulator's folder, and the runner shares
/// nothing under the config dir.
pub fn prefix() -> PathBuf {
    #[cfg(windows)]
    let base = pf_paths::seat::library_dir();
    #[cfg(not(windows))]
    let base = pf_paths::data_dir();
    base.join("emulators")
}

/// The emulator's own folder: exe, config and saves for a portable one. The path a plugin is
/// granted when its request is allowed, whether the copy lives here or is a Flatpak.
pub fn home_of(id: &str) -> PathBuf {
    prefix().join(id).join("app")
}

pub(crate) mod exchange;

/// hermir on [`prefix`]. With seats on the box a copy serves every account: a Linux Flatpak goes
/// into the system installation, a Windows archive keeps each account's data in its profile. The
/// SYSTEM host on Windows resolves that data in the session player's profile, and keeps what its
/// own `apply` changed under [`state_dir`], so a seat's revert never touches another's bindings.
pub fn open() -> hermir::Result<Hermir> {
    open_in(None)
}

/// [`open`], with `account`'s profile folder as hermir's home on Windows: a seat account's copies
/// rather than the session player's. Elsewhere `account` is never given.
fn open_in(account: Option<&Path>) -> hermir::Result<Hermir> {
    #[cfg(not(windows))]
    let _ = account;
    Hermir::open(Options {
        prefix: Some(prefix()),
        #[cfg(windows)]
        runner: Box::new(AsPlayer),
        #[cfg(windows)]
        shared_install: pf_paths::seat::role().is_seat_host() || crate::seats::enabled(),
        #[cfg(windows)]
        home: account.map(Path::to_path_buf).or_else(session_home),
        flatpak_installation: if box_wide() {
            hermir::FlatpakInstallation::System
        } else {
            hermir::FlatpakInstallation::User
        },
        state_dir: state_dir(),
        ..Options::default()
    })
}

/// A Linux host on a box with seats, the owner's row included: its Flatpaks are the box's.
fn box_wide() -> bool {
    cfg!(target_os = "linux") && pf_paths::seat::role().is_seat_host()
}

/// Where this host keeps hermir's snapshots, save backups and their lock. On Windows every host
/// opens one shared prefix, so each keeps its own under its config dir; a POSIX prefix is one
/// user's already.
fn state_dir() -> Option<PathBuf> {
    cfg!(windows).then(|| pf_paths::config_dir().join("emulator-state"))
}

/// The profile folder of the player signed in to this host's session, as the machine records it
/// ([`crate::windows::theme::session_profile`]); `None` with nobody signed in, or on a host that
/// isn't SYSTEM.
#[cfg(windows)]
fn session_home() -> Option<PathBuf> {
    crate::windows::theme::session_profile()
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
/// directory to land on even when the copy itself is a Flatpak. On a Linux box with seats a
/// Flatpak goes in box-wide ([`install_box_wide`]); an AppImage or archive stays this account's.
/// Players may write in [`home_of`] only, set before hermir writes its manifests beside it.
/// `progress` hears a download; a box-wide Flatpak reports none.
pub fn install(id: &str, progress: &dyn hermir::progress::Progress) -> hermir::Result<Installed> {
    let h = open()?;
    let emulator = h.emulator(id)?;
    let home = home_of(id);
    crate::plugins::open_for_players(&home);
    let row = match box_flatpak(emulator.entry()).filter(|_| box_wide()) {
        Some(app) => install_box_wide(&h, id, app)?,
        None => emulator.install(progress)?,
    };
    std::fs::create_dir_all(&home).map_err(|e| hermir::Error::Io {
        op: "create",
        path: home.clone(),
        source: e,
    })?;
    crate::plugins::open_for_players(&home);
    Ok(row)
}

/// Removes the managed copy where its row says it went: a box-wide Flatpak through the seat
/// supervisor, which purges root's data of it and never a person's.
pub fn remove(id: &str, purge: bool) -> hermir::Result<()> {
    let h = open()?;
    let emulator = h.emulator(id)?;
    match emulator.managed()? {
        Some(Installed {
            exe: hermir::Exe::FlatpakRun(app),
            installation: Some(hermir::FlatpakInstallation::System),
            ..
        }) => {
            supervisor(id, Command::RemoveFlatpak { id: app, purge })?;
            let _lock = h.store().lock()?;
            h.store().forget(id)
        }
        _ => emulator.remove(purge),
    }
}

/// The Flathub app id of `entry`'s Linux channel; `None` for an AppImage or archive.
fn box_flatpak(entry: &hermir::Entry) -> Option<&str> {
    match entry.channels.get(hermir::Os::Linux) {
        Some(hermir::Channel::Flatpak { flatpak }) => Some(flatpak),
        _ => None,
    }
}

/// `app` installed into the box's Flatpak installation by the seat supervisor, which runs as
/// root; every seat sees it and keeps its own data in `~/.var/app`. A Flatpak of this account's
/// own goes, its data stays, so `flatpak run` never picks the stale copy.
fn install_box_wide(h: &Hermir, id: &str, app: &str) -> hermir::Result<Installed> {
    let version = box_wide_install(id, app)?;
    let emulator = h.emulator(id)?;
    let own = emulator.managed()?;
    if own.is_some_and(|r| r.installation != Some(hermir::FlatpakInstallation::System))
        && let Err(error) = emulator.remove(false)
    {
        tracing::warn!(emulator = %id, %error, "own Flatpak not removed beside the box-wide one");
    }
    let row = Installed {
        emulator: id.into(),
        channel: "flatpak".into(),
        exe: hermir::Exe::FlatpakRun(app.into()),
        version,
        release: Some(emulator.resolve()?.release),
        sha256: None,
        verified: hermir::Verified::Flatpak,
        kept: Vec::new(),
        installation: Some(hermir::FlatpakInstallation::System),
        shared: false,
        installed_at: jiff::Timestamp::now().to_string(),
    };
    let _lock = h.store().lock()?;
    h.store().record(row.clone())?;
    Ok(row)
}

/// `app` installed box-wide, or updated when it is there; the version Flatpak reports after.
fn box_wide_install(id: &str, app: &str) -> hermir::Result<Option<String>> {
    match supervisor(id, Command::InstallFlatpak { id: app.into() })? {
        CommandResult::Flatpak { version, .. } => Ok(version),
        other => Err(place(id, format!("the seat supervisor answered {other:?}"))),
    }
}

/// A day: these emulators release often, and a check costs one lookup per copy.
const UPDATE_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The first pass waits out the host's start; a pass that finds someone playing waits this long.
const UPDATE_WAIT: Duration = Duration::from_secs(15 * 60);

/// Held by an update and by a launch's prepare: a launch never starts a copy being replaced.
static UPDATING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn updating() -> std::sync::MutexGuard<'static, ()> {
    UPDATING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keeps every managed copy on its channel's newest release: a pass a day, never while a stream
/// or a desk game runs.
pub fn keep_updated() {
    let spawned = std::thread::Builder::new()
        .name("emulator-updates".into())
        .spawn(|| loop {
            std::thread::sleep(UPDATE_WAIT);
            if crate::session_status::count() > 0 || !crate::desk::snapshot().is_empty() {
                continue;
            }
            update_all();
            std::thread::sleep(UPDATE_EVERY);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "emulator update thread did not start");
    }
}

/// One pass over the managed copies. A box-wide Flatpak updates through the seat supervisor, any
/// other copy through hermir. An AppImage or archive waits while a process runs from its folder,
/// whose files the update replaces; a Flatpak updates beside a running copy.
fn update_all() {
    let h = match open() {
        Ok(h) => h,
        Err(error) => return tracing::warn!(%error, "emulators not checked for updates"),
    };
    for entry in h.catalog().entries() {
        let id = entry.id.as_str();
        let Ok(emulator) = h.emulator(id) else {
            continue;
        };
        let row = match emulator.managed() {
            Ok(Some(row)) => row,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(emulator = %id, %error, "emulator not checked for an update");
                continue;
            }
        };
        let _held = updating();
        let updated = match &row.exe {
            hermir::Exe::FlatpakRun(app)
                if row.installation == Some(hermir::FlatpakInstallation::System) =>
            {
                update_box_wide(&h, id, app, &row)
            }
            hermir::Exe::FlatpakRun(_) => emulator.update(&hermir::progress::Quiet),
            _ if in_use(id) => {
                tracing::info!(emulator = %id, "emulator update waits: a copy is running");
                continue;
            }
            _ => emulator.update(&hermir::progress::Quiet),
        };
        let was = row.version.as_deref().unwrap_or("?");
        match updated {
            Ok(Some(new)) => {
                let now = new.version.as_deref().unwrap_or("?");
                tracing::info!(emulator = %id, from = was, to = now, "emulator updated");
            }
            Ok(None) => tracing::debug!(emulator = %id, version = was, "emulator up to date"),
            Err(error) => tracing::warn!(emulator = %id, %error, "emulator not updated"),
        }
    }
}

/// A box-wide Flatpak's update, recorded when the version Flatpak reports moved.
fn update_box_wide(
    h: &Hermir,
    id: &str,
    app: &str,
    row: &Installed,
) -> hermir::Result<Option<Installed>> {
    let version = box_wide_install(id, app)?;
    if version == row.version {
        return Ok(None);
    }
    let fresh = Installed {
        version,
        installed_at: jiff::Timestamp::now().to_string(),
        ..row.clone()
    };
    let _lock = h.store().lock()?;
    h.store().record(fresh.clone())?;
    Ok(Some(fresh))
}

/// Whether a process this host can see runs from `id`'s folder.
fn in_use(id: &str) -> bool {
    let spec = crate::library::DetectSpec {
        install_dir: Some(home_of(id)),
        ..Default::default()
    };
    !crate::procscan::Scanner::system()
        .find(&spec, None)
        .is_empty()
}

/// One Flatpak change by the seat supervisor; its refusal is `id`'s, in its words.
fn supervisor(id: &str, command: Command) -> hermir::Result<CommandResult> {
    crate::seats::call(command).map_err(|e| place(id, e.message))
}

fn place(id: &str, why: String) -> hermir::Error {
    hermir::Error::Place {
        what: id.into(),
        why,
    }
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

/// A libretro core from the buildbot into that folder; `progress` hears its download.
pub fn install_core(
    core: &str,
    progress: &dyn hermir::progress::Progress,
) -> hermir::Result<PathBuf> {
    open()?.install_core(core, progress)
}

/// Whether RetroArch's cores folder `dir` already holds `core`, under the name hermir gives it.
pub fn has_core(dir: &Path, core: &str) -> bool {
    core_file(dir, core).is_file()
}

fn core_file(dir: &Path, core: &str) -> PathBuf {
    dir.join(format!(
        "{core}_libretro.{}",
        std::env::consts::DLL_EXTENSION
    ))
}

/// A core moved in from an admin-only folder keeps that folder's ACL, and the player's RetroArch
/// can't load it. The cores folder is the player's, so the core gets the player's read and
/// execute ACE through one handle that refuses a link on the way, never a copy by path.
#[cfg(windows)]
fn open_core(dir: &Path, core: &str) {
    use pf_paths_win::player::{grant_on_file, READ_EXECUTE};
    let path = core_file(dir, core);
    if !path.is_file() || !pf_paths_win::admin_only_acl(&path) {
        return;
    }
    let Some(sid) = crate::windows::theme::session_sid() else {
        return;
    };
    match grant_on_file(&path, &sid, READ_EXECUTE) {
        Ok(()) => tracing::debug!(core, path = %path.display(), "core opened to the player"),
        Err(error) => tracing::warn!(core, %error, "core left unreadable to the player"),
    }
}

/// How long a launch waits for the client's first pad, which it declares just after the
/// handshake, and for the rest of that burst to stop changing; never past [`SETTLE_ALL`].
const SETTLE_FIRST: Duration = Duration::from_millis(500);
const SETTLE_QUIET: Duration = Duration::from_millis(150);
const SETTLE_ALL: Duration = Duration::from_millis(1500);

/// A stream's players: the pads its `roster` holds, in wire order, each as the device its kind
/// builds, so the bindings name a pad before its first frame builds it. A session that has
/// declared none by then seats the pads its `device` brought last time ([`remember_pads`]): a
/// controller switched on after the launch is most often that one. Else one pad of `kind`.
pub fn session_players(
    kind: GamepadPref,
    roster: Option<&crate::pad_feed::PadFeed>,
    device: Option<&str>,
) -> Vec<hermir::Player> {
    let read = || {
        roster
            .map(crate::pad_feed::PadFeed::roster)
            .unwrap_or_default()
    };
    let (start, mut pads) = (Instant::now(), read());
    let mut quiet = Instant::now();
    while roster.is_some() && start.elapsed() < SETTLE_ALL {
        let settled = if pads.is_empty() {
            start.elapsed() >= SETTLE_FIRST
        } else {
            quiet.elapsed() >= SETTLE_QUIET
        };
        if settled {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
        let now = read();
        if now != pads {
            (pads, quiet) = (now, Instant::now());
        }
    }
    let mut kinds: Vec<_> = pads.iter().map(|p| (p.kind, p.slot)).collect();
    if kinds.is_empty() {
        let last = device.map(remembered_pads).unwrap_or_default();
        kinds = last.into_iter().map(|k| (k, None)).collect();
    }
    if kinds.is_empty() {
        kinds.push((kind, None));
    }
    kinds
        .into_iter()
        .zip(0u8..)
        .map(|((kind, slot), n)| hermir::Player {
            seat: n + 1,
            pad: hermir::PadRef {
                index: u32::from(n),
                // A pad not built yet has no slot: its place stands in. Only a uhid name
                // carries the slot, and only a GUID that keeps the name's CRC reads it.
                ..pad_ref(kind, slot.unwrap_or(n))
            },
        })
        .collect()
}

/// Each device's last pads, most recent device first, under the config dir.
const PAD_MEMORY: &str = "pad-kinds.json";
/// Devices kept; the least recent drops.
const PAD_MEMORY_DEVICES: usize = 64;

/// One device's row of [`PAD_MEMORY`]: its pairing fingerprint and its pads' kinds, in wire order.
#[derive(serde::Serialize, serde::Deserialize)]
struct DevicePads {
    device: String,
    kinds: Vec<String>,
}

/// Keep the kinds `device`'s pads were this session, for a launch whose pad is still off. A
/// session with no pad keeps the last.
pub fn remember_pads(device: &str, kinds: &[GamepadPref]) {
    static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = WRITE.lock();
    let path = pf_paths::config_dir().join(PAD_MEMORY);
    if let Err(error) = remember_pads_in(&path, device, kinds) {
        tracing::debug!(%error, "pad memory not written");
    }
}

fn remember_pads_in(path: &Path, device: &str, kinds: &[GamepadPref]) -> std::io::Result<()> {
    if kinds.is_empty() {
        return Ok(());
    }
    let mut all = pad_memory(path);
    all.retain(|d| d.device != device);
    let kinds = kinds.iter().map(|k| k.as_str().to_string()).collect();
    all.insert(
        0,
        DevicePads {
            device: device.into(),
            kinds,
        },
    );
    all.truncate(PAD_MEMORY_DEVICES);
    pf_paths::replace_file(path, &serde_json::to_vec(&all)?)
}

fn pad_memory(path: &Path) -> Vec<DevicePads> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The kinds [`remember_pads`] kept for `device`.
fn remembered_pads(device: &str) -> Vec<GamepadPref> {
    remembered_pads_in(&pf_paths::config_dir().join(PAD_MEMORY), device)
}

fn remembered_pads_in(path: &Path, device: &str) -> Vec<GamepadPref> {
    let row = pad_memory(path).into_iter().find(|d| d.device == device);
    row.map_or_else(Vec::new, |d| {
        d.kinds
            .iter()
            .filter_map(|k| GamepadPref::from_name(k))
            .collect()
    })
}

/// Microsoft, Sony and Nintendo: the identities the host's uinput, uhid and usbip pads carry.
const PAD_VENDORS: [u16; 3] = [0x045e, 0x054c, 0x057e];
/// The host's other pads, by id: Steam's own virtual pad (`28DE:11FF`) is a desk pad.
const HOST_MADE: [(u16, u16); 8] = [
    (0x28de, 0x1205),
    (0x28de, 0x1102),
    (0x28de, 0x1302),
    (0x28de, 0x1304),
    (0x2dc8, 0x6012),
    (0x2dc8, 0x6003),
    (0x2dc8, 0x6009),
    (0x0f0d, 0x01ab),
];

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

/// SDL's bus numbers.
const USB: u16 = 0x03;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const BLUETOOTH: u16 = 0x05;

/// SDL's GUID for a pad its HIDAPI driver drives: the bus, a CRC, the USB ids, hidapi's release,
/// `h`, then the driver's byte. A Sony or Valve pad's CRC is of the name SDL renames it to; any
/// other's, a Switch Pro renamed and re-derived included, is of hidapi's strings. A Switch Pro's
/// last byte is SDL's controller type for it, 3.
fn hidapi_guid(bus: u16, vendor: u16, product: u16, hid: &Hid) -> String {
    let renamed = match (vendor, product) {
        (0x054c, _) => sdl_name(vendor, product),
        (0x28de, 0x1205) => Some("Steam Deck"),
        (0x28de, 0x1102 | 0x1302 | 0x1304) => Some("Steam Controller"),
        _ => None,
    };
    let crc = match (renamed, hid.manufacturer.as_str()) {
        (Some(name), _) => crc16(name.as_bytes()),
        (_, "") => crc16(hid.product.as_bytes()),
        (_, m) => crc16(format!("{m} {}", hid.product).as_bytes()),
    };
    let data: u8 = if (vendor, product) == (0x057e, 0x2009) {
        3
    } else {
        0
    };
    let words = [bus, crc, vendor, 0, product, 0, hid.release];
    let mut guid: String = words
        .iter()
        .map(|w| format!("{:02x}{:02x}", w & 0xff, w >> 8))
        .collect();
    guid.push_str(&format!("68{data:02x}"));
    guid
}

/// [`hidapi_guid`] of a USB pad SDL names itself: the pads Linux builds over uhid and usbip.
/// `None` for any other, which SDL reads through evdev.
fn named_hidapi_guid(vendor: u16, product: u16, hid: &Hid) -> Option<String> {
    sdl_name(vendor, product).map(|_| hidapi_guid(USB, vendor, product, hid))
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

/// The pad a macOS session of `kind` makes first: an `IOHIDUserDevice` of its pad-kind row,
/// which SDL drives through HIDAPI. An Xbox pad is Bluetooth there, the rest USB to SDL; a kind
/// with no macOS pad of its own, Xbox 360 among them, is the Xbox Wireless pad.
#[cfg(target_os = "macos")]
fn pad_ref(kind: GamepadPref, _slot: u8) -> hermir::PadRef {
    use pf_driver_proto::gamepad as g;
    let devtype = match kind {
        GamepadPref::XboxOne => g::DEVTYPE_XBOX_ONE_S,
        GamepadPref::XboxElite => g::DEVTYPE_XBOX_ELITE,
        GamepadPref::DualSense => g::DEVTYPE_DUALSENSE,
        GamepadPref::DualSenseEdge => g::DEVTYPE_DUALSENSE_EDGE,
        GamepadPref::DualShock4 => g::DEVTYPE_DUALSHOCK4,
        GamepadPref::SwitchPro => g::DEVTYPE_SWITCH_PRO,
        _ => g::DEVTYPE_XBOX,
    };
    let row = pf_driver_proto::pad_kind::served_kind(devtype);
    let bus = if row.vid == 0x045e { BLUETOOTH } else { USB };
    let hid = Hid {
        manufacturer: row.manufacturer.into(),
        product: row.product.into(),
        release: row.version,
    };
    hermir::PadRef {
        name: row.product.into(),
        bus,
        vendor: row.vid,
        product: row.pid,
        version: row.version,
        index: 0,
        evdev: None,
        guid: Some(hidapi_guid(bus, row.vid, row.pid, &hid)),
        gamepad_name: sdl_name(row.vid, row.pid).map(Into::into),
    }
}

/// The pad a session of `kind` makes at OS slot `slot`, as the kernel, hidapi and SDL 3 name
/// it (each row read off Lichii's SDL on Bazzite). The Xbox pads are evdev; the rest HIDAPI,
/// over usbip with the USB release or over uhid with release 0. A Joy-Con pair and the Switch 2
/// pads reach no SDL joystick there, and seat as an Xbox 360 pad.
#[cfg(not(target_os = "macos"))]
fn pad_ref(kind: GamepadPref, slot: u8) -> hermir::PadRef {
    use GamepadPref as K;
    #[cfg(target_os = "linux")]
    let usbip = pf_inject::dualsense_usbip::usbip_preferred();
    #[cfg(not(target_os = "linux"))]
    let usbip = false;
    let usb = |manufacturer: &str, product: &str, release| {
        Some(Hid {
            manufacturer: manufacturer.into(),
            product: product.into(),
            release,
        })
    };
    let uhid = |name: &str| usb("", name, 0);
    let numbered = |name: &str| format!("Punktfunk {name} {slot}");
    // (kernel name, bus, vendor, product, kernel version, what hidapi reads; None = evdev)
    let (name, bus, vendor, product, version, hid) = match kind {
        K::XboxOne => (
            "Microsoft X-Box One S pad".into(),
            USB,
            0x045e,
            0x02ea,
            0x0408,
            None,
        ),
        K::XboxElite => {
            let name = "Microsoft X-Box One Elite 2 pad".into();
            (name, USB, 0x045e, 0x0b00, 0x0511, None)
        }
        K::DualSense if usbip => {
            let (sie, ds) = (
                "Sony Interactive Entertainment",
                "DualSense Wireless Controller",
            );
            let hid = usb(sie, ds, 0x0100);
            (format!("{sie} {ds}"), USB, 0x054c, 0x0ce6, 0x8111, hid)
        }
        // hid-playstation and hid-nintendo set 0x8000 on the version of the pads they drive.
        K::DualSense => (numbered("DualSense"), USB, 0x054c, 0x0ce6, 0x8100, None),
        K::DualSenseEdge => (
            numbered("DualSense Edge"),
            USB,
            0x054c,
            0x0df2,
            0x8100,
            None,
        ),
        K::DualShock4 => (numbered("DualShock 4"), USB, 0x054c, 0x09cc, 0x8100, None),
        K::SwitchPro => {
            let name = numbered("Switch Pro Controller");
            (name, USB, 0x057e, 0x2009, 0x8200, None)
        }
        K::SteamDeck => {
            let hid = usb("Valve Software", "Steam Deck Controller", 0x0300);
            let name = "Valve Software Steam Deck Controller".into();
            (name, USB, 0x28de, 0x1205, 0x0110, hid)
        }
        K::SteamController => {
            let hid = usb("Valve Software", "Wired Controller", 0x0300);
            let name = "Valve Software Wired Controller".into();
            (name, USB, 0x28de, 0x1102, 0x0110, hid)
        }
        K::SteamController2 => {
            let hid = usb("Valve Software", "Steam Controller", 0x0307);
            let name = "Valve Software Steam Controller".into();
            (name, USB, 0x28de, 0x1302, 0x0111, hid)
        }
        K::SteamController2Puck => {
            let hid = usb("Valve Software", "Steam Controller Puck", 0x0002);
            let name = "Valve Software Steam Controller Puck".into();
            (name, USB, 0x28de, 0x1304, 0x0111, hid)
        }
        K::EightBitDoUltimate2 => {
            let name = "8BitDo Ultimate 2 Wireless".into();
            (name, BLUETOOTH, 0x2dc8, 0x6012, 0x0100, None)
        }
        K::EightBitDoPro2 => ("8BitDo Pro 2".into(), USB, 0x2dc8, 0x6003, 0x0100, None),
        K::EightBitDoPro3 => ("8BitDo Pro 3".into(), USB, 0x2dc8, 0x6009, 0x0100, None),
        K::HoripadSteam => {
            let name = "Wireless HORIPAD For Steam".into();
            (name, USB, 0x0f0d, 0x01ab, 0x0100, None)
        }
        _ => return hermir::PadRef::xbox360(u32::from(slot)),
    };
    let evdev = matches!(kind, K::XboxOne | K::XboxElite);
    let hid = hid.or_else(|| (!evdev).then(|| uhid(&name)).flatten());
    hermir::PadRef {
        guid: hid.map(|h| hidapi_guid(bus, vendor, product, &h)),
        gamepad_name: sdl_name(vendor, product)
            .map(Into::into)
            .or_else(|| (!evdev).then(|| sdl_pad_name(kind, &name))),
        name,
        bus,
        vendor,
        product,
        version,
        index: 0,
        evdev: None,
    }
}

/// SDL 3's name for a HIDAPI pad [`sdl_name`] has no row for: its own name for the Valve pads,
/// else the HID name.
#[cfg(not(target_os = "macos"))]
fn sdl_pad_name(kind: GamepadPref, hid_name: &str) -> String {
    match kind {
        GamepadPref::SteamDeck => "Steam Deck Controller".into(),
        GamepadPref::SteamController
        | GamepadPref::SteamController2
        | GamepadPref::SteamController2Puck => "Steam Controller".into(),
        _ => hid_name.into(),
    }
}

/// The desk's gamepads in a `/proc/bus/input/devices` listing, in event-node order, each with a
/// joystick node: not the host's own virtual or usbip-attached pads, kept by Steam's `filter`,
/// and with an event node `open` lets the host open (a pad lent to a seat is closed to the
/// desk). A pad's motion sensors get a joystick node too; the accelerometer property (bit 6)
/// tells them apart. A pad SDL drives through HIDAPI gets that GUID only on USB, and only when
/// its hidraw node opens too, as SDL needs it to. `sys` is `/sys`, where hidapi's strings are.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pads(
    text: &str,
    sys: &Path,
    filter: &SdlFilter,
    open: &dyn Fn(&Path) -> bool,
) -> Vec<hermir::PadRef> {
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
        if sensors || !handlers.split_whitespace().any(|h| h.starts_with("js")) {
            continue;
        }
        let made =
            ours && (PAD_VENDORS.contains(&vendor) || HOST_MADE.contains(&(vendor, product)));
        let event = handlers
            .split_whitespace()
            .find_map(|w| w.strip_prefix("event")?.parse::<u32>().ok())
            .unwrap_or(u32::MAX);
        let evdev = (event != u32::MAX).then(|| PathBuf::from(format!("/dev/input/event{event}")));
        if made
            || !gamepad(field("B: KEY="))
            || !filter.keeps(vendor, product)
            || !evdev.as_deref().is_some_and(open)
        {
            continue;
        }
        let name = field("N: Name=")
            .unwrap_or("")
            .trim_matches('"')
            .to_string();
        let guid = (sdl_name(vendor, product).is_some()
            && bus == 3
            && hidraw_of(sys, sysfs).is_some_and(|node| open(&node)))
        .then(|| hid_of(sys, sysfs).and_then(|h| named_hidapi_guid(vendor, product, &h)))
        .flatten();
        out.push((
            event,
            hermir::PadRef {
                name,
                bus,
                vendor,
                product,
                version,
                index: 0,
                evdev,
                guid,
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

/// The hidraw node of the HID device under the input node at `sysfs`, `sys` being `/sys`.
fn hidraw_of(sys: &Path, sysfs: &str) -> Option<PathBuf> {
    let hid = sys
        .join(sysfs.trim_start_matches('/'))
        .parent()?
        .parent()?
        .join("hidraw");
    let node = std::fs::read_dir(hid).ok()?.flatten().next()?.file_name();
    Some(Path::new("/dev").join(node))
}

/// Whether a `B: KEY=` bitmap holds a key of the gamepad block, `BTN_GAMEPAD` (0x130) to
/// 0x13f: a left Joy-Con has `BTN_TL` and no `BTN_SOUTH`. The kernel prints the bitmap in the
/// reader's longs, the highest first.
fn gamepad(key: Option<&str>) -> bool {
    const BTN_GAMEPAD: usize = 0x130;
    let bits = usize::BITS as usize;
    key.and_then(|k| k.split_whitespace().rev().nth(BTN_GAMEPAD / bits))
        .and_then(|w| u64::from_str_radix(w, 16).ok())
        .is_some_and(|w| w >> (BTN_GAMEPAD % bits) & 0xffff != 0)
}

/// Steam's virtual pad. It always passes Steam's filter: Steam sets SDL's hint that lets it by.
const STEAM_VIRTUAL_PAD: (u16, u16) = (0x28de, 0x11ff);

/// Steam's pad filter, as SDL reads `SDL_GAMECONTROLLER_IGNORE_DEVICES` and its `_EXCEPT`. The
/// empty filter, a launch outside Steam's, keeps every pad.
#[derive(Debug, Default)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct SdlFilter {
    ignore: Vec<(u16, u16)>,
    except: Vec<(u16, u16)>,
}

impl SdlFilter {
    /// Each list as SDL scans it: every `0x` number in turn, vendor then product, cut to 16
    /// bits. `None` when a list names a file (`@path`), which the host does not read.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
    pub(crate) fn parse(ignore: &str, except: &str) -> Option<SdlFilter> {
        fn ids(list: &str) -> Option<Vec<(u16, u16)>> {
            if list.starts_with('@') {
                return None;
            }
            let (mut words, mut rest) = (Vec::new(), list);
            while let Some(at) = rest.find("0x") {
                let hex = &rest[at + 2..];
                let len = hex
                    .find(|c: char| !c.is_ascii_hexdigit())
                    .unwrap_or(hex.len());
                // strtol: no digits reads 0, too many saturate.
                words.push(match len {
                    0 => 0,
                    _ => i64::from_str_radix(&hex[..len], 16).unwrap_or(i64::MAX) as u16,
                });
                rest = &hex[len..];
            }
            Some(words.chunks_exact(2).map(|w| (w[0], w[1])).collect())
        }
        Some(SdlFilter {
            ignore: ids(ignore)?,
            except: ids(except)?,
        })
    }

    /// Whether SDL keeps the pad: with an `except` list only the pads on it, else every pad
    /// but `ignore`'s. Steam's virtual pad always.
    fn keeps(&self, vendor: u16, product: u16) -> bool {
        let pad = (vendor, product);
        if pad == STEAM_VIRTUAL_PAD {
            return true;
        }
        match self.except.is_empty() {
            true => !self.ignore.contains(&pad),
            false => self.except.contains(&pad),
        }
    }
}

/// Whether the host names a desk pad as SDL does. It names seven pads, on USB only: the four
/// [`sdl_name`] pads once they carry their HIDAPI GUID, and the Xbox 360, One S and Elite 2
/// pads, which SDL reads through evdev under the names hermir gives them. Any other pad at the
/// desk leaves the whole set to the emulator's own bindings.
fn named(pad: &hermir::PadRef) -> bool {
    pad.bus == 3
        && match sdl_name(pad.vendor, pad.product) {
            Some(_) => pad.guid.is_some(),
            None => matches!(
                (pad.vendor, pad.product),
                (0x045e, 0x028e | 0x02ea | 0x0b00)
            ),
        }
}

/// The desk's pads in a listing ([`pads`]), or none when one of them is a pad the host
/// can't name as SDL does: the emulator keeps its own bindings, and the log names that pad.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn desk_pads_in(
    text: &str,
    sys: &Path,
    filter: &SdlFilter,
    open: &dyn Fn(&Path) -> bool,
) -> Vec<hermir::PadRef> {
    let pads = pads(text, sys, filter, open);
    if let Some(pad) = pads.iter().find(|p| !named(p)) {
        let id = format!("{:04x}:{:04x}", pad.vendor, pad.product);
        tracing::info!(unnamed = %pad.name, %id, bus = pad.bus, "desk pads left unbound");
        return Vec::new();
    }
    pads
}

/// The pads at the box's own screen, each named as SDL names it, for an emulator the host
/// starts with no stream. `filter` is Steam's.
#[cfg(target_os = "linux")]
pub(crate) fn desk_pads(filter: &SdlFilter) -> Vec<hermir::PadRef> {
    let text = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    desk_pads_in(&text, Path::new("/sys"), filter, &may_open)
}

/// Whether this host may open `node` as SDL does: an event node to read, a hidraw node to read
/// and write. A pad lent to a seat is its holder's alone.
#[cfg(target_os = "linux")]
fn may_open(node: &Path) -> bool {
    use rustix::fs::Access;
    let need = match node.starts_with("/dev/input") {
        true => Access::READ_OK,
        false => Access::READ_OK | Access::WRITE_OK,
    };
    rustix::fs::access(node, need).is_ok()
}

/// The desk's pads as players, seat 1 the first in event order, and every one of them as
/// `connected`. No pad writes no players.
fn desk_patch(pads: &[hermir::PadRef]) -> hermir::Patch {
    if pads.is_empty() {
        return hermir::Patch::default();
    }
    hermir::Patch {
        players: Some(
            pads.iter()
                .cloned()
                .zip(1..=u8::MAX)
                .map(|(pad, seat)| hermir::Player { seat, pad })
                .collect(),
        ),
        connected: Some(pads.to_vec()),
        ..Default::default()
    }
}

/// Whose session bindings are out: `prepared` counts the prepares, `owner` is the library id of
/// the launch whose prepare was last. `None` for a prepare outside a launch, and once reverted.
struct Bindings {
    prepared: u64,
    owner: Option<String>,
}

impl Bindings {
    /// Whether an exit holding `held` (its launch's [`Self::prepared_for`]) puts the bindings
    /// back: its own, or ones no launch owns.
    fn reverts_for(&self, held: Option<u64>) -> bool {
        self.owner.is_none() || held == Some(self.prepared)
    }

    /// Counts a prepare for `owner`; the count names the bindings it writes.
    fn prepared_for(&mut self, owner: Option<&str>) -> u64 {
        self.prepared += 1;
        self.owner = owner.map(str::to_string);
        self.prepared
    }
}

static BINDINGS: std::sync::Mutex<Bindings> = std::sync::Mutex::new(Bindings {
    prepared: 0,
    owner: None,
});

fn bindings() -> std::sync::MutexGuard<'static, Bindings> {
    BINDINGS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Puts the emulators' own bindings and settings back after a game, file by file, when they are
/// the exiting launch's (`held`, from its [`prepare_launch`]) or no launch's: a later launch's
/// prepare already put this one's back and wrote its own. Forced: an emulator rewrites its config
/// on exit, and that is no edit of the player's. Nothing outstanding is a quiet no-op.
pub fn revert_players(held: Option<u64>) {
    let Ok(h) = open() else { return };
    let mut out = bindings();
    if !out.reverts_for(held) {
        return;
    }
    revert_with(&h);
    out.owner = None;
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
/// `firmware_dir` (the regular files in it, not below, which a Windows player may then read,
/// `let_player_read`), and has the pads a session of kind `pad` makes written in seat order
/// (steps of kind `players`; reverted after the game), as [`session_players`] seats them.
/// `platform` is a catalog id or any of its aliases (RomM slug, ES-DE folder, libretro name).
/// Blocking: an installer may run. The bindings belong to no launch: any game's exit reverts
/// them.
pub fn prepare(
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
    pad: GamepadPref,
) -> hermir::Result<Vec<(String, hermir::Prepared)>> {
    let pads = Pads::Session {
        kind: pad,
        roster: None,
        device: None,
    };
    let session = hermir::Patch::default();
    prepare_as(
        None,
        &mut None,
        id,
        platform,
        firmware_dir,
        Some(&pads),
        session,
    )
}

/// [`prepare`], its bindings `owner`'s: the library id of the launch they are for. `held` gets
/// the count that names them before anything is written. `pads` are the players': a session's
/// seat as [`session_players`] does, the desk's as [`desk_patch`]; `None` writes no players.
/// `session` is the rest of the launch's patch, written and reverted with the players.
fn prepare_as(
    owner: Option<&str>,
    held: &mut Option<u64>,
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
    pads: Option<&Pads>,
    session: hermir::Patch,
) -> hermir::Result<Vec<(String, hermir::Prepared)>> {
    let h = open()?;
    // A game that ended without a lease reporting it (its session took the nested gamescope
    // down with it) still has its bindings out; the next launch is the latest they go back.
    {
        let mut out = bindings();
        revert_with(&h);
        *held = Some(out.prepared_for(owner));
    }
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
    #[cfg(windows)]
    let_player_read(&firmware);
    let players = match pads {
        Some(Pads::Session {
            kind,
            roster,
            device,
        }) => hermir::Patch {
            players: Some(session_players(*kind, roster.as_deref(), device.as_deref())),
            ..Default::default()
        },
        Some(Pads::Desk(pads)) => desk_patch(pads),
        None => hermir::Patch::default(),
    };
    let patch = hermir::Patch {
        players: players.players,
        connected: players.connected,
        ..session
    };
    emulator
        .copies()?
        .iter()
        .map(|copy| {
            let mut prepared = emulator.prepare(copy, platform, &firmware)?;
            let applied = emulator.apply(copy, &patch)?;
            for k in applied.knobs.iter().filter(|k| k.knob != "players") {
                tracing::info!(emulator = %id, knob = %k.knob, support = ?k.support, note = k.note.as_deref().unwrap_or(""), "emulator setting for the launch");
            }
            prepared.steps.extend(applied.steps);
            Ok((copy.exe.to_string(), prepared))
        })
        .collect()
}

/// The emulator opens what a plugin hands it as the session's player, and a plugin's state is
/// the runner's alone. Each such file gets a read ACE on the file itself: that player's while
/// it is the plugin's own, every account's while it is the host's copy of it
/// ([`handover::Scratch`]). Any other file is left as it is.
#[cfg(windows)]
fn let_player_read(files: &[PathBuf]) {
    /// `BUILTIN\Users`. A host copy holds game files and no secret, and an installer reads it
    /// as whoever is signed in.
    const USERS: &str = "S-1-5-32-545";
    let session = crate::windows::theme::session_sid();
    let copies: Vec<PathBuf> = (handover::scratch_base().canonicalize().ok())
        .into_iter()
        .collect();
    let mut roots: Vec<PathBuf> = [pf_paths::config_dir(), pf_paths::seat::library_dir()]
        .iter()
        .filter_map(|d| d.join("plugin-state").canonicalize().ok())
        .chain(copies.iter().cloned())
        .collect();
    roots.dedup();
    for file in files {
        let Ok(real) = file.canonicalize() else {
            continue;
        };
        let Some(own) = roots.iter().find_map(|root| {
            let plugin = real.strip_prefix(root).ok()?.components().next()?;
            Some(root.join(plugin))
        }) else {
            continue;
        };
        let sid = match copies.iter().any(|c| real.starts_with(c)) {
            true => USERS,
            false => match &session {
                Some(sid) => sid.as_str(),
                None => continue,
            },
        };
        if let Err(error) = pf_paths_win::grant_read_inside(&real, &own, sid) {
            tracing::warn!(file = %real.display(), %error, "plugin file not opened to the player");
        }
    }
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

/// The libretro cores `install` has, by the name a launch asks for; empty for every emulator
/// but RetroArch.
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
    on_best_in(None, id, f)
}

/// [`on_best`] on `account`'s copies, as [`open_in`] reads them.
fn on_best_in<T>(
    account: Option<&Path>,
    id: &str,
    f: impl FnOnce(&Hermir, &hermir::EmulatorHandle<'_>, &hermir::Install) -> hermir::Result<T>,
) -> hermir::Result<T> {
    let h = open_in(account)?;
    let emulator = h.emulator(id)?;
    let install = emulator
        .best()?
        .ok_or_else(|| hermir::Error::Invalid(format!("no copy of {id} on this host")))?;
    f(&h, &emulator, &install)
}

/// The save units of `platform` on the best copy; `game` is the game's folder. `account` as
/// [`open_in`] reads it, for this and the three below.
pub fn units(
    account: Option<&Path>,
    id: &str,
    platform: &str,
    game: Option<&Path>,
) -> hermir::Result<Vec<hermir::SaveUnit>> {
    on_best_in(account, id, |h, e, i| {
        e.units(i, &platform_id(h, platform), game)
    })
}

/// Each named unit written into `out`, a tar for a folder. A unit is one the copy lists
/// ([`units`]): the listing is where a save folder's own pattern holds.
pub fn export_units(
    account: Option<&Path>,
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
    on_best_in(account, id, |h, e, i| {
        let platform = platform_id(h, platform);
        let listed = e.units(i, &platform, game)?;
        units
            .iter()
            .map(|(kind, name)| {
                if !listed.iter().any(|u| u.kind == *kind && u.name == *name) {
                    return Err(hermir::Error::Invalid(format!(
                        "no unit {name} on this copy"
                    )));
                }
                Ok((*kind, e.export_unit(i, &platform, game, *kind, name, out)?))
            })
            .collect()
    })
}

/// One unit put back from the file `from`, into the first of `kinds` with a place for it.
#[allow(clippy::too_many_arguments)] // hermir's own `import_unit`, plus whose copies
pub fn import_unit(
    account: Option<&Path>,
    id: &str,
    platform: &str,
    game: Option<&Path>,
    kinds: &[hermir::SaveKind],
    name: &str,
    from: &Path,
    others: &[String],
) -> hermir::Result<hermir::PrepareStep> {
    on_best_in(account, id, |h, e, i| {
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

/// A game's update or DLC files, installed the way the emulator takes them. On Windows the
/// player may then read the ones a plugin keeps (`let_player_read`).
pub fn install_content(
    id: &str,
    platform: &str,
    kind: &str,
    files: &[PathBuf],
) -> hermir::Result<Vec<hermir::ContentStep>> {
    #[cfg(windows)]
    let_player_read(files);
    on_best(id, |h, e, i| {
        e.install_content(i, &platform_id(h, platform), kind, files)
    })
}

/// Whether `id` reads a `kind` add-on of `platform` where it lies, as hermir says
/// (`ContentMethod::keeps_path`); every other way copies it in or runs an installer on it.
pub fn content_keeps_path(id: &str, platform: &str, kind: &str) -> bool {
    let Ok(h) = open_in(None) else { return false };
    let Ok(emulator) = h.emulator(id) else {
        return false;
    };
    let content = emulator.entry().content.get(&platform_id(&h, platform));
    let method = match (content, kind) {
        (Some(c), "update") => &c.update,
        (Some(c), "dlc") => &c.dlc,
        _ => return false,
    };
    method.keeps_path()
}

/// Whether the best copy has `platform`'s firmware; `None` when the platform needs none.
pub fn firmware_status(
    account: Option<&Path>,
    id: &str,
    platform: &str,
) -> hermir::Result<Option<hermir::FirmwareStatus>> {
    on_best_in(account, id, |h, e, i| {
        e.firmware_status(i, &platform_id(h, platform))
    })
}

/// The command that starts `file` in the best copy of `id`, fullscreen. On Windows without
/// hermir's per-user variables ([`drop_home_vars`]).
pub fn launch_spec(
    id: &str,
    platform: &str,
    file: &Path,
    core: Option<&str>,
) -> hermir::Result<hermir::LaunchSpec> {
    let mut spec = on_best(id, |h, e, i| {
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
    })?;
    if cfg!(windows) {
        drop_home_vars(&mut spec);
    }
    Ok(spec)
}

/// `%USERPROFILE%`, `%APPDATA%` and `%LOCALAPPDATA%` out of `spec`. hermir adds them for the
/// home [`open`] gave it, the session player's, and a Windows launch starts as that player
/// with their own environment block.
fn drop_home_vars(spec: &mut hermir::LaunchSpec) {
    // Only the names: the home is the player's either way.
    let names = hermir::RealEnv::with_home(hermir::Os::Windows, PathBuf::new()).home_vars();
    for (name, _) in names {
        spec.env.remove(name);
    }
}

/// Points hermir at the operator's own copy of `id`; `keep: false` forgets it.
pub fn adopt(id: &str, exe: &Path, keep: bool) -> hermir::Result<Option<hermir::Install>> {
    open()?.emulator(id)?.adopt(exe, keep)
}

/// The folder whose read grant lets a plugin move emulator saves: hermir's own backups of
/// the saves it replaced. The grant is the operator's one yes for every emulator.
pub fn saves_grant() -> PathBuf {
    state_dir().unwrap_or_else(prefix).join(".save-backups")
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

/// Before an `emulator` entry starts: the RetroArch core it names when this host's RetroArch
/// lacks it, and on Windows readable by the player, every copy past its first-run questions, the
/// platform's firmware from the plugin's `firmware/<platform>` when it staged some, and the
/// launch's `pads` as players (a session's, or the desk's from `desk_pads`). The emulator's own
/// update check goes off, and a stream's `screen` sets the render scale of a copy still at 1×.
/// Only this launch's exit reverts them: their count comes back, for [`revert_players`]. What it
/// did goes to the log. A stream's launch waits for the client's pads, at most [`SETTLE_ALL`],
/// and never on the rest to succeed.
pub(crate) fn prepare_launch(
    library_id: &str,
    pads: &Pads,
    screen: Option<hermir::Size>,
) -> Option<u64> {
    let entry = crate::library::entry_for_library_id(library_id)?;
    let spec = entry.launch.as_ref().filter(|s| s.kind == "emulator")?;
    let arg = |name: &str| {
        spec.args
            .iter()
            .flatten()
            .find(|a| a.name == name)
            .map(|a| a.value.as_str())
    };
    let platform = arg("platform");
    if let Some(core) = arg("core").filter(|c| valid_core(c))
        && let Some(dir) = cores_dir().ok().flatten()
    {
        if !has_core(&dir, core) {
            match install_core(core, &hermir::progress::Quiet) {
                Ok(path) => {
                    tracing::info!(core, path = %path.display(), "core installed for the launch")
                }
                Err(error) => tracing::warn!(core, %error, "core not installed for the launch"),
            }
        }
        #[cfg(windows)]
        open_core(&dir, core);
    }
    let staged = entry
        .provider
        .as_deref()
        .zip(platform)
        .and_then(|(provider, platform)| staged_firmware(provider, platform));
    let mut held = None;
    let session = hermir::Patch {
        video: screen.map(|output| hermir::Video {
            output: Some(output),
            ..Default::default()
        }),
        update_check: Some(false),
        ..Default::default()
    };
    // An update in flight finishes first: its copy is half replaced until then.
    let _held = updating();
    match prepare_as(
        Some(library_id),
        &mut held,
        &spec.value,
        platform,
        staged.as_ref().map(handover::Scratch::path),
        Some(pads),
        session,
    ) {
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
    held
}

/// The plain files of `<plugin-state>/<provider>/firmware/<platform>` in the library this host
/// reads (a seat's is the copy of the box's), copied into a scratch folder of the host's own.
/// hermir reads the copy: a link the plugin swaps into its folder reaches nothing.
fn staged_firmware(provider: &str, platform: &str) -> Option<handover::Scratch> {
    let dir = staged_firmware_in(&pf_paths::seat::library_dir(), provider, platform)?;
    handover::staged(&dir)
        .inspect_err(
            |error| tracing::warn!(provider, platform, %error, "staged firmware not copied"),
        )
        .ok()
}

/// That folder, held open below the plugin's state folder ([`handover::Dir`]), when it is one.
/// The walk starts at that folder's canonical path: only the person can make it a link, since
/// no sandbox binds the state root.
fn staged_firmware_in(base: &Path, provider: &str, platform: &str) -> Option<handover::Dir> {
    let mut parts = Path::new(provider).components();
    let (Some(std::path::Component::Normal(_)), None) = (parts.next(), parts.next()) else {
        return None;
    };
    let own = base
        .join("plugin-state")
        .join(provider)
        .canonicalize()
        .ok()?;
    handover::Dir::open(&own, &Path::new("firmware").join(platform), false).ok()
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

/// Files a plugin hands the host, or takes back, in a folder it may rewrite while the host works:
/// its own state folder, or a game folder in a root it may reach. A path is walked below that
/// root one real folder at a time and its file opened without following a link: a link swapped
/// in after a check reaches nothing. hermir reads and writes a [`Scratch`] folder of the
/// host's own, or a [`mirror`] of the plugin's content there. Only a file an emulator keeps by
/// its path goes to hermir as it lies, checked and held ([`hold`]).
pub(crate) mod handover {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::{Component, Path, PathBuf};

    /// A folder below a plugin's state folder, held open.
    pub(crate) struct Dir {
        path: PathBuf,
        #[cfg(unix)]
        fd: std::os::fd::OwnedFd,
        #[cfg(windows)]
        root: PathBuf,
        /// Each folder from the root down, open without delete sharing: none is renamed away.
        #[cfg(windows)]
        _held: Vec<File>,
    }

    fn refused(what: &Path) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is no plain folder or file below the state folder",
                what.display()
            ),
        )
    }

    /// `name` when it is one plain path component.
    fn plain(name: &OsStr) -> io::Result<&OsStr> {
        let mut parts = Path::new(name).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(n)), None) if n == name => Ok(name),
            _ => Err(refused(Path::new(name))),
        }
    }

    /// `file` when it is a plain file, not a folder, a pipe or a device.
    fn plain_file(file: File, name: &OsStr) -> io::Result<File> {
        if file.metadata()?.is_file() {
            return Ok(file);
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is no plain file", Path::new(name).display()),
        ))
    }

    impl Dir {
        /// `rel` below `root`, a canonical folder its plugin can't replace: each component a
        /// real folder, never a link, made when `make` and missing. `..` or an absolute `rel` is
        /// refused.
        pub(crate) fn open(root: &Path, rel: &Path, make: bool) -> io::Result<Dir> {
            let names = rel
                .components()
                .map(|c| match c {
                    Component::Normal(name) => Ok(name),
                    _ => Err(refused(rel)),
                })
                .collect::<io::Result<Vec<_>>>()?;
            sys::open(root, &names, make)
        }

        /// The plain file `name` in this folder, opened without following a link.
        pub(crate) fn read(&self, name: &OsStr) -> io::Result<File> {
            plain_file(sys::read(self, plain(name)?)?, name)
        }

        /// A new file `name` in this folder, in place of a file or link of that name.
        pub(crate) fn create(&self, name: &OsStr) -> io::Result<File> {
            sys::create(self, plain(name)?)
        }
    }

    /// A fresh folder of the host's own, outside every root a plugin may bind; it goes when
    /// dropped.
    pub(crate) struct Scratch(PathBuf);

    /// Where every [`Scratch`] lives: owner-only, in the host's config folder.
    pub(crate) fn scratch_base() -> PathBuf {
        pf_paths::config_dir().join("emulator-files")
    }

    impl Scratch {
        pub(crate) fn new() -> io::Result<Scratch> {
            Self::new_in(&scratch_base())
        }

        pub(crate) fn new_in(base: &Path) -> io::Result<Scratch> {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            pf_paths::create_private_dir(base)?;
            let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // What a crashed or restarted host left: no folder outlives the longest install,
            // four hours, so one older than six is nobody's.
            if next == 0 {
                let stale = std::time::Duration::from_secs(6 * 3600);
                for entry in std::fs::read_dir(base)?.flatten() {
                    let age = entry.metadata().and_then(|m| m.modified());
                    if age.is_ok_and(|at| at.elapsed().is_ok_and(|age| age > stale)) {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
            let dir = base.join(format!("{}-{next}", std::process::id()));
            // One of this name is a dead process's.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir(&dir)?;
            Ok(Scratch(dir))
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The plain files directly in `dir`, hard-linked into a new [`Scratch`] under their own
    /// names, copied where no link can be made and on Windows, where an installer reads them
    /// as the player ([`link`]). A link or a folder in it is left out.
    pub(crate) fn staged(dir: &Dir) -> io::Result<Scratch> {
        staged_in(dir, &scratch_base())
    }

    pub(crate) fn staged_in(dir: &Dir, base: &Path) -> io::Result<Scratch> {
        let scratch = Scratch::new_in(base)?;
        for entry in std::fs::read_dir(&dir.path)?.flatten() {
            let name = entry.file_name();
            if let Ok(mut from) = dir.read(&name) {
                let to = scratch.path().join(&name);
                if !(cfg!(unix) && sys::hard_link(&from, dir, &name, &to).is_ok()) {
                    io::copy(&mut from, &mut File::create_new(&to)?)?;
                }
            }
        }
        Ok(scratch)
    }

    /// The host's file `from` copied into `into` under its own name. The copy is written
    /// beside it and renamed over it, so what was there stays whole until the new bytes are.
    pub(crate) fn place(from: &Path, into: &Dir) -> io::Result<()> {
        let name = plain(from.file_name().ok_or_else(|| refused(from))?)?;
        let mut part = name.to_os_string();
        part.push(".punktfunk-part");
        let written = io::copy(&mut File::open(from)?, &mut into.create(&part)?)
            .and_then(|_| sys::rename(into, &part, name));
        if written.is_err() {
            let _ = sys::remove(into, &part);
        }
        written
    }

    /// A plain file below a root its plugin may rewrite, opened without following a link and
    /// held open: for an emulator that keeps the path it is given, where a copy would only be
    /// a second file to store. On Windows the handles pin the name: while this lives, neither
    /// the file nor a folder above it is renamed or replaced, so what hermir opens by that
    /// path is this file. Elsewhere a name can't be pinned; hermir reads the file's ticket by
    /// path, as the host's own account, and tells the plugin none of what it read.
    pub(crate) struct Held {
        path: PathBuf,
        _dir: Dir,
        _file: File,
    }

    impl Held {
        pub(crate) fn path(&self) -> &Path {
            &self.path
        }
    }

    /// [`Held`] for `rel` below `root`. A folder, a link or a missing file is refused.
    pub(crate) fn hold(root: &Path, rel: &Path) -> io::Result<Held> {
        let name = rel.file_name().ok_or_else(|| refused(rel))?;
        let dir = Dir::open(root, rel.parent().unwrap_or(Path::new("")), false)?;
        let file = dir.read(name)?;
        Ok(Held {
            path: root.join(rel),
            _dir: dir,
            _file: file,
        })
    }

    /// `rel` below `root`, a plain file or a folder, mirrored at `into.join(rel)`, a place in
    /// a [`Scratch`], as it is now:
    /// each folder made, each plain file linked ([`link`]), nothing taken through a link. A file
    /// in a title folder (`meta/meta.xml` up to four folders above it, where hermir looks)
    /// brings that whole folder. Answers the mirror's path for `rel`.
    pub(crate) fn mirror(root: &Path, rel: &Path, into: &Path) -> io::Result<PathBuf> {
        let at = into.join(rel);
        if Dir::open(root, rel, false).is_ok() {
            tree(root, rel, into)?;
            return Ok(at);
        }
        let parent = rel.parent().unwrap_or(Path::new(""));
        let name = rel.file_name().ok_or_else(|| refused(rel))?;
        let title = parent.ancestors().take(4).find(|a| {
            Dir::open(root, &a.join("meta"), false)
                .and_then(|meta| meta.read("meta.xml".as_ref()))
                .is_ok()
        });
        if let Some(title) = title {
            tree(root, title, into)?;
        }
        let dir = Dir::open(root, parent, false)?;
        std::fs::create_dir_all(into.join(parent))?;
        link(dir.read(name)?, &dir, name, &at)?;
        Ok(at)
    }

    /// The folder `rel` below `root` mirrored at `into.join(rel)`. Only names come from the
    /// listing; each is opened below the held folder. A link, pipe or device is left out; any
    /// other entry that fails fails the mirror, so no half a title is installed.
    fn tree(root: &Path, rel: &Path, into: &Path) -> io::Result<()> {
        let dir = Dir::open(root, rel, false)?;
        let to = into.join(rel);
        std::fs::create_dir_all(&to)?;
        for entry in std::fs::read_dir(&dir.path)? {
            let entry = entry?;
            let (name, kind) = (entry.file_name(), entry.file_type()?);
            if kind.is_dir() {
                tree(root, &rel.join(&name), into)?;
            } else if kind.is_file() {
                link(dir.read(&name)?, &dir, &name, &to.join(&name))?;
            }
        }
        Ok(())
    }

    /// `file`, open as `name` in `dir`, at the host's own `dest`, in place of what was there: a
    /// hard link where the system makes one, else a copy. Windows always copies, since a read
    /// is granted only on a file with one name (`let_player_read`).
    fn link(mut file: File, dir: &Dir, name: &OsStr, dest: &Path) -> io::Result<()> {
        // Made beside `dest` and renamed over it: a copy that fails leaves the old file whole.
        let mut part = dest.as_os_str().to_os_string();
        part.push(".punktfunk-part");
        let part = PathBuf::from(part);
        let _ = std::fs::remove_file(&part);
        let made = if cfg!(unix) && sys::hard_link(&file, dir, name, &part).is_ok() {
            Ok(())
        } else {
            File::create_new(&part).and_then(|mut to| io::copy(&mut file, &mut to).map(drop))
        };
        let made = made.and_then(|()| std::fs::rename(&part, dest));
        if made.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        made
    }

    #[cfg(unix)]
    mod sys {
        use super::{refused, Dir};
        use std::ffi::{CString, OsStr};
        use std::fs::File;
        use std::io;
        use std::os::fd::{AsRawFd as _, OwnedFd};
        use std::os::unix::ffi::OsStrExt as _;
        use std::path::Path;

        fn c(name: &OsStr) -> io::Result<CString> {
            Ok(CString::new(name.as_bytes())?)
        }

        /// `name` in `dir`, never through a link.
        fn at(
            dir: &OwnedFd,
            name: &OsStr,
            flags: libc::c_int,
            mode: libc::c_uint,
        ) -> io::Result<OwnedFd> {
            use rustix::fs::{Mode, OFlags};
            let flags = flags | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let flags = OFlags::from_bits_retain(flags as _);
            Ok(rustix::fs::openat(
                dir,
                name,
                flags,
                Mode::from_raw_mode(mode as _),
            )?)
        }

        pub(super) fn open(root: &Path, names: &[&OsStr], make: bool) -> io::Result<Dir> {
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut fd: OwnedFd = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(root)?
                .into();
            for name in names {
                if make {
                    let c_name = c(name)?;
                    // SAFETY: `fd` is open for the call; `c_name` is NUL-terminated and outlives it.
                    if unsafe { libc::mkdirat(fd.as_raw_fd(), c_name.as_ptr(), 0o777) } != 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::AlreadyExists {
                            return Err(e);
                        }
                    }
                }
                fd = at(&fd, name, libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(|e| {
                    // A link fails as a file does; only a link is a way out.
                    match at(&fd, name, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
                        Err(probe) if probe.raw_os_error() == Some(libc::ELOOP) => {
                            refused(Path::new(name))
                        }
                        _ => e,
                    }
                })?;
            }
            let path = names.iter().fold(root.to_path_buf(), |p, n| p.join(n));
            Ok(Dir { path, fd })
        }

        pub(super) fn read(dir: &Dir, name: &OsStr) -> io::Result<File> {
            at(&dir.fd, name, libc::O_RDONLY | libc::O_NONBLOCK, 0).map(File::from)
        }

        /// A hard link at `dest` to the open `file` itself, through its `/proc` entry, so no
        /// path the plugin can change is resolved.
        pub(super) fn hard_link(file: &File, _: &Dir, _: &OsStr, dest: &Path) -> io::Result<()> {
            let from = c(OsStr::new(&format!("/proc/self/fd/{}", file.as_raw_fd())))?;
            let to = c(dest.as_os_str())?;
            // SAFETY: both strings are NUL-terminated and outlive the call.
            let linked = unsafe {
                libc::linkat(
                    libc::AT_FDCWD,
                    from.as_ptr(),
                    libc::AT_FDCWD,
                    to.as_ptr(),
                    libc::AT_SYMLINK_FOLLOW,
                )
            };
            if linked != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// `from` renamed to `to`, both in `dir`. A file or link at `to` is replaced itself.
        pub(super) fn rename(dir: &Dir, from: &OsStr, to: &OsStr) -> io::Result<()> {
            let (from, to) = (c(from)?, c(to)?);
            let fd = dir.fd.as_raw_fd();
            // SAFETY: `dir.fd` is open for the call; both names are NUL-terminated and outlive it.
            if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        /// The file or link `name` in `dir`, removed.
        pub(super) fn remove(dir: &Dir, name: &OsStr) -> io::Result<()> {
            let name = c(name)?;
            // SAFETY: `dir.fd` is open for the call; `name` is NUL-terminated and outlives it.
            if unsafe { libc::unlinkat(dir.fd.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }

        pub(super) fn create(dir: &Dir, name: &OsStr) -> io::Result<File> {
            let c_name = c(name)?;
            // SAFETY: `dir.fd` is open for the call; `c_name` is NUL-terminated and outlives it.
            // Flags 0 remove a file or the link itself, never a folder or what a link names.
            if unsafe { libc::unlinkat(dir.fd.as_raw_fd(), c_name.as_ptr(), 0) } != 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::NotFound {
                    return Err(e);
                }
            }
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL;
            at(&dir.fd, name, flags, 0o666)
                .map(File::from)
                .map_err(|e| match e.kind() {
                    io::ErrorKind::AlreadyExists => refused(&dir.path.join(name)),
                    _ => e,
                })
        }
    }

    #[cfg(windows)]
    mod sys {
        use super::{refused, Dir};
        use std::ffi::OsStr;
        use std::fs::File;
        use std::io;
        use std::path::{Path, PathBuf};

        enum Kind {
            Dir,
            Read,
            /// A new file.
            Create,
            /// A new file that goes when its handle closes.
            Pin,
            /// What is there now, opened to delete: a link itself, never what it names.
            Old,
        }

        /// `path` opened without following a reparse point, kept only when its real path lies
        /// inside `root`, only [`Kind::Old`] when it is a reparse point, and for [`Kind::Dir`]
        /// only when it is a folder. A file made elsewhere through a link swapped in meanwhile
        /// is deleted again through its own handle.
        fn checked(path: &Path, root: &Path, kind: Kind) -> io::Result<File> {
            use ::windows::Win32::Storage::FileSystem::{
                DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE,
                FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
            };
            use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
            let flags = FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS;
            let flags = match kind {
                Kind::Pin => flags | FILE_FLAG_DELETE_ON_CLOSE,
                _ => flags,
            };
            let mut options = std::fs::OpenOptions::new();
            options
                .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
                .custom_flags(flags.0);
            match kind {
                Kind::Dir | Kind::Read => options.read(true),
                Kind::Create | Kind::Pin => options
                    .write(true)
                    .create_new(true)
                    .access_mode((FILE_GENERIC_WRITE | DELETE).0),
                Kind::Old => options.access_mode((DELETE | FILE_READ_ATTRIBUTES).0),
            };
            let file = options.open(path)?;
            let meta = file.metadata()?;
            let link = meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            let inside = (!link || matches!(kind, Kind::Old))
                && final_path(&file).is_some_and(|real| real.starts_with(root));
            if !inside {
                if matches!(kind, Kind::Create)
                    && let Err(error) = delete(&file)
                {
                    tracing::warn!(%error, "file made outside a state folder not deleted");
                }
                return Err(refused(path));
            }
            if matches!(kind, Kind::Dir) && !meta.is_dir() {
                return Err(io::ErrorKind::NotADirectory.into());
            }
            Ok(file)
        }

        /// Where `file` really is, in the spelling `canonicalize` gives.
        fn final_path(file: &File) -> Option<PathBuf> {
            use ::windows::Win32::Foundation::HANDLE;
            use ::windows::Win32::Storage::FileSystem::{
                GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED,
            };
            use std::os::windows::{ffi::OsStringExt as _, io::AsRawHandle as _};
            let mut buf = vec![0u16; 32_768];
            let handle = HANDLE(file.as_raw_handle());
            // SAFETY: `handle` is `file`'s, open for the call, which writes at most `buf.len()`.
            let n = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) };
            let n = n as usize;
            (n > 0 && n < buf.len()).then(|| std::ffi::OsString::from_wide(&buf[..n]).into())
        }

        /// `file` goes when its last handle closes.
        fn delete(file: &File) -> io::Result<()> {
            use ::windows::Win32::Foundation::HANDLE;
            use ::windows::Win32::Storage::FileSystem::{
                FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
            };
            use std::os::windows::io::AsRawHandle as _;
            let info = FILE_DISPOSITION_INFO { DeleteFile: true };
            let size = std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32;
            // SAFETY: the handle is `file`'s, opened with DELETE; `info` lives for the call and
            // `size` is its size.
            unsafe {
                SetFileInformationByHandle(
                    HANDLE(file.as_raw_handle()),
                    FileDispositionInfo,
                    (&raw const info).cast(),
                    size,
                )
            }
            .map_err(io::Error::other)
        }

        pub(super) fn open(root: &Path, names: &[&OsStr], make: bool) -> io::Result<Dir> {
            let mut path = root.to_path_buf();
            let mut held = vec![checked(&path, root, Kind::Dir)?];
            for name in names {
                path.push(name);
                let next = match checked(&path, root, Kind::Dir) {
                    Err(e) if make && e.kind() == io::ErrorKind::NotFound => make_dir(&path, root),
                    next => next,
                };
                held.push(next?);
            }
            Ok(Dir {
                path,
                root: root.to_path_buf(),
                _held: held,
            })
        }

        /// The new folder `path`, its parent held open. A file of the host's own sits in that
        /// parent meanwhile, and only an empty folder turns into a mount point: the new folder
        /// lands inside.
        fn make_dir(path: &Path, root: &Path) -> io::Result<File> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let pin = path.with_file_name(format!(".punktfunk-{}-{n}", std::process::id()));
            let _pin = checked(&pin, root, Kind::Pin)?;
            match std::fs::create_dir(path) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                _ => {}
            }
            checked(path, root, Kind::Dir)
        }

        pub(super) fn read(dir: &Dir, name: &OsStr) -> io::Result<File> {
            checked(&dir.path.join(name), &dir.root, Kind::Read)
        }

        /// A hard link at `dest` to `name` in `dir`. `_file` holds that name without delete
        /// sharing, as `dir` holds every folder above it, so the name still is that file.
        pub(super) fn hard_link(
            _file: &File,
            dir: &Dir,
            name: &OsStr,
            dest: &Path,
        ) -> io::Result<()> {
            std::fs::hard_link(dir.path.join(name), dest)
        }

        /// `from` renamed to `to`, both in `dir`, whose folders are all held open. A file or
        /// link at `to` is replaced itself, never what a link names.
        pub(super) fn rename(dir: &Dir, from: &OsStr, to: &OsStr) -> io::Result<()> {
            std::fs::rename(dir.path.join(from), dir.path.join(to))
        }

        /// The file or link `name` in `dir`, removed through its own handle.
        pub(super) fn remove(dir: &Dir, name: &OsStr) -> io::Result<()> {
            delete(&checked(&dir.path.join(name), &dir.root, Kind::Old)?)
        }

        /// What is there now goes through its own handle, opened inside the root, before the
        /// new file is made.
        pub(super) fn create(dir: &Dir, name: &OsStr) -> io::Result<File> {
            let path = dir.path.join(name);
            match checked(&path, &dir.root, Kind::Old) {
                Ok(old) => delete(&old)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            checked(&path, &dir.root, Kind::Create)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Windows launch can't set variables, and in the player's home it needs none.
    #[test]
    fn a_windows_launch_in_the_players_home_carries_no_variables() {
        let dir = tempfile::tempdir().unwrap();
        let h = Hermir::open(Options {
            prefix: Some(dir.path().join("emulators")),
            os: Some(hermir::Os::Windows),
            home: Some(dir.path().join("Users").join("player")),
            ..Options::default()
        })
        .unwrap();
        let copy = hermir::Install {
            emulator: "ryujinx".into(),
            kind: hermir::InstallKind::Native,
            exe: hermir::Exe::Path(r"E:\Ryubing\Ryujinx.exe".into()),
            version: None,
            config_root: None,
        };
        let request = hermir::LaunchRequest {
            file: Some(r"D:\Switch\Animal Crossing New Horizons.nsp".into()),
            platform: Some("switch".into()),
            fullscreen: Some(true),
            ..Default::default()
        };
        let mut spec = h
            .emulator("ryujinx")
            .unwrap()
            .launch(&copy, &request)
            .unwrap();
        assert!(spec.env.contains_key("APPDATA"));
        drop_home_vars(&mut spec);
        assert!(spec.env.is_empty(), "{:?}", spec.env);
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
        let guid = named_hidapi_guid(0x057e, 0x2009, &want).unwrap();
        assert_eq!(&guid[4..8], format!("{:02x}{:02x}", crc & 0xff, crc >> 8));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(pad_ref(GamepadPref::SwitchPro, 0).guid, Some(guid));
        assert_eq!(hid_of(sys.path(), "/devices/virtual/input/input3"), None);
    }

    // Desk listings are written from the kernel's key codes and drivers, not captured: an Xbox
    // pad under xpad, a DualSense under hid-playstation, a left Joy-Con under hid-nintendo
    // (BTN_TL, no BTN_SOUTH), a keyboard. Expected GUIDs are derived; each says how.
    const XPAD_KEYS: &str = "B: KEY=7cdb000000000000 0 0 0 0";
    const DS_KEYS: &str = "B: KEY=7fdb000000000000 0 0 0 0";
    const JOYCON_L_KEYS: &str = "B: KEY=f00000000 0 0 0 27e0000000000000 0 0 0 0";
    const KBD_KEYS: &str =
        "B: KEY=1000000000007 ff9f207ac14057ff febeffdfffefffff fffffffffffffffe";

    /// No pad is lent: every node opens.
    fn all_open(_: &Path) -> bool {
        true
    }

    /// A wired Xbox 360 pad's block at `sysfs`.
    fn xpad(sysfs: &str, event: u32, version: &str) -> String {
        format!(
            "I: Bus=0003 Vendor=045e Product=028e Version={version}\nN: Name=\"Microsoft X-Box 360 pad\"\nS: Sysfs={sysfs}\nH: Handlers=event{event} js{event}\n{XPAD_KEYS}\n"
        )
    }

    const USB_XPAD: &str = "/devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.0/input/input8";

    /// A HID pad's sysfs facts under `sys`: its HID name and hidraw node, and for a USB pad the
    /// USB device's manufacturer, product and release two folders up. Answers the folder that
    /// holds its input nodes.
    fn hid_pad(sys: &Path, hid: &str, name: &str, hidraw: &str, usb: Option<[&str; 3]>) -> String {
        let dir = sys.join(hid.trim_start_matches('/'));
        std::fs::create_dir_all(dir.join("hidraw").join(hidraw)).unwrap();
        std::fs::write(dir.join("uevent"), format!("HID_NAME={name}\n")).unwrap();
        if let Some(strings) = usb {
            let dev = dir.parent().unwrap().parent().unwrap();
            for (attr, value) in ["manufacturer", "product", "bcdDevice"].iter().zip(strings) {
                std::fs::write(dev.join(attr), format!("{value}\n")).unwrap();
            }
        }
        format!("{hid}/input")
    }

    #[test]
    fn a_physical_pad_is_the_desks_and_never_the_hosts() {
        let keyboard = format!("I: Bus=0003 Vendor=1532 Product=0241 Version=0111\nN: Name=\"Razer Keyboard\"\nS: Sysfs=/devices/pci0000:00/0000:00:14.0/usb1/1-4/1-4:1.2/0003:1532:0241.0003/input/input9\nH: Handlers=sysrq kbd leds event5 js5\n{KBD_KEYS}\n");
        let text = [
            xpad("/devices/virtual/input/input30", 12, "0110"),
            xpad("/devices/virtual/input/input29", 11, "0110"),
            xpad(USB_XPAD, 4, "0114"),
            keyboard,
        ]
        .join("\n");
        let (nowhere, none) = (Path::new("/nonexistent"), SdlFilter::default());
        let desk = desk_pads_in(&text, nowhere, &none, &all_open);
        assert_eq!(desk.len(), 1, "the keyboard's js node is no pad: {desk:?}");
        let pad = &desk[0];
        assert_eq!((pad.index, pad.bus, pad.guid.as_deref()), (0, 3, None));
        assert_eq!(pad.evdev.as_deref(), Some(Path::new("/dev/input/event4")));
        // Derived: hermir's evdev form of the host's 360 pad above, at this pad's version 0x0114.
        assert_eq!(pad.sdl_guid(true), "030081b85e0400008e02000014010000");
        assert_eq!(pad.sdl_name(), "Xbox 360 Controller");
        // Lent to a seat, its event node is closed to the host.
        let lent = |node: &Path| node != Path::new("/dev/input/event4");
        assert!(desk_pads_in(&text, nowhere, &none, &lent).is_empty());
    }

    #[test]
    fn a_dualsense_at_the_desk_is_named_on_usb_only() {
        let sys = tempfile::tempdir().unwrap();
        let usb = hid_pad(
            sys.path(),
            "/devices/pci0000:00/0000:00:14.0/usb1/1-3/1-3:1.3/0003:054C:0CE6.0015",
            "Sony Interactive Entertainment DualSense Wireless Controller",
            "hidraw4",
            Some([
                "Sony Interactive Entertainment",
                "DualSense Wireless Controller",
                "0100",
            ]),
        );
        let bt = hid_pad(
            sys.path(),
            "/devices/pci0000:00/0000:00:14.0/usb1/1-10/1-10:1.0/bluetooth/hci0/hci0:256/0005:054C:0CE6.0007",
            "DualSense Wireless Controller",
            "hidraw5",
            None,
        );
        let ds = "Vendor=054c Product=0ce6";
        let on_bt = format!(
            "I: Bus=0005 {ds} Version=8100\nN: Name=\"DualSense Wireless Controller\"\nS: Sysfs={bt}/input25\nH: Handlers=event20 js3\nB: PROP=0\n{DS_KEYS}\n\n\
             I: Bus=0005 {ds} Version=8100\nN: Name=\"DualSense Wireless Controller Motion Sensors\"\nS: Sysfs={bt}/input26\nH: Handlers=event21 js4\nB: PROP=40\n\n\
             I: Bus=0005 {ds} Version=8100\nN: Name=\"DualSense Wireless Controller Touchpad\"\nS: Sysfs={bt}/input27\nH: Handlers=event22 mouse2\nB: PROP=5\n"
        );
        let on_usb = format!(
            "I: Bus=0003 {ds} Version=8111\nN: Name=\"Sony Interactive Entertainment DualSense Wireless Controller\"\nS: Sysfs={usb}/input78\nH: Handlers=event10 js2\nB: PROP=0\n{DS_KEYS}\n"
        );
        let none = SdlFilter::default();
        let desk = desk_pads_in(&on_usb, sys.path(), &none, &all_open);
        let got: Vec<_> = desk
            .iter()
            .map(|p| (p.index, p.bus, p.sdl_guid(true), p.sdl_name()))
            .collect();
        // Derived: the usbip DualSense's GUID above, which SDL 3.4 reported; a pad on USB has the
        // same hidapi facts.
        let guid = "030057564c050000e60c000000016800".to_string();
        assert_eq!(got, [(0, 3, guid, "DualSense Wireless Controller".into())]);
        // On Bluetooth it has no GUID, so it binds nobody, alone or beside the USB one.
        let read = pads(&on_bt, sys.path(), &none, &all_open);
        assert_eq!(
            (read.len(), read[0].bus, read[0].guid.as_deref()),
            (1, 5, None)
        );
        assert!(desk_pads_in(&on_bt, sys.path(), &none, &all_open).is_empty());
        let both = [on_usb.as_str(), &on_bt].join("\n");
        assert_eq!(pads(&both, sys.path(), &none, &all_open).len(), 2);
        assert!(desk_pads_in(&both, sys.path(), &none, &all_open).is_empty());
        // SDL can't open a hidraw node the host can't: it reads the pad otherwise, unmodelled.
        let closed = |node: &Path| node != Path::new("/dev/hidraw4");
        let read = pads(&on_usb, sys.path(), &none, &closed);
        assert_eq!((read.len(), read[0].guid.as_deref()), (1, None));
        assert!(desk_pads_in(&on_usb, sys.path(), &none, &closed).is_empty());
    }

    #[test]
    fn a_joy_con_at_the_desk_leaves_every_pad_unbound() {
        let joycon = format!("I: Bus=0005 Vendor=057e Product=2006 Version=0001\nN: Name=\"Nintendo Switch Left Joy-Con\"\nS: Sysfs=/devices/pci0000:00/0000:00:14.0/usb1/1-10/1-10:1.0/bluetooth/hci0/hci0:257/0005:057E:2006.0008/input/input30\nH: Handlers=event23 js6\nB: PROP=0\n{JOYCON_L_KEYS}\n");
        let text = [xpad(USB_XPAD, 4, "0114"), joycon].join("\n");
        let (nowhere, none) = (Path::new("/nonexistent"), SdlFilter::default());
        let read = pads(&text, nowhere, &none, &all_open);
        assert_eq!(read.len(), 2, "a left Joy-Con is a pad without BTN_SOUTH");
        assert!(desk_pads_in(&text, nowhere, &none, &all_open).is_empty());
    }

    #[test]
    fn a_generic_pad_at_the_desk_leaves_every_pad_unbound() {
        let f310 = format!("I: Bus=0003 Vendor=046d Product=c21d Version=4014\nN: Name=\"Logitech Gamepad F310\"\nS: Sysfs=/devices/pci0000:00/0000:00:14.0/usb1/1-4/1-4:1.0/input/input9\nH: Handlers=event5 js5\n{XPAD_KEYS}\n");
        let (nowhere, none) = (Path::new("/nonexistent"), SdlFilter::default());
        let alone = desk_pads_in(&f310, nowhere, &none, &all_open);
        assert!(alone.is_empty(), "no SDL name the host knows: {alone:?}");
        let text = [xpad(USB_XPAD, 4, "0114"), f310].join("\n");
        assert_eq!(pads(&text, nowhere, &none, &all_open).len(), 2);
        assert!(desk_pads_in(&text, nowhere, &none, &all_open).is_empty());
    }

    /// The seven pads the host names are the kinds it makes; off USB, or a HIDAPI pad without
    /// its GUID, they are not named, and nothing else is.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn only_the_seven_modelled_pads_are_named() {
        use GamepadPref::*;
        for kind in [
            Xbox360,
            XboxOne,
            XboxElite,
            DualSense,
            DualSenseEdge,
            DualShock4,
            SwitchPro,
        ] {
            let pad = pad_ref(kind, 0);
            assert!(named(&pad), "{kind:?}");
            assert!(
                !named(&hermir::PadRef { bus: 5, ..pad }),
                "{kind:?} on Bluetooth"
            );
        }
        let ds = pad_ref(DualSense, 0);
        assert!(!named(&hermir::PadRef { guid: None, ..ds }));
        let steam = hermir::PadRef {
            name: "Microsoft X-Box 360 pad 0".into(),
            vendor: 0x28de,
            product: 0x11ff,
            ..hermir::PadRef::xbox360(0)
        };
        assert!(!named(&steam), "Steam's pad");
        let series = hermir::PadRef {
            product: 0x0b12,
            ..hermir::PadRef::xbox360(0)
        };
        assert!(!named(&series), "an Xbox pad hermir has no name for");
    }

    #[test]
    fn steams_filter_drops_a_hidden_pad_and_keeps_its_own() {
        let steam_pad = format!("I: Bus=0003 Vendor=28de Product=11ff Version=0001\nN: Name=\"Microsoft X-Box 360 pad 0\"\nS: Sysfs=/devices/virtual/input/input40\nH: Handlers=event15 js1\n{XPAD_KEYS}\n");
        let text = [xpad(USB_XPAD, 4, "0114"), steam_pad].join("\n");
        let nowhere = Path::new("/nonexistent");
        let ids = |f: &SdlFilter| -> Vec<(u16, u16)> {
            let read = pads(&text, nowhere, f, &all_open);
            read.iter().map(|p| (p.vendor, p.product)).collect()
        };
        let steam = SdlFilter::parse("0x045e/0x028e,0x054c/0x0ce6", "").unwrap();
        assert_eq!(ids(&steam), [(0x28de, 0x11ff)]);
        assert_eq!(
            ids(&SdlFilter::default()),
            [(0x045e, 0x028e), (0x28de, 0x11ff)]
        );
        let except = SdlFilter::parse("", "0x054c/0x0ce6").unwrap();
        assert_eq!(
            ids(&except),
            [(0x28de, 0x11ff)],
            "only its list, and Steam's pad"
        );
        // Steam's pad passes the filter but is not one the host names: the set stays unbound.
        assert!(desk_pads_in(&text, nowhere, &steam, &all_open).is_empty());
        // SDL's scan: pairs in turn, an odd one left over, 16 bits each; a file it isn't read.
        let odd = SdlFilter::parse("0x1045E/0x028e,0x054c", "").unwrap();
        assert_eq!(odd.ignore, [(0x045e, 0x028e)]);
        assert!(SdlFilter::parse("@/home/deck/ignore.txt", "").is_none());
    }

    #[test]
    fn every_pad_at_the_desk_is_a_player_and_connected() {
        let second = hermir::PadRef {
            index: 1,
            ..hermir::PadRef::xbox360(0)
        };
        let pads = [hermir::PadRef::xbox360(0), second];
        let patch = desk_patch(&pads);
        let seats: Vec<_> = patch
            .players
            .iter()
            .flatten()
            .map(|p| (p.seat, p.pad.index))
            .collect();
        assert_eq!(seats, [(1, 0), (2, 1)]);
        assert_eq!(patch.connected.as_deref(), Some(&pads[..]));
        patch.validate().unwrap();
    }

    #[test]
    fn no_pad_at_the_desk_seats_nobody() {
        let none = SdlFilter::default();
        assert!(desk_pads_in("", Path::new("/nonexistent"), &none, &all_open).is_empty());
        assert_eq!(desk_patch(&[]), hermir::Patch::default());
    }

    /// Every pad the session declared is a player, as the kind it declared, in wire order, built
    /// or not: no default stands in for a pad whose first frame has not come yet.
    #[test]
    fn a_session_seats_each_pad_it_declared_as_its_own_kind() {
        let feed = crate::pad_feed::PadFeed::new();
        feed.note_pad(2, Some((GamepadPref::SwitchPro, None)));
        feed.note_pad(0, Some((GamepadPref::DualSense, Some(1))));
        let seats = session_players(GamepadPref::Xbox360, Some(&feed), None);
        let got: Vec<_> = seats
            .iter()
            .map(|p| (p.seat, p.pad.index, p.pad.vendor, p.pad.product))
            .collect();
        assert_eq!(got, [(1, 0, 0x054c, 0x0ce6), (2, 1, 0x057e, 0x2009)]);
        assert_eq!(seats[0].pad.sdl_name(), "DualSense Wireless Controller");
        assert_eq!(seats[1].pad.sdl_name(), "Nintendo Switch Pro Controller");
    }

    #[test]
    fn a_session_that_declares_no_pad_seats_one_of_its_default_kind() {
        let seats = session_players(GamepadPref::DualSense, None, None);
        assert_eq!(seats.len(), 1);
        assert_eq!((seats[0].seat, seats[0].pad.product), (1, 0x0ce6));
        let quiet = crate::pad_feed::PadFeed::new();
        let seats = session_players(GamepadPref::SwitchPro, Some(&quiet), None);
        assert_eq!((seats.len(), seats[0].pad.product), (1, 0x2009));
    }

    /// A device's pads are kept per device, most recent first: a session without one keeps the
    /// last, and only the most recent devices stay.
    #[test]
    fn each_device_keeps_the_pads_it_brought_last() {
        use GamepadPref::*;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAD_MEMORY);
        assert!(remembered_pads_in(&path, "deck").is_empty());
        remember_pads_in(&path, "deck", &[SteamDeck]).unwrap();
        remember_pads_in(&path, "laptop", &[SwitchPro, DualSense]).unwrap();
        remember_pads_in(&path, "deck", &[]).unwrap();
        assert_eq!(remembered_pads_in(&path, "deck"), [SteamDeck]);
        assert_eq!(remembered_pads_in(&path, "laptop"), [SwitchPro, DualSense]);
        for n in 0..PAD_MEMORY_DEVICES {
            remember_pads_in(&path, &format!("other{n}"), &[Xbox360]).unwrap();
        }
        assert!(
            remembered_pads_in(&path, "deck").is_empty(),
            "the oldest dropped"
        );
        assert_eq!(pad_memory(&path).len(), PAD_MEMORY_DEVICES);
    }

    /// Every kind a host builds survives the memory's spelling of it.
    #[test]
    fn every_kind_reads_back_as_itself() {
        for n in 0..=u8::MAX {
            let kind = GamepadPref::from_u8(n);
            assert_eq!(
                GamepadPref::from_name(kind.as_str()),
                Some(kind),
                "{kind:?}"
            );
        }
    }

    /// Each kind's GUID is the one Lichii's SDL 3 logged for the pad the host built, on Bazzite
    /// (2026-11-24): uhid pads at slot 0, the Valve pads over usbip.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_pad_kind_wears_the_guid_sdl_reported_for_it() {
        use GamepadPref::*;
        for (kind, guid) in [
            (Xbox360, "030081b85e0400008e02000010010000"),
            (XboxOne, "0300e8fb5e040000ea02000008040000"),
            (XboxElite, "030010f85e040000000b000011050000"),
            (DualSense, "030057564c050000e60c000000006800"),
            (DualSenseEdge, "0300e0274c050000f20d000000006800"),
            (DualShock4, "03008fe54c050000cc09000000006800"),
            (SwitchPro, "030025b77e0500000920000000006803"),
            (SteamDeck, "0300f617de2800000512000000036800"),
            (SteamController, "03002854de2800000211000000036800"),
            (SteamController2, "03002854de2800000213000007036800"),
            (SteamController2Puck, "03002854de2800000413000002006800"),
            (EightBitDoUltimate2, "05006042c82d00001260000000006800"),
            (EightBitDoPro2, "030095acc82d00000360000000006800"),
            (EightBitDoPro3, "0300546cc82d00000960000000006800"),
            (HoripadSteam, "030065120d0f0000ab01000000006800"),
        ] {
            assert_eq!(pad_ref(kind, 0).sdl_guid(true), guid, "{kind:?}");
        }
        assert_eq!(pad_ref(SteamDeck, 0).sdl_name(), "Steam Deck Controller");
    }

    /// SDL reads a Mac's pads through HIDAPI: an Xbox pad on Bluetooth with its GUID's CRC of
    /// hidapi's strings, a DualSense on USB with the CRC of the name SDL renames it to.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_session_seats_its_iohid_pads_as_sdl_reads_them() {
        let crc = |s: &str| {
            let c = crc16(s.as_bytes());
            format!("{:02x}{:02x}", c & 0xff, c >> 8)
        };
        let xbox = pad_ref(GamepadPref::Xbox360, 0);
        let guid = xbox.guid.clone().unwrap();
        assert_eq!((xbox.bus, xbox.vendor, xbox.product), (5, 0x045e, 0x0b13));
        assert_eq!(&guid[..4], "0500");
        assert_eq!(&guid[4..8], crc("Microsoft Xbox Wireless Controller"));
        assert_eq!(&guid[8..24], "5e040000130b0000");
        assert!(guid.ends_with("6800"));
        // What SDL 3.4 reported for a USB DualSense of the same strings and release.
        let ds = pad_ref(GamepadPref::DualSense, 0).guid;
        assert_eq!(ds.as_deref(), Some("030057564c050000e60c000000016800"));
    }

    #[test]
    fn only_a_flathub_channel_installs_box_wide() {
        let catalog = hermir::Catalog::embedded().unwrap();
        let channel = |id: &str| box_flatpak(catalog.get(id).unwrap());
        assert_eq!(channel("pcsx2"), Some("net.pcsx2.PCSX2"));
        assert_eq!(channel("retroarch"), Some("org.libretro.RetroArch"));
        assert_eq!(channel("supermodel"), Some("com.supermodel3.Supermodel"));
        for own in ["rpcs3", "shadps4", "vita3k", "xenia-canary"] {
            assert_eq!(channel(own), None, "{own}");
        }
    }

    #[test]
    fn staged_firmware_stays_inside_the_plugins_own_state() {
        let base = tempfile::tempdir().unwrap();
        let firmware = base.path().join("plugin-state/rom-manager/firmware");
        std::fs::create_dir_all(firmware.join("ps2")).unwrap();
        let found = |provider, platform| staged_firmware_in(base.path(), provider, platform);
        assert!(found("rom-manager", "ps2").is_some());
        assert!(found("rom-manager", "psx").is_none());
        assert!(found("other", "ps2").is_none());
        assert!(found("rom-manager", "..").is_none());
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), firmware.join("psx")).unwrap();
            assert!(found("rom-manager", "psx").is_none());
            // A state folder the person moved elsewhere by a link is read where it lies.
            let elsewhere = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(elsewhere.path().join("firmware/ps2")).unwrap();
            std::os::unix::fs::symlink(elsewhere.path(), base.path().join("plugin-state/linked"))
                .unwrap();
            assert!(found("linked", "ps2").is_some());
            assert!(found("linked/firmware", "..").is_none());
        }
    }

    /// Content a plugin hands over is mirrored with its title folder, and a link the plugin
    /// planted, at the file or inside the folder, brings nothing from outside.
    #[cfg(unix)]
    #[test]
    fn a_content_mirror_takes_nothing_through_a_link() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let (state, outside) = (tmp.path().join("state"), tmp.path().join("outside"));
        let title = state.join("dl/title");
        std::fs::create_dir_all(title.join("meta")).unwrap();
        std::fs::create_dir_all(title.join("code")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"theirs").unwrap();
        std::fs::write(title.join("meta/meta.xml"), b"<title_id>0</title_id>").unwrap();
        std::fs::write(title.join("code/app.rpx"), b"app").unwrap();
        symlink(outside.join("secret"), title.join("code/stolen")).unwrap();
        symlink(outside.join("secret"), state.join("dl/key.rap")).unwrap();
        std::fs::write(state.join("dl/plain.rap"), b"rap").unwrap();
        let state = state.canonicalize().unwrap();
        let into = tmp.path().join("mirror");

        let at = handover::mirror(&state, Path::new("dl/title/code/app.rpx"), &into).unwrap();
        assert_eq!(at, into.join("dl/title/code/app.rpx"));
        assert_eq!(std::fs::read(&at).unwrap(), b"app");
        assert!(into.join("dl/title/meta/meta.xml").is_file());
        assert!(!into.join("dl/title/code/stolen").exists());

        let rap = handover::mirror(&state, Path::new("dl/plain.rap"), &into).unwrap();
        assert_eq!(std::fs::read(&rap).unwrap(), b"rap");
        assert!(handover::mirror(&state, Path::new("dl/key.rap"), &into).is_err());
        assert!(!into.join("dl/key.rap").exists());

        // A file an emulator keeps by its path is handed over where it lies, held; a link or a
        // folder is not.
        let held = handover::hold(&state, Path::new("dl/plain.rap")).unwrap();
        assert_eq!(held.path(), state.join("dl/plain.rap"));
        assert!(handover::hold(&state, Path::new("dl/key.rap")).is_err());
        assert!(handover::hold(&state, Path::new("dl/title")).is_err());
        drop(held);

        // A file the plugin replaced under the same name is mirrored anew.
        std::fs::remove_file(state.join("dl/plain.rap")).unwrap();
        std::fs::write(state.join("dl/plain.rap"), b"rap v2").unwrap();
        handover::mirror(&state, Path::new("dl/plain.rap"), &into).unwrap();
        assert_eq!(std::fs::read(&rap).unwrap(), b"rap v2");
    }

    /// A plugin's link in its state folder reaches nothing outside it: a placed file replaces
    /// the link, a linked folder or file is refused, and staged firmware leaves links out.
    #[cfg(unix)]
    #[test]
    fn a_link_in_a_state_folder_reaches_nothing_outside() {
        use std::ffi::OsStr;
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let (state, outside) = (tmp.path().join("state"), tmp.path().join("outside"));
        std::fs::create_dir_all(state.join("out")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("hooks.json"), b"theirs").unwrap();
        symlink(outside.join("hooks.json"), state.join("out/Mcd001.ps2")).unwrap();
        symlink(&outside, state.join("linked")).unwrap();
        symlink(outside.join("hooks.json"), state.join("from.ps2")).unwrap();
        std::fs::write(state.join("plain.ps2"), b"plain").unwrap();
        let state = state.canonicalize().unwrap();
        let scratch = tmp.path().join("scratch");

        let exported = handover::Scratch::new_in(&scratch).unwrap();
        let unit = exported.path().join("Mcd001.ps2");
        std::fs::write(&unit, b"save").unwrap();
        let out = handover::Dir::open(&state, Path::new("out"), true).unwrap();
        handover::place(&unit, &out).unwrap();
        assert_eq!(
            std::fs::read(outside.join("hooks.json")).unwrap(),
            b"theirs"
        );
        assert_eq!(
            std::fs::read(state.join("out/Mcd001.ps2")).unwrap(),
            b"save"
        );
        for rel in ["linked", "linked/new", "../outside"] {
            assert!(
                handover::Dir::open(&state, Path::new(rel), true).is_err(),
                "{rel}"
            );
        }
        assert!(!outside.join("new").exists());

        let own = handover::Dir::open(&state, Path::new(""), false).unwrap();
        assert!(own.read(OsStr::new("from.ps2")).is_err());
        assert!(own.read(OsStr::new("out/Mcd001.ps2")).is_err());
        let staged = handover::staged_in(&own, &scratch).unwrap();
        let names: Vec<_> = std::fs::read_dir(staged.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names, ["plain.ps2"]);
    }

    #[test]
    fn an_exit_reverts_only_the_bindings_its_own_launch_still_holds() {
        let mut out = Bindings {
            prepared: 0,
            owner: None,
        };
        let a = Some(out.prepared_for(Some("emu-a")));
        let b = Some(out.prepared_for(Some("emu-b")));
        assert!(!out.reverts_for(a), "A's exit leaves B's bindings");
        assert!(
            !out.reverts_for(None),
            "a launch that prepared nothing leaves them"
        );
        assert!(out.reverts_for(b));
        // The same title launched again: the first launch's exit leaves the second's.
        let again = Some(out.prepared_for(Some("emu-b")));
        assert!(!out.reverts_for(b));
        assert!(out.reverts_for(again));
        // A prepare outside a launch is any exit's to revert.
        out.prepared_for(None);
        assert!(out.reverts_for(None) && out.reverts_for(a));
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
