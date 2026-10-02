//! Sony DualSense as a USB/IP composite device through in-kernel `vhci_hcd`.
//! Wine needs a USB parent for ContainerId pairing; `snd-usb-audio` needs one for the
//! ALSA card GE-Proton enumerates. `/dev/uhid` provides neither.
//!
//! Four interfaces from wired `054c:0ce6`: UAC 1.0 control; S16LE 4-channel 48 kHz OUT
//! on isochronous `0x01`; S16LE 2-channel 48 kHz IN on `0x82`; HID interrupt IN/OUT on
//! `0x84`/`0x03`. [`tests::config_descriptor_matches_hardware`] pins `wTotalLength`
//! `0x00E3`. HID report length is the 273-byte [`DUALSENSE_RDESC`], not the 289
//! hardware advertises; `hid-playstation` binds the shorter one.
//!
//! Audio OUT is converted to `f32` and published once per pad via [`take_audio_rx`].
//! Queue overflow drops rather than blocking a URB reply (that stalls the kernel ISO
//! ring). The vendored USB/IP server serializes URBs, so HID polls may wait behind
//! audio; concurrency needs seqnum-keyed out-of-order completions.

use super::dualsense_proto::{
    ds_pairing_reply, parse_ds_output, DsEncoder, DsFeedback, DsState, DEVTYPE_DUALSENSE,
    DS_FEATURE_CALIBRATION, DS_FEATURE_FIRMWARE, DS_INPUT_REPORT_LEN, DS_PRODUCT, DS_VENDOR,
    DUALSENSE_RDESC,
};
use super::steam_usbip::{attach_device, boxed, UsbipAttachment};
use anyhow::Result;
use std::any::Any;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use usbip_sim::{
    Direction, IsoPacket, SetupPacket, UsbAltSetting, UsbDevice, UsbEndpoint, UsbInterface,
    UsbInterfaceHandler, Version,
};

/// ch0/1 headphone L/R (ch1 is also the internal mono speaker); ch2/ch3 haptic coils.
/// Same layout as `pad_sink` / `split_quad`.
pub const PAD_AUDIO_CHANNELS: usize = 4;

/// Hardware isochronous OUT (haptics + speaker).
const EP_AUDIO_OUT: u8 = 0x01;
/// Hardware isochronous IN (headset mic).
const EP_AUDIO_IN: u8 = 0x82;
/// HID input report `0x01`.
const EP_HID_IN: u8 = 0x84;
/// HID output report `0x02`.
const EP_HID_OUT: u8 = 0x03;

/// HID `bInterval` 4 = 1 ms at high speed, not hardware's 6 (4 ms / 250 Hz). Matching
/// hardware would add up to 4 ms of input latency the uhid pad does not have.
const HID_INTERVAL: u8 = 4;
/// Isochronous `bInterval` 4 = one packet per millisecond at high speed, as hardware.
const AUDIO_INTERVAL: u8 = 4;

/// 49 frames × 4 ch × 2 bytes. 48 kHz is 48 frames/ms; the spare is adaptive-sink slack
/// for host clock drift. Hardware declares 392.
const AUDIO_OUT_MPS: u16 = 392;
/// 49 frames × 2 ch × 2 bytes.
const AUDIO_IN_MPS: u16 = 196;

/// Decoded chunks queued before drop. Consumer wakes every 5 ms, each chunk ~1 ms;
/// drop rather than block the URB reply (that stalls the kernel ISO ring).
const AUDIO_QUEUE_DEPTH: usize = 256;

/// Audio receivers for live usbip pads, indexed by wire pad.
///
/// Pad creation is on the session input thread; the streamer lives in another crate, so
/// they join here rather than by a call graph. [`take_audio_rx`] hands the single-consumer
/// `Receiver` over once; the streamer's open-with-backoff covers looking before the pad exists.
static AUDIO_RX: Mutex<Vec<Option<Receiver<Vec<f32>>>>> = Mutex::new(Vec::new());

/// Take the unpublished audio receiver for wire pad `pad`, if any. Interleaved `f32`
/// of [`PAD_AUDIO_CHANNELS`] at 48 kHz.
pub fn take_audio_rx(pad: u8) -> Option<Receiver<Vec<f32>>> {
    let mut g = AUDIO_RX.lock().ok()?;
    g.get_mut(pad as usize).and_then(Option::take)
}

fn publish_audio_rx(pad: u8, rx: Receiver<Vec<f32>>) {
    if let Ok(mut g) = AUDIO_RX.lock() {
        if g.len() <= pad as usize {
            g.resize_with(pad as usize + 1, || None);
        }
        g[pad as usize] = Some(rx);
    }
}

fn clear_audio_rx(pad: u8) {
    if let Ok(mut g) = AUDIO_RX.lock() {
        if let Some(slot) = g.get_mut(pad as usize) {
            *slot = None;
        }
    }
}

