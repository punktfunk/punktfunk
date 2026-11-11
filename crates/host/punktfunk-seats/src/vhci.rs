//! A seat's USB/IP pads (`design/seat-pad-broker.md` §1.2). A pad server the supervisor starts
//! serves the device; the supervisor attaches its connected socket to a vhci port, records
//! whose the port is, and udev's fence ([`fence`]) maps every node under it to that seat.
//!
//! [`MAP`] holds one file per seat's port: `<account> <sockfd>`, the socket number `attach`
//! took, as vhci's `status` shows it. A row whose port is free or holds another socket is
//! stale. While the supervisor runs, a new vhci interface starts unauthorized: the fence
//! authorizes a seat's only when [`interface_allowed`] takes what the kernel parsed.

use anyhow::{Context, Result};
use pf_inject::usbip;
use rustix::event::{poll, PollFd, PollFlags};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub const MAP: &str = "/run/punktfunk/pads/vhci";
/// Ports one seat may hold: four of a 16-port controller leaves the box and the others theirs.
pub const MAX_PER_SEAT: usize = 4;

/// Port choice and the map change together.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// At the supervisor's start: makes the map, drops stale rows (attachments outlive a restart,
/// the kernel holds their sockets) and starts every new vhci interface unauthorized.
pub fn prepare() {
    let _held = lock();
    let dir = Path::new(MAP);
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::warn!(%error, dir = MAP, "vhci port map not made");
    }
    prune(dir, &usbip::vhci_used_rows());
    // The fence rule is the only thing that authorizes a gated interface; without it the gate
    // stays open, or the owner's own virtual Deck never binds.
    let ruled = pf_seats::linux::fence::rule_installed();
    if !ruled {
        tracing::warn!("seat USB interfaces are not gated: the udev fence rule is missing");
    }
    set_gate(ruled);
}

/// Opens the gate again at the supervisor's stop (`ExecStopPost=`): no seat attaches while it
/// is down. A seat that still holds a port keeps it shut, or its device could re-configure into
/// an interface the kernel binds before the fence runs.
pub fn ungate() {
    let dir = Path::new(MAP);
    prune(dir, &usbip::vhci_used_rows());
    let held = std::fs::read_dir(dir).map_or(0, |entries| entries.count());
    if held > 0 {
        tracing::warn!(held, "vhci gate stays shut: a seat holds a port");
        return;
    }
    set_gate(false);
}

/// Every vhci root hub's `interface_authorized_default`: `0` while `gated`.
fn set_gate(gated: bool) {
    let Some(entries) = usbip::vhci_base().and_then(|base| std::fs::read_dir(base).ok()) else {
        return;
    };
    let value = if gated { "0" } else { "1" };
    let knobs = entries
        .flatten()
        .map(|e| e.path().join("interface_authorized_default"));
    for knob in knobs.filter(|knob| knob.exists()) {
        if let Err(error) = std::fs::write(&knob, value) {
            tracing::warn!(%error, hub = %knob.display(), gated, "vhci hub gate not set");
        }
    }
}

/// Attach `sock` as `devid` at `speed` for `account`: the port it took, `None` once the seat
/// holds [`MAX_PER_SEAT`]. The row is written before `attach`, so the fence always finds it.
pub fn attach(account: &str, sock: BorrowedFd<'_>, devid: u32, speed: u32) -> Result<Option<u16>> {
    let _held = lock();
    let dir = Path::new(MAP);
    prune(dir, &usbip::vhci_used_rows());
    if held_in(dir, account) >= MAX_PER_SEAT {
        return Ok(None);
    }
    let port = usbip::vhci_find_free_port(speed)?;
    let fd = sock.as_raw_fd();
    write_row(dir, port, account, fd).context("record the port's seat")?;
    if let Err(error) = usbip::vhci_attach(port, fd, devid, speed) {
        let _ = std::fs::remove_file(dir.join(port.to_string()));
        return Err(error);
    }
    Ok(Some(port))
}

/// Holds `sock` until its connection ends, then takes it out of systemd's fd store. `recorded`
/// is the number `attach` took; after a restart `sock` has another. The port's row stays for
/// [`prune`], which drops it once the kernel has freed the port: until then a udev event from
/// the device's teardown still reads it as the seat's.
pub fn watch(port: u16, recorded: RawFd, sock: OwnedFd) {
    loop {
        let mut fds = [PollFd::new(&sock, PollFlags::RDHUP)];
        match poll(&mut fds, None) {
            Err(rustix::io::Errno::INTR) => continue,
            Ok(_) if fds[0].revents().is_empty() => continue,
            _ => break,
        }
    }
    // Before `sock` closes: its number, once free, can name a new attach on this port.
    pf_inject::pad_broker::forget_usb(port, recorded);
}

/// The seat whose row names `port`.
pub fn account_at(port: u16) -> Option<String> {
    read_row(Path::new(MAP), port).map(|(account, _)| account)
}

