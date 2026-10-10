//! Virtual DualSense / DualSense Edge via `/dev/uhid`.
//!
//! `hid-playstation` binds VID `054C` / PID `0CE6` (Edge: `0DF2`) and exposes the full HID
//! surface: gamepad, motion, touchpad, lightbar, player LEDs, adaptive triggers. This module
//! writes input report `0x01` and reads output report `0x02` (rumble / LED / trigger feedback)
//! as [`punktfunk_core::quic::HidOutput`].
//!
//! Descriptor, feature blobs, [`DsState`], `0x01` serializer and `0x02` parser live in
//! [`super::dualsense_proto`] (shared with the Windows UMDF backend). The uinput X-Box-360
//! pad is [`super::gamepad`].

use super::dualsense_proto::{
    ds_pairing_reply, edge_paddle_bits, parse_ds_output, DsEncoder, DsFeedback, DsState,
    DEVTYPE_DUALSENSE, DEVTYPE_DUALSENSE_EDGE, DS_EDGE_PRODUCT, DS_FEATURE_CALIBRATION,
    DS_FEATURE_FIRMWARE, DS_PRODUCT, DS_TOUCH_H, DS_TOUCH_W, DS_VENDOR, DUALSENSE_EDGE_RDESC,
    DUALSENSE_RDESC,
};
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;

/// CREATE2 identity: DualSense vs Edge. Same codec; Edge is PID, descriptor, and `buttons[2]`.
pub struct DsUhidIdentity {
    /// `pf_driver_proto::gamepad` device type; keys the pairing MAC.
    device_type: u8,
    product: u32,
    rdesc: &'static [u8],
    name: &'static str,
    phys: &'static str,
    slug: &'static str,
}

impl DsUhidIdentity {
    pub const fn dualsense() -> DsUhidIdentity {
        DsUhidIdentity {
            device_type: DEVTYPE_DUALSENSE,
            product: DS_PRODUCT,
            rdesc: DUALSENSE_RDESC,
            name: "DualSense",
            phys: "dualsense",
            slug: "ds",
        }
    }

    pub const fn dualsense_edge() -> DsUhidIdentity {
        DsUhidIdentity {
            device_type: DEVTYPE_DUALSENSE_EDGE,
            product: DS_EDGE_PRODUCT,
            rdesc: DUALSENSE_EDGE_RDESC,
            name: "DualSense Edge",
            phys: "dualsense-edge",
            slug: "dsedge",
        }
    }
}

/// Virtual DualSense on `/dev/uhid`. Drop unbinds `hid-playstation`.
pub struct DualSensePad {
    dev: UhidDevice,
    device_type: u8,
    enc: DsEncoder,
}

impl DualSensePad {
    /// `index` is only for unique name/uniq; identity is `id`. The uniq is cosmetic:
    /// `hid-playstation` replaces it with the pairing-report MAC ([`ds_pairing_reply`]).
    pub fn open(index: u8, id: &DsUhidIdentity) -> Result<DualSensePad> {
        let dev = UhidDevice::open(&Create2 {
            bus: crate::uhid_abi::BUS_USB,
            name: &format!("Punktfunk {} {index}", id.name),
            phys: &format!("punktfunk/{}/{index}", id.phys),
            uniq: &format!("punktfunk-{}-{index}", id.slug),
            rdesc: id.rdesc,
            vendor: DS_VENDOR,
            product: id.product,
            version: 0x0100,
        })?;
        Ok(DualSensePad {
            dev,
            device_type: id.device_type,
            enc: DsEncoder::default(),
        })
    }

    pub fn write_state(&mut self, st: &DsState) -> Result<()> {
        let r = self.enc.encode(st);
        self.dev.write_input(&r)
    }