fn ep(address: u8, attributes: u8, max_packet_size: u16, interval: u8) -> UsbEndpoint {
    UsbEndpoint {
        address,
        attributes,
        max_packet_size,
        interval,
    }
}

/// HID class descriptor for interface 3. Hardware is `bcdHID 1.11`, country 0 — the Deck
/// helper in [`super::steam_usbip`] bakes 1.10/33, so this is a local copy.
fn hid_class_descriptor(report_len: usize) -> Vec<u8> {
    let l = report_len as u16;
    #[rustfmt::skip]
    let d = vec![
        0x09, 0x21,       // bLength, bDescriptorType (HID)
        0x11, 0x01,       // bcdHID 1.11
        0x00,             // bCountryCode
        0x01,             // bNumDescriptors
        0x22,             // bDescriptorType (Report)
        (l & 0xff) as u8, (l >> 8) as u8, // wDescriptorLength
    ];
    d
}

/// Interface 0 Audio Control class descriptors, verbatim from hardware. `wTotalLength`
/// `0x0049` (73) is the length of this block.
#[rustfmt::skip]
fn audio_control_descriptor() -> Vec<u8> {
    vec![
        // HEADER: bcdADC 1.00, wTotalLength 73, 2 streaming interfaces (1, 2)
        0x0A, 0x24, 0x01, 0x00, 0x01, 0x49, 0x00, 0x02, 0x01, 0x02,
        // INPUT_TERMINAL 1: USB Streaming (0x0101), assoc 6, 4 ch, FL|FR|RL|RR (0x0033)
        0x0C, 0x24, 0x02, 0x01, 0x01, 0x01, 0x06, 0x04, 0x33, 0x00, 0x00, 0x00,
        // FEATURE_UNIT 2: source 1, 1-byte controls, master mute+volume then 4 silent channels
        0x0C, 0x24, 0x06, 0x02, 0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
        // OUTPUT_TERMINAL 3: Speaker (0x0301), assoc 4, source 2
        0x09, 0x24, 0x03, 0x03, 0x01, 0x03, 0x04, 0x02, 0x00,
        // INPUT_TERMINAL 4: Headset (0x0402), assoc 3, 2 ch, L|R (0x0003)
        0x0C, 0x24, 0x02, 0x04, 0x02, 0x04, 0x03, 0x02, 0x03, 0x00, 0x00, 0x00,
        // FEATURE_UNIT 5: source 4, master mute+volume then 1 silent channel
        0x09, 0x24, 0x06, 0x05, 0x04, 0x01, 0x03, 0x00, 0x00,
        // OUTPUT_TERMINAL 6: USB Streaming (0x0101), assoc 1, source 5
        0x09, 0x24, 0x03, 0x06, 0x01, 0x01, 0x01, 0x05, 0x00,
    ]
}

/// Audio Streaming class descriptors: `AS_GENERAL` (terminal link) then Type-I PCM S16
/// at 48 kHz over `channels`.
#[rustfmt::skip]
fn audio_streaming_descriptor(terminal_link: u8, channels: u8) -> Vec<u8> {
    vec![
        // AS_GENERAL: bTerminalLink, bDelay 1 frame, wFormatTag PCM (0x0001)
        0x07, 0x24, 0x01, terminal_link, 0x01, 0x01, 0x00,
        // FORMAT_TYPE_I: channels, 2-byte subframes, 16-bit, 1 discrete rate, 48000 (24-bit LE)
        0x0B, 0x24, 0x02, 0x01, channels, 0x02, 0x10, 0x01, 0x80, 0xBB, 0x00,
    ]
}

/// UAC 1.0 isochronous extras past the 7-byte endpoint (`bRefresh`, `bSynchAddress`)
/// plus the following `AS_ENDPOINT`.
fn audio_endpoint_extras() -> (Vec<u8>, Vec<u8>) {
    let in_descriptor = vec![0x00, 0x00]; // bRefresh, bSynchAddress
    #[rustfmt::skip]
    let trailer = vec![
        // AS_ENDPOINT (CS_ENDPOINT / EP_GENERAL): no controls, no lock delay
        0x07, 0x25, 0x01, 0x00, 0x00, 0x00, 0x00,
    ];
    (in_descriptor, trailer)
}

/// `SET_INTERFACE` (`snd-usb-audio` arms/disarms a streaming altsetting), `GET_INTERFACE`,
/// `GET_STATUS`. `None` if the request is none of those.
fn standard_interface_reply(setup: SetupPacket, current_alt: &mut u8) -> Option<Vec<u8>> {
    match (setup.request_type, setup.request) {
        (0x01, 0x0B) => {
            *current_alt = setup.value as u8;
            Some(Vec::new())
        }
        (0x81, 0x0A) => Some(vec![*current_alt]),
        // GET_STATUS — interfaces report 0.
        (0x81, 0x00) => Some(vec![0x00, 0x00]),
        _ => None,
    }
}

