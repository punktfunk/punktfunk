//! Desk pads lent to a seat: a USB game controller plugged into the box that only its holder's
//! account opens. One record per lent key in [`DIR`], root's alone: the holder's account while
//! its seat runs, [`PARKED`] while it doesn't. The directory exists only while a lend does, and
//! udev's rule line runs [`super::node_owner`] on USB nodes only while it exists.
//!
//! A change of holder rewrites the record, then unbinds the device from its driver and binds
//! it back: every open handle dies, raw USB too, and its nodes come back under the new record. What may be
//! lent is read from sysfs and udev's database, never from the caller.

use super::node_owner::made_devices::put;
use crate::backend::{pad_handover, pad_not_a_controller, pad_not_present};
use crate::backend::{BackendError, PresentPad};
use crate::model::{pad_name, valid_pad_key, Ledger, LentPad, RuntimeState, Seat};
use std::collections::BTreeMap;
use std::io::{self, ErrorKind};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub const DIR: &str = "/run/punktfunk/pads/lent";
const SYS: &str = "/sys";
/// udev's database: `+input:inputN` holds an input device's properties.
const UDEV_DATA: &str = "/run/udev/data";
/// A lent pad's record while its seat doesn't run.
pub(super) const PARKED: &str = "-";
/// Receivers whose pads appear only once one wakes: the Steam Controller Puck, and the 2015
/// Steam Controller's dongle.
const RECEIVERS: [(u16, u16); 3] = [(0x28de, 0x1304), (0x28de, 0x1305), (0x28de, 0x1142)];
const HUB_CLASS: u8 = 0x09;

pub use crate::model::pad_interface_allowed as interface_allowed;

/// Whether an interface is a boot keyboard or a boot mouse.
pub(super) fn boot_input(class: [u8; 3]) -> bool {
    matches!(class, [0x03, 0x01, 0x01 | 0x02])
}

/// Whether a seat in `state` holds its lent pads live.
pub(super) fn runs(state: &RuntimeState) -> bool {
    matches!(state, RuntimeState::Running | RuntimeState::Starting)
}

/// Whether `dir` is a directory root's alone: the udev helper trusts no record anywhere else.
pub(super) fn trusted(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir)
        .is_ok_and(|meta| meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o077 == 0)
}

/// A lend's record: its holder, an account or [`PARKED`], then the port the lend found its
/// pad in.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Record {
    pub holder: String,
    pub port: Option<String>,
}

impl Record {
    fn of(holder: &str, port: Option<&str>) -> Record {
        Record {
            holder: holder.to_owned(),
            port: port.map(str::to_owned),
        }
    }

    /// The record's file: the holder's line, then the port's.
    fn text(&self) -> String {
        match &self.port {
            Some(port) => format!("{}\n{port}", self.holder),
            None => self.holder.clone(),
        }
    }
}

/// `dir`'s record of `key`.
pub(super) fn record(dir: &Path, key: &str) -> Option<Record> {
    let text = std::fs::read_to_string(dir.join(key)).ok()?;
    let mut lines = text.lines().map(str::trim);
    let holder = lines.next()?.to_owned();
    let port = lines.next().filter(|port| !port.is_empty());
    Some(Record::of(&holder, port))
}

/// The holder `records` names for `device`: the first of its keys with a record whose lend
/// names this device and not a twin of it ([`Usb::lent_as`]).
pub(super) fn holder_of(sys: &Path, records: &Path, device: &Usb) -> Option<String> {
    let mut all = None;
    device.candidates().find_map(|key| {
        let record = record(records, &key)?;
        let all = all.get_or_insert_with(|| present(sys));
        device
            .lent_as(&key, record.port.as_deref(), all)
            .then_some(record.holder)
    })
}

/// A USB device on a real bus, as sysfs shows it.
pub(super) struct Usb {
    dir: PathBuf,
    vid: u16,
    pid: u16,
    class: Option<u8>,
    /// Present only when it could be a key: 1 to 64 of `[A-Za-z0-9._-]`.
    serial: Option<String>,
    /// `<host controller>-<port chain>`, cleaned for a key.
    port: Option<String>,
    name: String,
}

impl Usb {
    /// The device at `rel` under `sys`, unless it is virtual, under `vhci_hcd` or a root hub.
    fn read(sys: &Path, rel: &Path) -> Option<Usb> {
        let names: Vec<&str> = rel.iter().map(|n| n.to_str()).collect::<Option<_>>()?;
        let busid = *names.last()?;
        let virtual_bus = names.starts_with(&["devices", "virtual"])
            || names.iter().any(|n| n.starts_with("vhci_hcd"));
        if virtual_bus || is_bus(busid) {
            return None;
        }
        let dir = sys.join(rel);
        let attr = |name: &str| {
            let text = std::fs::read_to_string(dir.join(name)).ok()?;
            Some(text.trim().to_owned()).filter(|t| !t.is_empty())
        };
        let hex = |name: &str| u16::from_str_radix(&attr(name)?, 16).ok();
        let serial = attr("serial").filter(|s| {
            s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        });
        let port = names
            .iter()
            .position(|n| is_bus(n))
            .filter(|&at| at > 0)
            .and_then(|at| {
                let controller: String = (names[at - 1].chars())
                    .map(|c| match c {
                        'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' => c,
                        _ => '_',
                    })
                    .collect();
                let (_, chain) = busid.split_once('-')?;
                Some(format!("{controller}-{chain}"))
            });
        Some(Usb {
            vid: hex("idVendor")?,
            pid: hex("idProduct")?,
            class: attr("bDeviceClass").and_then(|c| u8::from_str_radix(&c, 16).ok()),
            serial,
            port,
            name: pad_name(
                &attr("product")
                    .or_else(|| attr("manufacturer"))
                    .unwrap_or_default(),
            ),
            dir,
        })
    }