    /// Drain UHID events. GET_REPORT (0x05/0x09/0x20) must be answered or `hid-playstation`
    /// never binds; call often right after [`open`](Self::open). DualSense feedback is OUTPUT,
    /// so a SET_REPORT needs only the ack `poll` sends.
    pub fn service(&mut self, pad: u8) -> DsFeedback {
        let mut fb = DsFeedback::default();
        let device_type = self.device_type;
        self.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => parse_ds_output(pad, data, &mut fb),
            UhidEvent::GetReport { id, rnum } => {
                // Per-pad MAC becomes HID uniq; SDL/Steam dedup on it (`ds_pairing_reply`).
                let pairing = ds_pairing_reply(device_type, pad);
                let data: Option<&[u8]> = match rnum {
                    0x05 => Some(DS_FEATURE_CALIBRATION),
                    0x09 => Some(&pairing),
                    0x20 => Some(DS_FEATURE_FIRMWARE),
                    _ => None,
                };
                let _ = dev.reply_get_report(id, data);
            }
            UhidEvent::SetReport(_) => {}
        });
        self.enc.observe(&fb.hidout);
        fb
    }
}

/// Kernel presentation: usbip (`usb_device` parent) or UHID fallback.
///
/// Wine ContainerId and GE-Proton raw-ALSA walk sysfs for a `usb_device` parent UHID does not have.
/// UHID has no speaker pairing for libScePad-style titles. See [`crate::dualsense_usbip`].
pub enum DsTransport {
    Usbip(crate::dualsense_usbip::DualSenseUsbip),
    Uhid(DualSensePad),
}

/// Try usbip (`vhci_hcd` + `punktfunk` write on sysfs `attach`); else UHID.
/// Opt-in: [`crate::dualsense_usbip::usbip_preferred`].
fn open_transport(idx: u8) -> Result<DsTransport> {
    if crate::dualsense_usbip::usbip_preferred() {
        match crate::dualsense_usbip::DualSenseUsbip::open(idx) {
            Ok(u) => return Ok(DsTransport::Usbip(u)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "usbip DualSense unavailable — falling back to UHID")
            }
        }
    }
    let p = DualSensePad::open(idx, &DsUhidIdentity::dualsense())?;
    tracing::info!(
        index = idx,
        "virtual DualSense created (UHID hid-playstation)"
    );
    Ok(DsTransport::Uhid(p))
}

/// DualSense [`PadProto`]: transport + [`DsState`] + handshake. Slot table / heartbeat live in [`UhidManager`].
pub struct DsLinuxProto {
    /// Steam back-grip fold. DualSense has no HID slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for DsLinuxProto {
    fn default() -> DsLinuxProto {
        DsLinuxProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for DsLinuxProto {
    type Pad = DsTransport;
    type State = DsState;
    const LABEL: &'static str = "DualSense";
    const DEVICE: &'static str = "DualSense";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DsTransport> {
        open_transport(idx)
    }

    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        DsState::merge_frame(prev, f, buttons)
    }

