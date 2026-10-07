//! Virtual Steam Controller 2 over USB/IP (`vhci_hcd`) — the Steam-promotable
//! transport for the as-is backend ([`super::steam_controller2`]).
//!
//! Steam Input requires a real USB parent (`Interface` ≥ 0). UHID enumerates
//! `Interface: -1` and is dropped. Wired identity is `28DE:1302` (one HID
//! interface). Puck is `28DE:1304`: CDC 0–1, HID slots 2–5, management HID 6.
//!
//! Report bodies are not translated. Client kind selects only the USB topology
//! that owned those bytes. Interrupt-OUT and SET_REPORT traffic is returned
//! to the physical-device owner.
//!
//! Pads behind one physical Puck share one virtual Puck ([`PuckHub`]) at their
//! own slots, as the client names them in its identity.
//!
//! Pin the topologies with [`tests::device_matches_wired_capture`] and
//! [`tests::device_matches_puck_capture`]. Attach is
//! [`super::steam_usbip::attach_device`].

use super::steam_usbip::{attach_device, boxed, UsbipAttachment};
use super::triton_proto::{
    identity_for, parse_triton_rumble, serialize_triton_state, triton_feature_reply, triton_serial,
    triton_unit_id, Sc2Identity, TritonState, TRITON_RDESC, TRITON_STATE_LEN,
};
use anyhow::Result;
use parking_lot::Mutex;
use std::any::Any;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use usbip_sim::{
    Direction, SetupPacket, UsbDevice, UsbEndpoint, UsbInterface, UsbInterfaceHandler, UsbSpeed,
    Version,
};

const TRITON_VENDOR: u16 = 0x28DE;
const TRITON_WIRED_PRODUCT: u16 = 0x1302;
const TRITON_PUCK_PRODUCT: u16 = 0x1304;

/// Interface 6: Puck management HID. Not a controller slot.
const PUCK_MANAGEMENT_RDESC: &[u8] = &[
    0x06, 0x00, 0xFF, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x42, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08,
    0x95, 0x35, 0x09, 0x42, 0x81, 0x02, 0x85, 0x79, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95,
    0x01, 0x09, 0x79, 0x81, 0x02, 0x85, 0x01, 0x95, 0x3F, 0x09, 0x01, 0xB1, 0x02, 0x85, 0x02, 0x95,
    0x3F, 0x09, 0x01, 0xB1, 0x02, 0xC0,
];

/// Steam writes since the last [`TritonUsbip::service`] drain.
#[derive(Debug, Default)]
pub struct TritonUsbFeedback {
    /// `(low, high)` from the last `0x80` output report.
    pub rumble: Option<(u16, u16)>,
    pub raw: Vec<(u8, Vec<u8>)>,
}

#[derive(Clone, Copy, Debug)]
struct InputReport {
    data: [u8; 64],
    len: u8,
}

impl Default for InputReport {
    fn default() -> Self {
        Self {
            data: [0; 64],
            len: 0,
        }
    }
}

/// How long a pad goes without state before it reads neutral once.
const STALE: Duration = Duration::from_millis(500);

/// Sparse reports (battery/RSSI/wireless) queue; `0x42`/`0x45` state is newest-wins and served
/// once. An id the descriptor lacks (`0x47`) is dropped. A poll with nothing new waits, as on the real pad; after [`STALE`] without state
/// the pad reads neutral once, so a stalled client does not hold a stick or a gyro rate.
#[derive(Debug)]
struct InputReports {
    latest_state: InputReport,
    pending: VecDeque<InputReport>,
    /// `latest_state` has not been served.
    fresh: bool,
    /// When the last state arrived; `None` once the neutral read went out.
    written: Option<Instant>,
}

impl InputReports {
    fn new(latest_state: InputReport) -> Self {
        Self {
            latest_state,
            pending: VecDeque::new(),
            fresh: true,
            written: None,
        }
    }

    /// An empty Puck slot: nothing to serve.
    fn quiet() -> Self {
        Self {
            fresh: false,
            ..Self::new(InputReport::default())
        }
    }

    fn with_pending(latest_state: InputReport, pending: InputReport) -> Self {
        let mut reports = Self::new(latest_state);
        reports.pending.push_back(pending);
        reports
    }

    fn write(&mut self, report: InputReport) {
        match report.data[0] {
            0x42 | 0x45 => {
                self.latest_state = report;
                self.fresh = true;
                self.written = Some(Instant::now());
            }
            id if pf_driver_proto::triton::input_len(id).is_none() => {}
            // Battery 0x43, RSSI 0x44/0x7B, wireless 0x79: queue until a poll consumes them.
            _ => {
                if self.pending.len() >= 32 {
                    self.pending.pop_front();
                }
                self.pending.push_back(report);
            }
        }
    }

    /// What the next poll gets; `None` when nothing is new.
    fn read(&mut self, now: Instant) -> Option<InputReport> {
        if let Some(report) = self.pending.pop_front() {
            return Some(report);
        }
        if std::mem::take(&mut self.fresh) {
            return Some(self.latest_state);
        }
        if self.written.is_some_and(|t| now.duration_since(t) >= STALE) {
            self.written = None;
            return Some(neutral_report());
        }
        None
    }
}

/// HID class descriptor: bcdHID 1.11, country 0. Do not reuse the Deck helper (1.10 / country 33).
fn triton_hid_desc() -> Vec<u8> {
    let l = TRITON_RDESC.len() as u16;
    vec![
        0x09,
        0x21,
        0x11,
        0x01,
        0,
        1,
        0x22,
        (l & 0xff) as u8,
        (l >> 8) as u8,
    ]
}

