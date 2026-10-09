//! Virtual Switch 2 Pro Controller / GameCube controller over USB/IP (`vhci_hcd`).
//!
//! SDL and Steam reach a Switch 2 pad only through libusb: HID interface 0 carries input report
//! `0x05` and rumble, vendor interface 1 a bulk command channel they must claim. UHID has no
//! USB parent and no bulk interface, so this is the one transport; the host falls back to the
//! Switch Pro identity without `vhci_hcd` ([`available`]). The codec is
//! [`super::switch2_proto`]; attach is [`super::usbip::attach_device`].
//!
//! Each interrupt-IN poll serves the latest state with a fresh sequence byte and µs clock.
//! bInterval 4 on an absolute deadline paces the polls at the 250 Hz SDL calibrates its sensor
//! clock against; relative sleeps drift to about 5 ms and SDL then misreads the gyro scale.

use super::switch2_proto::{
    bulk_reply, parse_rumble, rdesc, serial, Command, Model, Switch2State, REPORT_INTERVAL_MS,
    VENDOR,
};
use super::usbip::{attach_device, boxed, ep, hid_class_descriptor, UsbipAttachment};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use parking_lot::Mutex;
use punktfunk_core::quic::{HidOutput, RichInput};
use std::any::Any;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use usbip_sim::{
    Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, UsbSpeed,
    Version,
};

/// Whether this host can attach a USB/IP device at all. The Switch 2 pads' only gate.
pub fn available() -> bool {
    super::usbip::vhci_base().is_some()
}

/// Host writes since the last [`Switch2Usbip::service`] drain.
#[derive(Debug, Default)]
pub struct Switch2Feedback {
    /// `(low, high)` from the last rumble report.
    pub rumble: Option<(u16, u16)>,
    /// Player-light bits from the last `0x09` command.
    pub leds: Option<u8>,
}

#[derive(Debug)]
struct HidHandler {
    model: Model,
    /// Shared with [`Switch2Usbip::write_state`].
    state: Arc<Mutex<Switch2State>>,
    feedback: Arc<Mutex<Switch2Feedback>>,
    seq: u8,
    epoch: Instant,
}

impl HidHandler {
    fn rumble(&self, report: &[u8]) {
        if let Some(r) = parse_rumble(self.model, report) {
            self.feedback.lock().rumble = Some(r);
        }
    }
}