    fn key(&self, kind: char, tail: &str) -> Option<String> {
        let key = format!("usb-{:04x}-{:04x}-{kind}-{tail}", self.vid, self.pid);
        valid_pad_key(&key).then_some(key)
    }

    fn serial_key(&self) -> Option<String> {
        self.key('s', self.serial.as_deref()?)
    }

    fn port_key(&self) -> Option<String> {
        self.key('p', self.port.as_deref()?)
    }

    /// The keys a lend of this device may name: its serial's, then its port's.
    pub(super) fn candidates(&self) -> impl Iterator<Item = String> {
        [self.serial_key(), self.port_key()].into_iter().flatten()
    }

    /// The key a lend mints: the serial's, unless another device in `present` has the same ids
    /// and serial, then the port's.
    fn minted(&self, present: &[Usb]) -> Option<String> {
        let twin = |o: &&Usb| o.vid == self.vid && o.pid == self.pid && o.serial == self.serial;
        let serial = if present.iter().filter(twin).count() > 1 {
            None
        } else {
            self.serial_key()
        };
        serial.or_else(|| self.port_key())
    }

    /// Whether a lend of `key` that found its pad in `port` names this device: it carries the
    /// key, and it sits in `port` or no other device in `present` carries the key too. Two
    /// pads of one model can report one serial; the port tells them apart while both are in.
    pub(super) fn lent_as(&self, key: &str, port: Option<&str>, present: &[Usb]) -> bool {
        let carries = |usb: &Usb| usb.candidates().any(|k| k == key);
        let here = port.is_some() && self.port.as_deref() == port;
        let twin = || {
            let other = |usb: &&Usb| usb.dir.file_name() != self.dir.file_name();
            present.iter().filter(other).any(carries)
        };
        carries(self) && (here || !twin())
    }

    fn pad(&self, key: String) -> PresentPad {
        PresentPad {
            key,
            name: self.name.clone(),
            vendor_id: self.vid,
            product_id: self.pid,
            port: self.port.clone(),
        }
    }

    /// The class, subclass and protocol of each interface; `None` when one can't be read.
    fn interfaces(&self) -> Option<Vec<[u8; 3]>> {
        let prefix = format!("{}:", self.dir.file_name()?.to_str()?);
        std::fs::read_dir(&self.dir)
            .ok()?
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .map(|e| interface(&e.path()))
            .collect()
    }
}

/// `usbN`, a root hub's name.
fn is_bus(name: &str) -> bool {
    name.strip_prefix("usb")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// An interface's class, subclass and protocol.
fn interface(dir: &Path) -> Option<[u8; 3]> {
    let byte = |name: &str| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        u8::from_str_radix(text.trim(), 16).ok()
    };
    Some([
        byte("bInterfaceClass")?,
        byte("bInterfaceSubClass")?,
        byte("bInterfaceProtocol")?,
    ])
}

/// Every USB device plugged into a real bus, root hubs aside.
fn present(sys: &Path) -> Vec<Usb> {
    let Ok(root) = sys.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(root.join("bus/usb/devices")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().contains(':'))
        .filter_map(|e| e.path().canonicalize().ok())
        .filter_map(|dir| Usb::read(&root, dir.strip_prefix(&root).ok()?))
        .collect()
}

/// The USB device the node at `devpath` lies in, the nearest directory with `idVendor`, and
/// the class of the interface the node lies on, when it lies on one.
pub(super) fn node_device(sys: &Path, devpath: &str) -> Option<(Usb, Option<[u8; 3]>)> {
    let rel = Path::new(devpath.trim_start_matches('/'));
    let device = rel
        .ancestors()
        .find(|at| !at.as_os_str().is_empty() && sys.join(at).join("idVendor").is_file())?;
    let usb = Usb::read(sys, device)?;
    let on = rel.strip_prefix(device).ok()?.iter().next();
    let class = on.and_then(|name| interface(&usb.dir.join(name)));
    Some((usb, class))
}

/// Calls `visit` on `dir` and every directory under it. Links are not followed.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    visit(dir);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&entry.path(), visit);
        }
    }
}

/// Ends every handle on the device at `dev` and makes its nodes again: unbinds it from its
/// driver, then binds it back. The unbind ends usbfs handles too, which `authorized` keeps.
/// A `change` in between has the rules run on its own node: a new node takes the seat udev
/// last gave its device, and with it that seat's session's access. Once unbound, the bind runs
/// whatever the `change` did, or the pad stays gone until it is plugged in again.
fn cycle(dev: &Path) -> io::Result<()> {
    let driver = dev.join("driver").canonicalize()?;
    let name = dev
        .file_name()
        .ok_or(ErrorKind::InvalidInput)?
        .as_encoded_bytes();
    std::fs::write(driver.join("unbind"), name)?;
    let changed = std::fs::write(dev.join("uevent"), "change");
    std::fs::write(driver.join("bind"), name)?;
    changed
}