/// Interface 0 Audio Control. Answers feature-unit mute/volume so `snd-usb-audio` builds
/// a mixer; refusing only drops the control, but `amixer`/`wpctl` then look unlike hardware.
#[derive(Debug, Default)]
struct AudioControlHandler {
    current_alt: u8,
}

impl UsbInterfaceHandler for AudioControlHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        audio_control_descriptor()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        if let Some(r) = standard_interface_reply(setup, &mut self.current_alt) {
            return Ok(r);
        }
        // wValue high byte is the control selector: 0x01 MUTE (1 byte), 0x02 VOLUME
        // (2 bytes, signed 1/256 dB).
        let selector = (setup.value >> 8) as u8;
        Ok(match (setup.request_type, setup.request, selector) {
            (0xA1, 0x81..=0x84, 0x01) => vec![0x00], // never muted
            (0xA1, 0x81, 0x02) => 0i16.to_le_bytes().to_vec(), // 0 dB
            (0xA1, 0x82, 0x02) => (-60i16 * 256).to_le_bytes().to_vec(), // −60 dB
            (0xA1, 0x83, 0x02) => 0i16.to_le_bytes().to_vec(), // 0 dB
            (0xA1, 0x84, 0x02) => 0x0030i16.to_le_bytes().to_vec(), // 3/16 dB, hardware step
            // Unknown class request: ACK, as hardware does.
            _ => Vec::new(),
        })
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 1 Audio Streaming OUT. Each isochronous packet is one service interval of
/// the game's haptic/speaker quad.
#[derive(Debug)]
struct SpeakerStreamHandler {
    tx: SyncSender<Vec<f32>>,
    current_alt: u8,
    /// Queue-full drops; logged on powers of two so a stall cannot flood the log.
    dropped: u64,
}

impl UsbInterfaceHandler for SpeakerStreamHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        // Alt 0 is zero-bandwidth and has no class descriptor; alt 1 is in `build_device`.
        Vec::new()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        Ok(standard_interface_reply(setup, &mut self.current_alt).unwrap_or_default())
    }

    fn handle_iso_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        packets: &[IsoPacket<'_>],
    ) -> std::io::Result<Vec<Vec<u8>>> {
        let total: usize = packets.iter().map(|p| p.data.len()).sum();
        if total >= 2 {
            let mut pcm = Vec::with_capacity(total / 2);
            for p in packets {
                // Truncated frame: `chunks_exact` drops a trailing odd byte.
                for s in p.data.chunks_exact(2) {
                    pcm.push(i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0);
                }
            }
            if !pcm.is_empty() && self.tx.try_send(pcm).is_err() {
                self.dropped += 1;
                if self.dropped.is_power_of_two() {
                    tracing::debug!(
                        dropped = self.dropped,
                        "pad usb audio queue full — dropping (streamer behind?)"
                    );
                }
            }
        }
        // OUT: empty replies; the caller fills actual_length.
        Ok(vec![Vec::new(); packets.len()])
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 2 Audio Streaming IN (headset mic). Topology only; streams silence until
/// pad-mic capture exists.
#[derive(Debug, Default)]
struct MicStreamHandler {
    current_alt: u8,
}

impl UsbInterfaceHandler for MicStreamHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        Vec::new()
    }

    fn handle_urb(
        &mut self,
        _interface: &UsbInterface,
        _ep: UsbEndpoint,
        _len: u32,
        setup: SetupPacket,
        _req: &[u8],
    ) -> std::io::Result<Vec<u8>> {
        Ok(standard_interface_reply(setup, &mut self.current_alt).unwrap_or_default())
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

/// Interface 3 HID. Same codec as [`super::dualsense::DualSensePad`]: input `0x01`,
/// output `0x02`, feature `0x05`/`0x09`/`0x20` (`hid-playstation` init; without them no
/// input devices appear).
struct HidHandler {
    report: Arc<Mutex<[u8; DS_INPUT_REPORT_LEN]>>,
    feedback: Arc<Mutex<DsFeedback>>,
    pad: u8,
    current_alt: u8,
}

// Hand-written: `DsFeedback` is not `Debug` (it holds `HidOutput`s); the trait only
// wants a name for tracing.
impl std::fmt::Debug for HidHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidHandler")
            .field("pad", &self.pad)
            .field("current_alt", &self.current_alt)
            .finish()
    }
}

