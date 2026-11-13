//! Virtual Steam Deck / Steam Controller on `/dev/uhid`.
//!
//! Valve VID `28DE` / Deck PID `1205` binds `hid-steam` (gamepad evdev including
//! the four back grips, plus a separate IMU evdev). Steam Input re-grabs it when
//! Steam runs. Descriptor, serializer, mappers, rumble parser: [`super::steam_proto`].
//!
//! Deck `hid-steam` drops events under default `lizard_mode` until `gamepad_mode`
//! is on. The kernel toggles that only when `b9.6` is held ~450 ms with no hidraw
//! client. This module writes sysfs `lizard_mode=N` (needs root) and pulses `b9.6`
//! for [`MODE_ENTER`]. [`MENU_HOLD_CAP`] inserts a one-frame release so a long
//! Start-hold cannot toggle off.
//!
//! Steam rumble (`0xEB`) and kernel settings writes are FEATURE SET_REPORT;
//! answer `err = 0` or the kernel stalls ~5 s per command.

use super::steam_proto::{
    btn, parse_steam_output, sc_merge_frame, serial_reply, serialize_deck_state,
    serialize_sc_state, ScState, SteamFeedback, SteamModel, SteamState, STEAMDECK_RDESC,
    STEAM_REPORT_LEN, STEAM_VENDOR,
};
use crate::uhid_abi::{UhidDevice, UhidEvent};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// ~450 ms of continuous `b9.6` toggles `gamepad_mode`; 650 ms is the margin.
const MODE_ENTER: Duration = Duration::from_millis(650);
/// Stay under the kernel's ~450 ms `b9.6` toggle; a one-frame release resets the timer.
const MENU_HOLD_CAP: Duration = Duration::from_millis(350);

/// Once per process, write `hid_steam.lizard_mode=N` so Deck events are not gated. Needs root;
/// on failure the per-pad `b9.6` pulse still enters `gamepad_mode`.
fn try_clear_lizard_mode() {
    static TRIED: AtomicBool = AtomicBool::new(false);
    if TRIED.swap(true, Ordering::Relaxed) {
        return;
    }
    match std::fs::write("/sys/module/hid_steam/parameters/lizard_mode", "N") {
        Ok(()) => {
            tracing::info!("cleared hid_steam lizard_mode (Steam Deck gamepad events always flow)")
        }
        Err(e) => tracing::debug!(
            error = %e,
            "hid_steam lizard_mode not cleared (no root?) — using the gamepad_mode pulse + guard"
        ),
    }
}

/// The `CREATE2` identity of pad `index` as `model`: what this host opens, and what the seat
/// broker builds for a seat.
pub(crate) fn identity(index: u8, model: SteamModel) -> crate::uhid_abi::Identity {
    let (name, phys, uniq) = match model {
        SteamModel::Deck => ("Steam Deck", "steam", "steam"),
        SteamModel::Controller => ("Steam Controller", "steamctrl", "steamctrl"),
    };
    crate::uhid_abi::Identity {
        bus: crate::uhid_abi::BUS_USB,
        name: format!("Punktfunk {name} {index}"),
        phys: format!("punktfunk/{phys}/{index}"),
        uniq: format!("punktfunk-{uniq}-{index}"),
        rdesc: STEAMDECK_RDESC,
        vendor: STEAM_VENDOR,
        product: model.product(),
        version: 0x0100,
    }
}

/// Virtual Steam Deck or classic Steam Controller on `/dev/uhid`. Drop unbinds `hid-steam`.
pub struct SteamDeckPad {
    dev: UhidDevice,
    model: SteamModel,
    seq: u32,
    created: Instant,
    /// Continuous `b9.6` hold start for [`MENU_HOLD_CAP`]; `None` while released.
    menu_hold_since: Option<Instant>,
}

impl SteamDeckPad {
    pub fn open(index: u8) -> Result<SteamDeckPad> {
        SteamDeckPad::open_model(index, SteamModel::Deck)
    }