/// Feature GET reply for a Puck slot: report 2 answers the dongle's queries, report 1 the pad's.
/// Tag 4 of the `0x83` attributes is [`pf_driver_proto::triton::FW_BUILD_TIME`], as in the
/// report-1 `0xF2` reply. An older build time makes Steam offer a firmware update.
fn triton_puck_feature_reply(last_set: &[u8], serial: &str, unit_id: u32, status: u8) -> [u8; 64] {
    let Some((&report_id, body)) = last_set.split_first() else {
        return triton_feature_reply(last_set, serial, unit_id);
    };
    if report_id == 0x01 {
        if body.first() == Some(&0xED) {
            let mut reply = [0u8; 64];
            reply[..3].copy_from_slice(&[0x01, 0xED, 0]);
            let payload = body.get(2..).unwrap_or_default();
            if payload.starts_with(b"user/wireless_transport") {
                reply[2] = 1;
                reply[3] = 2; // slot 0 → transport 0 XOR 2
            } else if status == 0x02 && payload.starts_with(b"esb/bond") {
                reply[2] = 0x18;
                write_puck_bond(&mut reply[3..27], serial, unit_id);
            }
            return reply;
        }
        return triton_feature_reply(last_set, serial, unit_id);
    }
    if report_id != 0x02 {
        return triton_feature_reply(last_set, serial, unit_id);
    }

    let cmd = body.first().copied().unwrap_or(0xB4);
    let mut reply = [0u8; 64];
    reply[0] = 0x02;
    reply[1] = cmd;
    match cmd {
        0x83 => {
            reply[2] = 0x19;
            let attrs = [
                (0x01, TRITON_PUCK_PRODUCT as u32),
                (0x02, 0),
                (0x0A, unit_id ^ 0xFC),
                (0x04, pf_driver_proto::triton::FW_BUILD_TIME),
                (0x09, 0x47),
            ];
            let mut o = 3;
            for (id, value) in attrs {
                reply[o] = id;
                reply[o + 1..o + 5].copy_from_slice(&value.to_le_bytes());
                o += 5;
            }
        }
        0xA3 => {
            reply[2] = 0x18;
            if status == 0x02 {
                write_puck_bond(&mut reply[3..27], serial, unit_id);
            }
        }
        0xB4 => reply[..4].copy_from_slice(&[0x02, 0xB4, 0x01, status]),
        _ => {
            let n = body.len().min(63);
            reply[1..1 + n].copy_from_slice(&body[..n]);
        }
    }
    reply
}

fn write_puck_bond(out: &mut [u8], serial: &str, unit_id: u32) {
    out[..4].copy_from_slice(&unit_id.to_le_bytes());
    out[4..8].copy_from_slice(&(unit_id ^ 0x67BF_44D2).to_le_bytes());
    let serial = serial.as_bytes();
    let len = serial.len().min(16);
    out[8..8 + len].copy_from_slice(&serial[..len]);
}

/// Who one interface answers as. Shared with its handler: a Puck slot changes hands as pads
/// come and go.
#[derive(Debug, Default)]
struct SlotPad {
    serial: String,
    unit_id: u32,
    /// `None` wired; Puck `0xB4` is 2 (connected) or 1 (empty).
    puck_status: Option<u8>,
    /// A real pad's recorded replies, answered before the canned table.
    identity: Option<Arc<Sc2Identity>>,
}

impl SlotPad {
    fn new(index: u8, puck_status: Option<u8>, identity: Option<Arc<Sc2Identity>>) -> SlotPad {
        SlotPad {
            serial: triton_serial(index),
            unit_id: triton_unit_id(index),
            puck_status,
            identity,
        }
    }
}

#[derive(Debug)]
struct TritonHandler {
    /// Shared with [`TritonUsbip::write_state`].
    reports: Arc<Mutex<InputReports>>,
    feedback: Arc<Mutex<TritonUsbFeedback>>,
    pad: Arc<Mutex<SlotPad>>,
    /// Last feature SET_REPORT, id-first. GET echoes this command.
    last_set: Vec<u8>,
    last_get_logged: u8,
}

impl TritonHandler {
    fn queue_raw(&self, kind: u8, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        let mut fb = self.feedback.lock();
        if fb.raw.len() >= 32 {
            fb.raw.remove(0);
        }
        fb.raw.push((kind, data));
    }
}