impl UsbInterfaceHandler for HidHandler {
    fn get_class_specific_descriptor(&self) -> Vec<u8> {
        hid_class_descriptor(DUALSENSE_RDESC.len())
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
            if let Some(r) = standard_interface_reply(setup, &mut self.current_alt) {
                return Ok(r);
            }
            return Ok(match (setup.request_type, setup.request) {
                (0x81, 0x06) if (setup.value >> 8) == 0x22 => DUALSENSE_RDESC.to_vec(),
                // HID GET_REPORT(Feature): wValue low byte is the report id.
                (0xA1, 0x01) => {
                    let pairing = ds_pairing_reply(DEVTYPE_DUALSENSE, self.pad);
                    match setup.value as u8 {
                        0x05 => DS_FEATURE_CALIBRATION.to_vec(),
                        0x09 => pairing.to_vec(),
                        0x20 => DS_FEATURE_FIRMWARE.to_vec(),
                        _ => Vec::new(),
                    }
                }
                // HID SET_REPORT: known writers use interrupt OUT, but parse here too so a
                // control-pipe writer is not dropped.
                (0x21, 0x09) => {
                    self.absorb_output(req);
                    Vec::new()
                }
                (0x21, 0x0A) | (0x21, 0x0B) => Vec::new(),
                _ => Vec::new(),
            });
        }
        match ep.direction() {
            Direction::In => Ok(self
                .report
                .lock()
                .map(|g| g.to_vec())
                .unwrap_or_else(|_| vec![0u8; DS_INPUT_REPORT_LEN])),
            Direction::Out => {
                self.absorb_output(req);
                Ok(Vec::new())
            }
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

impl HidHandler {
    /// Merge one HID output into pending feedback. Writers may split rumble and LEDs
    /// across reports; `service` drains on its own cadence.
    fn absorb_output(&mut self, data: &[u8]) {
        let mut fb = DsFeedback::default();
        parse_ds_output(self.pad, data, &mut fb);
        if let Ok(mut g) = self.feedback.lock() {
            if fb.rumble.is_some() {
                g.rumble = fb.rumble;
            }
            g.hidout.extend(fb.hidout);
        }
    }
}

/// Assemble the 4-interface composite. `index` is the wire pad; it does not appear in
/// descriptors (no `iSerialNumber`; `hid-playstation` takes identity from feature `0x09`).
fn build_device(
    index: u8,
    report: &Arc<Mutex<[u8; DS_INPUT_REPORT_LEN]>>,
    feedback: &Arc<Mutex<DsFeedback>>,
    audio_tx: &SyncSender<Vec<f32>>,
) -> UsbDevice {
    let (ep_extra, ep_trailer) = audio_endpoint_extras();

    let mut dev = UsbDevice::new(0); // one device per server, so the default bus_id "0-0-0" stands.
    dev.vendor_id = DS_VENDOR as u16;
    dev.product_id = DS_PRODUCT as u16;
    dev.usb_version = Version::from(0x0200u16);
    dev.device_bcd = Version::from(0x0100u16);
    dev.configuration_attributes = 0xC0; // self-powered, as hardware reports
    dev.configuration_max_power = 250; // 500 mA in 2 mA units
    dev.set_manufacturer_name("Sony Interactive Entertainment");
    dev.set_product_name("DualSense Wireless Controller");
    // Hardware has no iSerialNumber. `UsbDevice::default` fills "Serial", which ALSA/PipeWire
    // bake into card/node names. Clear it so matchers see the same names as a physical pad.
    // UCM keys on `${CardComponents}` (`USB054c:0ce6`), not the name — do not treat this as
    // a UCM-selection fix.
    dev.unset_serial_number();

    dev
        // Interface 0: Audio Control, no endpoints.
        .with_interface(
            0x01,
            0x01,
            0x00,
            None,
            vec![],
            boxed(AudioControlHandler::default()),
        )
        // Interface 1: Audio Streaming OUT, alt 0 idle / alt 1 isochronous.
        .with_interface(
            0x01,
            0x02,
            0x00,
            None,
            vec![],
            boxed(SpeakerStreamHandler {
                tx: audio_tx.clone(),
                current_alt: 0,
                dropped: 0,
            }),
        )
        .with_alt_settings(vec![UsbAltSetting {
            alternate_setting: 1,
            interface_class: 0x01,
            interface_subclass: 0x02,
            interface_protocol: 0x00,
            class_specific_descriptor: audio_streaming_descriptor(1, 4),
            // 0x09 = isochronous, adaptive, data — the sink must ride the host's clock.
            endpoints: vec![ep(EP_AUDIO_OUT, 0x09, AUDIO_OUT_MPS, AUDIO_INTERVAL)],
            endpoint_extra: vec![ep_extra.clone()],
            endpoint_trailers: vec![ep_trailer.clone()],
        }])
        // Interface 2: Audio Streaming IN (headset mic).
        .with_interface(
            0x01,
            0x02,
            0x00,
            None,
            vec![],
            boxed(MicStreamHandler::default()),
        )
        .with_alt_settings(vec![UsbAltSetting {
            alternate_setting: 1,
            interface_class: 0x01,
            interface_subclass: 0x02,
            interface_protocol: 0x00,
            class_specific_descriptor: audio_streaming_descriptor(6, 2),
            // 0x05 = isochronous, asynchronous, data — a source runs on its own clock.
            endpoints: vec![ep(EP_AUDIO_IN, 0x05, AUDIO_IN_MPS, AUDIO_INTERVAL)],
            endpoint_extra: vec![ep_extra],
            endpoint_trailers: vec![ep_trailer],
        }])
        // Interface 3: HID.
        .with_interface(
            0x03,
            0x00,
            0x00,
            None,
            vec![
                ep(EP_HID_IN, 0x03, 64, HID_INTERVAL),
                ep(EP_HID_OUT, 0x03, 64, HID_INTERVAL),
            ],
            boxed(HidHandler {
                report: report.clone(),
                feedback: feedback.clone(),
                pad: index,
                current_alt: 0,
            }),
        )
}

/// Virtual DualSense over USB/IP, with its own UAC sound card.
///
/// Drop detaches the `vhci_hcd` port (pad and ALSA card go together) and withdraws the
/// audio receiver.
pub struct DualSenseUsbip {
    report: Arc<Mutex<[u8; DS_INPUT_REPORT_LEN]>>,
    feedback: Arc<Mutex<DsFeedback>>,
    pad: u8,
    enc: DsEncoder,
    _attach: UsbipAttachment,
}

impl DualSenseUsbip {
    /// Bind wire pad `index` and attach via `vhci_hcd`. Fails (caller degrades to uhid)
    /// when `vhci_hcd` is missing or sysfs `attach` is not writable — see
    /// [`super::steam_usbip::attach_device`].
    pub fn open(index: u8) -> Result<DualSenseUsbip> {
        let report = Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN]));
        let feedback = Arc::new(Mutex::new(DsFeedback::default()));
        let (tx, rx) = sync_channel::<Vec<f32>>(AUDIO_QUEUE_DEPTH);
        // Pads already up, so the bind wait below judges this pad's own device.
        let before: Vec<_> = vhci_dualsenses().map(|t| t.sysfs_path).collect();