    /// Deck only: classic SC (`ID_CONTROLLER_STATE`) has no `gamepad_mode` gate.
    pub fn open_model(index: u8, model: SteamModel) -> Result<SteamDeckPad> {
        if model == SteamModel::Deck {
            try_clear_lizard_mode();
        }
        let kind = match model {
            SteamModel::Deck => crate::pad_broker::PadKind::SteamDeck,
            SteamModel::Controller => crate::pad_broker::PadKind::SteamController,
        };
        let dev = UhidDevice::open_kind(kind, index, &identity(index, model))?;
        Ok(SteamDeckPad {
            dev,
            model,
            seq: 0,
            created: Instant::now(),
            menu_hold_since: None,
        })
    }

    /// Deck: apply the mode-entry overlay and anti-toggle guard, then serialize.
    pub fn write_state(&mut self, st: &SteamState) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let mut r = [0u8; STEAM_REPORT_LEN];
        match self.model {
            SteamModel::Deck => {
                let mut s = *st;
                s.buttons = self.effective_buttons(st.buttons);
                serialize_deck_state(&mut r, &s, self.seq);
            }
            SteamModel::Controller => serialize_sc_state(&mut r, st, self.seq),
        }
        self.dev.write_input(&r)
    }

    /// Create-time `b9.6` pulse still running (Deck only).
    fn in_mode_entry(&self) -> bool {
        self.model == SteamModel::Deck && self.created.elapsed() < MODE_ENTER
    }

    fn effective_buttons(&mut self, mut buttons: u64) -> u64 {
        if self.in_mode_entry() {
            return btn::STEAM_MENU_RIGHT;
        }
        if buttons & btn::MENU != 0 {
            let now = Instant::now();
            match self.menu_hold_since {
                None => self.menu_hold_since = Some(now),
                Some(since) if now.duration_since(since) >= MENU_HOLD_CAP => {
                    buttons &= !btn::MENU; // one-frame release; kernel ~450 ms toggle timer resets
                    self.menu_hold_since = None;
                }
                Some(_) => {}
            }
        } else {
            self.menu_hold_since = None;
        }
        buttons
    }

    /// Non-blocking. Answer GET_REPORT with the serial; `poll` acks SET_REPORT. A Deck's `0xEB`
    /// write is rumble; a Steam Controller's writes go back for the physical pad.
    pub fn service(&mut self) -> SteamFeedback {
        let mut fb = SteamFeedback::default();
        let model = self.model;
        self.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) | UhidEvent::SetReport(data) => match model {
                SteamModel::Deck => {
                    if let Some(r) = parse_steam_output(data).rumble {
                        fb.rumble = Some(r);
                    }
                }
                // `data` leads with report id 0.
                SteamModel::Controller => fb.forward_sc_set(data.get(1..).unwrap_or_default()),
            },
            UhidEvent::GetReport { id, .. } => {
                let _ = dev.reply_get_report(id, Some(&serial_reply("PUNKTFUNK01")));
            }
        });
        fb
    }
}

/// Per-pad transport from [`open_transport`]. UHID reports `Interface: -1`, so Steam
/// Input will not promote it. Gadget and usbip present USB interface 2, which Steam Input promotes.
pub enum DeckTransport {
    Uhid(SteamDeckPad),
    Gadget(crate::steam_gadget::SteamDeckGadget),
    Usbip(crate::steam_usbip::SteamUsbip),
}

impl DeckTransport {
    fn write_state(&mut self, st: &SteamState) {
        match self {
            DeckTransport::Uhid(p) => {
                let _ = p.write_state(st);
            }
            DeckTransport::Gadget(g) => g.write_state(st),
            DeckTransport::Usbip(u) => u.write_state(st),
        }
    }
    fn service(&mut self) -> Option<(u16, u16)> {
        match self {
            DeckTransport::Uhid(p) => p.service().rumble,
            DeckTransport::Gadget(g) => g.service().rumble,
            DeckTransport::Usbip(u) => u.service().rumble,
        }
    }
    fn in_mode_entry(&self) -> bool {
        match self {
            // Steam Input hidraw-reads promoted transports and bypasses hid-steam's evdev mode gate.
            DeckTransport::Uhid(p) => p.in_mode_entry(),
            DeckTransport::Gadget(_) | DeckTransport::Usbip(_) => false,
        }
    }
}

