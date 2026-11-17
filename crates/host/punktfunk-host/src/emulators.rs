//! Managed emulators: hermir, opened on this host's prefix. A plugin only asks; an install runs
//! on the operator's click and lands under `<prefix>/<id>/app`, the folder the plugin
//! is then granted, so its launch templates may point inside it.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use hermir::progress::Quiet;
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
pub fn install(id: &str) -> hermir::Result<Installed> {
    let h = open()?;
    let emulator = h.emulator(id)?;
    let home = home_of(id);
    crate::plugins::open_for_players(&home);
    let row = match box_flatpak(emulator.entry()).filter(|_| box_wide()) {
        Some(app) => install_box_wide(&h, id, app)?,
        None => emulator.install(&Quiet)?,
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
    let version = match supervisor(id, Command::InstallFlatpak { id: app.into() })? {
        CommandResult::Flatpak { version, .. } => version,
        other => return Err(place(id, format!("the seat supervisor answered {other:?}"))),
    };
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

/// A libretro core from the buildbot into that folder.
pub fn install_core(core: &str) -> hermir::Result<PathBuf> {
    open()?.install_core(core, &Quiet)
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

/// Puts the emulators' own bindings back after a game, file by file, when they are the exiting
/// launch's (`held`, from its [`prepare_launch`]) or no launch's: a later launch's prepare
/// already put this one's back and wrote its own. Forced: an emulator rewrites its config on
/// exit, and that is no edit of the player's. Nothing outstanding is a quiet no-op.
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
/// `let_player_read`), and has the session's pads written in seat order (steps of kind
/// `players`; reverted after the game): see [`session_players`] for `pad` and `slots`.
/// `platform` is a catalog id or any of its aliases (RomM slug, ES-DE folder, libretro name).
/// Blocking: an installer may run. The bindings belong to no launch: any game's exit reverts
/// them.
pub fn prepare(
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
    pad: GamepadPref,
    slots: Option<&AtomicU16>,
) -> hermir::Result<Vec<(String, hermir::Prepared)>> {
    prepare_as(None, &mut None, id, platform, firmware_dir, pad, slots)
}

/// [`prepare`], its bindings `owner`'s: the library id of the launch they are for. `held` gets
/// the count that names them before anything is written.
fn prepare_as(
    owner: Option<&str>,
    held: &mut Option<u64>,
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
    pad: GamepadPref,
    slots: Option<&AtomicU16>,
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

/// Whether `id` keeps the path of a `kind` add-on of `platform`: Ryujinx registers the file
/// where it lies; every other way copies it in or runs an installer on it.
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
    matches!(method, hermir::ContentMethod::Adapter { adapter, .. } if adapter == "ryujinx")
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
/// platform's firmware from the plugin's `firmware/<platform>` when it staged some, the session's
/// pads, which only this launch's exit reverts: their count comes back, for [`revert_players`].
/// What it did goes to the log. A launch waits for the client's pads, at most [`SETTLE_ALL`], and
/// never on the rest to succeed.
pub fn prepare_launch(
    library_id: &str,
    pad: GamepadPref,
    slots: Option<&AtomicU16>,
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
            match install_core(core) {
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
    match prepare_as(
        Some(library_id),
        &mut held,
        &spec.value,
        platform,
        staged.as_ref().map(handover::Scratch::path),
        pad,
        slots,
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
    fn only_a_flathub_channel_installs_box_wide() {
        let catalog = hermir::Catalog::embedded().unwrap();
        let channel = |id: &str| box_flatpak(catalog.get(id).unwrap());
        assert_eq!(channel("pcsx2"), Some("net.pcsx2.PCSX2"));
        assert_eq!(channel("retroarch"), Some("org.libretro.RetroArch"));
        for own in ["rpcs3", "shadps4", "supermodel", "vita3k", "xenia-canary"] {
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