/// Has udev apply its rules again to the device at `dev` and everything under it.
fn refresh(dev: &Path) {
    walk(dev, &mut |dir| {
        let uevent = dir.join("uevent");
        if uevent.is_file()
            && let Err(error) = std::fs::write(&uevent, "change")
        {
            tracing::debug!(%error, dir = %dir.display(), "lent pad node not refreshed");
        }
    });
}

/// Makes `dir` hold exactly `want`, key to holder, and answers each key whose record changed or
/// went. With nothing to hold, `dir` goes.
fn write_all(dir: &Path, want: &BTreeMap<&str, Record>) -> io::Result<Vec<String>> {
    let had: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    if want.is_empty() {
        return match std::fs::remove_dir_all(dir) {
            Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
            _ => Ok(had),
        };
    }
    let mut changed = Vec::new();
    for (key, wanted) in want {
        if record(dir, key).as_ref() != Some(wanted) {
            put(dir, key, &wanted.text())?;
            changed.push((*key).to_owned());
        }
    }
    for name in had {
        if !want.contains_key(name.as_str()) {
            std::fs::remove_file(dir.join(&name))?;
            changed.push(name);
        }
    }
    Ok(changed)
}

/// What a row's record says: its account while its seat runs.
fn holder(seat: &Seat, live: bool) -> &str {
    if live {
        &seat.account
    } else {
        PARKED
    }
}

fn handover(step: &str, error: &io::Error) -> BackendError {
    tracing::warn!(%error, step, "desk pad not handed over");
    pad_handover()
}

/// The lend records and the hardware they name, all under one root: `/` in service.
pub(super) struct Lent {
    sys: PathBuf,
    udev: PathBuf,
    dir: PathBuf,
    /// Held across each record change and the cycle after it, so two never cross.
    lock: Mutex<()>,
}

impl Lent {
    pub(super) fn under(root: &Path) -> Lent {
        let at = |path: &str| root.join(path.trim_start_matches('/'));
        Lent {
            sys: at(SYS),
            udev: at(UDEV_DATA),
            dir: at(DIR),
            lock: Mutex::new(()),
        }
    }