        let attach = attach_device(
            || build_device(index, &report, &feedback, &tx),
            &format!("virtual DualSense {index}"),
        )?;

        // `vhci_hcd` accepts the socket immediately and enumerates later. A bad URB or
        // descriptor then appears and vanishes. Return `Ok` only after a HID driver binds;
        // this transport replaces uhid, so the caller's uhid fallback covers the rest.
        if let Err(e) = wait_until_bound(index, &before) {
            drop(attach); // detach the port before the caller retries or degrades
            return Err(e);
        }

        // Publish only after bind so a failed bringup leaves no stale receiver for the streamer.
        publish_audio_rx(index, rx);
        tracing::info!(
            index,
            "virtual DualSense created (usbip — real USB topology, so wine derives a ContainerId \
             and GE finds a real ALSA card)"
        );
        Ok(DualSenseUsbip {
            report,
            feedback,
            pad: index,
            enc: DsEncoder::default(),
            _attach: attach,
        })
    }

    /// Serialize `st` as report `0x01` for the next interrupt-IN poll.
    pub fn write_state(&mut self, st: &DsState) {
        let r = self.enc.encode(st);
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    /// Drain HID feedback written since the last call.
    pub fn service(&mut self) -> DsFeedback {
        let fb = self
            .feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default();
        self.enc.observe(&fb.hidout);
        fb
    }
}

impl Drop for DualSenseUsbip {
    fn drop(&mut self) {
        clear_audio_rx(self.pad);
    }
}

/// Enumerate + HID-bind grace (3 s). Bind is ~330 ms idle; a failed URB tears the device
/// down ~400 ms after attach. `PUNKTFUNK_DUALSENSE_USBIP_GRACE_MS` overrides; `0` skips.
const BIND_GRACE: std::time::Duration = std::time::Duration::from_millis(3000);