/// udev's program for a device under a vhci hub; `false` when `devpath` is not one. Prints the
/// seat's lines when the port is a seat's, on every event. On an interface's `add` only, it
/// authorizes and probes its device's interfaces ([`authorize`]): a seat's only as
/// [`seat_may_bind`] says. A later event can come after the seat's row is gone, mid-teardown,
/// and must not read the device as nobody's.
pub fn fence(devpath: &str) -> bool {
    let Some(busid) = busid_of(devpath) else {
        return false;
    };
    let account = seat_of(&busid);
    if let Some(account) = &account {
        pf_seats::linux::fence::print_for(account);
    }
    let added = std::env::var("ACTION").as_deref() == Ok("add");
    if added && std::env::var("DEVTYPE").as_deref() == Ok("usb_interface") {
        let interface = Path::new("/sys").join(devpath.trim_start_matches('/'));
        if let Some(device) = interface.parent() {
            authorize(device, account.is_some());
        }
    }
    true
}

/// Authorizes each of `device`'s interfaces that may bind, then probes those still unbound:
/// the kernel probes none it made unauthorized, and a driver that claims a sibling at probe
/// (`cdc_acm`, `snd-usb-audio`) is refused an unauthorized one.
fn authorize(device: &Path, seat: bool) {
    let Some(prefix) = device
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| format!("{n}:"))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(device) else {
        return;
    };
    let interfaces: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix))
        })
        .map(|e| e.path())
        .collect();
    for interface in &interfaces {
        if !seat || seat_may_bind(device, interface) == Some(true) {
            let _ = std::fs::write(interface.join("authorized"), "1");
        }
    }
    for interface in &interfaces {
        let authorized =
            std::fs::read_to_string(interface.join("authorized")).is_ok_and(|a| a.trim() == "1");
        if authorized
            && !interface.join("driver").exists()
            && let Some(name) = interface.file_name()
        {
            let _ = std::fs::write("/sys/bus/usb/drivers_probe", name.as_encoded_bytes());
        }
    }
}

/// [`interface_allowed`] on the ids and class sysfs shows the kernel parsed for `interface`.
fn seat_may_bind(device: &Path, interface: &Path) -> Option<bool> {
    let hex = |dir: &Path, name: &str| -> Option<u16> {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        u16::from_str_radix(text.trim(), 16).ok()
    };
    let byte = |name: &str| u8::try_from(hex(interface, name)?).ok();
    Some(interface_allowed(
        hex(device, "idVendor")?,
        hex(device, "idProduct")?,
        byte("bInterfaceClass")?,
        byte("bInterfaceSubClass")?,
        byte("bInterfaceProtocol")?,
    ))
}

/// Whether a seat's interface may bind: its device is one [`usbip::EMULATED`] lists, and it is
/// HID, audio, CDC-ACM, CDC data or vendor-specific. A vendor interface binds by VID:PID alone,
/// so the list keeps a NIC's driver off it; ACM stops at protocol 6, short of RNDIS's `ff`.
fn interface_allowed(vid: u16, pid: u16, class: u8, subclass: u8, protocol: u8) -> bool {
    usbip::EMULATED.contains(&(vid, pid))
        && matches!(
            (class, subclass, protocol),
            (0x01 | 0x03 | 0x0a | 0xff, _, _) | (0x02, 0x02, 0x00..=0x06)
        )
}

/// The seat whose port holds device `busid`: the port's row, when its socket is the one
/// `status` shows.
fn seat_of(busid: &str) -> Option<String> {
    let (port, fd, _) = usbip::vhci_used_rows()
        .into_iter()
        .find(|(_, _, row_busid)| row_busid == busid)?;
    let (account, recorded) = read_row(Path::new(MAP), port)?;
    (recorded == fd).then_some(account)
}

/// The port's device name under a vhci hub, `11-1` in
/// `/devices/platform/vhci_hcd.0/usb11/11-1/…`; a device behind a hub there is the port's too.
fn busid_of(devpath: &str) -> Option<String> {
    let mut parts = devpath
        .split('/')
        .skip_while(|part| !part.starts_with("vhci_hcd"));
    parts.next()?;
    if !parts.next()?.starts_with("usb") {
        return None;
    }
    let busid = parts.next()?.split('.').next()?;
    let (bus, port) = busid.split_once('-')?;
    let plain = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (plain(bus) && plain(port)).then(|| busid.to_owned())
}

fn write_row(dir: &Path, port: u16, account: &str, fd: RawFd) -> std::io::Result<()> {
    std::fs::write(dir.join(port.to_string()), format!("{account} {fd}\n"))
}

fn read_row(dir: &Path, port: u16) -> Option<(String, RawFd)> {
    let row = std::fs::read_to_string(dir.join(port.to_string())).ok()?;
    let mut words = row.split_whitespace();
    let account = words.next()?.to_owned();
    Some((account, words.next()?.parse().ok()?))
}