impl UsbInterfaceHandler for TritonHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        triton_hid_desc()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        use punktfunk_core::quic::{HID_RAW_FEATURE, HID_RAW_OUTPUT};
        if ep.is_ep0() {
            Ok(match (setup.request_type, setup.request) {
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => TRITON_RDESC.to_vec(),
                // Feature GET: the real pad's recorded reply to the last SET, else a canned one
                // echoing its command. A wrong command byte makes Steam drop the pad.
                (0xA1, 0x01) => {
                    let pad = self.pad.lock();
                    let recorded = pad.identity.as_ref().and_then(|i| i.reply(&self.last_set));
                    let reply = if let Some(reply) = recorded {
                        reply
                    } else if let Some(status) = pad.puck_status {
                        triton_puck_feature_reply(&self.last_set, &pad.serial, pad.unit_id, status)
                    } else {
                        triton_feature_reply(&self.last_set, &pad.serial, pad.unit_id)
                    };
                    drop(pad);
                    if reply[1] != self.last_get_logged {
                        self.last_get_logged = reply[1];
                        tracing::debug!(
                            cmd = %format_args!("{:#04x}", reply[1]),
                            "virtual SC2 usbip: answering feature GET"
                        );
                    }
                    reply.to_vec()
                }
                // SET_REPORT: type in wValue high (2=OUT, 3=FEATURE), id in low.
                // EP0 payload may omit the id; normalize to id-first for the physical owner.
                (0x21, 0x09) => {
                    let report_type = (setup.value >> 8) as u8;
                    let id = (setup.value & 0xFF) as u8;
                    let framed = if req.first() == Some(&id) && id != 0 {
                        req.to_vec()
                    } else {
                        let mut v = Vec::with_capacity(req.len() + 1);
                        v.push(id);
                        v.extend_from_slice(req);
                        v
                    };
                    match report_type {
                        2 => {
                            if let Some(r) = parse_triton_rumble(&framed) {
                                self.feedback.lock().rumble = Some(r);
                            }
                            self.queue_raw(HID_RAW_OUTPUT, framed);
                        }
                        3 => {
                            self.last_set = framed.clone();
                            if pf_driver_proto::triton::forwards_to_pad(&framed) {
                                self.queue_raw(HID_RAW_FEATURE, framed);
                            }
                        }
                        _ => {}
                    }
                    vec![]
                }
                (0x21, 0x0A) | (0x21, 0x0B) => vec![], // SET_IDLE / SET_PROTOCOL
                _ => vec![],
            })
        } else if let Direction::In = ep.direction() {
            match self.reports.lock().read(Instant::now()) {
                Some(r) => Ok(r.data[..r.len as usize].to_vec()),
                None => Err(std::io::ErrorKind::WouldBlock.into()),
            }
        } else {
            // Interrupt-OUT is already id-first (`SDL_hid_write`); EP0 SET_REPORT may not be.
            if !req.is_empty() {
                if let Some(r) = parse_triton_rumble(req) {
                    self.feedback.lock().rumble = Some(r);
                }
                self.queue_raw(HID_RAW_OUTPUT, req.to_vec());
            }
            Ok(vec![])
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug)]
struct IdleHandler {
    class_descriptor: Vec<u8>,
    report_descriptor: &'static [u8],
    input_report: &'static [u8],
}

impl UsbInterfaceHandler for IdleHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        self.class_descriptor.clone()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if ep.is_ep0()
            && setup.request_type == 0x81
            && setup.request == 0x06
            && (setup.value >> 8) == 0x22
        {
            Ok(self.report_descriptor.to_vec())
        } else if !ep.is_ep0() && matches!(ep.direction(), Direction::In) {
            Ok(self.input_report.to_vec())
        } else {
            Ok(vec![])
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug)]
struct CdcControlHandler;

impl UsbInterfaceHandler for CdcControlHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        vec![
            0x05, 0x24, 0x00, 0x10, 0x01, // CDC header, bcdCDC 1.10
            0x05, 0x24, 0x01, 0x00, 0x01, // call management → data interface 1
            0x04, 0x24, 0x02, 0x02, // ACM: line coding + serial state
            0x05, 0x24, 0x06, 0x00, 0x01, // union: master 0, slave 1
        ]
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        Ok(match (setup.request_type, setup.request) {
            (0xA1, 0x21) => vec![0x00, 0xC2, 0x01, 0x00, 0x00, 0x00, 0x08], // 115200 8N1
            _ => vec![], // SET_LINE_CODING / SET_CONTROL_LINE_STATE / endpoint polls
        })
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug, Default)]
struct PuckManagementHandler {
    last_set: Vec<u8>,
}

impl UsbInterfaceHandler for PuckManagementHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        let len = PUCK_MANAGEMENT_RDESC.len() as u16;
        vec![
            0x09,
            0x21,
            0x11,
            0x01,
            0,
            1,
            0x22,
            len as u8,
            (len >> 8) as u8,
        ]
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if !ep.is_ep0() {
            return Ok(vec![]);
        }
        Ok(match (setup.request_type, setup.request) {
            (0x81, 0x06) if (setup.value >> 8) == 0x22 => PUCK_MANAGEMENT_RDESC.to_vec(),
            (0x21, 0x09) => {
                let id = (setup.value & 0xFF) as u8;
                self.last_set.clear();
                if req.first() != Some(&id) && id != 0 {
                    self.last_set.push(id);
                }
                self.last_set.extend_from_slice(req);
                vec![]
            }
            (0xA1, 0x01) => {
                let mut reply = vec![0u8; 64];
                reply[0] = 0x02;
                let command = self.last_set.get(1).copied().unwrap_or(0);
                reply[1] = command;
                // Management HID is not a slot: 0xB4 GET is 02 B4 01 01, never a connected pad.
                if command == 0xB4 {
                    reply[2] = 1;
                    reply[3] = 1;
                }
                reply
            }
            (0x21, 0x0A) | (0x21, 0x0B) => vec![],
            _ => vec![],
        })
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Wired `28DE:1302`. `reports` and `feedback` are shared with the owning [`TritonUsbip`].
fn build_triton_device(
    index: u8,
    reports: &Arc<Mutex<InputReports>>,
    feedback: &Arc<Mutex<TritonUsbFeedback>>,
    identity: Option<&Arc<Sc2Identity>>,
) -> UsbDevice {
    let ep = |addr: u8| UsbEndpoint {
        address: addr,
        attributes: 0x03, // interrupt
        max_packet_size: 64,
        // Full-speed bInterval is milliseconds, not the HS 2^(n-1)×125 µs exponent.
        // 1 = 1 kHz. Do not "fix" to 4: that is 4 ms / 250 Hz on FS.
        interval: 1,
    };
    let mut dev = UsbDevice::new(0);
    dev.vendor_id = TRITON_VENDOR;
    dev.product_id = TRITON_WIRED_PRODUCT;
    dev.usb_version = Version::from(0x0200u16);
    dev.device_bcd = Version::from(0x0307u16);
    dev.device_class = 0xEF; // IAD (miscellaneous)
    dev.device_subclass = 0x02;
    dev.device_protocol = 0x01;
    dev.speed = UsbSpeed::Full as u32;
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name("Steam Controller");
    dev.set_serial_number(
        &identity
            .and_then(|i| i.serial.clone())
            .unwrap_or_else(|| triton_serial(index)),
    );
    dev.unset_configuration_name(); // iConfiguration = 0
    dev.configuration_attributes = 0xA0; // bus powered + remote wakeup
    dev.configuration_max_power = 250; // 500 mA in 2 mA units
    dev.with_interface(
        0x03, // HID
        0x00,
        0x00,
        None, // iInterface = 0
        vec![ep(0x81), ep(0x01)],
        boxed(TritonHandler {
            reports: reports.clone(),
            feedback: feedback.clone(),
            pad: Arc::new(Mutex::new(SlotPad::new(index, None, identity.cloned()))),
            last_set: Vec::new(),
            last_get_logged: 0,
        }),
    )
}

