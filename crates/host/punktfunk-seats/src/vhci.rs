//! A seat's USB/IP pads (`design/seat-pad-broker.md` §1.2). The seat runs its own usbip server
//! and import handshake; the supervisor attaches the connected socket to a vhci port, records
//! whose the port is, and udev's fence ([`fence`]) maps every node under it to that seat.
//!
//! [`MAP`] holds one file per port a seat attached: `<account> <sockfd>`, the socket number
//! `attach` took, as vhci's `status` shows it. A row whose port is free or holds another socket
//! is stale. While the supervisor runs, a new vhci device starts unauthorized: the fence lets a
//! seat's configure only when every interface is a class [`classes_allowed`] takes.

use anyhow::{bail, Context, Result};
use pf_inject::usbip;
use rustix::event::{poll, PollFd, PollFlags};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
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
/// the kernel holds their sockets) and starts every new vhci device unauthorized.
pub fn prepare() {
    let _held = lock();
    let dir = Path::new(MAP);
    if let Err(error) = std::fs::create_dir_all(dir) {
        tracing::warn!(%error, dir = MAP, "vhci port map not made");
    }
    prune(dir, &usbip::vhci_used_rows());
    // The fence rule lifts the gate; without it the owner's own virtual Deck never configures.
    if !pf_seats::linux::fence::rule_installed() {
        tracing::warn!("seat USB devices are not class-gated: the udev fence rule is missing");
        return;
    }
    let Some(entries) = usbip::vhci_base().and_then(|base| std::fs::read_dir(base).ok()) else {
        return;
    };
    let hubs = entries
        .flatten()
        .map(|e| e.path().join("authorized_default"));
    for knob in hubs.filter(|knob| knob.exists()) {
        if let Err(error) = std::fs::write(&knob, "0") {
            tracing::warn!(%error, hub = %knob.display(), "vhci hub left ungated");
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

/// Holds `sock` until its connection ends, then forgets `port`'s row if it still names it.
/// Held, the fd's number stays the row's.
pub fn watch(port: u16, sock: OwnedFd) {
    loop {
        let mut fds = [PollFd::new(&sock, PollFlags::RDHUP)];
        match poll(&mut fds, None) {
            Err(rustix::io::Errno::INTR) => continue,
            Ok(_) if fds[0].revents().is_empty() => continue,
            _ => break,
        }
    }
    let _held = lock();
    let dir = Path::new(MAP);
    if read_row(dir, port).is_some_and(|(_, fd)| fd == sock.as_raw_fd()) {
        let _ = std::fs::remove_file(dir.join(port.to_string()));
    }
}

/// Detach `port` for `account`, while it still holds the socket its row names. A port no seat
/// attached, or another seat's, is refused.
pub fn detach(account: &str, port: u16) -> Result<()> {
    let _held = lock();
    let dir = Path::new(MAP);
    let Some((owner, fd)) = read_row(dir, port) else {
        bail!("port {port} is not a seat's");
    };
    if owner != account {
        bail!("port {port} is another seat's");
    }
    let _ = std::fs::remove_file(dir.join(port.to_string()));
    if usbip::vhci_port_holds(port, fd) {
        usbip::vhci_detach(port)?;
    }
    Ok(())
}

/// udev's program for a device under a vhci hub; `false` when `devpath` is not one. Prints the
/// seat's lines when the port is a seat's, and `PF_VHCI_USB=allow` for a USB device that may
/// configure: a seat's when its classes pass, anyone else's always.
pub fn fence(devpath: &str) -> bool {
    let Some(busid) = busid_of(devpath) else {
        return false;
    };
    let account = seat_of(&busid);
    if let Some(account) = &account {
        pf_seats::linux::fence::print_for(account);
    }
    if std::env::var("DEVTYPE").as_deref() == Ok("usb_device") {
        let descriptors = Path::new("/sys")
            .join(devpath.trim_start_matches('/'))
            .join("descriptors");
        let allowed =
            account.is_none() || std::fs::read(descriptors).is_ok_and(|d| classes_allowed(&d));
        if allowed {
            println!("PF_VHCI_USB=allow");
        }
    }
    true
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

/// Whether a seat's USB device may configure: at least one interface, and every one HID,
/// audio, CDC-ACM (with its data interface) or vendor-specific. `descriptors` is sysfs's: the
/// device descriptor, then each configuration's descriptors.
pub fn classes_allowed(descriptors: &[u8]) -> bool {
    const INTERFACE: u8 = 4;
    let mut interfaces = 0;
    let mut at = 0;
    while at + 2 <= descriptors.len() {
        let (length, kind) = (descriptors[at] as usize, descriptors[at + 1]);
        if length < 2 || at + length > descriptors.len() {
            return false;
        }
        if kind == INTERFACE {
            if length < 7 {
                return false;
            }
            let allowed = matches!(
                (descriptors[at + 5], descriptors[at + 6]),
                (0x03, _) | (0x01, _) | (0x02, 0x02) | (0x0a, _) | (0xff, _)
            );
            if !allowed {
                return false;
            }
            interfaces += 1;
        }
        at += length;
    }
    interfaces > 0 && at == descriptors.len()
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

    /// The gate: every interface HID, audio, ACM or vendor, and at least one.
    #[test]
    fn the_class_gate_takes_pads_and_refuses_storage() {
        fn iface(class: u8, sub: u8) -> [u8; 9] {
            [9, 4, 0, 0, 1, class, sub, 0, 0]
        }
        let device = [
            18u8, 1, 0, 2, 0, 0, 0, 64, 0xde, 0x28, 0x05, 0x12, 0, 3, 1, 2, 3, 1,
        ];
        let config = [9u8, 2, 0, 0, 1, 1, 0, 0x80, 50];
        let endpoint = [7u8, 5, 0x81, 3, 64, 0, 4];
        let mut deck = Vec::new();
        deck.extend(device);
        deck.extend(config);
        for sub in [0, 1, 0] {
            deck.extend(iface(3, sub));
            deck.extend(endpoint);
        }
        assert!(classes_allowed(&deck), "three HID interfaces");

        let mut puck = deck.clone();
        puck.extend(iface(2, 2));
        puck.extend(iface(0x0a, 0));
        puck.extend(iface(0xff, 0));
        puck.extend(iface(1, 1));
        assert!(classes_allowed(&puck), "ACM, data, vendor and audio");

        let mut stick = deck.clone();
        stick.extend(iface(8, 6));
        assert!(!classes_allowed(&stick), "mass storage");
        let mut modem = deck.clone();
        modem.extend(iface(2, 6));
        assert!(!classes_allowed(&modem), "CDC ethernet");
        assert!(!classes_allowed(&deck[..27]), "no interface");
        let mut torn = deck.clone();
        torn.push(0);
        assert!(!classes_allowed(&torn), "a torn descriptor");
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
