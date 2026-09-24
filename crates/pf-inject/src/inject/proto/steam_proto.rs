//! Steam Controller / Steam Deck HID contract, shared by UHID and the USB
//! gadget/usbip backends. Layout is `drivers/hid/hid-steam.c`
//! (`steam_do_deck_input_event` / `steam_do_deck_sensors_event`); see
//! `design/steam-controller-deck-support.md`.
//!
//! Three traps DualSense does not have:
//! - Input reports are unnumbered (raw 64 bytes, no report-id prefix).
//!   FEATURE get/set reports carry a leading `0x00` that `steam_recv_report`
//!   strips.
//! - `steam_do_deck_input_event` early-returns unless `gamepad_mode` is on
//!   (`!gamepad_mode && lizard_mode`). The backend enters gamepad mode; this
//!   module does not.
//! - `UHID_SET_REPORT` must be answered.
#![allow(dead_code)]

use punktfunk_core::input::gamepad as gs;
use punktfunk_core::quic::RichInput;

/// `hid-steam` matches VID/PID on `BUS_USB`; no usage-page probe.
pub const STEAM_VENDOR: u32 = 0x28DE;
/// Same PID on LCD and OLED Decks.
pub const STEAMDECK_PRODUCT: u32 = 0x1205;
/// Wired Steam Controller (`ID_CONTROLLER_STATE`, report id 1).
pub const STEAMCTRL_WIRED_PRODUCT: u32 = 0x1102;

/// Unnumbered 64-byte frame (report-id 0).
pub const STEAM_REPORT_LEN: usize = 64;

// Command IDs from `drivers/hid/hid-steam.c`.
pub const ID_CLEAR_DIGITAL_MAPPINGS: u8 = 0x81;
pub const ID_GET_ATTRIBUTES_VALUES: u8 = 0x83;
pub const ID_SET_SETTINGS_VALUES: u8 = 0x87;
pub const ID_LOAD_DEFAULT_SETTINGS: u8 = 0x8E;
pub const ID_GET_DEVICE_INFO: u8 = 0xA1;
pub const ID_GET_STRING_ATTRIBUTE: u8 = 0xAE;
pub const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;
/// Host rumble: `steam_haptic_rumble` `[0xEB, 9, …]`. Classic SC pad pulses use `0x8F`.
pub const ID_TRIGGER_RUMBLE_CMD: u8 = 0xEB;
pub const ID_TRIGGER_HAPTIC_PULSE: u8 = 0x8F;
pub const ID_CONTROLLER_STATE: u8 = 0x01;
pub const ID_CONTROLLER_DECK_STATE: u8 = 0x09;
/// Deck state header length byte: 64, as a real Deck sends. SDL drops any other value.
pub const DECK_STATE_LEN: u8 = 64;

/// Controller is the dual-trackpad, report-id-1 identity on the same path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamModel {
    Deck,
    Controller,
}

impl SteamModel {
    pub fn product(self) -> u32 {
        match self {
            SteamModel::Deck => STEAMDECK_PRODUCT,
            SteamModel::Controller => STEAMCTRL_WIRED_PRODUCT,
        }
    }
}

/// Unnumbered 64-byte input + feature. Field layout is cosmetic (`hid-steam`
/// is a raw-event driver) but `steam_probe` needs `hid_parse` plus a non-empty
/// FEATURE list (`steam_is_valve_interface`).
#[rustfmt::skip]
pub const STEAMDECK_RDESC: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (Vendor-Defined 0xFF00)
    0x09, 0x01,       // Usage (0x01)
    0xA1, 0x01,       // Collection (Application)
    0x15, 0x00,       //   Logical Minimum (0)
    0x26, 0xFF, 0x00, //   Logical Maximum (255)
    0x75, 0x08,       //   Report Size (8 bits)
    0x95, 0x40,       //   Report Count (64)
    0x09, 0x01,       //   Usage (0x01)
    0x81, 0x02,       //   Input (Data,Var,Abs)    — the 64-byte state report
    0x09, 0x01,       //   Usage (0x01)
    0x95, 0x40,       //   Report Count (64)
    0xB1, 0x02,       //   Feature (Data,Var,Abs)  — makes steam_is_valve_interface() true
    0xC0,             // End Collection
];

/// Packed across report bytes 8..16 as bit `(byte-8)*8 + bit`. Bytes 12 and 15
/// carry no buttons (`steam_do_deck_input_event`).
pub mod btn {
    // byte 8
    pub const RT_FULL: u64 = 1 << 0; // BTN_TR2
    pub const LT_FULL: u64 = 1 << 1; // BTN_TL2
    pub const RB: u64 = 1 << 2; // BTN_TR
    pub const LB: u64 = 1 << 3; // BTN_TL
    pub const Y: u64 = 1 << 4;
    pub const B: u64 = 1 << 5;
    pub const X: u64 = 1 << 6;
    pub const A: u64 = 1 << 7;
    // byte 9
    pub const DPAD_UP: u64 = 1 << 8;
    pub const DPAD_RIGHT: u64 = 1 << 9;
    pub const DPAD_LEFT: u64 = 1 << 10;
    pub const DPAD_DOWN: u64 = 1 << 11;
    pub const VIEW: u64 = 1 << 12; // BTN_SELECT
    pub const STEAM: u64 = 1 << 13; // BTN_MODE
    pub const MENU: u64 = 1 << 14; // BTN_START
    pub const L5: u64 = 1 << 15; // BTN_GRIPL2 (bottom left)
                                 // byte 10
    pub const R5: u64 = 1 << 16; // BTN_GRIPR2 (bottom right)
    pub const LPAD_CLICK: u64 = 1 << 17; // BTN_THUMB
    pub const RPAD_CLICK: u64 = 1 << 18; // BTN_THUMB2
    pub const LPAD_TOUCH: u64 = 1 << 19; // gates ABS_HAT0
    pub const RPAD_TOUCH: u64 = 1 << 20; // gates ABS_HAT1
    pub const L3: u64 = 1 << 22; // BTN_THUMBL
                                 // byte 11
    pub const R3: u64 = 1 << 26; // BTN_THUMBR
                                 // byte 13
    pub const L4: u64 = 1 << 41; // BTN_GRIPL (top left)
    pub const R4: u64 = 1 << 42; // BTN_GRIPR (top right)
    pub const LJOY_TOUCH: u64 = 1 << 46;
    pub const RJOY_TOUCH: u64 = 1 << 47;
    // byte 14
    pub const QAM: u64 = 1 << 50; // BTN_BASE
    /// Held ~450 ms with no hidraw client toggles `gamepad_mode` (byte 9 bit 6).
    pub const STEAM_MENU_RIGHT: u64 = MENU;
}