/// InputPlumber hidraw-grabs managed pads and re-emits them under another identity.
/// A grab remaps the Deck (trackpads as stick/mouse, gyro gone). One-shot `/proc` comm scan.
fn warn_if_inputplumber() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(true);
    if !ONCE.swap(false, Ordering::Relaxed) {
        return;
    }
    let running = std::fs::read_dir("/proc")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "inputplumber")
        });
    if running {
        tracing::warn!(
            "InputPlumber is running on this host — if it manages the virtual Steam Deck pad, \
             games see InputPlumber's re-emitted device instead (trackpads may arrive as a \
             stick/mouse, gyro may vanish). Check `inputplumber devices` and exclude the \
             virtual pad from management if inputs look remapped."
        );
    }
}

/// Steam-Input-promotable Deck: `raw_gadget` → `usbip`/`vhci_hcd` → UHID.
/// UHID works everywhere; `Interface: -1` so Steam Input will not promote it.
fn open_transport(idx: u8) -> Result<DeckTransport> {
    warn_if_inputplumber();
    use crate::{steam_gadget, steam_usbip};
    if steam_gadget::gadget_preferred() {
        steam_gadget::ensure_modules();
        match steam_gadget::SteamDeckGadget::open(idx) {
            Ok(g) => {
                tracing::info!(
                    index = idx,
                    "virtual Steam Deck created (USB gadget — Steam Input recognizes it)"
                );
                return Ok(DeckTransport::Gadget(g));
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "USB-gadget Deck unavailable — trying usbip")
            }
        }
    }
    if steam_usbip::usbip_preferred() {
        match steam_usbip::SteamUsbip::open(idx) {
            Ok(u) => return Ok(DeckTransport::Usbip(u)),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "usbip Deck unavailable — falling back to UHID")
            }
        }
    }
    let p = SteamDeckPad::open(idx)?;
    tracing::warn!(
        index = idx,
        "virtual Steam Deck created as UHID hid-steam — Steam Input WON'T promote it (no USB \
         interface), so it won't appear in Game Mode. Load vhci_hcd (usbip) so the pad arrives as a \
         real USB device: `sudo modprobe vhci_hcd`, and ensure it loads at boot."
    );
    Ok(DeckTransport::Uhid(p))
}

/// Deck [`PadProto`]: [`open_transport`], [`SteamState`] mappers, handshake `service`.
/// Slot table / unplug / heartbeat / rumble dedup live in [`UhidManager`]. Mode-entry
/// pulse rides [`force_heartbeat`](PadProto::force_heartbeat).
#[derive(Default)]
pub struct SteamProto;

impl DeckTransport {
    /// `false` once a uhid pad's relay to the seat supervisor ended, or a usbip pad's server.
    fn alive(&self) -> bool {
        match self {
            DeckTransport::Uhid(pad) => pad.dev.alive(),
            DeckTransport::Usbip(pad) => pad.alive(),
            DeckTransport::Gadget(_) => true,
        }
    }
}

impl PadProto for SteamProto {
    type Pad = DeckTransport;

    fn alive(&self, pad: &DeckTransport) -> bool {
        pad.alive()
    }
    type State = SteamState;
    const LABEL: &'static str = "Steam Deck";
    const DEVICE: &'static str = "Steam Deck";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DeckTransport> {
        open_transport(idx)
    }

    fn merge_frame(
        &self,
        prev: &SteamState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SteamState {
        SteamState::merge_frame(prev, f)
    }

    fn apply_rich(&self, st: &mut SteamState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut DeckTransport, st: &SteamState) {
        pad.write_state(st);
    }

    /// Handshake + rumble. No host→client rich feedback, so `hidout` stays empty.
    fn service(&self, pad: &mut DeckTransport, _idx: u8) -> PadFeedback {
        let rumble = pad.service();
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            // `Some` iff `0xEB` this poll. Arms abandoned-rumble force-off; hidraw never surfaces a stop.
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }

    /// Keep writing while the create-time `b9.6` pulse is still running.
    fn force_heartbeat(&self, pad: &DeckTransport) -> bool {
        pad.in_mode_entry()
    }
}

/// Session Steam Deck pads (`PUNKTFUNK_GAMEPAD=steamdeck`).
pub type SteamControllerManager = UhidManager<SteamProto>;

/// A classic Steam Controller's transport: usbip (`vhci_hcd`), a USB device Steam adopts, else
/// UHID, which only `hid-steam` reads (`Interface: -1`).
pub enum ScTransport {
    Usbip {
        pad: crate::steam_usbip::SteamUsbip,
        /// Sequence of the typed frames; a mirrored report carries the pad's own.
        seq: u32,
    },
    Uhid(SteamDeckPad),
}