/// Puck `28DE:1304`: slot `n` is interface `n + 2`, each driven by its [`PuckSlot`].
fn build_puck_device(serial: &str, slots: &[PuckSlot; 4]) -> UsbDevice {
    let interrupt = |addr: u8, interval: u8| UsbEndpoint {
        address: addr,
        attributes: 0x03,
        max_packet_size: 64,
        interval,
    };
    let bulk = |addr: u8| UsbEndpoint {
        address: addr,
        attributes: 0x02,
        max_packet_size: 64,
        interval: 0,
    };

    let mut dev = UsbDevice::new(0);
    dev.vendor_id = TRITON_VENDOR;
    dev.product_id = TRITON_PUCK_PRODUCT;
    dev.usb_version = Version::from(0x0201u16);
    dev.device_bcd = Version::from(0x0002u16);
    dev.device_class = 0xEF;
    dev.device_subclass = 0x02;
    dev.device_protocol = 0x01;
    dev.speed = UsbSpeed::Full as u32;
    dev.configuration_attributes = 0xA0;
    dev.configuration_max_power = 250;
    dev.configuration_descriptor_prefix = vec![0x08, 0x0B, 0x00, 0x02, 0x02, 0x02, 0x00, 0x00]; // CDC IAD, interfaces 0–1
    dev.bos_descriptor = Some(vec![
        0x05, 0x0F, 0x0C, 0x00, 0x01, // BOS, one capability
        0x07, 0x10, 0x02, 0x00, 0x00, 0x00, 0x00, // USB 2 extension, no LPM
    ]);
    dev.set_manufacturer_name("Valve Software");
    dev.set_product_name("Steam Controller Puck");
    dev.set_serial_number(serial);
    dev.unset_configuration_name();

    dev = dev.with_interface(
        0x02,
        0x02,
        0x00,
        None,
        vec![UsbEndpoint {
            address: 0x81,
            attributes: 0x03,
            max_packet_size: 16,
            interval: 10,
        }],
        boxed(CdcControlHandler),
    );
    dev = dev.with_interface(
        0x0A,
        0x00,
        0x00,
        None,
        vec![bulk(0x82), bulk(0x01)],
        boxed(IdleHandler {
            class_descriptor: vec![],
            report_descriptor: &[],
            input_report: &[],
        }),
    );

    for (n, slot) in (0u8..).zip(slots) {
        let handler = boxed(TritonHandler {
            reports: slot.reports.clone(),
            feedback: slot.feedback.clone(),
            pad: slot.pad.clone(),
            last_set: Vec::new(),
            last_get_logged: 0,
        });
        dev = dev.with_interface(
            0x03,
            0x00,
            0x00,
            None,
            vec![interrupt(0x83 + n, 2), interrupt(0x02 + n, 2)],
            handler,
        );
    }

    dev.with_interface(
        0x03,
        0x00,
        0x00,
        None,
        vec![interrupt(0x87, 32), interrupt(0x06, 32)],
        boxed(PuckManagementHandler::default()),
    )
}

/// One Puck slot: its interface's reports, Steam's writes to it, and who it answers as.
#[derive(Clone, Debug)]
struct PuckSlot {
    reports: Arc<Mutex<InputReports>>,
    feedback: Arc<Mutex<TritonUsbFeedback>>,
    pad: Arc<Mutex<SlotPad>>,
}

impl PuckSlot {
    fn empty(index: u8) -> PuckSlot {
        PuckSlot {
            reports: Arc::new(Mutex::new(InputReports::quiet())),
            feedback: Arc::default(),
            pad: Arc::new(Mutex::new(SlotPad::new(index, Some(0x01), None))),
        }
    }
}

/// A virtual Puck. Pads from one physical Puck each sit at their own slot; the last to leave
/// detaches it.
pub struct PuckHub {
    slots: [PuckSlot; 4],
    taken: Mutex<[bool; 4]>,
    attach: Mutex<Option<UsbipAttachment>>,
}

impl PuckHub {
    fn new(index: u8) -> PuckHub {
        PuckHub {
            slots: std::array::from_fn(|_| PuckSlot::empty(index)),
            taken: Mutex::new([false; 4]),
            attach: Mutex::new(None),
        }
    }