/// Analog fields are the raw LE values the kernel reads; it then negates Y
/// (`ABS_Y = -raw`). [`serialize_deck_state`] is a memcpy of these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SteamState {
    /// Report bytes 8..16.
    pub buttons: u64,
    /// Report 48/50/52/54. Kernel negates Y.
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    /// Report 44/46 → ABS_HAT2Y/X.
    pub lt: u16,
    pub rt: u16,
    /// Report 16/18/20/22, centre 0. Kernel surfaces these only while `*PAD_TOUCH` is set.
    pub lpad_x: i16,
    pub lpad_y: i16,
    pub rpad_x: i16,
    pub rpad_y: i16,
    pub lpad_pressure: u16,
    pub rpad_pressure: u16,
    /// `[X, Y, Z]`. Kernel maps to ABS_X/Z/Y + ABS_RX/RZ/RY (Z/RZ negated)
    /// on the sensors evdev.
    pub accel: [i16; 3],
    pub gyro: [i16; 3],
    /// Trackpad clicks from [`apply_rich`]. Kept out of `buttons` because
    /// the manager rebuilds `buttons` from the gamepad frame every tick.
    /// [`serialize_deck_state`] ORs them with `from_gamepad`'s `RPAD_CLICK`
    /// so each source releases independently.
    pub lpad_click: bool,
    pub rpad_click: bool,
}

impl SteamState {
    /// Still pad: 1 g up, not free-fall zeros. Routed through
    /// [`super::steam_remap::motion_wire_to_deck`] so 1 g has one definition
    /// ([`gs::MOTION_NEUTRAL_ACCEL`]).
    pub fn neutral() -> SteamState {
        let (_, accel) = super::steam_remap::motion_wire_to_deck([0; 3], gs::MOTION_NEUTRAL_ACCEL);
        SteamState {
            accel,
            ..SteamState::default()
        }
    }