impl ScTransport {
    /// usbip as the pad its client captured ([`crate::triton_proto::identity_for`]), else UHID.
    fn open(idx: u8) -> Result<ScTransport> {
        use crate::steam_usbip;
        if steam_usbip::usbip_preferred() {
            let identity = crate::triton_proto::identity_for(idx);
            match steam_usbip::SteamUsbip::open_controller(idx, identity) {
                Ok(pad) => {
                    tracing::info!(index = idx, "virtual Steam Controller created (usbip)");
                    return Ok(ScTransport::Usbip { pad, seq: 0 });
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "usbip Steam Controller unavailable — falling back to UHID"
                ),
            }
        }
        let p = SteamDeckPad::open_model(idx, SteamModel::Controller)?;
        tracing::warn!(
            index = idx,
            "virtual Steam Controller created as UHID hid-steam — Steam WON'T list it (no USB \
             interface). Load vhci_hcd (usbip) so the pad arrives as a real USB device: `sudo \
             modprobe vhci_hcd`, and ensure it loads at boot."
        );
        Ok(ScTransport::Uhid(p))
    }

    fn write_state(&mut self, st: &ScState) {
        let now = Instant::now();
        match self {
            ScTransport::Usbip { pad, seq } => pad.write_report(st.report(seq, now)),
            ScTransport::Uhid(p) => {
                let _ = p.dev.write_input(&st.report(&mut p.seq, now));
            }
        }
    }

    /// The host stack's feature SETs for the physical pad, report id first.
    fn service(&mut self) -> Vec<Vec<u8>> {
        match self {
            ScTransport::Usbip { pad, .. } => pad.service().raw,
            ScTransport::Uhid(p) => p.service().raw,
        }
    }
}

/// Classic Steam Controller [`PadProto`]: wired identity `28DE:1102` (`ID_CONTROLLER_STATE`)
/// over [`ScTransport`]. A client that captured the physical pad sends its reports and gets
/// Steam's feature writes back ([`ScState`]); any other client drives the typed mapping.
///
/// Typed: one stick + two pads + two grips ([`sc_from_gamepad`]/[`serialize_sc_state`]).
/// Wire right stick drives the right pad; left-pad contact shadows the left stick;
/// PADDLE1/2 land on the two grips (3/4 fold via remap). No FF: the pad has no motors.
pub struct ScProto {
    /// Fold for wire paddles beyond the SC's two grips (PADDLE3/4).
    remap: crate::steam_remap::RemapConfig,
}

impl Default for ScProto {
    fn default() -> ScProto {
        ScProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for ScProto {
    type Pad = ScTransport;

    /// `false` once a uhid pad's relay to the seat supervisor ended, or a usbip pad's server.
    fn alive(&self, pad: &ScTransport) -> bool {
        match pad {
            ScTransport::Uhid(pad) => pad.dev.alive(),
            ScTransport::Usbip { pad, .. } => pad.alive(),
        }
    }
    type State = ScState;
    const LABEL: &'static str = "Steam Controller";
    const DEVICE: &'static str = "Steam Controller";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<ScTransport> {
        ScTransport::open(idx)
    }

    /// Typed frames keep arriving beside a mirrored pad's reports: they update the fallback
    /// and show the client is alive.
    fn merge_frame(&self, prev: &ScState, f: &punktfunk_core::input::GamepadFrame) -> ScState {
        let mut next = *prev;
        let typed = sc_merge_frame(&prev.typed, f, self.remap.paddles);
        next.typed_frame(typed, Instant::now());
        next
    }

    fn apply_rich(&self, st: &mut ScState, rich: RichInput) {
        st.apply_rich(rich, Instant::now());
    }

    fn heard(&self, st: &mut ScState, now: Instant) {
        st.hear(now);
    }

    fn write_state(&self, pad: &mut ScTransport, st: &ScState) {
        pad.write_state(st);
    }

    /// Serial GET_REPORT, and the stack's feature SETs back to the physical pad on 0xCD. The pad
    /// has no motors: its haptics are among those writes.
    fn service(&self, pad: &mut ScTransport, idx: u8) -> PadFeedback {
        let hidout = pad
            .service()
            .into_iter()
            .map(|data| HidOutput::HidRaw {
                pad: idx,
                kind: HID_RAW_FEATURE,
                data,
            })
            .collect();
        PadFeedback {
            hidout,
            ..PadFeedback::default()
        }
    }
}

/// Session classic Steam Controllers (`PUNKTFUNK_GAMEPAD=steamcontroller`).
pub type SteamCtrlManager = UhidManager<ScProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steam_proto::sc_from_gamepad;
    use crate::test_sysfs::{input_devices, wait_input_gone};