/// Wait until a HID driver has bound the pad's HID interface, or the grace expires.
///
/// The `usb_device` node alone is not enough: it can appear and vanish when a later URB
/// fails. A bound HID driver with an `input` child means the pad actually came up. Only a
/// node outside `before` (the pads already attached) is this pad's.
fn wait_until_bound(index: u8, before: &[std::path::PathBuf]) -> Result<()> {
    let grace = std::env::var("PUNKTFUNK_DUALSENSE_USBIP_GRACE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(BIND_GRACE);
    if grace.is_zero() {
        return Ok(());
    }

    let deadline = Instant::now() + grace;
    let mut saw_device = false;
    loop {
        if let Some(t) = vhci_dualsenses().find(|t| !before.contains(&t.sysfs_path)) {
            saw_device = true;
            if hid_input_bound(&t.sysfs_path) {
                tracing::debug!(
                    index,
                    sysfs = %t.sysfs_path.display(),
                    "usbip DualSense bound a HID driver"
                );
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    if saw_device {
        anyhow::bail!(
            "the virtual DualSense enumerated but no HID driver bound within {:?} — it is present \
             in sysfs without an input device. Check `dmesg` for a `playstation`/`hid-generic` \
             probe failure",
            grace
        )
    }
    anyhow::bail!(
        "the virtual DualSense never enumerated within {grace:?} — `vhci_hcd` accepted the attach \
         but no 054c:0ce6 device appeared (or it appeared and was torn down again). Check `dmesg`; \
         a transport fault here reads as `recv xbuf` / `sendmsg failed` from vhci_hcd"
    )
}

/// True when the pad's HID interface has a bound driver that registered an input device.
/// `hid-playstation` or `hid-generic` both count.
fn hid_input_bound(sysfs: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(sysfs) else {
        return false;
    };
    for e in entries.flatten() {
        // HID function is interface 3; its sysfs node is `<busid>:1.3`.
        if !e.file_name().to_string_lossy().ends_with(":1.3") {
            continue;
        }
        let Ok(children) = std::fs::read_dir(e.path()) else {
            continue;
        };
        for c in children.flatten() {
            // Bound HID device is `0003:054C:0CE6.000N`. `input/` exists only after a driver
            // claimed it; a failed probe leaves the directory absent.
            if c.file_name().to_string_lossy().starts_with("0003:")
                && c.path().join("input").is_dir()
            {
                return true;
            }
        }
    }
    false
}

/// Sysfs `usb_device` of an attached virtual DualSense, plus the udev fields wine packs
/// into a Windows ContainerId.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbTopology {
    /// `usb_device` node both ContainerId derivations must land on.
    pub sysfs_path: std::path::PathBuf,
    /// `BUSNUM` / `DEVNUM` wine packs into the GUID.
    pub busnum: String,
    pub devnum: String,
}

/// Sysfs `usb_device` of the attached virtual DualSense, if any.
///
/// Devtest helper: with a real USB device, PipeWire and wine walk udev themselves.
/// Matches vendor/product under `vhci_hcd`; several pads return the first (hardware
/// has no `iSerialNumber` either).
pub fn find_usb_topology() -> Option<UsbTopology> {
    vhci_dualsenses().next()
}

/// Every virtual DualSense `usb_device` under `vhci_hcd`.
fn vhci_dualsenses() -> impl Iterator<Item = UsbTopology> {
    let attr = |dir: &std::path::Path, name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let entries = std::fs::read_dir("/sys/bus/usb/devices")
        .into_iter()
        .flatten();
    entries.flatten().filter_map(move |entry| {
        let dir = entry.path();
        // Interfaces (`11-2:1.0`) have no idVendor; only usb_device nodes do.
        if attr(&dir, "idVendor").as_deref() != Some("054c")
            || attr(&dir, "idProduct").as_deref() != Some("0ce6")
        {
            return None;
        }
        let real = std::fs::canonicalize(&dir).unwrap_or(dir);
        if !real.to_string_lossy().contains("vhci_hcd") {
            return None; // a physically plugged pad, not ours
        }
        Some(UsbTopology {
            busnum: attr(&real, "busnum").unwrap_or_default(),
            devnum: attr(&real, "devnum").unwrap_or_default(),
            sysfs_path: real,
        })
    })
}

/// Prefer usbip DualSense over uhid when the `PUNKTFUNK_DUALSENSE_USBIP` row is on.
///
/// Opt-in: this mints a real ALSA card that supersedes the pad-audio sinks.
pub fn usbip_preferred() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_DUALSENSE_USBIP")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dualsense_proto::serialize_state;

    /// Byte length of the configuration descriptor `UsbDevice::handle_urb` would emit.
    /// [`config_descriptor_matches_hardware`] pins it to hardware `wTotalLength`.
    fn assembled_config_len() -> usize {
        let (ep_extra, ep_trailer) = audio_endpoint_extras();
        let iso_ep_len = 7 + ep_extra.len() + ep_trailer.len();
        9                                          // configuration descriptor
            + 9 + audio_control_descriptor().len() // interface 0 + its AC block
            + 9                                    // interface 1 alt 0
            + 9 + audio_streaming_descriptor(1, 4).len() + iso_ep_len // interface 1 alt 1
            + 9                                    // interface 2 alt 0
            + 9 + audio_streaming_descriptor(6, 2).len() + iso_ep_len // interface 2 alt 1
            + 9 + hid_class_descriptor(DUALSENSE_RDESC.len()).len() + 7 + 7 // interface 3
    }

    /// Assembled config length equals hardware `wTotalLength` `0x00e3` (`lsusb -v` on
    /// wired `054c:0ce6`).
    #[test]
    fn config_descriptor_matches_hardware() {
        assert_eq!(assembled_config_len(), 0x00E3);
    }

    /// Audio Control `wTotalLength` (HEADER bytes 5..7) must equal the block length, or
    /// `snd-usb-audio` walks off the topology and creates no PCM.
    #[test]
    fn audio_control_total_length_is_self_consistent() {
        let d = audio_control_descriptor();
        let stated = u16::from_le_bytes([d[5], d[6]]) as usize;
        assert_eq!(stated, d.len(), "AC header wTotalLength vs actual block");
        assert_eq!(stated, 0x49, "hardware publishes 73");
    }

    /// Kernel walks the AC block by `bLength`; one wrong byte desynchronises the rest.
    #[test]
    fn audio_control_sub_descriptors_walk_cleanly() {
        let d = audio_control_descriptor();
        let mut off = 0;
        let mut seen = 0;
        while off < d.len() {
            let len = d[off] as usize;
            assert!(len >= 3, "descriptor at {off} has absurd bLength {len}");
            assert_eq!(d[off + 1], 0x24, "CS_INTERFACE type at {off}");
            off += len;
            seen += 1;
        }
        assert_eq!(off, d.len(), "descriptor walk overran the block");
        assert_eq!(seen, 7, "header + 2 input + 2 feature + 2 output terminals");
    }

    /// OUT format is 48 kHz S16 4-channel. GE-Proton rejects any other haptic PCM.
    #[test]
    fn streaming_format_is_48k_s16_quad() {
        let d = audio_streaming_descriptor(1, 4);
        let fmt = &d[7..]; // skip AS_GENERAL
        assert_eq!(fmt[0] as usize, fmt.len(), "FORMAT_TYPE bLength");
        assert_eq!(fmt[3], 0x01, "FORMAT_TYPE_I");
        assert_eq!(fmt[4], 4, "bNrChannels");
        assert_eq!(fmt[5], 2, "bSubframeSize");
        assert_eq!(fmt[6], 16, "bBitResolution");
        let rate = u32::from_le_bytes([fmt[8], fmt[9], fmt[10], 0]);
        assert_eq!(rate, 48_000);
    }

    /// OUT max packet holds 1 ms of the quad plus one spare frame, or an adaptive sink
    /// underruns when the host clock drifts.
    #[test]
    fn audio_out_packet_holds_a_millisecond_of_quad() {
        let bytes_per_frame = PAD_AUDIO_CHANNELS * 2;
        assert_eq!(AUDIO_OUT_MPS as usize, 49 * bytes_per_frame);
        assert!(AUDIO_OUT_MPS as usize >= 48 * bytes_per_frame);
    }

    #[test]
    fn iso_out_decodes_s16_quad_to_f32() {
        let (tx, rx) = sync_channel::<Vec<f32>>(4);
        let mut h = SpeakerStreamHandler {
            tx,
            current_alt: 1,
            dropped: 0,
        };
        // Two frames: full-scale positive on ch0, full-scale negative on ch3.
        let mut raw = Vec::new();
        for s in [i16::MAX, 0, 0, i16::MIN, 0, 0, 0, 0] {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        let packets = [IsoPacket {
            data: &raw,
            requested_len: raw.len(),
        }];
        let intf = probe_interface();
        let replies = h
            .handle_iso_urb(&intf, ep(EP_AUDIO_OUT, 0x09, AUDIO_OUT_MPS, 4), &packets)
            .expect("iso urb");
        assert_eq!(replies.len(), 1, "one reply per packet");
        assert!(replies[0].is_empty(), "OUT packets carry no reply payload");

        let pcm = rx.try_recv().expect("decoded chunk");
        assert_eq!(pcm.len(), 8, "2 frames x 4 channels");
        assert!(
            (pcm[0] - 0.999_97).abs() < 1e-4,
            "ch0 full scale: {}",
            pcm[0]
        );
        assert_eq!(pcm[3], -1.0, "ch3 full scale negative");
        assert_eq!(pcm[4], 0.0);
    }

    #[test]
    fn iso_out_drops_rather_than_blocking_when_the_streamer_stalls() {
        let (tx, _rx) = sync_channel::<Vec<f32>>(1);
        let mut h = SpeakerStreamHandler {
            tx,
            current_alt: 1,
            dropped: 0,
        };
        let raw = vec![0u8; 8 * PAD_AUDIO_CHANNELS * 2];
        let packets = [IsoPacket {
            data: &raw,
            requested_len: raw.len(),
        }];
        let intf = probe_interface();
        for _ in 0..8 {
            let replies = h
                .handle_iso_urb(&intf, ep(EP_AUDIO_OUT, 0x09, AUDIO_OUT_MPS, 4), &packets)
                .expect("iso urb");
            assert_eq!(replies.len(), 1);
        }
        assert!(h.dropped > 0, "a full queue must register drops");
    }

    /// `snd-usb-audio` arms alt 1 with `SET_INTERFACE` and returns to alt 0 to stop.
    /// Losing the request leaves the endpoint idle.
    #[test]
    fn set_interface_is_tracked_and_reported() {
        let mut alt = 0u8;
        let set = SetupPacket {
            request_type: 0x01,
            request: 0x0B,
            value: 1,
            index: 1,
            length: 0,
        };
        assert_eq!(standard_interface_reply(set, &mut alt), Some(Vec::new()));
        assert_eq!(alt, 1);
        let get = SetupPacket {
            request_type: 0x81,
            request: 0x0A,
            value: 0,
            index: 1,
            length: 1,
        };
        assert_eq!(standard_interface_reply(get, &mut alt), Some(vec![1]));
    }

    /// `hid-playstation` publishes no input device until feature `0x05`/`0x09`/`0x20`
    /// answer; `0x09` MAC must differ per pad or SDL/Steam merge them.
    #[test]
    fn hid_feature_reports_answer_and_the_mac_is_per_pad() {
        let feature = |h: &mut HidHandler, id: u8| {
            let setup = SetupPacket {
                request_type: 0xA1,
                request: 0x01,
                value: 0x0300 | id as u16,
                index: 3,
                length: 64,
            };
            h.handle_urb(
                &probe_interface(),
                UsbEndpoint {
                    address: 0x80,
                    attributes: 0x00,
                    max_packet_size: 64,
                    interval: 0,
                },
                64,
                setup,
                &[],
            )
            .expect("feature")
        };
        let mk = |pad| HidHandler {
            report: Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN])),
            feedback: Arc::new(Mutex::new(DsFeedback::default())),
            pad,
            current_alt: 0,
        };
        let (mut a, mut b) = (mk(0), mk(1));
        assert_eq!(feature(&mut a, 0x05), DS_FEATURE_CALIBRATION.to_vec());
        assert_eq!(feature(&mut a, 0x20), DS_FEATURE_FIRMWARE.to_vec());
        let (m0, m1) = (feature(&mut a, 0x09), feature(&mut b, 0x09));
        assert_eq!(m0[0], 0x09, "reply keeps its report id");
        assert_ne!(m0[1..7], m1[1..7], "per-pad MAC must differ");
    }

    #[test]
    fn interrupt_out_report_becomes_feedback() {
        let feedback = Arc::new(Mutex::new(DsFeedback::default()));
        let mut h = HidHandler {
            report: Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN])),
            feedback: feedback.clone(),
            pad: 0,
            current_alt: 0,
        };
        // Report 0x02 with the compatible-vibration flag and both motors set.
        let mut out = vec![0u8; 48];
        out[0] = 0x02;
        out[1] = 0x01; // valid_flag0: compatible vibration
        out[3] = 0x80; // right (high-frequency) motor
        out[4] = 0x40; // left (low-frequency) motor
        h.handle_urb(
            &probe_interface(),
            ep(EP_HID_OUT, 0x03, 64, HID_INTERVAL),
            0,
            SetupPacket {
                request_type: 0,
                request: 0,
                value: 0,
                index: 0,
                length: 0,
            },
            &out,
        )
        .expect("interrupt out");
        let got = feedback.lock().unwrap().rumble;
        assert_eq!(got, Some((0x4000, 0x8000)));
    }

    #[test]
    fn interrupt_in_serves_the_current_state_report() {
        let report = Arc::new(Mutex::new([0u8; DS_INPUT_REPORT_LEN]));
        let mut h = HidHandler {
            report: report.clone(),
            feedback: Arc::new(Mutex::new(DsFeedback::default())),
            pad: 0,
            current_alt: 0,
        };
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &DsState::neutral(), 7, 0);
        *report.lock().unwrap() = r;
        let got = h
            .handle_urb(
                &probe_interface(),
                ep(EP_HID_IN, 0x03, 64, HID_INTERVAL),
                64,
                SetupPacket {
                    request_type: 0,
                    request: 0,
                    value: 0,
                    index: 0,
                    length: 0,
                },
                &[],
            )
            .expect("interrupt in");
        assert_eq!(got.len(), DS_INPUT_REPORT_LEN);
        assert_eq!(got[0], 0x01, "report id");
        assert_eq!(got[7], 7, "sequence number");
    }

    #[test]
    fn audio_receiver_handoff_is_take_once_per_pad() {
        let (tx, rx) = sync_channel::<Vec<f32>>(1);
        publish_audio_rx(9, rx);
        tx.send(vec![0.25; 4]).expect("send");
        let taken = take_audio_rx(9).expect("first take wins");
        assert!(take_audio_rx(9).is_none(), "second take must find nothing");
        assert_eq!(taken.try_recv().expect("chunk"), vec![0.25; 4]);
        assert!(take_audio_rx(8).is_none(), "other pads unaffected");
        clear_audio_rx(9);
    }

    /// `UsbInterfaceHandler::handle_iso_urb` takes an interface it never reads; build a throwaway.
    fn probe_interface() -> UsbInterface {
        UsbInterface {
            interface_class: 0,
            interface_subclass: 0,
            interface_protocol: 0,
            endpoints: vec![],
            string_interface: 0,
            class_specific_descriptor: vec![],
            alt_settings: vec![],
            handler: boxed(MicStreamHandler::default()),
        }
    }
}