    /// Zero gyro only (gravity stays). `true` if anything changed —
    /// `PadProto::neutralize_gyro`.
    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    /// Drop trackpad + motion. A pad that took this slot inside the replug
    /// grace must not inherit the last finger or rotation (`PadProto::clear_rich`).
    pub fn clear_rich(&mut self) {
        let fresh = SteamState::neutral();
        self.lpad_x = fresh.lpad_x;
        self.lpad_y = fresh.lpad_y;
        self.rpad_x = fresh.rpad_x;
        self.rpad_y = fresh.rpad_y;
        self.lpad_pressure = fresh.lpad_pressure;
        self.rpad_pressure = fresh.rpad_pressure;
        self.lpad_click = fresh.lpad_click;
        self.rpad_click = fresh.rpad_click;
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    pub fn press(&mut self, mask: u64, down: bool) {
        if down {
            self.buttons |= mask;
        } else {
            self.buttons &= !mask;
        }
    }

    /// XInput frame → Deck. Sticks pass through (kernel negates Y). Triggers
    /// scale u8 → u16 via [`trigger_u16`] and set the full-pull bit. Trackpad
    /// and motion arrive via [`apply_rich`].
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> SteamState {
        let on = |bit: u32| buttons & bit != 0;
        let mut s = SteamState {
            lx,
            ly,
            rx,
            ry,
            lt: trigger_u16(lt),
            rt: trigger_u16(rt),
            ..SteamState::neutral()
        };
        let mut b = 0u64;
        let set = |b: &mut u64, on: bool, m: u64| {
            if on {
                *b |= m;
            }
        };
        set(&mut b, on(gs::BTN_A), btn::A);
        set(&mut b, on(gs::BTN_B), btn::B);
        set(&mut b, on(gs::BTN_X), btn::X);
        set(&mut b, on(gs::BTN_Y), btn::Y);
        set(&mut b, on(gs::BTN_LB), btn::LB);
        set(&mut b, on(gs::BTN_RB), btn::RB);
        set(&mut b, lt > 0, btn::LT_FULL);
        set(&mut b, rt > 0, btn::RT_FULL);
        set(&mut b, on(gs::BTN_BACK), btn::VIEW);
        set(&mut b, on(gs::BTN_START), btn::MENU);
        set(&mut b, on(gs::BTN_GUIDE), btn::STEAM);
        set(&mut b, on(gs::BTN_LS_CLICK), btn::L3);
        set(&mut b, on(gs::BTN_RS_CLICK), btn::R3);
        set(&mut b, on(gs::BTN_DPAD_UP), btn::DPAD_UP);
        set(&mut b, on(gs::BTN_DPAD_DOWN), btn::DPAD_DOWN);
        set(&mut b, on(gs::BTN_DPAD_LEFT), btn::DPAD_LEFT);
        set(&mut b, on(gs::BTN_DPAD_RIGHT), btn::DPAD_RIGHT);
        // DualSense touchpad-click → Deck right-pad click (same pad apply_rich uses).
        set(&mut b, on(gs::BTN_TOUCHPAD), btn::RPAD_CLICK);
        // PADDLE1/2/3/4 = R4/L4/R5/L5 (`input::gamepad`); MISC1 = QAM.
        set(&mut b, on(gs::BTN_PADDLE1), btn::R4);
        set(&mut b, on(gs::BTN_PADDLE2), btn::L4);
        set(&mut b, on(gs::BTN_PADDLE3), btn::R5);
        set(&mut b, on(gs::BTN_PADDLE4), btn::L5);
        set(&mut b, on(gs::BTN_MISC1), btn::QAM);
        s.buttons = b;
        s
    }

    /// One rich event. [`RichInput::Touchpad`] is the right pad (DualSense
    /// analogue); left pad is [`RichInput::TouchpadEx`]. Wire Y is screen
    /// convention (+down); Deck raw is stick convention (+up, centre origin)
    /// so Y is negated here. Leaving it through inverts both trackpads.
    pub fn apply_rich(&mut self, rich: RichInput) {
        /// Screen +down → Deck +up. Saturating: `-32768` has no i16 negation.
        fn flip_y(y: i16) -> i16 {
            (y as i32).saturating_neg().clamp(-32768, 32767) as i16
        }
        match rich {
            RichInput::Touchpad { active, x, y, .. } => {
                self.press(btn::RPAD_TOUCH, active);
                // Wire 0..=65535 (centre 32768, +y down) → centred s16 (+y up).
                self.rpad_x = ((x as i32) - 32768) as i16;
                self.rpad_y = (32768 - (y as i32)).min(32767) as i16;
            }
            RichInput::Motion { gyro, accel, .. } => {
                // Wire is DualSense units; hid-steam wants 16 LSB/°·s and 16384 LSB/g.
                let (g, a) = super::steam_remap::motion_wire_to_deck(gyro, accel);
                self.gyro = g;
                self.accel = a;
            }
            RichInput::TouchpadEx {
                surface,
                touch,
                click,
                x,
                y,
                ..
            } => {
                // surface 1 = left pad; 0 (single) and 2 = right. Y flipped to +up.
                if surface == 1 {
                    self.press(btn::LPAD_TOUCH, touch);
                    // Click stays out of `buttons`: `handle()` rebuilds that mask every frame.
                    self.lpad_click = click;
                    self.lpad_x = x;
                    self.lpad_y = flip_y(y);
                } else {
                    self.press(btn::RPAD_TOUCH, touch);
                    self.rpad_click = click;
                    self.rpad_x = x;
                    self.rpad_y = flip_y(y);
                }
            }
            // HidReport is Triton passthrough, not Deck/SC state.
            RichInput::HidReport { .. } => {}
        }
    }
}

/// `ID_CONTROLLER_DECK_STATE` into the unnumbered 64-byte frame
/// `steam_do_deck_input_event` parses. `data[0]=0x01` is a message type, not
/// a HID report id.
pub fn serialize_deck_state(r: &mut [u8; STEAM_REPORT_LEN], st: &SteamState, seq: u32) {
    r.fill(0);
    r[0] = 0x01;
    r[1] = 0x00;
    r[2] = ID_CONTROLLER_DECK_STATE;
    // SDL drops a Deck frame whose length byte is not 64; the kernel ignores it.
    r[3] = DECK_STATE_LEN;
    r[4..8].copy_from_slice(&seq.to_le_bytes());
    // Rich clicks live outside `buttons` so a button-only frame cannot wipe
    // them. OR with `from_gamepad`'s `RPAD_CLICK` so each source releases
    // independently.
    let mut buttons = st.buttons;
    if st.lpad_click {
        buttons |= btn::LPAD_CLICK;
    }
    if st.rpad_click {
        buttons |= btn::RPAD_CLICK;
    }
    r[8..16].copy_from_slice(&buttons.to_le_bytes()); // bytes 12 and 15 stay 0
    r[16..18].copy_from_slice(&st.lpad_x.to_le_bytes());
    r[18..20].copy_from_slice(&st.lpad_y.to_le_bytes());
    r[20..22].copy_from_slice(&st.rpad_x.to_le_bytes());
    r[22..24].copy_from_slice(&st.rpad_y.to_le_bytes());
    r[24..26].copy_from_slice(&st.accel[0].to_le_bytes()); // accel X → IMU ABS_X
    r[26..28].copy_from_slice(&st.accel[1].to_le_bytes()); // accel Y → IMU ABS_Z (kernel negates)
    r[28..30].copy_from_slice(&st.accel[2].to_le_bytes()); // accel Z → IMU ABS_Y
    r[30..32].copy_from_slice(&st.gyro[0].to_le_bytes()); //  gyro X  → IMU ABS_RX
    r[32..34].copy_from_slice(&st.gyro[1].to_le_bytes()); //  gyro Y  → IMU ABS_RZ (kernel negates)
    r[34..36].copy_from_slice(&st.gyro[2].to_le_bytes()); //  gyro Z  → IMU ABS_RY
                                                          // 36..44 quaternion: left 0; kernel does not surface it.
    r[44..46].copy_from_slice(&st.lt.to_le_bytes()); // left trigger  → ABS_HAT2Y
    r[46..48].copy_from_slice(&st.rt.to_le_bytes()); // right trigger → ABS_HAT2X
    r[48..50].copy_from_slice(&st.lx.to_le_bytes()); // left joystick X  → ABS_X
    r[50..52].copy_from_slice(&st.ly.to_le_bytes()); // left joystick Y  → ABS_Y (kernel negates)
    r[52..54].copy_from_slice(&st.rx.to_le_bytes()); // right joystick X → ABS_RX
    r[54..56].copy_from_slice(&st.ry.to_le_bytes()); // right joystick Y → ABS_RY (kernel negates)
    r[56..58].copy_from_slice(&st.lpad_pressure.to_le_bytes());
    r[58..60].copy_from_slice(&st.rpad_pressure.to_le_bytes());
}

/// Classic Steam Controller mapping. Low 16 button bits match the Deck;
/// the SC tail (`steam_do_input_event`):
/// - `9.7`/`10.0` = the two grips (Deck L5/R5). Wire `BTN_PADDLE2`/`BTN_PADDLE1`
///   land here; fold PADDLE3/4 via [`super::steam_remap`] first.
/// - `10.2` = right-pad click (no right stick): `BTN_RS_CLICK` and DualSense
///   `BTN_TOUCHPAD`.
/// - `10.6` = joystick click = `BTN_LS_CLICK` (Deck L3). No QAM slot.
///
/// Right stick drives `rpad_x/y` + `10.4` while deflected. Left-pad
/// `TouchpadEx` shadows the joystick at bytes 16..20 while touched.
pub fn sc_from_gamepad(
    buttons: u32,
    lx: i16,
    ly: i16,
    rx: i16,
    ry: i16,
    lt: u8,
    rt: u8,
) -> SteamState {
    let on = |bit: u32| buttons & bit != 0;
    let mut s = SteamState {
        lx,
        ly,
        rx: 0,
        ry: 0,
        lt: trigger_u16(lt),
        rt: trigger_u16(rt),
        rpad_x: rx,
        rpad_y: ry,
        ..SteamState::neutral()
    };
    let mut b = 0u64;
    let set = |b: &mut u64, on: bool, m: u64| {
        if on {
            *b |= m;
        }
    };
    set(&mut b, on(gs::BTN_A), btn::A);
    set(&mut b, on(gs::BTN_B), btn::B);
    set(&mut b, on(gs::BTN_X), btn::X);
    set(&mut b, on(gs::BTN_Y), btn::Y);
    set(&mut b, on(gs::BTN_LB), btn::LB);
    set(&mut b, on(gs::BTN_RB), btn::RB);
    set(&mut b, lt > 0, btn::LT_FULL);
    set(&mut b, rt > 0, btn::RT_FULL);
    set(&mut b, on(gs::BTN_BACK), btn::VIEW);
    set(&mut b, on(gs::BTN_START), btn::MENU);
    set(&mut b, on(gs::BTN_GUIDE), btn::STEAM);
    set(&mut b, on(gs::BTN_DPAD_UP), btn::DPAD_UP);
    set(&mut b, on(gs::BTN_DPAD_DOWN), btn::DPAD_DOWN);
    set(&mut b, on(gs::BTN_DPAD_LEFT), btn::DPAD_LEFT);
    set(&mut b, on(gs::BTN_DPAD_RIGHT), btn::DPAD_RIGHT);
    // Grips at Deck L5/R5 (9.7 / 10.0): wire L4/R4 (PADDLE2/PADDLE1).
    set(&mut b, on(gs::BTN_PADDLE2), btn::L5);
    set(&mut b, on(gs::BTN_PADDLE1), btn::R5);
    // 10.6 = joystick click (Deck L3); 10.2 = right-pad click.
    set(&mut b, on(gs::BTN_LS_CLICK), btn::L3);
    set(
        &mut b,
        on(gs::BTN_RS_CLICK) || on(gs::BTN_TOUCHPAD),
        btn::RPAD_CLICK,
    );
    // 10.4 while the stick is deflected (coords are live then).
    set(&mut b, rx != 0 || ry != 0, btn::RPAD_TOUCH);
    s.buttons = b;
    s
}

/// `ID_CONTROLLER_STATE` into the unnumbered 64-byte frame
/// `steam_do_input_event` parses. 24-bit buttons at 8..11, **u8** triggers
/// at 11/12 (Deck uses u16 at 44/46), joystick/left-pad multiplex at 16..20
/// (`10.3` touched → left-pad coords), right pad at 20..24. Accel/gyro at
/// 28..39 is hidraw-only. Kernel negates both Y axes.
pub fn serialize_sc_state(r: &mut [u8; STEAM_REPORT_LEN], st: &SteamState, seq: u32) {
    r.fill(0);
    r[0] = 0x01;
    r[1] = 0x00;
    r[2] = ID_CONTROLLER_STATE;
    r[3] = 0x3C;
    r[4..8].copy_from_slice(&seq.to_le_bytes());
    // Merge rich clicks: 10.1 left (hidraw-only; no kernel key), 10.2 right.
    let mut buttons = st.buttons;
    if st.lpad_click {
        buttons |= btn::LPAD_CLICK;
    }
    if st.rpad_click {
        buttons |= btn::RPAD_CLICK;
    }
    r[8] = (buttons & 0xFF) as u8;
    r[9] = ((buttons >> 8) & 0xFF) as u8;
    r[10] = ((buttons >> 16) & 0xFF) as u8;
    r[11] = (st.lt >> 7).min(255) as u8; // u8; Deck uses u16 at 44/46
    r[12] = (st.rt >> 7).min(255) as u8;
    // 16..20: left pad if `10.3`, else joystick.
    let (x, y) = if buttons & btn::LPAD_TOUCH != 0 {
        (st.lpad_x, st.lpad_y)
    } else {
        (st.lx, st.ly)
    };
    r[16..18].copy_from_slice(&x.to_le_bytes());
    r[18..20].copy_from_slice(&y.to_le_bytes());
    r[20..22].copy_from_slice(&st.rpad_x.to_le_bytes());
    r[22..24].copy_from_slice(&st.rpad_y.to_le_bytes());
    // 28..39 IMU for hidraw; kernel maps none (no SC sensors evdev).
    r[28..30].copy_from_slice(&st.accel[0].to_le_bytes());
    r[30..32].copy_from_slice(&st.accel[1].to_le_bytes());
    r[32..34].copy_from_slice(&st.accel[2].to_le_bytes());
    r[34..36].copy_from_slice(&st.gyro[0].to_le_bytes());
    r[36..38].copy_from_slice(&st.gyro[1].to_le_bytes());
    r[38..40].copy_from_slice(&st.gyro[2].to_le_bytes());
}

/// Wire u8 `0..=255` → Deck u16 `0..=32767`. `v * 128` tops out at 32640
/// (full pull never reaches the declared max). Inverse `>> 7` still
/// round-trips: `32767 >> 7 == 255`.
fn trigger_u16(v: u8) -> u16 {
    ((v as u32 * 32767) / 255) as u16
}

/// `steam_get_serial` GET_REPORT. Feature reports are report-id 0 with a
/// leading byte the kernel strips (`steam_recv_report` copies `buf+1`), so
/// the wire is `[0x00, 0xAE, len, 0x01, ascii…]`. Kernel checks
/// `reply[0]==0xAE`, `1<=reply[1]<=21`, `reply[2]==0x01`; else `"XXXXXXXXXX"`.
pub fn serial_reply(serial: &str) -> [u8; STEAM_REPORT_LEN] {
    let mut buf = [0u8; STEAM_REPORT_LEN];
    let bytes = serial.as_bytes();
    // `min`, not `clamp(1, 21)`: clamp then `bytes[..len]` panics on empty
    // (service thread). A zero length lets the kernel reject and fall back
    // to `"XXXXXXXXXX"`.
    let len = bytes.len().min(21);
    buf[0] = 0x00; // report id 0 — stripped by steam_recv_report
    buf[1] = ID_GET_STRING_ATTRIBUTE;
    buf[2] = len as u8;
    buf[3] = ATTRIB_STR_UNIT_SERIAL;
    buf[4..4 + len].copy_from_slice(&bytes[..len]);
    buf
}

/// Rumble on the 0xCA plane. Classic SC trackpad pulses (`0x8F`) are not
/// parsed here.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct SteamFeedback {
    /// `(low, high)` = left/strong, right/weak.
    pub rumble: Option<(u16, u16)>,
}