    fn find_node(name: &str) -> Option<String> {
        let devs = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
        for block in devs.split("\n\n") {
            if !block
                .lines()
                .any(|l| l.trim() == format!("N: Name=\"{name}\""))
            {
                continue;
            }
            for l in block.lines() {
                if let Some(h) = l.strip_prefix("H: Handlers=") {
                    if let Some(ev) = h.split_whitespace().find(|t| t.starts_with("event")) {
                        return Some(format!("/dev/input/{ev}"));
                    }
                }
            }
        }
        None
    }

    fn key_is_down(node: &str, code: u16) -> bool {
        use std::os::fd::AsFd;
        let Ok(f) = std::fs::File::open(node) else {
            return false;
        };
        let mut bits = [0u8; 96];
        const EVIOCGKEY: libc::c_ulong = (2 << 30) | (96 << 16) | (0x45 << 8) | 0x18;
        let read = crate::uapi::ioctl_with(f.as_fd(), EVIOCGKEY, &mut bits);
        read.is_ok() && (bits[(code / 8) as usize] >> (code % 8)) & 1 == 1
    }

    fn abs_value(node: &str, abs: u16) -> Option<i32> {
        use std::os::fd::AsFd;
        let f = std::fs::File::open(node).ok()?;
        let mut info = [0u8; 24]; // struct input_absinfo { value, min, max, fuzz, flat, resolution }
        let req: libc::c_ulong =
            (2 << 30) | (24 << 16) | (0x45 << 8) | (0x40 + abs as libc::c_ulong);
        crate::uapi::ioctl_with(f.as_fd(), req, &mut info).ok()?;
        Some(i32::from_ne_bytes([info[0], info[1], info[2], info[3]]))
    }