    /// Seat a pad on `slot`: it reports connected and answers as `identity`. False when taken.
    fn seat(&self, slot: usize, index: u8, identity: Option<Arc<Sc2Identity>>) -> bool {
        let mut taken = self.taken.lock();
        if taken[slot] {
            return false;
        }
        taken[slot] = true;
        let s = &self.slots[slot];
        // Identity before the connect edge, so Steam's first query after it sees the pad.
        *s.pad.lock() = SlotPad::new(index, Some(0x02), identity);
        *s.feedback.lock() = TritonUsbFeedback::default();
        *s.reports.lock() = InputReports::with_pending(neutral_report(), puck_connect_report());
        true
    }

    /// The pad on `slot` left: one disconnect edge, then the slot is quiet.
    fn vacate(&self, slot: usize) {
        let s = &self.slots[slot];
        let mut reports = InputReports::quiet();
        reports.pending.push_back(puck_disconnect_report());
        *s.reports.lock() = reports;
        let mut pad = s.pad.lock();
        pad.puck_status = Some(0x01);
        pad.identity = None;
        drop(pad);
        self.taken.lock()[slot] = false;
    }
}

/// This session's virtual Pucks, by the physical Puck's USB serial.
#[derive(Default)]
pub struct PuckHubs(HashMap<String, Weak<PuckHub>>);

impl PuckHubs {
    /// Seat a pad on the live Puck `serial` names; `None` when there is none or `slot` is taken.
    fn join(
        &mut self,
        serial: &str,
        slot: usize,
        index: u8,
        identity: Option<Arc<Sc2Identity>>,
    ) -> Option<Arc<PuckHub>> {
        self.0.retain(|_, hub| hub.strong_count() > 0);
        let hub = self.0.get(serial)?.upgrade()?;
        hub.seat(slot, index, identity).then_some(hub)
    }
}

enum Owner {
    Wired { _attach: UsbipAttachment },
    Puck { hub: Arc<PuckHub>, slot: usize },
}

/// Drop detaches a wired pad's `vhci_hcd` port, or vacates a Puck pad's slot; the Puck goes
/// with its last pad.
pub struct TritonUsbip {
    reports: Arc<Mutex<InputReports>>,
    feedback: Arc<Mutex<TritonUsbFeedback>>,
    owner: Owner,
    seq: u8,
}

impl Drop for TritonUsbip {
    fn drop(&mut self) {
        if let Owner::Puck { hub, slot } = &self.owner {
            hub.vacate(*slot);
        }
    }
}

impl TritonUsbip {
    /// Attach a wired SC2 via `vhci_hcd` (root + module; [`super::steam_usbip::attach_device`]).
    /// `index` varies only the serial.
    pub fn open(index: u8) -> Result<TritonUsbip> {
        let reports = Arc::new(Mutex::new(InputReports::new(neutral_report())));
        let feedback = Arc::new(Mutex::new(TritonUsbFeedback::default()));
        let identity = identity_for(index);
        let attach = attach_device(
            || build_triton_device(index, &reports, &feedback, identity.as_ref()),
            &format!("virtual Steam Controller 2 {index}"),
        )?;
        Ok(TritonUsbip {
            reports,
            feedback,
            owner: Owner::Wired { _attach: attach },
            seq: 0,
        })
    }

    /// Seat a Puck pad at the slot its client named. It joins the virtual Puck of the same
    /// physical Puck when that slot is free; otherwise it gets a Puck of its own, attached with
    /// the pad already seated.
    pub fn open_puck(index: u8, hubs: &mut PuckHubs) -> Result<TritonUsbip> {
        let identity = identity_for(index);
        let serial = identity.as_ref().and_then(|i| i.serial.clone());
        let slot = identity.as_ref().map_or(0, |i| usize::from(i.slot.min(3)));
        if let Some(hub) = serial
            .as_deref()
            .and_then(|s| hubs.join(s, slot, index, identity.clone()))
        {
            tracing::info!(index, slot, "virtual Steam Controller 2 joined its Puck");
            return Ok(TritonUsbip::on_hub(hub, slot));
        }
        let hub = Arc::new(PuckHub::new(index));
        hub.seat(slot, index, identity);
        let usb_serial = serial
            .clone()
            .unwrap_or_else(|| format!("FVPFPUCK{index:04}"));
        let attach = attach_device(
            || build_puck_device(&usb_serial, &hub.slots),
            &format!("virtual Steam Controller 2 Puck {index}"),
        )?;
        *hub.attach.lock() = Some(attach);
        // A taken slot on a live Puck leaves that Puck registered; this one stays private.
        if let Some(serial) = serial {
            hubs.0.entry(serial).or_insert_with(|| Arc::downgrade(&hub));
        }
        Ok(TritonUsbip::on_hub(hub, slot))
    }

    fn on_hub(hub: Arc<PuckHub>, slot: usize) -> TritonUsbip {
        let s = &hub.slots[slot];
        TritonUsbip {
            reports: s.reports.clone(),
            feedback: s.feedback.clone(),
            owner: Owner::Puck {
                hub: hub.clone(),
                slot,
            },
            seq: 0,
        }
    }

    /// Push one interrupt-IN report. Continuous state newest-wins; sparse reports queue.
    pub fn write_state(&mut self, st: &TritonState) {
        let (data, len) = st.report(&mut self.seq);
        self.reports.lock().write(InputReport {
            data,
            len: len as u8,
        });
    }

    pub fn service(&mut self) -> TritonUsbFeedback {
        std::mem::take(&mut *self.feedback.lock())
    }
}