/// FEATURE `SET_REPORT` `[0x00, cmd, len, …]`. `0xEB` (`steam_haptic_rumble`)
/// is `[…, 0, intensity(2), left_speed(2), right_speed(2), gains(2)]`;
/// surfaced as `(low, high)` on the 0xCA plane.
pub fn parse_steam_output(data: &[u8]) -> SteamFeedback {
    let mut fb = SteamFeedback::default();
    // data[0] is report-id 0 (still present); command id is data[1].
    if data.len() >= 10 && data[1] == ID_TRIGGER_RUMBLE_CMD {
        let le = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        let left = le(6); // left_speed → low/strong
        let right = le(8); // right_speed → high/weak
        fb.rumble = Some((left, right));
    }
    fb
}

// Real-USB Deck (gadget + usbip): captured 3-interface descriptors so Steam
// Input promotes the device. UHID uses [`STEAMDECK_RDESC`] (`Interface: -1`
// is never promoted). Controller is interface 2 — Steam's driver filters on
// that number. Shared by steam_gadget and steam_usbip.

/// Interface 0, EP 0x81 (captured mouse).
#[rustfmt::skip]
pub const RDESC_DECK_MOUSE: &[u8] = &[
    0x05,0x01,0x09,0x02,0xa1,0x01,0x09,0x01,0xa1,0x00,0x05,0x09,0x19,0x01,0x29,0x02,
    0x15,0x00,0x25,0x01,0x75,0x01,0x95,0x02,0x81,0x02,0x75,0x06,0x95,0x01,0x81,0x01,
    0x05,0x01,0x09,0x30,0x09,0x31,0x15,0x81,0x25,0x7f,0x75,0x08,0x95,0x02,0x81,0x06,
    0x95,0x01,0x09,0x38,0x81,0x06,0x05,0x0c,0x0a,0x38,0x02,0x95,0x01,0x81,0x06,0xc0,0xc0];