impl UsbInterfaceHandler for HidHandler {
    /// bcdHID 1.11, country 0, one report descriptor.
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_class_descriptor(0x0111, 0, rdesc(self.model).len())
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
            return Ok(match (setup.request_type, setup.request) {
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => rdesc(self.model).to_vec(),
                // SET_REPORT (output): EP0 payload may omit the id.
                (0x21, 0x09) => {
                    let id = (setup.value & 0xFF) as u8;
                    if req.first() == Some(&id) {
                        self.rumble(req);
                    } else {
                        self.rumble(&[&[id][..], req].concat());
                    }
                    vec![]
                }
                _ => vec![], // SET_IDLE / SET_PROTOCOL
            });
        }
        if let Direction::In = ep.direction() {
            self.seq = self.seq.wrapping_add(1);
            let clock = self.epoch.elapsed().as_micros() as u32;
            Ok(self.state.lock().report(self.seq, clock).to_vec())
        } else {
            self.rumble(req);
            Ok(vec![])
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 1: each bulk-OUT command queues its reply for the bulk-IN reads that follow.
#[derive(Debug)]
struct BulkHandler {
    model: Model,
    index: u8,
    feedback: Arc<Mutex<Switch2Feedback>>,
    pending: VecDeque<u8>,
}

impl UsbInterfaceHandler for BulkHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        vec![]
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        len: u32,
        _setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0() {
            return Ok(vec![]);
        }
        if let Direction::In = ep.direction() {
            // An empty queue completes empty: deferring an IN stalls the USB/IP stream.
            let n = (len as usize).min(64).min(self.pending.len());
            return Ok(self.pending.drain(..n).collect());
        }
        if let Some((reply, what)) = bulk_reply(self.model, self.index, req) {
            // A reply the host never read is stale once it sends the next command.
            self.pending.clear();
            self.pending.extend(reply);
            if let Command::PlayerLeds(bits) = what {
                self.feedback.lock().leds = Some(bits);
            }
        }
        Ok(vec![])
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

fn build_device(
    model: Model,
    index: u8,
    state: &Arc<Mutex<Switch2State>>,
    feedback: &Arc<Mutex<Switch2Feedback>>,
) -> UsbDevice {
    let mut dev = UsbDevice::new(0);
    dev.vendor_id = VENDOR;
    dev.product_id = model.product();
    dev.usb_version = Version::from(0x0200u16);
    dev.device_bcd = Version::from(0x0101u16);
    dev.speed = UsbSpeed::Full as u32;
    dev.set_manufacturer_name("Nintendo Co., Ltd.");
    dev.set_product_name(model.name());
    dev.set_serial_number(&serial(model, index));
    dev.unset_configuration_name();
    dev.configuration_attributes = 0x80; // bus powered
    dev.configuration_max_power = 250; // 500 mA
    dev.absolute_interrupt_pacing = true;
    dev = dev.with_interface(
        0x03,
        0x00,
        0x00,
        None,
        vec![
            ep(0x81, 0x03, 64, REPORT_INTERVAL_MS),
            ep(0x03, 0x03, 64, REPORT_INTERVAL_MS),
        ],
        boxed(HidHandler {
            model,
            state: state.clone(),
            feedback: feedback.clone(),
            seq: 0,
            epoch: Instant::now(),
        }),
    );
    dev.with_interface(
        0xFF,
        0x00,
        0x00,
        None,
        vec![ep(0x82, 0x02, 64, 0), ep(0x02, 0x02, 64, 0)],
        boxed(BulkHandler {
            model,
            index,
            feedback: feedback.clone(),
            pending: VecDeque::new(),
        }),
    )
}

/// One attached pad. Drop detaches the `vhci_hcd` port and stops the emulation server.
pub struct Switch2Usbip {
    state: Arc<Mutex<Switch2State>>,
    feedback: Arc<Mutex<Switch2Feedback>>,
    _attach: UsbipAttachment,
}

impl Switch2Usbip {
    pub fn open(model: Model, index: u8) -> Result<Switch2Usbip> {
        let state = Arc::new(Mutex::new(Switch2State::neutral()));
        let feedback = Arc::new(Mutex::new(Switch2Feedback::default()));
        let attach = attach_device(
            || build_device(model, index, &state, &feedback),
            &format!("virtual {} {index}", model.name()),
        )?;
        Ok(Switch2Usbip {
            state,
            feedback,
            _attach: attach,
        })
    }

    /// The next interrupt-IN poll serves `st`.
    pub fn write_state(&self, st: &Switch2State) {
        *self.state.lock() = *st;
    }

    pub fn service(&self) -> Switch2Feedback {
        std::mem::take(&mut *self.feedback.lock())
    }
}

/// Switch 2 [`PadProto`]: the device paces its own reports, so a write only updates the state.
pub struct Switch2Proto {
    model: Model,
}

impl PadProto for Switch2Proto {
    type Pad = Switch2Usbip;
    type State = Switch2State;
    const LABEL: &'static str = "Switch 2";
    const DEVICE: &'static str = "Switch 2 controller";
    const CREATE_HINT: &'static str = " (load vhci_hcd: modprobe vhci_hcd)";

    fn open(&mut self, idx: u8) -> Result<Switch2Usbip> {
        let pad = Switch2Usbip::open(self.model, idx)?;
        tracing::info!(
            index = idx,
            model = self.model.name(),
            "virtual Switch 2 controller created (usbip)"
        );
        Ok(pad)
    }

    fn merge_frame(
        &self,
        prev: &Switch2State,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> Switch2State {
        Switch2State::merge_frame(self.model, prev, f)
    }

    fn apply_rich(&self, st: &mut Switch2State, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut Switch2Usbip, st: &Switch2State) {
        pad.write_state(st);
    }

    fn service(&self, pad: &mut Switch2Usbip, idx: u8) -> PadFeedback {
        let got = pad.service();
        let mut fb = PadFeedback::default();
        fb.rumble = got.rumble.map(|(low, high)| (low, high, 0, 0));
        fb.rumble_drove = Some(fb.rumble.is_some());
        if let Some(bits) = got.leds {
            fb.hidout.push(HidOutput::PlayerLeds { pad: idx, bits });
        }
        fb
    }
}

pub type Switch2Manager = UhidManager<Switch2Proto>;

pub fn manager(model: Model) -> Switch2Manager {
    UhidManager::with_backend(Switch2Proto { model })
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::gamepad as gs;
    use punktfunk_core::input::{GamepadEvent, GamepadFrame};
    use std::time::Duration;

    /// SDL's `FindBulkEndpoints` wants bulk IN and OUT on interface 1; the HID interface polls
    /// at 4 ms.
    #[test]
    fn device_has_the_layout_sdl_claims() {
        let state = Arc::new(Mutex::new(Switch2State::neutral()));
        let feedback = Arc::new(Mutex::new(Switch2Feedback::default()));
        let dev = build_device(Model::Pro, 2, &state, &feedback);
        assert_eq!((dev.vendor_id, dev.product_id), (0x057E, 0x2069));
        assert_eq!(dev.interfaces.len(), 2);
        let eps = |i: usize| -> Vec<(u8, u8, u8)> {
            dev.interfaces[i]
                .endpoints
                .iter()
                .map(|e| (e.address, e.attributes, e.interval))
                .collect()
        };
        assert_eq!(dev.interfaces[0].interface_class, 0x03);
        assert_eq!(eps(0), vec![(0x81, 3, 4), (0x03, 3, 4)]);
        assert_eq!(dev.interfaces[1].interface_class, 0xFF);
        assert_eq!(eps(1), vec![(0x82, 2, 0), (0x02, 2, 0)]);
    }

    /// Holds a Switch 2 Pro and a GameCube pad live for `PF_PAD_HOLD_SECS` (default 3) so an SDL
    /// probe can read them: faces, GR/GL and the triggers beat every 400 ms, a steady 100 °/s
    /// pitch, rumble and lights echoed to stdout.
    #[test]
    #[ignore = "attaches real vhci_hcd devices; needs root"]
    fn switch2_pads_hold_for_a_probe() {
        let secs = std::env::var("PF_PAD_HOLD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let mut pads = [(5u8, manager(Model::Pro)), (6, manager(Model::GameCube))];
        for (i, m) in &mut pads {
            m.handle(&GamepadEvent::Arrival {
                index: *i,
                kind: 0,
                capabilities: 0,
                audio_caps: 0,
            });
        }
        let live: usize = pads.iter().map(|(_, m)| m.live_pads()).sum();
        assert_eq!(live, 2, "both pads must attach");
        println!("Switch 2 pads up for {secs}s");
        let (start, mut last, mut beat) = (Instant::now(), Instant::now(), 0u32);
        let mut last_motion = Instant::now();
        while start.elapsed() < Duration::from_secs(secs) {
            if last_motion.elapsed() >= Duration::from_millis(20) {
                last_motion = Instant::now();
                for (i, m) in &mut pads {
                    m.apply_rich(RichInput::Motion {
                        pad: *i,
                        gyro: [(100 * gs::MOTION_GYRO_LSB_PER_DEG_S) as i16, 0, 0],
                        accel: [0, gs::MOTION_ACCEL_LSB_PER_G as i16, 0],
                    });
                }
            }
            if last.elapsed() >= Duration::from_millis(400) {
                last = Instant::now();
                beat += 1;
                let on = beat % 2 == 0;
                for (i, m) in &mut pads {
                    m.handle(&GamepadEvent::State(GamepadFrame {
                        index: *i as i16,
                        active_mask: 1 << *i,
                        buttons: if on {
                            gs::BTN_A | gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_RB
                        } else {
                            0
                        },
                        left_trigger: if on { 255 } else { 0 },
                        right_trigger: if on { 128 } else { 0 },
                        ..Default::default()
                    }));
                }
            }
            for (_, m) in &mut pads {
                m.pump(
                    |pad, low, high, _, _| println!("rumble pad={pad} low={low} high={high}"),
                    |out| println!("hidout {out:?}"),
                );
                m.heartbeat(Duration::from_millis(8));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