    fn hold(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `usb` may be lent: no hub, at least one interface and each a pad's
    /// ([`interface_allowed`]), and a game controller: a joystick input device under it, or a
    /// receiver whose pads appear later.
    fn lendable(&self, usb: &Usb) -> bool {
        let classes = usb.interfaces().unwrap_or_default();
        let pads_only =
            !classes.is_empty() && classes.iter().all(|&[c, s, p]| interface_allowed(c, s, p));
        let controller = RECEIVERS.contains(&(usb.vid, usb.pid)) || self.joystick(&usb.dir);
        usb.class.is_some_and(|c| c != HUB_CLASS) && pads_only && controller
    }

    /// Whether udev classes an input device under `dev` as a joystick.
    fn joystick(&self, dev: &Path) -> bool {
        let mut found = false;
        walk(dev, &mut |dir| {
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            found |= name.starts_with("input")
                && std::fs::read_to_string(self.udev.join(format!("+input:{name}")))
                    .is_ok_and(|db| db.lines().any(|l| l == "E:ID_INPUT_JOYSTICK=1"));
        });
        found
    }

    /// Every plugged-in device that may be lent, under the key a lend would mint, and every
    /// plugged-in one a row of `ledger` lends, under the key it lends it by.
    pub(super) fn desk_pads(&self, ledger: &Ledger) -> Vec<PresentPad> {
        let all = present(&self.sys);
        let lends = || ledger.seats.iter().flat_map(|seat| &seat.lent);
        let listed = |usb: &Usb| {
            let lent = lends().find(|pad| usb.lent_as(&pad.key, pad.port.as_deref(), &all));
            match lent {
                Some(pad) => Some(pad.key.clone()),
                None if self.lendable(usb) => usb.minted(&all),
                None => None,
            }
        };
        all.iter()
            .filter_map(|usb| Some(usb.pad(listed(usb)?)))
            .collect()
    }

    /// The plugged-in pad `key` names, through the gate. A key a row of `ledger` lends names
    /// the pad that lend does. A serial two plugged-in devices share names neither: each is
    /// listed, and lent, by its port.
    pub(super) fn lendable_pad(
        &self,
        key: &str,
        ledger: &Ledger,
    ) -> Result<PresentPad, BackendError> {
        let all = present(&self.sys);
        let mut lends = ledger.seats.iter().flat_map(|seat| &seat.lent);
        let port = lends
            .find(|pad| pad.key == key)
            .and_then(|pad| pad.port.as_deref());
        let Some(named) = all.iter().find(|usb| usb.lent_as(key, port, &all)) else {
            return Err(pad_not_present());
        };
        if !self.lendable(named) {
            return Err(pad_not_a_controller());
        }
        Ok(named.pad(key.to_owned()))
    }

    /// Every record as `ledger` has it, live by each row's state, then a cycle of each plugged-in
    /// device one of `keys` names whose record changed. A record already as the ledger has it
    /// was set live or parked, and its device cycled, by a list that ran in between.
    pub(super) fn settle(&self, ledger: &Ledger, keys: &[String]) -> Result<(), BackendError> {
        let _held = self.hold();
        let want: BTreeMap<&str, Record> = ledger
            .seats
            .iter()
            .flat_map(|seat| {
                let holder = holder(seat, runs(&seat.runtime.state));
                let of = move |pad: &LentPad| Record::of(holder, pad.port.as_deref());
                seat.lent.iter().map(move |pad| (pad.key.as_str(), of(pad)))
            })
            .collect();
        // The port each key's record remembers before the change and after: the device a
        // key leaves and the one it enters.
        let ports: Vec<(&String, [Option<String>; 2])> = keys
            .iter()
            .map(|key| {
                let was = record(&self.dir, key).and_then(|r| r.port);
                let is = want.get(key.as_str()).and_then(|r| r.port.clone());
                (key, [was, is])
            })
            .collect();
        let changed =
            write_all(&self.dir, &want).map_err(|e| handover("write the lend records", &e))?;
        let all = present(&self.sys);
        let moved = |usb: &Usb| {
            ports.iter().any(|(key, ports)| {
                let named = |port: &Option<String>| usb.lent_as(key, port.as_deref(), &all);
                changed.contains(key) && ports.iter().any(named)
            })
        };
        for usb in all.iter().filter(|usb| moved(usb)) {
            cycle(&usb.dir).map_err(|e| handover("cycle the device", &e))?;
        }
        Ok(())
    }

    /// Brings each of `seat`'s lent pads live or parked; a record that already says so changes
    /// nothing. At `starting`, a parked pad going live has udev's rules re-run: only root held
    /// its nodes. Any other change cycles the device. A failure is logged, never returned.
    pub(super) fn set_live(&self, seat: &Seat, live: bool, starting: bool) {
        if seat.lent.is_empty() {
            return;
        }
        let _held = self.hold();
        let holder = holder(seat, live);
        let mut all = None;
        for pad in &seat.lent {
            let port = pad.port.as_deref();
            let want = Record::of(holder, port);
            let was = record(&self.dir, &pad.key);
            if was.as_ref() == Some(&want) {
                continue;
            }
            if let Err(error) = put(&self.dir, &pad.key, &want.text()) {
                tracing::warn!(key = %pad.key, %error, "lent pad record not written");
                continue;
            }
            let gentle = starting && was.is_some_and(|was| was.holder == PARKED);
            let all: &Vec<Usb> = all.get_or_insert_with(|| present(&self.sys));
            for usb in all.iter().filter(|usb| usb.lent_as(&pad.key, port, all)) {
                if gentle {
                    refresh(&usb.dir);
                } else if let Err(error) = cycle(&usb.dir) {
                    tracing::warn!(key = %pad.key, %error, "lent pad not cycled");
                }
            }
        }
    }

    /// Drops `seat`'s records and cycles each pad that had one: they are the desk's again.
    pub(super) fn release(&self, seat: &Seat) {
        if seat.lent.is_empty() {
            return;
        }
        let _held = self.hold();
        let mut all = None;
        for pad in &seat.lent {
            match std::fs::remove_file(self.dir.join(&pad.key)) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(key = %pad.key, %error, "lent pad record not removed");
                    continue;
                }
            }
            let all: &Vec<Usb> = all.get_or_insert_with(|| present(&self.sys));
            let port = pad.port.as_deref();
            for usb in all.iter().filter(|usb| usb.lent_as(&pad.key, port, all)) {
                if let Err(error) = cycle(&usb.dir) {
                    tracing::warn!(key = %pad.key, %error, "lent pad not cycled");
                }
            }
        }
        // Fails while another record remains.
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
pub(super) mod fake {
    use super::Lent;
    use std::cell::Cell;
    use std::path::{Path, PathBuf};

    /// A box's USB hardware in a temp dir, laid out as the kernel and udev lay it out, and the
    /// lend records beside it.
    pub struct Desk {
        pub temp: tempfile::TempDir,
        pub lent: Lent,
        inputs: Cell<u32>,
    }

    /// A plugged-in device's devpath, and per interface the devpaths of a hidraw and an event node.
    pub struct Plugged {
        pub dev: String,
        pub hidraw: Vec<String>,
        pub event: Vec<String>,
    }

    impl Desk {
        pub fn new() -> Desk {
            let temp = tempfile::tempdir().unwrap();
            let lent = Lent::under(temp.path());
            Desk {
                temp,
                lent,
                inputs: Cell::new(1),
            }
        }

        pub fn sys(&self) -> PathBuf {
            self.lent.sys.clone()
        }

        pub fn records(&self) -> &Path {
            &self.lent.dir
        }