/// Interface 1, EP 0x82 (captured boot keyboard).
#[rustfmt::skip]
pub const RDESC_DECK_KBD: &[u8] = &[
    0x05,0x01,0x09,0x06,0xa1,0x01,0x05,0x07,0x19,0xe0,0x29,0xe7,0x15,0x00,0x25,0x01,
    0x75,0x01,0x95,0x08,0x81,0x02,0x81,0x01,0x19,0x00,0x29,0x65,0x15,0x00,0x25,0x65,
    0x75,0x08,0x95,0x06,0x81,0x00,0xc0];
/// Interface 2, EP 0x83 (Usage Page `0xFFFF`, `bCountryCode 33`). Steam filters on this interface.
#[rustfmt::skip]
pub const RDESC_DECK_CTRL: &[u8] = &[
    0x06,0xff,0xff,0x09,0x01,0xa1,0x01,0x09,0x02,0x09,0x03,0x15,0x00,0x26,0xff,0x00,
    0x75,0x08,0x95,0x40,0x81,0x02,0x09,0x06,0x09,0x07,0x15,0x00,0x26,0xff,0x00,0x75,
    0x08,0x95,0x40,0xb1,0x02,0xc0];

/// Stamped into `0x83` attrs `0x0a`/`0x04`. High word is `"PF"` (`0x5046`)
/// plus index so two virtual Decks never collide.
pub fn deck_unit_id(index: u8) -> u32 {
    0x5046_0000 | index as u32
}

/// Steam rejects a `"PF"`-leading serial and substitutes a hash. `'F'`-leading
/// passes, so the marker sits one slot in (`"FVPF"`) — distinct from a real
/// Deck `"FVZZ"`. Derived from [`deck_unit_id`] so `0xAE` and `0x83` agree.
pub fn deck_serial(index: u8) -> String {
    format!("FVPF{:08X}", deck_unit_id(index))
}

/// Header only (controls released). Real-USB transports stream this until the first [`serialize_deck_state`].
pub fn neutral_deck_report() -> [u8; STEAM_REPORT_LEN] {
    let mut r = [0u8; STEAM_REPORT_LEN];
    r[0] = 0x01;
    r[2] = ID_CONTROLLER_DECK_STATE;
    r[3] = DECK_STATE_LEN;
    r
}