/// Drops every row `used` does not back: its port free, or held on another socket. A port
/// attached but not yet addressed shows socket 0 and keeps its row.
fn prune(dir: &Path, used: &[(u16, RawFd, String)]) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let backed = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u16>().ok())
            .and_then(|port| read_row(dir, port).map(|(_, fd)| (port, fd)))
            .is_some_and(|(port, fd)| {
                used.iter()
                    .any(|(p, sockfd, _)| *p == port && (*sockfd == fd || *sockfd == 0))
            });
        if !backed {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn held_in(dir: &Path, account: &str) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .flatten()
            .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
            .filter(|row| row.split_whitespace().next() == Some(account))
            .count()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every device a seat's pad server serves passes the gate, interface by interface.
    #[test]
    fn every_served_device_passes_the_gate() {
        use pf_inject::usbip_relay::{served, Hello, Outputs, UsbKind};
        let (relay, _pad) = pf_inject::pad_broker::relay_pair().unwrap();
        for kind in UsbKind::ALL {
            let hello = Hello {
                kind,
                index: 0,
                slot: 0,
                identity: Vec::new(),
            };
            let dev = served(&hello, Outputs::new(&relay).unwrap())
                .unwrap()
                .device;
            for (n, i) in dev.interfaces.iter().enumerate() {
                assert!(
                    interface_allowed(
                        dev.vendor_id,
                        dev.product_id,
                        i.interface_class,
                        i.interface_subclass,
                        i.interface_protocol
                    ),
                    "{kind:?} interface {n}"
                );
            }
        }
    }

    /// The gate: a listed device, and an interface class no NIC or disk driver takes.
    #[test]
    fn the_interface_gate_takes_pads_and_refuses_nics_and_storage() {
        let cases = [
            ((0x28de, 0x1205, 0x03, 0x00, 0x00), true, "Deck HID"),
            ((0x057e, 0x2069, 0xff, 0x00, 0x00), true, "Switch 2 vendor"),
            ((0x28de, 0x1304, 0x02, 0x02, 0x00), true, "Puck ACM"),
            ((0x28de, 0x1304, 0x0a, 0x00, 0x00), true, "Puck CDC data"),
            ((0x054c, 0x0ce6, 0x01, 0x02, 0x00), true, "DualSense audio"),
            ((0x0bda, 0x8153, 0xff, 0xff, 0x00), false, "an r8152 NIC"),
            ((0x28de, 0x1205, 0x02, 0x02, 0xff), false, "RNDIS"),
            ((0x28de, 0x1205, 0x02, 0x06, 0x00), false, "CDC ethernet"),
            ((0x28de, 0x1205, 0x08, 0x06, 0x50), false, "mass storage"),
        ];
        for ((vid, pid, class, sub, protocol), allowed, what) in cases {
            assert_eq!(
                interface_allowed(vid, pid, class, sub, protocol),
                allowed,
                "{what}"
            );
        }
    }

    /// The fence finds the port's device under the vhci hub, hub-attached or not.
    #[test]
    fn a_vhci_devpath_names_its_device() {
        assert_eq!(
            busid_of("/devices/platform/vhci_hcd.0/usb11/11-1/11-1:1.2/0003:28DE:1205.000C/input/input42/event12"),
            Some("11-1".into())
        );
        assert_eq!(
            busid_of("/devices/platform/vhci_hcd.0/usb12/12-3"),
            Some("12-3".into())
        );
        assert_eq!(
            busid_of("/devices/platform/vhci_hcd.0/usb11/11-1.2/11-1.2:1.0"),
            Some("11-1".into()),
            "behind a hub it is still the port's"
        );
        assert_eq!(busid_of("/devices/platform/vhci_hcd.0/usb11"), None);
        assert_eq!(
            busid_of("/devices/platform/vhci_hcd.0/usb11/11-0:1.0"),
            None
        );
        assert_eq!(busid_of("/devices/pci0000:00/0000:00:14.0/usb1/1-2"), None);
    }

    /// A row is `<account> <sockfd>`; one no used port backs on that socket is pruned.
    #[test]
    fn the_map_round_trips_and_prunes() {
        let dir = std::env::temp_dir().join(format!("pf-vhci-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let used = |port, fd| vec![(port, fd, "11-1".to_owned())];
        write_row(&dir, 3, "pf-seat-1", 17).unwrap();
        assert_eq!(read_row(&dir, 3), Some(("pf-seat-1".into(), 17)));
        assert_eq!(read_row(&dir, 4), None);
        std::fs::write(dir.join("junk"), "x").unwrap();
        std::fs::write(dir.join("5"), "no-fd").unwrap();
        prune(&dir, &used(3, 17));
        assert_eq!(read_row(&dir, 3), Some(("pf-seat-1".into(), 17)), "backed");
        assert!(!dir.join("5").exists() && !dir.join("junk").exists());
        prune(&dir, &used(3, 0));
        assert!(dir.join("3").exists(), "attached, not yet addressed");
        prune(&dir, &used(3, 18));
        assert!(!dir.join("3").exists(), "another socket on the port");
        write_row(&dir, 1, "pf-seat-1", 9).unwrap();
        write_row(&dir, 2, "pf-seat-2", 9).unwrap();
        assert_eq!(held_in(&dir, "pf-seat-1"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