    /// Bind `hid-steam` (gamepad + IMU), enter `gamepad_mode` via the create pulse, land
    /// BTN_A and left-pad ABS_HAT0X, tear down on drop. Needs `hid-steam` + `input` group.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs hid-steam + the input group"]
    fn backend_binds_and_input_flows() {
        use punktfunk_core::input::gamepad as gs;
        const BTN_A: u16 = 0x130;
        const ABS_HAT0X: u16 = 0x10; // left trackpad X
        let mut pad = SteamDeckPad::open(0).expect("open SteamDeckPad (/dev/uhid + input group?)");
        // Service past MODE_ENTER; hid-steam registers the gamepad before the IMU.
        let mut st = SteamState::from_gamepad(gs::BTN_A | gs::BTN_PADDLE2, 0, 0, 0, 0, 0, 0);
        st.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 1,
            finger: 0,
            touch: true,
            click: false,
            x: -8000,
            y: 9000,
            pressure: 0,
        });
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(1200) {
            let _ = pad.service();
            pad.write_state(&st).expect("write_state");
            let settled = find_node("Steam Deck").is_some_and(|n| {
                input_devices().contains("Steam Deck Motion Sensors")
                    && key_is_down(&n, BTN_A)
                    && abs_value(&n, ABS_HAT0X) == Some(-8000)
            });
            if settled {
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        let devs = input_devices();
        assert!(devs.contains("Steam Deck"), "gamepad evdev not created");
        assert!(
            devs.contains("Steam Deck Motion Sensors"),
            "IMU evdev not created"
        );
        let node = find_node("Steam Deck").expect("gamepad evdev node");
        assert!(
            key_is_down(&node, BTN_A),
            "BTN_A not down — gamepad_mode entry or serialize failed"
        );
        assert_eq!(
            abs_value(&node, ABS_HAT0X),
            Some(-8000),
            "left trackpad (TouchpadEx surface 1) did not reach ABS_HAT0X"
        );
        drop(pad);
        assert!(
            wait_input_gone("Steam Deck Motion Sensors", Duration::from_millis(400)),
            "device not torn down on drop"
        );
    }

    /// Classic-SC `28DE:1102`: bind with no mode-entry pulse; BTN_A and right-stick land
    /// on ABS_RX (right pad), and the kernel's own settings write comes back for the
    /// physical pad while its lizard-mode commands do not.
    ///
    /// The pad is serviced from its own thread: `hid-steam` writes those settings inside an
    /// evdev open and holds the opener 5 s per unanswered one.
    #[test]
    #[ignore = "creates a real /dev/uhid device; needs hid-steam + the input group"]
    fn sc_backend_binds_and_input_flows() {
        use punktfunk_core::input::gamepad as gs;
        const BTN_A: u16 = 0x130;
        const ABS_RX: u16 = 0x03;
        let mut pad = SteamDeckPad::open_model(0, SteamModel::Controller)
            .expect("open SC pad (/dev/uhid + input group?)");
        let st = sc_from_gamepad(gs::BTN_A | gs::BTN_PADDLE1, 0, 0, 9000, 0, 0, 0);
        let stop = AtomicBool::new(false);
        let (seen, forwarded) = std::thread::scope(|s| {
            let serviced = s.spawn(|| {
                let mut forwarded = Vec::new();
                while !stop.load(Ordering::Relaxed) {
                    forwarded.extend(pad.service().raw);
                    pad.write_state(&st).expect("write_state");
                    std::thread::sleep(Duration::from_millis(4));
                }
                forwarded
            });
            let start = Instant::now();
            let mut seen = (false, None);
            while seen != (true, Some(9000)) && start.elapsed() < Duration::from_millis(1500) {
                if let Some(n) = find_node("Steam Controller") {
                    seen = (key_is_down(&n, BTN_A), abs_value(&n, ABS_RX));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            stop.store(true, Ordering::Relaxed);
            (seen, serviced.join().expect("service thread"))
        });
        assert!(
            seen.0,
            "BTN_A not down — SC serialize failed (no mode gate should apply)"
        );
        assert_eq!(
            seen.1,
            Some(9000),
            "wire right stick did not land on the right pad (ABS_RX)"
        );
        assert!(
            forwarded.iter().any(|f| f.starts_with(&[0, 0x87])),
            "the kernel's settings write never came back: {forwarded:02x?}"
        );
        assert!(
            forwarded
                .iter()
                .all(|f| !matches!(f[1], 0x81 | 0x85 | 0x8E)),
            "a lizard-mode command reached the physical pad: {forwarded:02x?}"
        );
    }

    /// Holds a Steam Controller live for `PF_PAD_HOLD_SECS` (default 3) so Steam or an SDL
    /// probe can adopt it: A beats every 400 ms over a steady 100 °/s pitch, and each feature
    /// write the host's stack sends for the physical pad is echoed to stdout.
    #[test]
    #[ignore = "attaches a real vhci_hcd device; needs root"]
    fn steam_controller_holds_for_a_probe() {
        use punktfunk_core::input::{gamepad as gs, GamepadEvent, GamepadFrame};
        let secs = std::env::var("PF_PAD_HOLD_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let mut m = SteamCtrlManager::new();
        m.handle(&GamepadEvent::Arrival {
            index: 5,
            kind: 0,
            capabilities: 0,
            audio_caps: 0,
        });
        assert_eq!(m.live_pads(), 1, "the pad must attach");
        println!("Steam Controller up for {secs}s");
        let (start, mut last, mut beat) = (Instant::now(), Instant::now(), 0u32);
        while start.elapsed() < Duration::from_secs(secs) {
            if last.elapsed() >= Duration::from_millis(400) {
                last = Instant::now();
                beat += 1;
                m.handle(&GamepadEvent::State(GamepadFrame {
                    index: 5,
                    active_mask: 1 << 5,
                    buttons: if beat % 2 == 0 { gs::BTN_A } else { 0 },
                    ..Default::default()
                }));
            }
            m.apply_rich(RichInput::Motion {
                pad: 5,
                gyro: [(100 * gs::MOTION_GYRO_LSB_PER_DEG_S) as i16, 0, 0],
                accel: [0, gs::MOTION_ACCEL_LSB_PER_G as i16, 0],
            });
            m.pump(
                |_, _, _, _, _| {},
                |out| println!("for the pad: {out:02x?}"),
            );
            m.heartbeat(Duration::from_millis(8));
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