/// HID feature GET_REPORT for the real-USB Deck (gadget + usbip). Serving
/// the real `0x83` blob stops Steam re-probing (gamepad-evdev churn).
/// Raw 64-byte EP0 payload (command id first, no report-id prefix) —
/// unlike [`serial_reply`], which carries the UHID report-id the kernel
/// strips. `unit_id` stamps [`deck_unit_id`] into the device-id attrs.
pub fn feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> [u8; STEAM_REPORT_LEN] {
    let cmd = last_set.first().copied().unwrap_or(ID_GET_STRING_ATTRIBUTE);
    let mut r = [0u8; STEAM_REPORT_LEN];
    match cmd {
        ID_GET_ATTRIBUTES_VALUES => {
            // [0x83, 0x2d, then 9 × (attr-id, u32-LE)].
            r[0] = ID_GET_ATTRIBUTES_VALUES;
            r[1] = 0x2d;
            let attrs: [(u8, u32); 9] = [
                (0x01, 0x1205), // product id
                (0x02, 0),
                (0x0a, unit_id), // unit serial number (per-instance)
                (0x04, unit_id ^ 0x5555_5555),
                (0x09, 0x2e),
                (0x0b, 0x0fa0),
                (0x0d, 0),
                (0x0c, 0),
                (0x0e, 0),
            ];
            let mut o = 2;
            for (id, val) in attrs {
                r[o] = id;
                r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                o += 5;
            }
        }
        ID_GET_STRING_ATTRIBUTE => {
            // [0xAE, len, attr, ascii…]. Serial (attr 0x01) wants
            // `reply[2]==0x01` and `1<=len<=21`; other attrs echo the id.
            let attr = last_set.get(2).copied().unwrap_or(ATTRIB_STR_UNIT_SERIAL);
            let b = serial.as_bytes();
            let len = b.len().clamp(1, 20);
            r[0] = ID_GET_STRING_ATTRIBUTE;
            r[1] = len as u8;
            r[2] = attr;
            r[3..3 + len].copy_from_slice(&b[..len]);
        }
        _ => {
            // Unknown cmd (e.g. 0x87 settings): echo last SET_REPORT.
            let n = last_set.len().min(STEAM_REPORT_LEN);
            r[..n].copy_from_slice(&last_set[..n]);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_declares_input_and_feature_reports() {
        assert!(
            STEAMDECK_RDESC.contains(&0xB1),
            "missing Feature main item — steam_is_valve_interface() would fail"
        );
        assert!(STEAMDECK_RDESC.contains(&0x81), "missing Input main item");
        assert_eq!(
            *STEAMDECK_RDESC.last().unwrap(),
            0xC0,
            "unterminated collection"
        );
    }

    /// Offsets match `steam_do_deck_input_event`; buttons pack into 8..16
    /// (12+15 zero). A one-byte slip is noise.
    #[test]
    fn serialize_is_byte_exact() {
        let mut st = SteamState::neutral();
        st.buttons = btn::A | btn::L4 | btn::R5 | btn::QAM;
        st.lx = 0x1122;
        st.ly = 0x3344;
        st.rx = 0x5566;
        st.ry = 0x778;
        st.lt = 0xABCD;
        st.rt = 0xEF01;
        st.lpad_x = 0x0A0B;
        st.lpad_y = 0x0C0D;
        st.rpad_x = 0x0E0F;
        st.rpad_y = 0x1011;
        st.accel = [0x0102, 0x0304, 0x0506];
        st.gyro = [0x0708, 0x090A, 0x0B0C];
        st.lpad_pressure = 0x1314;
        st.rpad_pressure = 0x1516;
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, &st, 0xAABB_CCDD);
        // SDL's `ucLength == 64` check (SDL_hidapi_steamdeck.c) drops anything else.
        assert_eq!(&r[0..4], &[0x01, 0x00, 0x09, 0x40]);
        assert_eq!(neutral_deck_report()[..4], r[..4]);
        assert_eq!(&r[4..8], &[0xDD, 0xCC, 0xBB, 0xAA]);
        // A=bit7 (byte8), L4=bit41 (byte13.1), R5=bit16 (byte10.0), QAM=bit50 (byte14.2).
        assert_eq!(r[8], 0x80); // A
        assert_eq!(r[10], 0x01); // R5
        assert_eq!(r[12], 0x00); // unused button byte
        assert_eq!(r[13], 0x02); // L4 (bit 1)
        assert_eq!(r[14], 0x04); // QAM (bit 2)
        assert_eq!(r[15], 0x00); // unused button byte
        assert_eq!(&r[16..18], &0x0A0Bi16.to_le_bytes()); // lpad X
        assert_eq!(&r[20..22], &0x0E0Fi16.to_le_bytes()); // rpad X
        assert_eq!(&r[24..26], &0x0102i16.to_le_bytes()); // accel X
        assert_eq!(&r[26..28], &0x0304i16.to_le_bytes()); // accel Y
        assert_eq!(&r[28..30], &0x0506i16.to_le_bytes()); // accel Z
        assert_eq!(&r[30..32], &0x0708i16.to_le_bytes()); // gyro X
        assert_eq!(&r[44..46], &0xABCDu16.to_le_bytes()); // left trigger
        assert_eq!(&r[46..48], &0xEF01u16.to_le_bytes()); // right trigger
        assert_eq!(&r[48..50], &0x1122i16.to_le_bytes()); // left joy X
        assert_eq!(&r[50..52], &0x3344i16.to_le_bytes()); // left joy Y
        assert_eq!(&r[52..54], &0x5566i16.to_le_bytes()); // right joy X
        assert_eq!(&r[56..58], &0x1314u16.to_le_bytes()); // left pad pressure
        assert_eq!(&r[58..60], &0x1516u16.to_le_bytes()); // right pad pressure
    }

    #[test]
    fn from_gamepad_and_rich_mapping() {
        let s = SteamState::from_gamepad(
            gs::BTN_A | gs::BTN_START | gs::BTN_GUIDE | gs::BTN_LB,
            1000,
            -2000,
            0,
            0,
            255,
            0,
        );
        assert_ne!(s.buttons & btn::A, 0);
        assert_ne!(s.buttons & btn::MENU, 0);
        assert_ne!(s.buttons & btn::STEAM, 0);
        assert_ne!(s.buttons & btn::LB, 0);
        assert_ne!(s.buttons & btn::LT_FULL, 0); // lt=255 → full-pull bit
        assert_eq!(s.lt, 32767); // full pull reaches the TOP of the declared range
        assert_eq!(s.lx, 1000);
        assert_eq!(s.ly, -2000);

        let mut s = SteamState::neutral();
        s.apply_rich(RichInput::Touchpad {
            pad: 0,
            finger: 0,
            active: true,
            x: 65535,
            y: 0,
        });
        assert_ne!(s.buttons & btn::RPAD_TOUCH, 0);
        assert_eq!(s.rpad_x, 32767); // 65535-32768
        assert_eq!(s.rpad_y, 32767); // wire y=0 top (screen) → Deck +up
                                     // DualSense → Deck: gyro ×16/20, accel ×16384/10000.
        s.apply_rich(RichInput::Motion {
            pad: 0,
            gyro: [1000, -2000, 0],
            accel: [10000, -5000, 0],
        });
        assert_eq!(s.gyro, [800, -1600, 0]);
        assert_eq!(s.accel, [16384, -8192, 0]);
    }

    /// Empty serial must not panic. `clamp(1, 21)` then `bytes[..len]`
    /// indexes a zero-length slice on the service thread. Kernel rejects
    /// `len==0` and falls back.
    #[test]
    fn empty_serial_reply_does_not_panic() {
        let r = serial_reply("");
        assert_eq!(r[1], ID_GET_STRING_ATTRIBUTE);
        assert_eq!(
            r[2], 0,
            "length the kernel will reject, rather than a panic"
        );

        let r = serial_reply("ABC123");
        assert_eq!(r[2], 6);
        assert_eq!(&r[4..10], b"ABC123");
        let long = "X".repeat(40);
        assert_eq!(
            serial_reply(&long)[2],
            21,
            "clamped to the protocol maximum"
        );
    }

    /// Paddle bits → four grips + QAM. `TouchpadEx` x passes through; y
    /// flips screen +down → Deck +up.
    #[test]
    fn back_buttons_and_dual_trackpad_mapping() {
        let s = SteamState::from_gamepad(
            gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4 | gs::BTN_MISC1,
            0,
            0,
            0,
            0,
            0,
            0,
        );
        assert_ne!(s.buttons & btn::R4, 0); // PADDLE1 = R4
        assert_ne!(s.buttons & btn::L4, 0); // PADDLE2 = L4
        assert_ne!(s.buttons & btn::R5, 0); // PADDLE3 = R5
        assert_ne!(s.buttons & btn::L5, 0); // PADDLE4 = L5
        assert_ne!(s.buttons & btn::QAM, 0); // MISC1 = QAM

        let mut s = SteamState::neutral();
        s.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 1,
            finger: 0,
            touch: true,
            click: true,
            x: -5000,
            y: 6000,
            pressure: 100,
        });
        assert_ne!(s.buttons & btn::LPAD_TOUCH, 0);
        // Click is its own field; `handle()` rebuilds `buttons` each frame.
        assert!(s.lpad_click);
        assert_eq!(s.buttons & btn::LPAD_CLICK, 0);
        assert_eq!((s.lpad_x, s.lpad_y), (-5000, -6000));
        s.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 2,
            finger: 0,
            touch: true,
            click: false,
            x: 7000,
            y: -8000,
            pressure: 0,
        });
        assert_ne!(s.buttons & btn::RPAD_TOUCH, 0);
        assert!(!s.rpad_click);
        assert_eq!((s.rpad_x, s.rpad_y), (7000, 8000));

        // Wire y = -32768 must clamp, not overflow.
        s.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 2,
            finger: 0,
            touch: true,
            click: false,
            x: 0,
            y: -32768,
            pressure: 0,
        });
        assert_eq!(s.rpad_y, 32767);
    }

    /// Rich-plane click must survive `handle`'s per-frame `from_gamepad`
    /// rebuild of `buttons`.
    #[test]
    fn rich_click_survives_a_buttons_rebuild() {
        let mut held = SteamState::neutral();
        held.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 1,
            finger: 0,
            touch: true,
            click: true,
            x: 0,
            y: 0,
            pressure: 0,
        });
        assert!(held.lpad_click);
        // Button-only frame rebuilds `buttons`; handle() must still carry the click.
        let mut merged = SteamState::from_gamepad(0, 0, 0, 0, 0, 0, 0);
        assert_eq!(merged.buttons & btn::LPAD_CLICK, 0); // rebuild alone drops the bit
        merged.lpad_click = held.lpad_click; // what handle() copies across
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_deck_state(&mut r, &merged, 0);
        let serialized = u64::from_le_bytes(r[8..16].try_into().unwrap());
        assert_ne!(serialized & btn::LPAD_CLICK, 0);
    }

    /// SC frame vs `ID_CONTROLLER_STATE`: 24-bit buttons, u8 triggers,
    /// joystick/left-pad multiplex, SC button tail (9.7/10.0/10.2/10.6).
    #[test]
    fn sc_serialize_and_mapping() {
        let s = sc_from_gamepad(
            gs::BTN_A | gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_LS_CLICK | gs::BTN_RS_CLICK,
            1000,
            -2000,
            3000,
            -4000,
            255,
            0,
        );
        assert_ne!(s.buttons & btn::A, 0);
        assert_ne!(s.buttons & btn::R5, 0); // PADDLE1 → right grip (10.0)
        assert_ne!(s.buttons & btn::L5, 0); // PADDLE2 → left grip (9.7)
        assert_ne!(s.buttons & btn::L3, 0); // LS click → joystick clicked (10.6)
        assert_ne!(s.buttons & btn::RPAD_CLICK, 0); // RS click → right-pad clicked (10.2)
        assert_ne!(s.buttons & btn::RPAD_TOUCH, 0); // deflected stick = touched pad (10.4)
        assert_eq!((s.rpad_x, s.rpad_y), (3000, -4000)); // right stick rides the right pad
        assert_eq!((s.rx, s.ry), (0, 0));

        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_sc_state(&mut r, &s, 0x0102_0304);
        assert_eq!(&r[0..4], &[0x01, 0x00, 0x01, 0x3C]); // ID_CONTROLLER_STATE
        assert_eq!(&r[4..8], &[0x04, 0x03, 0x02, 0x01]);
        assert_eq!(r[8] & 0x80, 0x80); // A = 8.7
        assert_eq!(r[9] & 0x80, 0x80); // left grip = 9.7
        assert_eq!(r[10] & 0x01, 0x01); // right grip = 10.0
        assert_eq!(r[10] & 0x04, 0x04); // right-pad clicked = 10.2
        assert_eq!(r[10] & 0x40, 0x40); // joystick clicked = 10.6
        assert_eq!(r[11], 255); // left trigger u8
        assert_eq!(r[12], 0); // right trigger u8
        assert_eq!(&r[16..18], &1000i16.to_le_bytes()); // joystick X (lpad untouched)
        assert_eq!(&r[18..20], &(-2000i16).to_le_bytes());
        assert_eq!(&r[20..22], &3000i16.to_le_bytes()); // right pad X
        assert_eq!(&r[22..24], &(-4000i16).to_le_bytes());

        // Surface-1 contact shadows joystick at 16..20 and sets 10.3 (+ 10.1 click).
        let mut s = sc_from_gamepad(0, 1234, 0, 0, 0, 0, 0);
        s.apply_rich(RichInput::TouchpadEx {
            pad: 0,
            surface: 1,
            finger: 0,
            touch: true,
            click: true,
            x: -5000,
            y: 6000,
            pressure: 0,
        });
        let mut r = [0u8; STEAM_REPORT_LEN];
        serialize_sc_state(&mut r, &s, 0);
        assert_eq!(r[10] & 0x08, 0x08); // left-pad touched = 10.3
        assert_eq!(r[10] & 0x02, 0x02); // left-pad clicked = 10.1 (rich click merged)
        assert_eq!(&r[16..18], &(-5000i16).to_le_bytes()); // lpad coords shadow the joystick
        assert_eq!(&r[18..20], &(-6000i16).to_le_bytes()); // screen +down → raw +up (flip)
    }

    /// Leading report-id byte the kernel strips; `reply[1..]` is what
    /// `steam_get_serial` validates: `[0xAE, len, 0x01, ascii…]`.
    #[test]
    fn serial_reply_has_stripped_prefix() {
        let r = serial_reply("PUNKTFUNK01");
        assert_eq!(r[0], 0x00); // report id, stripped by steam_recv_report
        assert_eq!(r[1], ID_GET_STRING_ATTRIBUTE); // becomes reply[0] after strip
        assert!((1..=21).contains(&r[2]));
        assert_eq!(r[3], ATTRIB_STR_UNIT_SERIAL);
        assert_eq!(&r[4..4 + r[2] as usize], b"PUNKTFUNK01");
    }

    #[test]
    fn parse_rumble_feedback() {
        // [report-id 0, 0xEB, len 9, 0, intensity(2), left(2), right(2), gains(2)]
        let mut d = vec![0u8; 12];
        d[1] = ID_TRIGGER_RUMBLE_CMD;
        d[2] = 9;
        d[6..8].copy_from_slice(&0x8000u16.to_le_bytes()); // left_speed
        d[8..10].copy_from_slice(&0x4000u16.to_le_bytes()); // right_speed
        assert_eq!(parse_steam_output(&d).rumble, Some((0x8000, 0x4000)));

        let mut d = vec![0u8; 12];
        d[1] = ID_SET_SETTINGS_VALUES; // a settings write — no rumble
        assert_eq!(parse_steam_output(&d).rumble, None);
    }

    /// Real-USB `0x83` attrs carry the per-instance unit id; `0xAE` carries
    /// the Steam-accepted serial. A slip is Steam re-probing.
    #[test]
    fn deck_feature_reply_contract() {
        let serial = deck_serial(0);
        let unit_id = deck_unit_id(0);
        assert_eq!(serial, "FVPF50460000"); // 12-char alphanumeric, derived from the unit id
        assert_eq!(serial.len(), 12);

        // 0x83 GET_ATTRIBUTES_VALUES: header + (0x0a, unit_id) at the 3rd attribute slot.
        let r = feature_reply(&[ID_GET_ATTRIBUTES_VALUES], &serial, unit_id);
        assert_eq!(r[0], ID_GET_ATTRIBUTES_VALUES);
        assert_eq!(r[1], 0x2d);
        assert_eq!(r[12], 0x0a); // 3rd attr id (slots at 2,7,12,…)
        assert_eq!(
            u32::from_le_bytes([r[13], r[14], r[15], r[16]]),
            unit_id,
            "unit serial attribute must carry the per-instance unit id"
        );

        // 0xAE GET_STRING_ATTRIBUTE: [0xAE, len, attr(0x01), ascii serial…].
        let r = feature_reply(
            &[ID_GET_STRING_ATTRIBUTE, 0, ATTRIB_STR_UNIT_SERIAL],
            &serial,
            unit_id,
        );
        assert_eq!(r[0], ID_GET_STRING_ATTRIBUTE);
        assert_eq!(r[1] as usize, serial.len());
        assert_eq!(r[2], ATTRIB_STR_UNIT_SERIAL);
        assert_eq!(&r[3..3 + serial.len()], serial.as_bytes());

        assert_ne!(deck_unit_id(0), deck_unit_id(1));
        assert_ne!(deck_serial(0), deck_serial(1));
    }
}
