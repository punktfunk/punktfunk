//! Virtual Steam Deck over USB/IP (`vhci_hcd`).
//!
//! Three interfaces (mouse 0, keyboard 1, controller 2) so Steam Input promotes
//! the pad — a UHID Deck reports `Interface: -1` and never is. Unlike
//! [`super::steam_gadget`] (`raw_gadget` + `dummy_hcd`) this uses in-tree
//! `vhci_hcd` through [`super::usbip::attach_device`].
//!
//! Descriptors and the `0x83`/`0xAE` feature contract live in
//! [`super::steam_proto`]. Callers degrade to UHID on failure.

use super::steam_proto::{
    deck_serial, feature_reply, neutral_deck_report, parse_steam_output, SteamFeedback, SteamState,
    RDESC_DECK_CTRL, RDESC_DECK_KBD, RDESC_DECK_MOUSE,
};
use super::usbip::{attach_device, boxed, ep, hid_class_descriptor, UsbipAttachment};
use anyhow::Result;
use std::any::Any;
use std::sync::{Arc, Mutex};
use usbip_sim::{
    Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, Version,
};

const STEAM_VENDOR: u16 = 0x28DE;
const STEAMDECK_PRODUCT: u16 = 0x1205;

/// Interface 2 (vendor HID). Steam Input filters on this layout; idle mouse/kbd
/// on 0/1 stay silent.
#[derive(Debug)]
struct ControllerHandler {
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<SteamFeedback>>,
    /// Last SET_REPORT; next GET_REPORT feeds [`feature_reply`].
    last_set: Vec<u8>,
    serial: String,
}

impl UsbInterfaceHandler for ControllerHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_class_descriptor(0x0110, 33, RDESC_DECK_CTRL.len())
    }
    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0() {
            Ok(match (setup.request_type, setup.request) {
                // GET_DESCRIPTOR report (wValue hi = 0x22).
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => RDESC_DECK_CTRL.to_vec(),
                (0xA1, 0x01) => feature_reply(&self.last_set, &self.serial).to_vec(),
                (0x21, 0x09) => {
                    self.last_set = req.to_vec();
                    // `parse_steam_output` expects `[report-id(0), cmd, …]`; EP0 OUT data is `[cmd, …]`.
                    let mut framed = Vec::with_capacity(req.len() + 1);
                    framed.push(0);
                    framed.extend_from_slice(req);
                    let fb = parse_steam_output(&framed);
                    if fb.rumble.is_some() {
                        if let Ok(mut g) = self.feedback.lock() {
                            *g = fb;
                        }
                    }
                    vec![]
                }
                (0x21, 0x0A) | (0x21, 0x0B) => vec![], // SET_IDLE / SET_PROTOCOL
                _ => vec![],
            })
        } else if let Direction::In = ep.direction() {
            // vhci_hcd does not throttle; usbip_sim paces interrupt-IN by bInterval.
            let r = self
                .report
                .lock()
                .map(|g| *g)
                .unwrap_or_else(|_| neutral_deck_report());
            Ok(r.to_vec())
        } else {
            Ok(vec![])
        }
    }
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Mouse/keyboard: report descriptor only; no state, no rumble.
#[derive(Debug)]
struct IdleHidHandler {
    report_desc: Vec<u8>,
}
impl UsbInterfaceHandler for IdleHidHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_class_descriptor(0x0110, 0, self.report_desc.len())
    }
    fn handle_urb(
        &mut self,
        _i: &UsbInterface,
        ep: UsbEndpoint,
        _l: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0() && setup.request == 0x06 && (setup.value >> 8) == 0x22 {
            Ok(self.report_desc.clone())
        } else {
            Ok(vec![])
        }
    }
    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Three-interface Deck; `report`/`feedback` are shared with [`SteamDeckUsbip`].
fn build_device(
    index: u8,
    report: &Arc<Mutex<[u8; 64]>>,
    feedback: &Arc<Mutex<SteamFeedback>>,
) -> UsbDevice {
    let mut dev = UsbDevice::new(0); // one device per server, so the default bus_id stands
    dev.vendor_id = STEAM_VENDOR;
    dev.product_id = STEAMDECK_PRODUCT;
    dev.usb_version = Version::from(0x0200u16);
    dev.device_bcd = Version::from(0x0300u16); // match the gadget's bcdDevice
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name("Steam Deck Controller");
    dev.set_serial_number(&deck_serial(index));
    dev.with_interface(
        0x03,
        0x00,
        0x02,
        Some("mouse"),
        vec![ep(0x81, 0x03, 8, 4)],
        boxed(IdleHidHandler {
            report_desc: RDESC_DECK_MOUSE.to_vec(),
        }),
    )
    .with_interface(
        0x03,
        0x01,
        0x01,
        Some("keyboard"),
        vec![ep(0x82, 0x03, 8, 4)],
        boxed(IdleHidHandler {
            report_desc: RDESC_DECK_KBD.to_vec(),
        }),
    )
    .with_interface(
        0x03,
        0x00,
        0x00,
        Some("controller"),
        vec![ep(0x83, 0x03, 64, 4)],
        boxed(ControllerHandler {
            report: report.clone(),
            feedback: feedback.clone(),
            last_set: vec![],
            serial: deck_serial(index),
        }),
    )
}