        /// Plugs a device in at `/devices/<at>`, whose last name is its bus id, with `ids`,
        /// `serial` and per interface its class and whether udev classes its input a joystick.
        pub fn plug(
            &self,
            at: &str,
            ids: (u16, u16),
            serial: Option<&str>,
            interfaces: &[([u8; 3], bool)],
        ) -> Plugged {
            let dev = format!("/devices/{at}");
            let dir = self.sys().join(&dev[1..]);
            let busid = dir.file_name().unwrap().to_str().unwrap().to_owned();
            let write = |path: &Path, text: &str| std::fs::write(path, text).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            write(&dir.join("idVendor"), &format!("{:04x}\n", ids.0));
            write(&dir.join("idProduct"), &format!("{:04x}\n", ids.1));
            write(&dir.join("bDeviceClass"), "00\n");
            write(&dir.join("product"), "Wireless Controller\n");
            write(&dir.join("uevent"), "");
            // One driver folder per device, so each device's bind is its own to check.
            let driver = self.sys().join("bus/usb/drivers").join(&busid);
            std::fs::create_dir_all(&driver).unwrap();
            let _ = std::fs::remove_file(dir.join("driver"));
            std::os::unix::fs::symlink(&driver, dir.join("driver")).unwrap();
            if let Some(serial) = serial {
                write(&dir.join("serial"), &format!("{serial}\n"));
            }
            let bus = self.sys().join("bus/usb/devices");
            std::fs::create_dir_all(&bus).unwrap();
            std::os::unix::fs::symlink(&dir, bus.join(&busid)).unwrap();
            let udev = &self.lent.udev;
            std::fs::create_dir_all(udev).unwrap();
            let mut plugged = Plugged {
                dev: dev.clone(),
                hidraw: Vec::new(),
                event: Vec::new(),
            };
            for (n, (class, joystick)) in interfaces.iter().enumerate() {
                let iface = format!("{busid}:1.{n}");
                std::fs::create_dir_all(dir.join(&iface)).unwrap();
                let names = [
                    "bInterfaceClass",
                    "bInterfaceSubClass",
                    "bInterfaceProtocol",
                ];
                for (name, byte) in names.iter().zip(class) {
                    write(&dir.join(&iface).join(name), &format!("{byte:02x}\n"));
                }
                let input = self.inputs.replace(self.inputs.get() + 1);
                let hid = format!("{iface}/0003:{:04X}:{:04X}.{input:04X}", ids.0, ids.1);
                let hidraw = format!("{hid}/hidraw/hidraw{input}");
                let event = format!("{hid}/input/input{input}/event{input}");
                for node in [&hidraw, &event] {
                    std::fs::create_dir_all(dir.join(node)).unwrap();
                    write(&dir.join(node).join("uevent"), "");
                }
                let class = if *joystick { "JOYSTICK" } else { "KEYBOARD" };
                let db = format!("E:ID_INPUT=1\nE:ID_INPUT_{class}=1\n");
                write(&udev.join(format!("+input:input{input}")), &db);
                plugged.hidraw.push(format!("{dev}/{hidraw}"));
                plugged.event.push(format!("{dev}/{event}"));
            }
            plugged
        }

        /// Writes the attribute `name` of the device at `dev`.
        pub fn set(&self, dev: &str, name: &str, text: &str) {
            std::fs::write(self.sys().join(&dev[1..]).join(name), text).unwrap();
        }

        /// Whether the device at `dev` was unbound from its driver and bound back since the
        /// last ask.
        pub fn cycled(&self, dev: &str) -> bool {
            let dir = self.sys().join(&dev[1..]);
            let busid = dir.file_name().unwrap().to_str().unwrap().to_owned();
            let driver = dir.join("driver");
            let took = |knob: &str| {
                let text = std::fs::read_to_string(driver.join(knob)).unwrap_or_default();
                std::fs::write(driver.join(knob), "").unwrap();
                text == busid
            };
            took("unbind") & took("bind")
        }

        /// Whether udev's rules were asked to run again on the device at `dev` itself since the
        /// last ask.
        pub fn poked(&self, dev: &str) -> bool {
            let uevent = self.sys().join(&dev[1..]).join("uevent");
            let poked = std::fs::read_to_string(&uevent).unwrap() == "change";
            std::fs::write(&uevent, "").unwrap();
            poked
        }

