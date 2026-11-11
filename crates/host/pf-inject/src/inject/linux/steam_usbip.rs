//! Virtual Steam Deck or wired Steam Controller over USB/IP (`vhci_hcd`).
//!
//! Three interfaces (mouse 0, keyboard 1, controller 2) so Steam Input promotes
//! the pad — a UHID one reports `Interface: -1` and never is. Unlike
//! [`super::steam_gadget`] (`raw_gadget` + `dummy_hcd`) this uses in-tree
//! `vhci_hcd` through [`super::usbip::attach_device`].
//!
//! Descriptors and the `0x83`/`0xAE` feature contract live in
//! [`super::steam_proto`]. Only interface 2 takes input reports from the pad; the
//! mouse and keyboard stay silent. Callers degrade to UHID on failure.

use super::steam_proto::{
    deck_serial, feature_reply, neutral_deck_report, neutral_sc_report, parse_steam_output,
    sc_mirror_reply, sc_serial, SteamFeedback, SteamModel, SteamState, RDESC_DECK_CTRL,
    RDESC_DECK_KBD, RDESC_DECK_MOUSE, RDESC_SC_CTRL,
};
use super::triton_proto::Sc2Identity;
use super::usbip::{attach_device, boxed, ep, hid_class_descriptor, UsbipAttachment};
use super::usbip_relay::{one_report, Hello, Output, Outputs, Served, UsbKind};
use anyhow::Result;
use std::any::Any;
use std::sync::{Arc, Mutex};
use usbip_sim::{
    Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, Version,
};

pub(crate) const STEAM_VENDOR: u16 = 0x28DE;
pub(crate) const STEAMDECK_PRODUCT: u16 = 0x1205;
pub(crate) const STEAMCTRL_PRODUCT: u16 = 0x1102;
/// The controller interface: the only one that takes input reports.
const CONTROLLER: u8 = 2;

fn ctrl_rdesc(model: SteamModel) -> &'static [u8] {
    match model {
        SteamModel::Deck => RDESC_DECK_CTRL,
        SteamModel::Controller => RDESC_SC_CTRL,
    }
}

/// Interface 2 (vendor HID). Steam Input filters on this layout; idle mouse/kbd
/// on 0/1 stay silent.
#[derive(Debug)]
struct ControllerHandler {
    model: SteamModel,
    report: Arc<Mutex<[u8; 64]>>,
    out: Outputs,
    /// Last SET_REPORT, command byte first; the next GET_REPORT answers it.
    last_set: Vec<u8>,
    serial: String,
    /// A captured Steam Controller's own replies, answered before the canned ones.
    identity: Option<Arc<Sc2Identity>>,
}

impl ControllerHandler {
    /// The GET_REPORT answer to `last_set`, command byte first.
    fn feature_get(&self) -> [u8; 64] {
        match self.model {
            SteamModel::Deck => feature_reply(&self.last_set, &self.serial),
            SteamModel::Controller => sc_mirror_reply(
                |asked| self.identity.as_ref().and_then(|i| i.reply(asked)),
                &self.last_set,
                &self.serial,
            ),
        }
    }
}

impl UsbInterfaceHandler for ControllerHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_class_descriptor(0x0110, 33, ctrl_rdesc(self.model).len())
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
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => ctrl_rdesc(self.model).to_vec(),
                (0xA1, 0x01) => self.feature_get().to_vec(),
                (0x21, 0x09) => {
                    self.last_set = req.to_vec();
                    self.out.report(CONTROLLER, Some(setup.value), req);
                    vec![]
                }
                (0x21, 0x0A) | (0x21, 0x0B) => vec![], // SET_IDLE / SET_PROTOCOL
                _ => vec![],
            })
        } else if let Direction::In = ep.direction() {
            // vhci_hcd does not throttle; usbip_sim paces interrupt-IN by bInterval.
            let r = *self.report.lock().unwrap_or_else(|e| e.into_inner());
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

/// Three-interface Deck or Steam Controller streaming `report`; Steam's writes go to `out`.
/// A captured Steam Controller's `identity` gives the pad its own serial.
fn build_device(
    index: u8,
    model: SteamModel,
    report: &Arc<Mutex<[u8; 64]>>,
    out: Outputs,
    identity: Option<&Arc<Sc2Identity>>,
) -> UsbDevice {
    let (product, name, canned) = match model {
        SteamModel::Deck => (
            STEAMDECK_PRODUCT,
            "Steam Deck Controller",
            deck_serial(index),
        ),
        SteamModel::Controller => (STEAMCTRL_PRODUCT, "Wired Controller", sc_serial(index)),
    };
    let serial = identity.and_then(|i| i.serial.clone()).unwrap_or(canned);
    let mut dev = UsbDevice::new(0); // one device per server, so the default bus_id stands
    dev.vendor_id = STEAM_VENDOR;
    dev.product_id = product;
    dev.usb_version = Version::from(0x0200u16);
    dev.device_bcd = Version::from(0x0300u16); // match the gadget's bcdDevice
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name(name);
    dev.set_serial_number(&serial);
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
            model,
            report: report.clone(),
            out,
            last_set: vec![],
            serial,
            identity: identity.cloned(),
        }),
    )
}