/// Virtual Deck on `vhci_hcd`. Drop detaches the port and stops the server.
pub struct SteamDeckUsbip {
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<SteamFeedback>>,
    _attach: UsbipAttachment,
    enc: super::steam_proto::DeckEncoder,
}

impl SteamDeckUsbip {
    /// Attach a virtual Deck. `index` varies only the serial.
    pub fn open(index: u8) -> Result<SteamDeckUsbip> {
        let report = Arc::new(Mutex::new(neutral_deck_report()));
        let feedback = Arc::new(Mutex::new(SteamFeedback::default()));
        let attach = attach_device(
            || build_device(index, &report, &feedback),
            &format!("virtual Steam Deck {index}"),
        )?;
        Ok(SteamDeckUsbip {
            report,
            feedback,
            _attach: attach,
            enc: Default::default(),
        })
    }

    pub fn write_state(&mut self, st: &SteamState) {
        let r = self.enc.encode(st);
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    pub fn service(&mut self) -> SteamFeedback {
        self.feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default()
    }
}

/// Default on. `PUNKTFUNK_STEAM_USBIP=0`/`false` skips usbip; `open` still degrades if `vhci_hcd` is missing.
pub fn usbip_preferred() -> bool {
    !matches!(
        std::env::var("PUNKTFUNK_STEAM_USBIP").ok().as_deref(),
        Some("0") | Some("false")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sysfs::{input_devices, wait_input_gone};
    use crate::usbip::ensure_modules;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    /// hid-steam hidraw on iface 2; `bInterfaceNumber` is the HID parent's attribute.
    fn hid_steam_iface2() -> Option<String> {
        std::fs::read_dir("/sys/class/hidraw")
            .ok()?
            .flatten()
            .find_map(|e| {
                let ue =
                    std::fs::read_to_string(e.path().join("device/uevent")).unwrap_or_default();
                let iface = std::fs::read_to_string(e.path().join("device/../bInterfaceNumber"))
                    .ok()
                    .and_then(|s| u8::from_str_radix(s.trim(), 16).ok());
                (ue.lines().any(|l| l == "DRIVER=hid-steam") && iface == Some(2))
                    .then(|| format!("/dev/{}", e.file_name().to_string_lossy()))
            })
    }

    /// `hid-steam` binds (`Steam Deck` evdev) and tears down on drop. Needs root + `vhci_hcd`.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_deck_binds_and_tears_down() {
        ensure_modules();
        let mut pad = SteamDeckUsbip::open(0).expect("open SteamDeckUsbip (root + vhci_hcd?)");
        let st = SteamState::from_gamepad(punktfunk_core::input::gamepad::BTN_A, 0, 0, 0, 0, 0, 0);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(800) {
            pad.write_state(&st);
            let _ = pad.service();
            if input_devices().contains("Steam Deck") {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(
            input_devices().contains("Steam Deck"),
            "hid-steam did not bind the usbip Deck"
        );
        drop(pad);
        assert!(
            wait_input_gone("Steam Deck Motion Sensors", Duration::from_millis(400)),
            "device not torn down on drop"
        );
    }

    /// Rumble via interface-2 hidraw SET_REPORT (`0xEB`); idle ifaces ACK and Steam
    /// filters on iface 2. Needs root + `vhci_hcd`.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_deck_rumble_flows_via_controller_interface() {
        use super::super::steam_proto::ID_TRIGGER_RUMBLE_CMD;
        ensure_modules();
        let mut pad = SteamDeckUsbip::open(0).expect("open SteamDeckUsbip (root + vhci_hcd?)");
        let st = SteamState::from_gamepad(0, 0, 0, 0, 0, 0, 0);
        let start = Instant::now();
        let mut node = None;
        while start.elapsed() < Duration::from_millis(1500) {
            pad.write_state(&st);
            let _ = pad.service();
            if let Some(n) = hid_steam_iface2() {
                node = Some(n);
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
        }
        let node = node.expect("no hid-steam hidraw on interface 2");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .expect("open hidraw");
        // steam_haptic_rumble: [report-id 0, 0xEB, len 9, 0, intensity(2), left(2), right(2), gain(2)]
        let mut buf = [0u8; 12];
        buf[1] = ID_TRIGGER_RUMBLE_CMD;
        buf[2] = 0x09;
        buf[6..8].copy_from_slice(&0xC000u16.to_le_bytes());
        buf[8..10].copy_from_slice(&0x4000u16.to_le_bytes());
        // HIDIOCSFEATURE(12)
        let req: libc::c_ulong =
            (3 << 30) | ((buf.len() as libc::c_ulong) << 16) | (0x48 << 8) | 0x06;
        // SAFETY: HIDIOCSFEATURE reads the 12-byte report from the live `buf` behind the valid
        // hidraw fd `f`; the length is encoded in the request, so nothing is written past it.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req as _, buf.as_mut_ptr()) };
        assert!(
            rc >= 0,
            "HIDIOCSFEATURE: {}",
            std::io::Error::last_os_error()
        );
        let start = Instant::now();
        let mut got = None;
        while got.is_none() && start.elapsed() < Duration::from_millis(1500) {
            got = pad.service().rumble;
            pad.write_state(&st);
            std::thread::sleep(Duration::from_millis(8));
        }
        assert_eq!(
            got,
            Some((0xC000, 0x4000)),
            "Deck rumble never surfaced from the interface-2 SET_REPORT"
        );
    }
}