        /// Whether udev's rules were asked to run again on `pad`'s first hidraw node since the
        /// last ask.
        pub fn refreshed(&self, pad: &Plugged) -> bool {
            let uevent = self.sys().join(&pad.hidraw[0][1..]).join("uevent");
            let refreshed = std::fs::read_to_string(&uevent).unwrap() == "change";
            std::fs::write(&uevent, "").unwrap();
            refreshed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Desk;
    use super::*;
    use crate::backend::PlatformBackend as _;
    use crate::model::{LentPad, RuntimeStatus, SeatId};
    use std::os::unix::fs::PermissionsExt as _;

    const PAD: [u8; 3] = [0x03, 0x00, 0x00];
    const AUDIO: [u8; 3] = [0x01, 0x01, 0x00];
    const XHCI: &str = "pci0000:00/0000:00:14.0/usb3";
    const DUALSENSE: (u16, u16) = (0x054c, 0x0ce6);

    fn seat(n: u8, runtime: RuntimeStatus, keys: &[&str]) -> Seat {
        Seat {
            id: SeatId::parse(format!("0123456789abcdef0123456789abcd{n:02x}")).unwrap(),
            name: format!("Seat {n}"),
            account: format!("pf-seat-{n}"),
            display_slot: 10 + n,
            native_port: 9_780 + u16::from(n),
            mgmt_port: 48_120 + u16::from(n),
            autostart: false,
            runtime,
            kind: crate::model::RowKind::made(),
            fingerprint: None,
            lent: keys
                .iter()
                .map(|key| LentPad {
                    key: (*key).into(),
                    name: "Pad".into(),
                    vendor_id: 0,
                    product_id: 0,
                    port: None,
                })
                .collect(),
        }
    }

    fn ledger(seats: Vec<Seat>) -> Ledger {
        Ledger {
            seats,
            ..Ledger::default()
        }
    }

    /// A plausible serial no other device shares names the pad in any port; any other pad is
    /// named by its controller and port chain.
    #[test]
    fn a_pad_is_keyed_by_its_serial_else_its_port() {
        let desk = Desk::new();
        let joy = [(PAD, true)];
        let xbox = (0x045e, 0x0b12);
        let at = |port: &str| format!("{XHCI}/{port}");
        let long = "x".repeat(65);
        let cases = [
            (
                at("3-1"),
                DUALSENSE,
                Some("a0b1c2d3e4f5"),
                "usb-054c-0ce6-s-a0b1c2d3e4f5",
                "serial",
            ),
            (
                at("3-2/3-2.1"),
                DUALSENSE,
                None,
                "usb-054c-0ce6-p-0000_00_14.0-2.1",
                "behind a hub",
            ),
            (
                at("3-3"),
                DUALSENSE,
                Some("has space"),
                "usb-054c-0ce6-p-0000_00_14.0-3",
                "bad serial",
            ),
            (
                at("3-4"),
                xbox,
                Some("TWIN"),
                "usb-045e-0b12-p-0000_00_14.0-4",
                "a twin",
            ),
            (
                at("3-5"),
                xbox,
                Some("TWIN"),
                "usb-045e-0b12-p-0000_00_14.0-5",
                "its twin",
            ),
            (
                "platform/soc/xhci-hcd.0.auto/usb1/1-1".into(),
                DUALSENSE,
                Some(long.as_str()),
                "usb-054c-0ce6-p-xhci_hcd.0.auto-1",
                "a long serial on a non-PCI controller",
            ),
        ];
        let devs: Vec<String> = (cases.iter())
            .map(|(at, ids, serial, ..)| desk.plug(at, *ids, *serial, &joy).dev)
            .collect();
        let all = present(&desk.sys());
        for (dev, (_, _, _, key, what)) in devs.iter().zip(&cases) {
            let usb = Usb::read(&desk.sys(), Path::new(&dev[1..])).unwrap();
            assert_eq!(usb.minted(&all).as_deref(), Some(*key), "{what}");
            assert!(usb.candidates().any(|k| k == *key), "{what}");
        }
        let twin = Usb::read(&desk.sys(), Path::new(&devs[3][1..])).unwrap();
        assert_eq!(
            twin.candidates().next().as_deref(),
            Some("usb-045e-0b12-s-TWIN"),
            "a lend under the serial still finds a twin"
        );
    }

    /// A pad lent by its serial is the lent pad in any port. While a twin that reports the same
    /// serial is plugged in, only the device in the port the lend remembers is: the twin is the
    /// desk's, listed by its port, and a stop of the seat leaves it alone.
    #[test]
    fn a_twin_of_a_lent_pad_stays_the_desk_s() {
        let desk = Desk::new();
        let joy = [(PAD, true)];
        let key = "usb-054c-0ce6-s-SAME";
        let usb = |dev: &str| Usb::read(&desk.sys(), Path::new(&dev[1..])).unwrap();
        let holds = |dev: &str| holder_of(&desk.sys(), desk.records(), &usb(dev));
        let a = desk.plug(&format!("{XHCI}/3-1"), DUALSENSE, Some("SAME"), &joy);
        let found = desk.lent.lendable_pad(key, &Ledger::default()).unwrap();
        assert_eq!(found.port.as_deref(), Some("0000_00_14.0-1"));
        let mut row = seat(1, RuntimeStatus::running(), &[key]);
        row.lent[0].port = found.port;
        let lends = ledger(vec![row.clone()]);
        desk.lent.settle(&lends, &[key.to_owned()]).unwrap();
        assert!(desk.cycled(&a.dev));
        assert_eq!(holds(&a.dev).as_deref(), Some("pf-seat-1"));

        let b = desk.plug(&format!("{XHCI}/3-2"), DUALSENSE, Some("SAME"), &joy);
        assert_eq!(holds(&a.dev).as_deref(), Some("pf-seat-1"), "in its port");
        assert_eq!(holds(&b.dev), None, "the twin");
        let twin = "usb-054c-0ce6-p-0000_00_14.0-2";
        let mut listed: Vec<String> = (desk.lent.desk_pads(&lends).into_iter())
            .map(|pad| pad.key)
            .collect();
        listed.sort();
        assert_eq!(listed, [twin, key]);
        let again = desk.lent.lendable_pad(key, &lends).unwrap();
        assert_eq!(
            again.port.as_deref(),
            Some("0000_00_14.0-1"),
            "the lend's pad"
        );
        let fresh = desk.lent.lendable_pad(key, &Ledger::default()).unwrap_err();
        assert_eq!(fresh.code, "pad_not_present", "a serial two devices share");
        assert!(desk.lent.lendable_pad(twin, &lends).is_ok());

        desk.lent.set_live(&row, false, false);
        assert!(desk.cycled(&a.dev), "the lent pad parks");
        assert!(!desk.cycled(&b.dev), "the desk's twin is left alone");
        desk.lent
            .settle(&Ledger::default(), &[key.to_owned()])
            .unwrap();
        assert!(desk.cycled(&a.dev) && !desk.cycled(&b.dev), "taken back");
    }

    /// A game controller on a real bus may be lent, and a receiver with no pad awake; a keyboard,
    /// a composite with storage, a hub and a USB/IP device may not.
    #[test]
    fn only_a_game_controller_on_a_real_bus_may_be_lent() {
        let desk = Desk::new();
        let puck = [
            ([0x02, 0x02, 0x01], false),
            ([0x0a, 0, 0], false),
            (PAD, false),
        ];
        /// A serial, the ids, the interfaces, and the code a lend is refused with.
        type Case<'a> = (
            &'a str,
            (u16, u16),
            &'a [([u8; 3], bool)],
            Result<(), &'a str>,
        );
        let cases: [Case; 6] = [
            ("pad", DUALSENSE, &[(AUDIO, false), (PAD, true)], Ok(())),
            ("puck", (0x28de, 0x1304), &puck, Ok(())),
            (
                "keyboard",
                (0x046d, 0xc31c),
                &[([0x03, 0x01, 0x01], false)],
                Err("pad_not_a_controller"),
            ),
            (
                "storage",
                DUALSENSE,
                &[(PAD, true), ([0x08, 0x06, 0x50], false)],
                Err("pad_not_a_controller"),
            ),
            (
                "hub",
                DUALSENSE,
                &[(PAD, true)],
                Err("pad_not_a_controller"),
            ),
            ("vhci", DUALSENSE, &[(PAD, true)], Err("pad_not_present")),
        ];
        for (n, (serial, ids, interfaces, _)) in cases.iter().enumerate() {
            let at = match *serial {
                "vhci" => "platform/vhci_hcd.0/usb11/11-1".to_owned(),
                _ => format!("{XHCI}/3-{}", n + 1),
            };
            let pad = desk.plug(&at, *ids, Some(serial), interfaces);
            if *serial == "hub" {
                desk.set(&pad.dev, "bDeviceClass", "09\n");
            }
        }
        for (serial, (vid, pid), _, want) in cases {
            let key = format!("usb-{vid:04x}-{pid:04x}-s-{serial}");
            let got = desk.lent.lendable_pad(&key, &Ledger::default());
            assert_eq!(
                got.as_ref().map(drop).map_err(|e| e.code.as_str()),
                want,
                "{serial}"
            );
            if let Ok(pad) = got {
                assert_eq!(
                    (pad.key.as_str(), pad.name.as_str()),
                    (key.as_str(), "Wireless Controller")
                );
            }
        }
        let none = Ledger::default();
        let gone = desk
            .lent
            .lendable_pad("usb-054c-0ce6-s-gone", &none)
            .unwrap_err();
        assert_eq!(gone.code, "pad_not_present");
        let by_port = desk
            .lent
            .lendable_pad("usb-054c-0ce6-p-0000_00_14.0-1", &none);
        assert!(by_port.is_ok(), "the port's key names the same pad");

        let keyboard = "usb-046d-c31c-s-keyboard";
        let lent = ledger(vec![seat(1, RuntimeStatus::stopped(), &[keyboard])]);
        let mut listed: Vec<String> = (desk.lent.desk_pads(&lent).into_iter())
            .map(|pad| pad.key)
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [keyboard, "usb-054c-0ce6-s-pad", "usb-28de-1304-s-puck"],
            "a lent device is listed even when it fails the gate"
        );
    }