/// The server's half: the device, and the pad's 64-byte reports into interface 2 only.
pub(crate) fn served(
    index: u8,
    model: SteamModel,
    identity: Option<Arc<Sc2Identity>>,
    out: Outputs,
) -> Served {
    let rest = match model {
        SteamModel::Deck => neutral_deck_report(),
        SteamModel::Controller => neutral_sc_report(),
    };
    let report = Arc::new(Mutex::new(rest));
    Served {
        device: build_device(index, model, &report, out, identity.as_ref()),
        input: one_report(report, CONTROLLER, None),
    }
}

/// What Steam wrote, parsed: the Deck's rumble; a Steam Controller's writes for its pad.
fn feedback(model: SteamModel, outputs: Vec<Output>) -> SteamFeedback {
    let mut fb = SteamFeedback::default();
    for output in outputs {
        let Output::Report {
            iface: CONTROLLER,
            set: Some(_),
            data,
        } = output
        else {
            continue;
        };
        match model {
            SteamModel::Deck => {
                // `parse_steam_output` expects `[report-id(0), cmd, …]`; EP0 OUT data is `[cmd, …]`.
                let framed = [&[0][..], &data].concat();
                if let Some(r) = parse_steam_output(&framed).rumble {
                    fb.rumble = Some(r);
                }
            }
            SteamModel::Controller => fb.forward_sc_set(&data),
        }
    }
    fb
}

/// Virtual Deck or Steam Controller on `vhci_hcd`. Drop detaches the port and stops the server.
pub struct SteamUsbip {
    model: SteamModel,
    attach: UsbipAttachment,
    enc: super::steam_proto::DeckEncoder,
}

impl SteamUsbip {
    /// Attach a virtual Deck. `index` varies only the serial.
    pub fn open(index: u8) -> Result<SteamUsbip> {
        Self::attach(index, SteamModel::Deck, None)
    }

    /// Attach a virtual wired Steam Controller, as `identity` when its client sent one.
    pub fn open_controller(index: u8, identity: Option<Arc<Sc2Identity>>) -> Result<SteamUsbip> {
        Self::attach(index, SteamModel::Controller, identity)
    }

    fn attach(
        index: u8,
        model: SteamModel,
        identity: Option<Arc<Sc2Identity>>,
    ) -> Result<SteamUsbip> {
        let (kind, label) = match model {
            SteamModel::Deck => (UsbKind::SteamDeck, "Steam Deck"),
            SteamModel::Controller => (UsbKind::SteamController, "Steam Controller"),
        };
        let hello = Hello {
            kind,
            index,
            slot: 0,
            seat: 0,
            identity: identity.map(|i| i.blob()).unwrap_or_default(),
        };
        let attach = attach_device(hello, None, &format!("virtual {label} {index}"))?;
        Ok(SteamUsbip {
            model,
            attach,
            enc: Default::default(),
        })
    }

    /// The next Deck frame for `st`.
    pub fn write_state(&mut self, st: &SteamState) {
        let r = self.enc.encode(st);
        self.write_report(r);
    }

    /// The frame the controller interface streams from now on.
    pub fn write_report(&mut self, r: [u8; 64]) {
        self.attach.input(CONTROLLER, &r);
    }

    pub fn service(&mut self) -> SteamFeedback {
        feedback(self.model, self.attach.outputs())
    }

    /// `false` once the device is gone under the pad.
    pub fn alive(&self) -> bool {
        self.attach.alive()
    }
}