/// Idle `0x42` state — what the wired endpoint streams before the first write.
fn neutral_report() -> InputReport {
    let mut report = InputReport {
        len: TRITON_STATE_LEN as u8,
        ..InputReport::default()
    };
    let mut s = [0u8; TRITON_STATE_LEN];
    serialize_triton_state(&mut s, &TritonState::neutral(), 0);
    report.data[..TRITON_STATE_LEN].copy_from_slice(&s);
    report
}

/// Puck wireless-connect edge (`0x79 0x02`), queued before the first state packet.
fn puck_connect_report() -> InputReport {
    wireless_report(0x02)
}

/// Puck wireless-disconnect edge (`0x79 0x01`): sent once as a pad leaves its slot. Steam
/// re-probes a slot that keeps sending it.
fn puck_disconnect_report() -> InputReport {
    wireless_report(0x01)
}

fn wireless_report(status: u8) -> InputReport {
    let mut report = InputReport {
        len: 2,
        ..InputReport::default()
    };
    report.data[..2].copy_from_slice(&[0x79, status]);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sysfs::{hid_entry, wait_hid_gone};

    #[test]
    fn sparse_input_report_survives_following_state() {
        let mut reports = InputReports::new(neutral_report());
        let mut signal = InputReport {
            len: 13,
            ..InputReport::default()
        };
        signal.data[..3].copy_from_slice(&[0x7B, 0xF8, 0x01]);
        let mut next_state = neutral_report();
        next_state.data[1] = 9;

        reports.write(signal);
        reports.write(next_state);

        let now = Instant::now();
        assert_eq!(reports.read(now).unwrap().data[..3], [0x7B, 0xF8, 0x01]);
        assert_eq!(reports.read(now).unwrap().data[1], 9);
        assert!(reports.read(now).is_none(), "a state is served once");
    }

    /// A client that stops sending leaves the pad neutral once, then quiet.
    #[test]
    fn a_stalled_pad_reads_neutral_once() {
        let mut reports = InputReports::new(neutral_report());
        let now = Instant::now();
        assert!(
            reports.read(now).is_some(),
            "the first poll gets the idle state"
        );
        let mut held = neutral_report();
        held.data[10] = 0xFF;
        reports.write(held);
        assert_eq!(reports.read(now).unwrap().data[10], 0xFF);
        assert!(reports.read(now + STALE / 2).is_none());
        let neutral = reports
            .read(now + STALE * 2)
            .expect("neutral after the stall");
        assert_eq!(neutral.data[10], 0);
        assert!(reports.read(now + STALE * 4).is_none());
    }

    #[test]
    fn device_matches_wired_capture() {
        let reports = Arc::new(Mutex::new(InputReports::new(InputReport::default())));
        let feedback = Arc::new(Mutex::new(TritonUsbFeedback::default()));
        let dev = build_triton_device(3, &reports, &feedback, None);
        assert_eq!((dev.vendor_id, dev.product_id), (0x28DE, 0x1302));
        assert_eq!(
            (dev.device_class, dev.device_subclass, dev.device_protocol),
            (0xEF, 0x02, 0x01)
        );
        assert_eq!(dev.speed, UsbSpeed::Full as u32);
        assert_eq!(dev.interfaces.len(), 1);
        let i = &dev.interfaces[0];
        assert_eq!(
            (
                i.interface_class,
                i.interface_subclass,
                i.interface_protocol
            ),
            (0x03, 0x00, 0x00)
        );
        let eps: Vec<(u8, u8, u16, u8)> = i
            .endpoints
            .iter()
            .map(|e| (e.address, e.attributes, e.max_packet_size, e.interval))
            .collect();
        assert_eq!(eps, vec![(0x81, 3, 64, 1), (0x01, 3, 64, 1)]);
        let hid = triton_hid_desc();
        assert_eq!(&hid[2..4], &[0x11, 0x01]);
        assert_eq!(
            u16::from_le_bytes([hid[7], hid[8]]) as usize,
            TRITON_RDESC.len()
        );
        assert!(triton_serial(3).starts_with("FVPF")); // conflict-gate exclusion prefix
    }

    /// Two pads from one Puck share it at their own slots; a taken slot is refused, and a
    /// pad that leaves sends one disconnect edge and goes quiet.
    #[test]
    fn pads_from_one_puck_share_it_at_their_slots() {
        let mut hubs = PuckHubs::default();
        let hub = Arc::new(PuckHub::new(0));
        assert!(hub.seat(2, 0, None));
        hubs.0.insert("PUCK1".into(), Arc::downgrade(&hub));

        let joined = hubs
            .join("PUCK1", 0, 1, None)
            .expect("a free slot on a live Puck");
        assert!(Arc::ptr_eq(&joined, &hub));
        assert!(hubs.join("PUCK1", 2, 3, None).is_none(), "slot 2 is taken");
        assert!(hubs.join("PUCK2", 1, 3, None).is_none(), "another Puck");

        let now = Instant::now();
        let slot0 = &hub.slots[0];
        assert_eq!(slot0.pad.lock().puck_status, Some(0x02));
        assert_eq!(slot0.pad.lock().serial, triton_serial(1));
        assert_eq!(
            slot0.reports.lock().read(now).unwrap().data[..2],
            [0x79, 0x02]
        );
        assert!(
            hub.slots[1].reports.lock().read(now).is_none(),
            "an empty slot is quiet"
        );

        hub.vacate(0);
        assert_eq!(slot0.pad.lock().puck_status, Some(0x01));
        assert_eq!(
            slot0.reports.lock().read(now).unwrap().data[..2],
            [0x79, 0x01]
        );
        assert!(slot0.reports.lock().read(now + STALE * 2).is_none());
        assert!(
            hubs.join("PUCK1", 0, 4, None).is_some(),
            "a vacated slot is free again"
        );

        drop((joined, hub));
        assert!(
            hubs.join("PUCK1", 1, 5, None).is_none(),
            "the Puck went with its pads"
        );
    }

    #[test]
    fn device_matches_puck_capture() {
        let hub = PuckHub::new(1);
        assert!(hub.seat(0, 1, None));
        let dev = build_puck_device("FVPFPUCK0001", &hub.slots);
        assert_eq!((dev.vendor_id, dev.product_id), (0x28DE, 0x1304));
        assert_eq!(
            (
                dev.usb_version.major,
                dev.usb_version.minor,
                dev.usb_version.patch,
            ),
            (0x02, 0x00, 0x01)
        );
        assert_eq!(
            (
                dev.device_bcd.major,
                dev.device_bcd.minor,
                dev.device_bcd.patch,
            ),
            (0x00, 0x00, 0x02)
        );
        assert_eq!(
            (dev.device_class, dev.device_subclass, dev.device_protocol),
            (0xEF, 0x02, 0x01)
        );
        assert_eq!(dev.speed, UsbSpeed::Full as u32);
        assert_eq!(
            (dev.configuration_attributes, dev.configuration_max_power),
            (0xA0, 250)
        );
        assert_eq!(
            dev.configuration_descriptor_prefix,
            [0x08, 0x0B, 0x00, 0x02, 0x02, 0x02, 0x00, 0x00]
        );
        assert_eq!(dev.bos_descriptor.as_ref().map(Vec::len), Some(12));
        assert_eq!(dev.interfaces.len(), 7);
        let classes: Vec<(u8, u8, u8)> = dev
            .interfaces
            .iter()
            .map(|i| {
                (
                    i.interface_class,
                    i.interface_subclass,
                    i.interface_protocol,
                )
            })
            .collect();
        assert_eq!(
            classes,
            [
                (0x02, 0x02, 0x00),
                (0x0A, 0x00, 0x00),
                (0x03, 0x00, 0x00),
                (0x03, 0x00, 0x00),
                (0x03, 0x00, 0x00),
                (0x03, 0x00, 0x00),
                (0x03, 0x00, 0x00),
            ]
        );
        let endpoints: Vec<Vec<(u8, u8, u16, u8)>> = dev
            .interfaces
            .iter()
            .map(|i| {
                i.endpoints
                    .iter()
                    .map(|e| (e.address, e.attributes, e.max_packet_size, e.interval))
                    .collect()
            })
            .collect();
        assert_eq!(endpoints[0], [(0x81, 3, 16, 10)]);
        assert_eq!(endpoints[1], [(0x82, 2, 64, 0), (0x01, 2, 64, 0)]);
        for slot in 0u8..4 {
            assert_eq!(
                endpoints[slot as usize + 2],
                [(0x83 + slot, 3, 64, 2), (0x02 + slot, 3, 64, 2)]
            );
            assert_eq!(
                dev.interfaces[slot as usize + 2]
                    .class_specific_descriptor
                    .len(),
                9
            );
        }
        assert_eq!(endpoints[6], [(0x87, 3, 64, 32), (0x06, 3, 64, 32)]);
        assert_eq!(dev.interfaces[6].class_specific_descriptor[7], 54);
        let config_len = 9
            + dev.configuration_descriptor_prefix.len()
            + dev
                .interfaces
                .iter()
                .map(|i| 9 + i.class_specific_descriptor.len() + 7 * i.endpoints.len())
                .sum::<usize>();
        assert_eq!(config_len, 0x00EB);
        let ep0 = UsbEndpoint {
            address: 0,
            attributes: 0,
            max_packet_size: 64,
            interval: 0,
        };
        let slot_status = |interface: usize| {
            let iface = dev.interfaces[interface].clone();
            let mut handler = iface.handler.lock().unwrap();
            handler
                .handle_urb(
                    &iface,
                    ep0,
                    0,
                    SetupPacket {
                        request_type: 0x21,
                        request: 0x09,
                        value: 0x0302,
                        index: interface as u16,
                        length: 3,
                    },
                    &[0x02, 0xB4, 0x00],
                )
                .unwrap();
            handler
                .handle_urb(
                    &iface,
                    ep0,
                    64,
                    SetupPacket {
                        request_type: 0xA1,
                        request: 0x01,
                        value: 0x0302,
                        index: interface as u16,
                        length: 64,
                    },
                    &[],
                )
                .unwrap()[..4]
                .to_vec()
        };
        assert_eq!(slot_status(2), [0x02, 0xB4, 0x01, 0x02]);
        for interface in 3..=5 {
            assert_eq!(slot_status(interface), [0x02, 0xB4, 0x01, 0x01]);
        }
        // An empty slot NAKs its polls, as the real Puck's does: never 0x79/0x01 every 2 ms.
        let iface = dev.interfaces[3].clone();
        let interrupt_in = iface.endpoints[0];
        let polled = iface.handler.lock().unwrap().handle_urb(
            &iface,
            interrupt_in,
            64,
            SetupPacket::default(),
            &[],
        );
        assert_eq!(polled.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    /// A stale tag-4 build time makes Steam offer to flash the Puck's pad on every stream.
    #[test]
    fn puck_attributes_carry_the_firmware_build_time() {
        let (serial, unit) = (triton_serial(1), triton_unit_id(1));
        let r = triton_puck_feature_reply(&[0x02, 0x83, 0x00], &serial, unit, 0x02);
        assert_eq!(&r[..3], &[0x02, 0x83, 0x19]);
        assert_eq!(r[18], 0x04);
        assert_eq!(
            r[19..23],
            pf_driver_proto::triton::FW_BUILD_TIME.to_le_bytes()
        );
        let f2 = triton_puck_feature_reply(&[0x01, 0xF2, 0x00, 0x00], &serial, unit, 0x02);
        assert_eq!(f2[4..8], r[19..23]); // report-1 firmware info agrees
    }

    #[test]
    fn out_and_feature_writes_are_captured() {
        use punktfunk_core::quic::{HID_RAW_FEATURE, HID_RAW_OUTPUT};
        let reports = Arc::new(Mutex::new(InputReports::new(InputReport::default())));
        let feedback = Arc::new(Mutex::new(TritonUsbFeedback::default()));
        let mut h = TritonHandler {
            reports,
            feedback: feedback.clone(),
            pad: Arc::new(Mutex::new(SlotPad::new(0, None, None))),
            last_set: Vec::new(),
            last_get_logged: 0,
        };
        let iface_dummy = UsbInterface {
            interface_class: 3,
            interface_subclass: 0,
            interface_protocol: 0,
            endpoints: vec![],
            string_interface: 0,
            class_specific_descriptor: vec![],
            alt_settings: vec![],
            handler: boxed(IdleDummy),
        };
        let ep_out = UsbEndpoint {
            address: 0x01,
            attributes: 0x03,
            max_packet_size: 64,
            interval: 1,
        };
        let ep0 = UsbEndpoint {
            address: 0x00,
            attributes: 0x00,
            max_packet_size: 64,
            interval: 0,
        };
        // `[0x80, type, intensity u16, left u16+gain, right u16+gain]`.
        let mut rumble = [0u8; 10];
        rumble[0] = 0x80;
        rumble[4..6].copy_from_slice(&0x2000u16.to_le_bytes());
        rumble[7..9].copy_from_slice(&0x4000u16.to_le_bytes());
        h.handle_urb(&iface_dummy, ep_out, 10, SetupPacket::default(), &rumble)
            .unwrap();
        // hidraw may SET_REPORT OUTPUT on EP0 instead of interrupt-OUT.
        let setup = SetupPacket {
            request_type: 0x21,
            request: 0x09,
            value: 0x0282,
            index: 0,
            length: 3,
        };
        h.handle_urb(&iface_dummy, ep0, 3, setup, &[0x01, 0x01, 0xF7])
            .unwrap();
        // Feature SET_REPORT: id rides wValue, not the payload — must still normalize.
        let setup = SetupPacket {
            request_type: 0x21,
            request: 0x09,
            value: 0x0301,
            index: 0,
            length: 5,
        };
        h.handle_urb(&iface_dummy, ep0, 5, setup, &[0x87, 3, 9, 0, 0])
            .unwrap();
        // hid-steam's lizard-on stays here.
        h.handle_urb(&iface_dummy, ep0, 2, setup, &[0x85, 0])
            .unwrap();
        let fb = feedback.lock();
        assert_eq!(fb.rumble, Some((0x2000, 0x4000)));
        assert_eq!(fb.raw.len(), 3);
        assert_eq!(fb.raw[0].0, HID_RAW_OUTPUT);
        assert_eq!(fb.raw[0].1[0], 0x80);
        assert_eq!(fb.raw[1], (HID_RAW_OUTPUT, vec![0x82, 0x01, 0x01, 0xF7]));
        assert_eq!(fb.raw[2].0, HID_RAW_FEATURE);
        assert_eq!(&fb.raw[2].1[..3], &[0x01, 0x87, 3]); // id-first for client replay
    }

    #[derive(Debug)]
    struct IdleDummy;
    impl UsbInterfaceHandler for IdleDummy {
        fn get_class_specific_descriptor(&self) -> Vec<u8> {
            vec![]
        }
        fn handle_urb(
            &mut self,
            _i: &UsbInterface,
            _e: UsbEndpoint,
            _l: u32,
            _s: SetupPacket,
            _r: &[u8],
        ) -> std::io::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn as_any(&mut self) -> &mut dyn Any {
            self
        }
    }

    /// Root + `vhci_hcd`: enumerates `28DE:1302` on a real interface, tears down on drop.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root + vhci_hcd"]
    fn usbip_triton_enumerates_and_tears_down() {
        super::super::steam_usbip::ensure_modules();
        let mut pad = TritonUsbip::open(0).expect("open TritonUsbip (root + vhci_hcd?)");
        let mut st = TritonState::neutral();
        let raw: &[u8] = &[0x42, 1, 0x01, 0, 0, 0]; // A held; truncated length is accepted
        st.raw[..raw.len()].copy_from_slice(raw);
        st.raw_len = raw.len() as u8;
        let start = std::time::Instant::now();
        let mut found = None;
        while start.elapsed() < std::time::Duration::from_millis(1500) {
            pad.write_state(&st);
            let _ = pad.service();
            if let Some(e) = hid_entry(":28DE:1302") {
                found = Some(e);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        let entry = found.expect("virtual 28DE:1302 did not enumerate via vhci_hcd");
        let target = std::fs::read_link(entry.path()).expect("hid device link");
        assert!(
            target.to_string_lossy().contains("vhci_hcd"),
            "28DE:1302 present but not via vhci_hcd: {}",
            target.display()
        );
        drop(pad);
        assert!(
            wait_hid_gone(":28DE:1302", std::time::Duration::from_millis(400)),
            "device not torn down on drop"
        );
    }
}