    /// Settling writes every lend's record from the ledger and cycles only the keys asked; once
    /// no lend is left, the records and their directory go.
    #[test]
    fn settle_writes_every_record_and_cycles_the_keys_asked() {
        let desk = Desk::new();
        let one = desk.plug(
            &format!("{XHCI}/3-1"),
            DUALSENSE,
            Some("ONE"),
            &[(PAD, true)],
        );
        let two = desk.plug(
            &format!("{XHCI}/3-2"),
            DUALSENSE,
            Some("TWO"),
            &[(PAD, true)],
        );
        let [k1, k2, gone] = ["ONE", "TWO", "GONE"].map(|s| format!("usb-054c-0ce6-s-{s}"));
        let lends = ledger(vec![
            seat(1, RuntimeStatus::running(), &[&k1]),
            seat(2, RuntimeStatus::stopped(), &[&k2, &gone]),
        ]);
        let dir = desk.records();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("usb-054c-0ce6-s-OLD"), "pf-seat-3\n").unwrap();
        desk.lent
            .settle(&lends, &[k1.clone(), gone.clone()])
            .unwrap();
        let held = |key: &str| record(dir, key).map(|r| r.holder);
        assert_eq!(held(&k1).as_deref(), Some("pf-seat-1"));
        assert_eq!(held(&k2).as_deref(), Some(PARKED));
        assert_eq!(held(&gone).as_deref(), Some(PARKED), "unplugged");
        assert_eq!(
            std::fs::read_dir(dir).unwrap().count(),
            3,
            "no lend backs the old one"
        );
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!((mode(dir), mode(&dir.join(&k1))), (0o700, 0o600));
        assert!(desk.cycled(&one.dev));
        assert!(desk.poked(&one.dev), "the device's own node follows");
        assert!(!desk.cycled(&two.dev), "not asked");
        desk.lent.settle(&lends, std::slice::from_ref(&k1)).unwrap();
        assert!(!desk.cycled(&one.dev), "its record stood: cycled already");