/// Default on. `PUNKTFUNK_STEAM_USBIP=0` skips usbip; `open` still degrades if `vhci_hcd` is missing.
pub fn usbip_preferred() -> bool {
    pf_host_config::env_on("PUNKTFUNK_STEAM_USBIP") != Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sysfs::{input_devices, wait_input_gone};
    use crate::usbip::ensure_modules;
    use std::os::fd::AsFd;
    use std::time::{Duration, Instant};

    use crate::usbip_relay::{drain, test_relay};

    /// A seat's vhci port binds only listed devices.
    #[test]
    fn both_pads_are_listed_devices() {
        for model in [SteamModel::Deck, SteamModel::Controller] {
            let dev = served(0, model, None, test_relay().0).device;
            assert!(crate::usbip::EMULATED.contains(&(dev.vendor_id, dev.product_id)));
        }
    }

    const EP0: UsbEndpoint = UsbEndpoint {
        address: 0,
        attributes: 0,
        max_packet_size: 64,
        interval: 0,
    };

    /// SET then GET on the controller interface, as hidraw's feature ioctls arrive.
    fn exchange(dev: &UsbDevice, set: &[u8]) -> Vec<u8> {
        let iface = dev.interfaces[2].clone();
        let mut handler = iface.handler.lock().unwrap();
        let mut setup = SetupPacket {
            request_type: 0x21,
            request: 0x09,
            value: 0x0300,
            index: 2,
            length: set.len() as u16,
        };
        handler.handle_urb(&iface, EP0, 0, setup, set).unwrap();
        (setup.request_type, setup.request) = (0xA1, 0x01);
        handler.handle_urb(&iface, EP0, 64, setup, &[]).unwrap()
    }

    /// The wired Steam Controller is the Deck's layout under its own ids: Steam's settings and
    /// haptics come back for the physical pad, lizard mode does not, and the pad answers the
    /// query dance from its canned table.
    #[test]
    fn a_steam_controller_forwards_writes_and_answers_queries() {
        let (out, pad) = test_relay();
        let dev = served(1, SteamModel::Controller, None, out).device;
        assert_eq!((dev.vendor_id, dev.product_id), (0x28DE, 0x1102));
        assert_eq!(dev.interfaces.len(), 3);

        assert_eq!(exchange(&dev, &[0x83])[..2], [0x83, 30]);
        let serial = exchange(&dev, &[0xAE, 0x15, 0x01]);
        assert_eq!(&serial[3..15], sc_serial(1).as_bytes());
        exchange(&dev, &[0x8E, 0x00]);
        exchange(&dev, &[0x8F, 7, 1, 0x10, 0x27, 0, 0, 1, 0, 0xAA]);
        let fb = feedback(SteamModel::Controller, drain(&pad));
        assert_eq!(fb.raw, [vec![0, 0x8F, 7, 1, 0x10, 0x27, 0, 0, 1, 0]]);
        assert_eq!(fb.rumble, None);

        let iface = dev.interfaces[2].clone();
        let polled = iface.handler.lock().unwrap().handle_urb(
            &iface,
            iface.endpoints[0],
            64,
            SetupPacket::default(),
            &[],
        );
        assert_eq!(polled.unwrap()[..4], [0x01, 0x00, 0x01, 0x3C]);
    }

    /// A captured pad answers as itself: its serial on the bus, its own reply before the
    /// canned one, with the report id the client recorded taken off.
    #[test]
    fn a_captured_steam_controller_answers_as_itself() {
        let mut reply = [0u8; 64];
        reply[..6].copy_from_slice(&[0x00, 0xAE, 0x03, 0x01, b'F', b'X']);
        let identity = Arc::new(Sc2Identity {
            serial: Some("FX1234".into()),
            replies: vec![(vec![0x00, 0xAE, 0x15, 0x01], reply)],
            slot: 0,
        });
        let dev = served(0, SteamModel::Controller, Some(identity), test_relay().0).device;
        assert_eq!(
            exchange(&dev, &[0xAE, 0x15, 0x01])[..5],
            [0xAE, 0x03, 0x01, b'F', b'X']
        );
        assert_eq!(
            exchange(&dev, &[0x83])[..2],
            [0x83, 30],
            "an unrecorded query"
        );
    }

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
        let mut pad = SteamUsbip::open(0).expect("open SteamUsbip (root + vhci_hcd?)");
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
        let mut pad = SteamUsbip::open(0).expect("open SteamUsbip (root + vhci_hcd?)");
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
        crate::uapi::ioctl_with(f.as_fd(), req, &mut buf).expect("HIDIOCSFEATURE");
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

    /// `hid-steam` binds the wired Steam Controller (`Steam Controller` evdev) and a haptic
    /// pulse written to its interface-2 hidraw comes back for the physical pad. Needs root +
    /// `vhci_hcd`.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_steam_controller_binds_and_returns_haptics() {
        ensure_modules();
        let mut pad = SteamUsbip::open_controller(0, None).expect("open (root + vhci_hcd?)");
        let start = Instant::now();
        let mut node = None;
        while node.is_none() && start.elapsed() < Duration::from_millis(1500) {
            let _ = pad.service();
            node = hid_steam_iface2();
            std::thread::sleep(Duration::from_millis(8));
        }
        let node = node.expect("no hid-steam hidraw on interface 2");
        assert!(
            input_devices().contains("Steam Controller"),
            "hid-steam did not bind the usbip Steam Controller"
        );
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .expect("open hidraw");
        // steam_haptic_pulse: [report-id 0, 0x8F, len 7, pad, duration(2), interval(2), count(2)]
        let pulse = [0u8, 0x8F, 7, 1, 0x10, 0x27, 0, 0, 1, 0];
        let mut buf = [0u8; 12];
        buf[..pulse.len()].copy_from_slice(&pulse);
        // HIDIOCSFEATURE(12)
        let req: libc::c_ulong =
            (3 << 30) | ((buf.len() as libc::c_ulong) << 16) | (0x48 << 8) | 0x06;
        crate::uapi::ioctl_with(f.as_fd(), req, &mut buf).expect("HIDIOCSFEATURE");
        let start = Instant::now();
        let mut got = Vec::new();
        while !got.contains(&pulse.to_vec()) && start.elapsed() < Duration::from_millis(1500) {
            got.extend(pad.service().raw);
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(
            got.contains(&pulse.to_vec()),
            "the pulse never surfaced: {got:02x?}"
        );
        drop(pad);
        assert!(
            wait_input_gone("Steam Controller", Duration::from_millis(400)),
            "device not torn down on drop"
        );
    }
}