    /// Steam dual pads split the one touchpad left/right; clicks ride `touch_click`.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DsTransport, st: &DsState) {
        match pad {
            DsTransport::Usbip(u) => u.write_state(st),
            DsTransport::Uhid(p) => {
                let _ = p.write_state(st);
            }
        }
    }

    /// Handshake + feedback: rumble on 0xCA, lightbar/LEDs/triggers on 0xCD.
    fn service(&self, pad: &mut DsTransport, idx: u8) -> PadFeedback {
        let fb = match pad {
            // usbip answers EP0 on the server thread; `idx` is baked into the handler.
            DsTransport::Usbip(u) => u.service(),
            DsTransport::Uhid(p) => p.service(idx),
        };
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb.hidout,
            // Arms abandoned-rumble force-off. hidraw (Steam Input) never surfaces a stop;
            // SDL re-asserts ~2 s, inside the idle window.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// Session DualSense pads (`PUNKTFUNK_GAMEPAD=dualsense`). Analog of [`GamepadManager`](super::gamepad::GamepadManager).
///
/// Touch/motion arrive on `apply_rich`, buttons on `handle`. [`UhidManager`] merges and heartbeats
/// report `0x01` — `hid-playstation`/Proton/SDL treat a multi-second gap as unplug.
pub type DualSenseManager = UhidManager<DsLinuxProto>;

/// DualSense Edge [`PadProto`]: PID `0DF2`, paddles on native `buttons[2]` (no fold).
///
/// `hid-playstation` binds Edge since 6.1 (vibration-v2). Fn/back as evdev (`BTN_TRIGGER_HAPPY1..4`)
/// needs ≥ 7.2; hidraw (SDL / Steam Input) sees them on any kernel.
#[derive(Default)]
pub struct DsEdgeLinuxProto;

impl PadProto for DsEdgeLinuxProto {
    type Pad = DualSensePad;
    type State = DsState;
    const LABEL: &'static str = "DualSense Edge";
    const DEVICE: &'static str = "DualSense Edge";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DualSensePad> {
        let p = DualSensePad::open(idx, &DsUhidIdentity::dualsense_edge())?;
        tracing::info!(
            index = idx,
            "virtual DualSense Edge created (UHID hid-playstation)"
        );
        Ok(p)
    }

    /// Same merge as DualSense, but paddles land on `buttons[2]` (rebuilt every frame, no persistence).
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let mut s = DsState::merge_frame(prev, f, f.buttons);
        s.buttons[2] |= edge_paddle_bits(f.buttons);
        s
    }

    /// Steam dual pads split the one touchpad left/right; clicks ride `touch_click`.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DualSensePad, st: &DsState) {
        let _ = pad.write_state(st);
    }

    /// Same handshake as DualSense. Edge rumble is vibration-v2 `valid_flag2` ([`parse_ds_output`]).
    fn service(&self, pad: &mut DualSensePad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb.hidout,
            // Arms abandoned-rumble force-off. hidraw (Steam Input) never surfaces a stop;
            // SDL re-asserts ~2 s, inside the idle window.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// Session Edge pads: `PUNKTFUNK_GAMEPAD=edge`, or a client's paddle-bearing pad kind.
pub type DualSenseEdgeManager = UhidManager<DsEdgeLinuxProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::HidOutput;
    use std::os::unix::io::AsRawFd;
    use std::time::{Duration, Instant};

    fn find_nodes(name: &str) -> Vec<(String, String)> {
        let s = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        let mut out = Vec::new();
        let mut cur = String::new();
        for line in s.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                cur = n.trim_matches('"').to_string();
            } else if let Some(h) = line.strip_prefix("H: Handlers=") {
                if cur.contains(name) {
                    if let Some(ev) = h.split_whitespace().find(|t| t.starts_with("event")) {
                        out.push((cur.clone(), format!("/dev/input/{ev}")));
                    }
                }
            }
        }
        out
    }

    /// Touchpad / motion / headset siblings do not advertise EV_FF.
    fn has_ff(node: &str) -> bool {
        let Ok(f) = std::fs::OpenOptions::new().read(true).open(node) else {
            return false;
        };
        let mut bits = [0u8; 8];
        // EVIOCGBIT(0, 8) event-type bitmap.
        let req: libc::c_ulong = (2 << 30) | (8 << 16) | (0x45 << 8) | 0x20;
        // SAFETY: EVIOCGBIT(0) copies at most 8 bytes (EV_MAX/8 < 8) into the live `bits` buffer
        // behind the valid evdev fd `f`; the kernel never writes past the ioctl's size argument.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req as _, bits.as_mut_ptr()) };
        rc >= 0 && (bits[0x15 / 8] >> (0x15 % 8)) & 1 == 1
    }

    /// Play FF_RUMBLE on `node` (SDL evdev haptic). Hold the fd: close erases the effect and stops rumble.
    fn evdev_rumble(node: &str, strong: u16, weak: u16) -> std::io::Result<(std::fs::File, i16)> {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)?;
        // struct ff_effect (48 B): type u16, id s16, direction u16, trigger, replay{len,delay},
        // pad to 16, union (ff_rumble_effect { strong, weak }).
        let mut eff = [0u8; 48];
        eff[0..2].copy_from_slice(&0x50u16.to_ne_bytes()); // FF_RUMBLE
        eff[2..4].copy_from_slice(&(-1i16).to_ne_bytes()); // id: kernel assigns
        eff[10..12].copy_from_slice(&5000u16.to_ne_bytes()); // replay.length ms
        eff[16..18].copy_from_slice(&strong.to_ne_bytes());
        eff[18..20].copy_from_slice(&weak.to_ne_bytes());
        // EVIOCSFF = _IOW('E', 0x80, struct ff_effect)
        let req: libc::c_ulong = (1 << 30) | (48 << 16) | (0x45 << 8) | 0x80;
        // SAFETY: EVIOCSFF reads/writes the 48-byte ff_effect behind the valid fd `f`; `eff` is
        // exactly sizeof(struct ff_effect) and outlives the synchronous call.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req as _, eff.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let id = i16::from_ne_bytes([eff[2], eff[3]]);
        // struct input_event (24 B on 64-bit): timeval 16, type u16, code u16, value s32.
        let mut ev = [0u8; 24];
        ev[16..18].copy_from_slice(&0x15u16.to_ne_bytes()); // EV_FF
        ev[18..20].copy_from_slice(&(id as u16).to_ne_bytes());
        ev[20..24].copy_from_slice(&1i32.to_ne_bytes()); // play
        f.write_all(&ev)?;
        Ok((f, id))
    }

    /// `(HID_NAME, HID_UNIQ, /dev/hidrawN)` for every hidraw class device.
    fn hidraw_devices() -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/sys/class/hidraw") else {
            return out;
        };
        for e in dir.flatten() {
            let ue = std::fs::read_to_string(e.path().join("device/uevent")).unwrap_or_default();
            let field = |k: &str| {
                ue.lines()
                    .find_map(|l| l.strip_prefix(k))
                    .unwrap_or_default()
                    .to_string()
            };
            out.push((
                field("HID_NAME="),
                field("HID_UNIQ="),
                format!("/dev/{}", e.file_name().to_string_lossy()),
            ));
        }
        out
    }

    /// Drain feedback for `ms` while heartbeating `st` (silence looks like unplug).
    fn collect(pad: &mut DualSensePad, st: &DsState, ms: u64) -> (Vec<(u16, u16)>, Vec<HidOutput>) {
        let start = Instant::now();
        let (mut levels, mut hidout) = (Vec::new(), Vec::<HidOutput>::new());
        while start.elapsed() < Duration::from_millis(ms) {
            let fb = pad.service(0);
            levels.extend(fb.rumble);
            hidout.extend(fb.hidout);
            let _ = pad.write_state(st);
            std::thread::sleep(Duration::from_millis(4));
        }
        (levels, hidout)
    }

    /// Kernel feedback: evdev FF (ff-memless → UHID_OUTPUT) and hidraw report `0x02` (the only
    /// adaptive-trigger path). Two pads must have distinct pairing uniqs or SDL/Steam dedup them.
    #[test]
    #[ignore = "creates real /dev/uhid devices; needs hid-playstation, the input group, and the 60-punktfunk.rules hidraw rules"]
    fn feedback_flows_via_evdev_ff_and_hidraw() {
        let mut pad0 = DualSensePad::open(0, &DsUhidIdentity::dualsense()).expect("open pad 0");
        let mut pad1 = DualSensePad::open(1, &DsUhidIdentity::dualsense()).expect("open pad 1");
        let st = DsState::neutral();
        // GET_REPORT handshake; return as soon as hid-playstation registers both pads.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1500) {
            let _ = pad0.service(0);
            let _ = pad1.service(1);
            let _ = pad0.write_state(&st);
            let _ = pad1.write_state(&st);
            let hidraws = hidraw_devices();
            let has = |name: &str| hidraws.iter().any(|(n, _, _)| n == name);
            if find_nodes("Punktfunk DualSense 0")
                .iter()
                .any(|(_, n)| has_ff(n))
                && has("Punktfunk DualSense 0")
                && has("Punktfunk DualSense 1")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let nodes = find_nodes("Punktfunk DualSense 0");
        assert!(
            !nodes.is_empty(),
            "hid-playstation did not bind the uhid device"
        );
        let ff_node = nodes
            .iter()
            .map(|(_, n)| n.as_str())
            .find(|n| has_ff(n))
            .expect("no FF-capable evdev among the pad's input devices");

        // Pairing MAC becomes HID_UNIQ; SDL/Steam dedup on it (`ds_pairing_reply`).
        let hidraws = hidraw_devices();
        let uniq = |name: &str| {
            hidraws
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(_, u, _)| u.clone())
                .unwrap_or_else(|| panic!("no hidraw for {name} in {hidraws:?}"))
        };
        assert_ne!(
            uniq("Punktfunk DualSense 0"),
            uniq("Punktfunk DualSense 1"),
            "pads share one pairing MAC — SDL/Steam will dedup them into one controller"
        );

        let (ff_fd, _) = evdev_rumble(ff_node, 0xC000, 0x4000).expect("EVIOCSFF/play");
        let (levels, _) = collect(&mut pad0, &st, 1000);
        assert!(
            levels.iter().any(|&(l, h)| l > 0 || h > 0),
            "evdev FF rumble never surfaced as UHID_OUTPUT: {levels:?}"
        );
        drop(ff_fd); // close erases the effect; the stop must surface
        let (levels, _) = collect(&mut pad0, &st, 800);
        assert!(
            levels.contains(&(0, 0)),
            "erase-on-close never produced a rumble stop: {levels:?}"
        );

        let hr = hidraws
            .iter()
            .find(|(n, _, _)| n == "Punktfunk DualSense 0")
            .map(|(_, _, d)| d.clone())
            .unwrap();
        let mut rep = [0u8; 48];
        rep[0] = 0x02; // USB output report id
        rep[1] = 0x03 | 0x04 | 0x08; // flag0: compat vibration + haptics select + R2 + L2
        rep[2] = 0x04 | 0x10; // flag1: lightbar + player LEDs
        rep[3] = 0x60; // motor right (high)
        rep[4] = 0xA0; // motor left (low)
        rep[11] = 0x21; // R2 trigger block: weapon mode + params
        rep[12] = 0x04;
        rep[13] = 0x07;
        rep[22] = 0x26; // L2 trigger block: vibration mode + params
        rep[23] = 0x02;
        rep[44] = 0x04; // player LED middle
        rep[45] = 0x10;
        rep[46] = 0x20;
        rep[47] = 0x30;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&hr)
            .and_then(|mut f| std::io::Write::write_all(&mut f, &rep))
            .unwrap_or_else(|e| {
                panic!(
                    "cannot write {hr} as this user ({e}) — Steam/SDL would be equally blocked; \
                     are the 60-punktfunk.rules hidraw rules installed?"
                )
            });
        let (levels, hidout) = collect(&mut pad0, &st, 1000);
        assert!(
            levels.contains(&(0xA000, 0x6000)),
            "hidraw rumble did not surface: {levels:?}"
        );
        let triggers: Vec<_> = hidout
            .iter()
            .filter_map(|h| match h {
                HidOutput::Trigger { which, effect, .. } => Some((*which, effect.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            triggers.len(),
            2,
            "expected both trigger blocks: {hidout:?}"
        );
        assert!(
            triggers.contains(&(1, rep[11..22].to_vec())),
            "R2 block not verbatim"
        );
        assert!(
            triggers.contains(&(0, rep[22..33].to_vec())),
            "L2 block not verbatim"
        );
        assert!(
            hidout.iter().any(|h| matches!(
                h,
                HidOutput::Led {
                    r: 0x10,
                    g: 0x20,
                    b: 0x30,
                    ..
                }
            )),
            "lightbar not surfaced: {hidout:?}"
        );
        assert!(
            hidout
                .iter()
                .any(|h| matches!(h, HidOutput::PlayerLeds { bits: 0x04, .. })),
            "player LEDs not surfaced: {hidout:?}"
        );
    }
}