        desk.lent.settle(&Ledger::default(), &[k1]).unwrap();
        assert!(!dir.exists(), "no lend left");
        assert!(desk.cycled(&one.dev));
    }

    /// A seat's pads follow it: parked while it is down, live while it runs, the desk's once it
    /// is removed. A second call with nothing changed writes nothing and cycles nothing.
    #[test]
    fn a_seat_s_pads_follow_it_and_a_repeat_changes_nothing() {
        let desk = Desk::new();
        let pad = desk.plug(
            &format!("{XHCI}/3-1"),
            DUALSENSE,
            Some("ONE"),
            &[(PAD, true)],
        );
        let key = "usb-054c-0ce6-s-ONE";
        let row = seat(1, RuntimeStatus::stopped(), &[key]);
        let file = desk.records().join(key);
        let written = || std::fs::metadata(&file).unwrap().ino();
        let lent = &desk.lent;
        let held = || record(desk.records(), key).map(|r| r.holder);
        let parked = Some(PARKED.to_owned());
        let live = Some("pf-seat-1".to_owned());

        lent.set_live(&row, false, false);
        assert_eq!(
            (held(), desk.cycled(&pad.dev)),
            (parked.clone(), true),
            "no record yet"
        );
        let at = written();
        lent.set_live(&row, false, false);
        assert_eq!(
            (written(), desk.cycled(&pad.dev)),
            (at, false),
            "parked again"
        );

        lent.set_live(&row, true, true);
        let started = (held(), desk.cycled(&pad.dev), desk.refreshed(&pad));
        assert_eq!(
            started,
            (live.clone(), false, true),
            "a start from parked re-runs the rules"
        );
        let at = written();
        lent.set_live(&row, true, false);
        let again = (written(), desk.cycled(&pad.dev), desk.refreshed(&pad));
        assert_eq!(again, (at, false, false), "running, as recorded");

        lent.set_live(&row, false, false);
        assert_eq!((held(), desk.cycled(&pad.dev)), (parked, true), "stopped");
        lent.set_live(&row, true, false);
        let found = (held(), desk.cycled(&pad.dev), desk.refreshed(&pad));
        assert_eq!(found, (live.clone(), true, false), "found running");

        lent.release(&row);
        assert!(!desk.records().exists() && desk.cycled(&pad.dev), "removed");
        lent.release(&row);
        assert!(!desk.cycled(&pad.dev), "removed again");
        lent.set_live(&row, true, true);
        assert_eq!(
            (held(), desk.cycled(&pad.dev)),
            (live, true),
            "a start with no record"
        );
    }

    /// The backend levels a row's pads with the state it reconciles.
    #[test]
    fn reconcile_parks_a_stopped_seat_s_pads() {
        let desk = Desk::new();
        let pad = desk.plug(
            &format!("{XHCI}/3-1"),
            DUALSENSE,
            Some("ONE"),
            &[(PAD, true)],
        );
        let key = "usb-054c-0ce6-s-ONE";
        let backend = super::super::LinuxBackend::detached(desk.temp.path(), desk.temp.path());
        let row = seat(1, RuntimeStatus::running(), &[key]);
        backend.reconcile(&row, &RuntimeStatus::stopped());
        let parked = record(desk.records(), key).map(|r| r.holder);
        assert_eq!(parked.as_deref(), Some(PARKED));
        assert!(desk.cycled(&pad.dev));
        let listed = backend.desk_pads(&ledger(vec![row])).unwrap();
        assert_eq!(listed[0].key, key);
    }

    #[test]
    fn only_a_directory_root_s_alone_is_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let dir = |name: &str, mode: u32| {
            let dir = temp.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            dir
        };
        let open = dir("open", 0o755);
        assert!(!trusted(&open), "others may read it");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(dir("closed", 0o700), &link).unwrap();
        assert!(!trusted(&link), "a link");
        assert!(!trusted(&temp.path().join("missing")));
        let root = rustix::process::geteuid().as_raw() == 0;
        assert_eq!(trusted(&temp.path().join("closed")), root, "root's alone");
    }

    #[test]
    fn the_interface_gate_takes_pads_and_refuses_nics_and_storage() {
        let cases = [
            ((0x03, 0x00, 0x00), true, "HID"),
            ((0xff, 0x00, 0x00), true, "vendor"),
            ((0x02, 0x02, 0x00), true, "CDC-ACM"),
            ((0x0a, 0x00, 0x00), true, "CDC data"),
            ((0x01, 0x02, 0x00), true, "audio"),
            ((0x02, 0x02, 0xff), false, "RNDIS"),
            ((0x02, 0x06, 0x00), false, "CDC ethernet"),
            ((0x08, 0x06, 0x50), false, "mass storage"),
            ((0x09, 0x00, 0x00), false, "hub"),
        ];
        for ((class, sub, protocol), allowed, what) in cases {
            assert_eq!(interface_allowed(class, sub, protocol), allowed, "{what}");
        }
    }
}
