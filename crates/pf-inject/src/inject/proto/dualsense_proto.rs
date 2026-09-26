//! Transport-independent DualSense HID contract: report descriptor, feature blobs, the
//! [`DsState`] model and GameStream mapper, input report `0x01`, output reports `0x02`/`0x31`.
//! Shared by [`super::dualsense`] (Linux UHID) and [`super::dualsense_windows`] (UMDF).
//!
//! Layout is the inputtino DualSense descriptor (`games-on-whales/inputtino`
//! `src/uhid/include/uhid/ps5.hpp`). `hid-playstation` and `hidclass` bind it as USB DualSense.
//!
//! Feature blobs must match `hid-playstation`'s request sizes (calibration 41, pairing 20,
//! firmware 64). A USB backend rejects a longer reply as a malicious URB and drops the device.
//! Tests pin sizes, field offsets, paddle bits, and valid-flag gating.

use punktfunk_core::input::gamepad as gs;
use punktfunk_core::quic::{HidOutput, RichInput};

// GET_REPORT during init. Without these hid-playstation never finishes calibration and
// creates no input devices. First byte of each array is the report id.
#[rustfmt::skip]
// 41 bytes: report 0x05 is a 40-byte feature; hid-playstation asks for id+40.
// A USB backend (see [`crate::dualsense_usbip`]) rejects a longer reply as a
// malicious URB and drops the device. hidraw/hidclass truncate, so extra pad hides.
pub const DS_FEATURE_CALIBRATION: &[u8] = &[ // report 0x05 (motion calibration)
    0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0xF4, 0x01, 0xF4, 0x01, 0x10, 0x27, 0xF0, 0xD8, 0x10, 0x27, 0xF0, 0xD8, 0x10,
    0x27, 0xF0, 0xD8, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
pub const DS_FEATURE_PAIRING: &[u8] = &[ // report 0x09 (pairing info: MAC at bytes 1..7)
    0x09, 0x74, 0xE7, 0xD6, 0x3A, 0x53, 0x35, 0x08, 0x25, 0x00, 0x1E, 0x00, 0xEE, 0x74, 0xD0, 0xBC,
    0x00, 0x00, 0x00, 0x00,
];
#[rustfmt::skip]
pub const DS_FEATURE_FIRMWARE: &[u8] = &[ // report 0x20; update version at bytes 44..46
    // Above Sony's shipping fw (0x0630) so Accessories/libScePad skip an update the
    // virtual pad cannot take. ≥ 0x0224 also selects COMPATIBLE_VIBRATION2, which
    // parse_ds_output must accept alongside flag0.
    0x20, 0x4A, 0x75, 0x6E, 0x20, 0x31, 0x39, 0x20, 0x32, 0x30, 0x32, 0x33, 0x31, 0x34, 0x3A, 0x34,
    0x37, 0x3A, 0x33, 0x34, 0x03, 0x00, 0x44, 0x00, 0x08, 0x02, 0x00, 0x01, 0x36, 0x00, 0x00, 0x01,
    0xC1, 0xC8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x99, 0x09, 0x00, 0x00,
    0x14, 0x00, 0x00, 0x00, 0x0B, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Pairing reply (`0x09`) for pad `pad`: [`DS_FEATURE_PAIRING`] with the MAC low octet offset
/// by the pad index. hid-playstation adopts the MAC as HID `uniq`; SDL/Steam dedup by that
/// serial, so identical MACs merge two virtual pads into one.
pub fn ds_pairing_reply(pad: u8) -> [u8; 20] {
    let mut r = [0u8; 20];
    r.copy_from_slice(DS_FEATURE_PAIRING);
    r[1] = r[1].wrapping_add(pad); // MAC lives at bytes 1..7, LSB first
    r
}

/// USB DualSense HID report descriptor (273 bytes). `hid-playstation` / `hidclass` bind on this.
#[rustfmt::skip]
pub const DUALSENSE_RDESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x2F, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x0A, 0x09, 0x25, 0x95, 0x1A, 0xB1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xB1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02,
    0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x84, 0x09, 0x2C, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x85, 0x09, 0x2D, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xA0, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02,
    0x85, 0xE0, 0x09, 0x2F, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0xF1, 0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x0F, 0xB1, 0x02,
    0x85, 0xF4, 0x09, 0x35, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF5, 0x09, 0x36, 0x95, 0x03, 0xB1, 0x02,
    0xC0,
];

/// DualSense Edge USB HID report descriptor (389 bytes). Versus [`DUALSENSE_RDESC`]: output
/// `0x02` is 47→63 bytes, feature `0xF2` 15→52, and profile slots `0x60..=0x7B` are appended.
/// Input `0x01` is bit-identical; Edge Fn/back bits ride reserved `buttons[2]` (see [`btn2`]).
#[rustfmt::skip]
pub const DUALSENSE_EDGE_RDESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x06, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x20, 0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07,
    0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00, 0x05,
    0x09, 0x19, 0x01, 0x29, 0x0F, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0F, 0x81, 0x02, 0x06,
    0x00, 0xFF, 0x09, 0x21, 0x95, 0x0D, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x22, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x34, 0x81, 0x02, 0x85, 0x02, 0x09, 0x23, 0x95, 0x3F, 0x91, 0x02,
    0x85, 0x05, 0x09, 0x33, 0x95, 0x28, 0xB1, 0x02, 0x85, 0x08, 0x09, 0x34, 0x95, 0x2F, 0xB1, 0x02,
    0x85, 0x09, 0x09, 0x24, 0x95, 0x13, 0xB1, 0x02, 0x85, 0x0A, 0x09, 0x25, 0x95, 0x1A, 0xB1, 0x02,
    0x85, 0x20, 0x09, 0x26, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x21, 0x09, 0x27, 0x95, 0x04, 0xB1, 0x02,
    0x85, 0x22, 0x09, 0x40, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x80, 0x09, 0x28, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x81, 0x09, 0x29, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x2A, 0x95, 0x09, 0xB1, 0x02,
    0x85, 0x83, 0x09, 0x2B, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x84, 0x09, 0x2C, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0x85, 0x09, 0x2D, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xA0, 0x09, 0x2E, 0x95, 0x01, 0xB1, 0x02,
    0x85, 0xE0, 0x09, 0x2F, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x30, 0x95, 0x3F, 0xB1, 0x02,
    0x85, 0xF1, 0x09, 0x31, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2, 0x09, 0x32, 0x95, 0x34, 0xB1, 0x02,
    0x85, 0xF4, 0x09, 0x35, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF5, 0x09, 0x36, 0x95, 0x03, 0xB1, 0x02,
    0x85, 0x60, 0x09, 0x41, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0x61, 0x09, 0x42, 0xB1, 0x02, 0x85, 0x62,
    0x09, 0x43, 0xB1, 0x02, 0x85, 0x63, 0x09, 0x44, 0xB1, 0x02, 0x85, 0x64, 0x09, 0x45, 0xB1, 0x02,
    0x85, 0x65, 0x09, 0x46, 0xB1, 0x02, 0x85, 0x68, 0x09, 0x47, 0xB1, 0x02, 0x85, 0x70, 0x09, 0x48,
    0xB1, 0x02, 0x85, 0x71, 0x09, 0x49, 0xB1, 0x02, 0x85, 0x72, 0x09, 0x4A, 0xB1, 0x02, 0x85, 0x73,
    0x09, 0x4B, 0xB1, 0x02, 0x85, 0x74, 0x09, 0x4C, 0xB1, 0x02, 0x85, 0x75, 0x09, 0x4D, 0xB1, 0x02,
    0x85, 0x76, 0x09, 0x4E, 0xB1, 0x02, 0x85, 0x77, 0x09, 0x4F, 0xB1, 0x02, 0x85, 0x78, 0x09, 0x50,
    0xB1, 0x02, 0x85, 0x79, 0x09, 0x51, 0xB1, 0x02, 0x85, 0x7A, 0x09, 0x52, 0xB1, 0x02, 0x85, 0x7B,
    0x09, 0x53, 0xB1, 0x02, 0xC0,
];

pub const DS_VENDOR: u32 = 0x054C;
pub const DS_PRODUCT: u32 = 0x0CE6;
pub const DS_EDGE_PRODUCT: u32 = 0x0DF2;
/// Report `0x01` is id + 63-byte body.
pub const DS_INPUT_REPORT_LEN: usize = 64;
/// Touchpad extents the kernel advertises as ABS_MT (0..=W-1 / 0..=H-1).
pub const DS_TOUCH_W: u16 = 1920;
pub const DS_TOUCH_H: u16 = 1080;

/// `buttons[0]`: face bits; low nibble is the hat (filled in `serialize_state`).
pub mod btn0 {
    pub const SQUARE: u8 = 0x10;
    pub const CROSS: u8 = 0x20;
    pub const CIRCLE: u8 = 0x40;
    pub const TRIANGLE: u8 = 0x80;
}
/// `buttons[1]`: shoulders, triggers-as-buttons, create/options, stick clicks.
pub mod btn1 {
    pub const L1: u8 = 0x01;
    pub const R1: u8 = 0x02;
    pub const L2: u8 = 0x04;
    pub const R2: u8 = 0x08;
    pub const CREATE: u8 = 0x10; // "Share"
    pub const OPTIONS: u8 = 0x20;
    pub const L3: u8 = 0x40;
    pub const R3: u8 = 0x80;
}
/// `buttons[2]`: PS, touchpad click, mute. Edge Fn/back occupy bits 4–7 (`DS_EDGE_BUTTONS_*` /
/// SDL `SDL_GAMEPAD_BUTTON_PS5_*`); plain DS5 leaves those reserved. Kernel: `BTN_TRIGGER_HAPPY1..4`.
pub mod btn2 {
    pub const PS: u8 = 0x01;
    pub const TOUCHPAD: u8 = 0x02;
    /// Mic-mute / capture — set from wire `BTN_MISC1` in `DsState::from_gamepad`.
    pub const MUTE: u8 = 0x04;
    pub const EDGE_FN_LEFT: u8 = 0x10;
    pub const EDGE_FN_RIGHT: u8 = 0x20;
    pub const EDGE_BACK_LEFT: u8 = 0x40;
    pub const EDGE_BACK_RIGHT: u8 = 0x80;
}

/// Wire paddles → Edge `buttons[2]`: PADDLE1/2 (R4/L4, Steam primary pair) → BACK right/left;
/// PADDLE3/4 (R5/L5) → Fn right/left. Lands on kernel `BTN_TRIGGER_HAPPY1..4` / SDL function
/// buttons instead of the fold/drop policy used on a plain DualSense.
pub fn edge_paddle_bits(buttons: u32) -> u8 {
    use punktfunk_core::input::gamepad as gs;
    let mut b = 0;
    if buttons & gs::BTN_PADDLE1 != 0 {
        b |= btn2::EDGE_BACK_RIGHT; // R4
    }
    if buttons & gs::BTN_PADDLE2 != 0 {
        b |= btn2::EDGE_BACK_LEFT; // L4
    }
    if buttons & gs::BTN_PADDLE3 != 0 {
        b |= btn2::EDGE_FN_RIGHT; // R5
    }
    if buttons & gs::BTN_PADDLE4 != 0 {
        b |= btn2::EDGE_FN_LEFT; // L5
    }
    b
}

#[derive(Clone, Copy, Default)]
pub struct Touch {
    pub active: bool,
    pub id: u8,
    pub x: u16, // 0..=DS_TOUCH_W-1
    pub y: u16, // 0..=DS_TOUCH_H-1
}

/// DualSense report-`0x01` state. Neutral stick is `0x80`; released trigger is `0x00`; hat `8` is centered.
#[derive(Clone, Copy, Default)]
pub struct DsState {
    pub lx: u8,
    pub ly: u8,
    pub rx: u8,
    pub ry: u8,
    pub l2: u8,
    pub r2: u8,
    pub dpad: u8, // 0..7 direction, 8 = neutral
    pub buttons: [u8; 4],
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
    pub touch: [Touch; 2],
    /// Rich-plane pad-click per contact (`TouchpadEx.click`). Serializers OR any slot into the
    /// one DualSense click bit. Lives outside `buttons`: `from_gamepad` rebuilds those every
    /// frame, so managers must persist this with `touch`/`gyro`/`accel`.
    pub touch_click: [bool; 2],
}

impl DsState {
    /// Centered, nothing pressed (sticks `0x80`, hat 8). Accel is 1 g up
    /// ([`gs::MOTION_NEUTRAL_ACCEL`]), not `[0, 0, 0]` — zero reads as free-fall.
    pub fn neutral() -> DsState {
        DsState {
            lx: 0x80,
            ly: 0x80,
            rx: 0x80,
            ry: 0x80,
            dpad: 8,
            accel: gs::MOTION_NEUTRAL_ACCEL,
            ..Default::default()
        }
    }

    /// Zero gyro only (gravity on accel is persistent). Returns whether it changed —
    /// `PadProto::neutralize_gyro` idle-motion watchdog.
    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    /// Reset touch, pad-click, and motion; leave buttons/sticks/triggers. `PadProto::clear_rich`:
    /// a pad that takes this slot during replug grace must not inherit the last one's contacts.
    pub fn clear_rich(&mut self) {
        let fresh = DsState::neutral();
        self.touch = fresh.touch;
        self.touch_click = fresh.touch_click;
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// GameStream/XInput frame → DualSense fields. Invert Y in i16 (XInput `+y` is up, DualSense
    /// `0` is up) before the 8-bit quantise. Touch and motion come from rich-input, not this frame.
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> DsState {
        use punktfunk_core::input::gamepad as gs;
        let to_u8 = |v: i16| (((v as i32) + 32768) >> 8) as u8;
        let on = |bit: u32| buttons & bit != 0;
        // Invert in i16, then quantise. `255 - to_u8(v)` is wrong: 0..=255 has no midpoint, so
        // `255 - 0x80` = 0x7F and a centred stick sits one LSB off `DsState::neutral`. Cost:
        // i16::MIN and -32767 share 255.
        let mut s = DsState {
            lx: to_u8(lx),
            ly: to_u8(ly.saturating_neg()),
            rx: to_u8(rx),
            ry: to_u8(ry.saturating_neg()),
            l2: lt,
            r2: rt,
            ..DsState::neutral()
        };
        s.set_dpad(
            on(gs::BTN_DPAD_UP),
            on(gs::BTN_DPAD_DOWN),
            on(gs::BTN_DPAD_LEFT),
            on(gs::BTN_DPAD_RIGHT),
        );
        let mut b0 = 0;
        if on(gs::BTN_A) {
            b0 |= btn0::CROSS;
        }
        if on(gs::BTN_B) {
            b0 |= btn0::CIRCLE;
        }
        if on(gs::BTN_X) {
            b0 |= btn0::SQUARE;
        }
        if on(gs::BTN_Y) {
            b0 |= btn0::TRIANGLE;
        }
        s.buttons[0] = b0; // face high nibble; serialize_state ORs the hat into the low nibble
        let mut b1 = 0;
        if on(gs::BTN_LB) {
            b1 |= btn1::L1;
        }
        if on(gs::BTN_RB) {
            b1 |= btn1::R1;
        }
        if lt > 0 {
            b1 |= btn1::L2;
        }
        if rt > 0 {
            b1 |= btn1::R2;
        }
        if on(gs::BTN_BACK) {
            b1 |= btn1::CREATE;
        }
        if on(gs::BTN_START) {
            b1 |= btn1::OPTIONS;
        }
        if on(gs::BTN_LS_CLICK) {
            b1 |= btn1::L3;
        }
        if on(gs::BTN_RS_CLICK) {
            b1 |= btn1::R3;
        }
        s.buttons[1] = b1;
        if on(gs::BTN_GUIDE) {
            s.buttons[2] |= btn2::PS;
        }
        if on(gs::BTN_TOUCHPAD) {
            s.buttons[2] |= btn2::TOUCHPAD;
        }
        // BTN_MISC1 → mute. Rebuilt each frame like PS/TOUCHPAD (no persistence slot).
        if on(gs::BTN_MISC1) {
            s.buttons[2] |= btn2::MUTE;
        }
        s
    }

    pub fn set_dpad(&mut self, up: bool, down: bool, left: bool, right: bool) {
        // DualSense hat: 0=N,1=NE,2=E,3=SE,4=S,5=SW,6=W,7=NW,8=neutral.
        self.dpad = match (up, right, down, left) {
            (true, false, false, false) => 0,
            (true, true, false, false) => 1,
            (false, true, false, false) => 2,
            (false, true, true, false) => 3,
            (false, false, true, false) => 4,
            (false, false, true, true) => 5,
            (false, false, false, true) => 6,
            (true, false, false, true) => 7,
            _ => 8,
        };
    }

    /// One rich event into this state. Shared by every DualSense-family backend; `touch_w`/
    /// `touch_h` are the advertised extents (1920×1080 vs DS4 1920×942).
    ///
    /// Wire touch is screen convention (top-left, +y down), same as the DualSense pad — no flip.
    /// Steam `TouchpadEx` surfaces split the one DualSense pad: left → contact 0 on the left
    /// half, right → contact 1 on the right half. Clicks land in [`DsState::touch_click`].
    pub fn apply_rich(&mut self, rich: RichInput, touch_w: u16, touch_h: u16) {
        // Normalized 0..=u16::MAX → 0..=extent-1 (kernel ABS_MT range).
        let scale = |n: u32, extent: u16| ((n * (extent - 1) as u32) / u16::MAX as u32) as u16;
        match rich {
            RichInput::Touchpad {
                finger,
                active,
                x,
                y,
                ..
            } => {
                // Two contacts. Clamp the untrusted wire `finger` and keep contact id = slot.
                let slot = (finger as usize).min(1);
                self.touch[slot] = Touch {
                    active,
                    id: slot as u8,
                    x: scale(x as u32, touch_w),
                    y: scale(y as u32, touch_h),
                };
            }
            RichInput::Motion { gyro, accel, .. } => {
                // The wire is already DualSense-convention units (20 LSB/°·s, 10000 LSB/g).
                self.gyro = gyro;
                self.accel = accel;
            }
            RichInput::TouchpadEx {
                surface,
                finger,
                touch,
                click,
                x,
                y,
                ..
            } => {
                let n = |v: i16| ((v as i32) + 32768) as u32; // signed centre-0 → 0..=65535
                let half = touch_w / 2;
                let (slot, tx) = match surface {
                    // The single / DualSense pad: full extent, slot by finger.
                    0 => ((finger as usize).min(1), scale(n(x), touch_w)),
                    // Steam LEFT pad → contact 0 on the left half.
                    1 => (0, scale(n(x), half)),
                    // Steam RIGHT pad (or anything newer) → contact 1 on the right half.
                    _ => (1, half + scale(n(x), half)),
                };
                self.touch[slot] = Touch {
                    active: touch,
                    id: slot as u8,
                    x: tx,
                    y: scale(n(y), touch_h),
                };
                self.touch_click[slot] = click;
            }
            // Raw as-is passthrough reports belong to the Triton backend, never a DS state.
            RichInput::HidReport { .. } => {}
        }
    }

    /// `buttons[2]` plus the touchpad-click bit if any [`DsState::touch_click`] slot is held.
    pub fn buttons2_with_click(&self) -> u8 {
        let mut b = self.buttons[2];
        if self.touch_click.iter().any(|c| *c) {
            b |= btn2::TOUCHPAD;
        }
        b
    }
}

/// Report `0x01`. Offsets match kernel `struct dualsense_input_report` (id at `r[0]`, so
/// struct offset N is `r[N + 1]`): x..rz 0–5, seq 6, buttons[4] 7–10, reserved[4] 11–14,
/// gyro[3] 15–20, accel[3] 21–26, sensor_timestamp 27–30, IMU temperature 31, points[2] 32–39,
/// status 52–53. The trigger status (41–42) is [`DsTriggers::stamp`]'s.
pub fn serialize_state(r: &mut [u8; DS_INPUT_REPORT_LEN], st: &DsState, seq: u8, ts: u32) {
    r[0] = 0x01;
    r[1] = st.lx;
    r[2] = st.ly;
    r[3] = st.rx;
    r[4] = st.ry;
    r[5] = st.l2;
    r[6] = st.r2;
    r[7] = seq; // seq_number (struct off 6)
    r[8] = (st.dpad & 0x0F) | (st.buttons[0] & 0xF0); // off 7: dpad + face buttons
    r[9] = st.buttons[1];
    r[10] = st.buttons2_with_click(); // off 9: PS/touchpad-click/mute; rich pad clicks OR in
    r[11] = st.buttons[3];
    for (i, v) in st.gyro.iter().enumerate() {
        r[16 + i * 2..18 + i * 2].copy_from_slice(&v.to_le_bytes()); // gyro at struct off 15
    }
    for (i, v) in st.accel.iter().enumerate() {
        r[22 + i * 2..24 + i * 2].copy_from_slice(&v.to_le_bytes()); // accel at struct off 21
    }
    r[28..32].copy_from_slice(&ts.to_le_bytes()); // sensor_timestamp (struct off 27)
    pack_touch(&mut r[33..37], &st.touch[0]); // touch point 1 (struct off 32)
    pack_touch(&mut r[37..41], &st.touch[1]); // touch point 2

    // IMU temperature: a real pad reads 0x0b–0x14 indoors.
    r[32] = 0x14;
    // A USB pad runs on cable power: charge complete (a zero battery reads ~5 % and SteamOS
    // warns), and the plug byte says USB data + power with no headset in the jack.
    r[53] = 0x2A;
    r[54] = 0x18;
}

/// Adaptive-trigger status the game reads back: report `0x01` struct offsets 41 (R2) and 42
/// (L2), high nibble = status, low nibble = the zone the trigger stops in. A game that arms a
/// Weapon effect fires on the 1 → 2 transition, so a pad that leaves these zero swallows every
/// shot while adaptive triggers are on. Only the official modes move the nibble; the firmware
/// derives it from the armed effect and the trigger position, and so does this.
///
/// Contract, from Nielk1's `TriggerEffectGenerator` (the effect factories' own documentation):
/// Feedback and Vibration "report 0 before the effect and 1 when in the effect"; Weapon reports
/// "0 before the effect and 1 when in the effect, and 2 after until again before the start
/// position". The 2 is positional and latched by position alone — arming an effect under an
/// already-held trigger reports 2 at once, which looks like a missing edge check and is not.
/// The LOW nibble is the one part no source documents; it is modelled here, not verified.
///
/// Lives on the pad, not [`DsState`]: [`parse_ds_output`] and [`serialize_state`] meet there.
#[derive(Clone, Copy, Default)]
pub struct DsTriggers([TriggerFb; 2]);

/// Official effect modes. `Off` (`0x05`) belongs here too and reports zero, which is what an
/// unmatched mode already produces.
///
/// Every other mode leaves the nibble at zero on real firmware, so a game driving one of them
/// reads zero from a real DualSense as well — widening this set would report a status the
/// hardware never gives. That includes the simple (`0x01`/`0x02`/`0x06`) and limited
/// (`0x11`/`0x12`) leftovers, which run the same effects under a different parameter layout,
/// and the unofficial Bow/Galloping/Machine (`0x22`/`0x23`/`0x27`).
mod trig_mode {
    pub const FEEDBACK: u8 = 0x21;
    pub const WEAPON: u8 = 0x25;
    pub const VIBRATION: u8 = 0x26;
}

/// One trigger's armed effect. `zones` is the 10-bit active-zone mask the official modes carry
/// in parameters 1-2: Feedback/Vibration set every resisting zone, Weapon just start and stop.
#[derive(Clone, Copy, Default)]
struct TriggerFb {
    mode: u8,
    zones: u16,
    /// Weapon only: past the stop. Held until the trigger comes back before the start zone,
    /// so easing off inside the effect does not re-arm the shot.
    fired: bool,
}

impl DsTriggers {
    /// Latch what the game just armed. Re-arming the same effect keeps `fired` — a game that
    /// re-sends its effect every frame would otherwise never hold a shot.
    pub fn observe(&mut self, hidout: &[HidOutput]) {
        for h in hidout {
            let HidOutput::Trigger { which, effect, .. } = h else {
                continue;
            };
            let at = |i: usize| effect.get(i).copied().unwrap_or(0);
            let (mode, zones) = (at(0), u16::from_le_bytes([at(1), at(2)]) & 0x03FF);
            let t = &mut self.0[usize::from(*which).min(1)];
            if (t.mode, t.zones) != (mode, zones) {
                *t = TriggerFb {
                    mode,
                    zones,
                    fired: false,
                };
            }
        }
    }

    /// Stamp both status bytes into a serialized report `0x01`. Call after [`serialize_state`]
    /// with the same trigger positions.
    pub fn stamp(&mut self, r: &mut [u8; DS_INPUT_REPORT_LEN], l2: u8, r2: u8) {
        r[42] = self.0[1].status_byte(r2); // wire `which` 1 = R2, struct off 41
        r[43] = self.0[0].status_byte(l2); // wire `which` 0 = L2, struct off 42
    }
}

impl TriggerFb {
    /// Status nibble over stop zone for the current trigger position. With no effect armed a
    /// real pad reports zone 9, the end of travel.
    fn status_byte(&mut self, pos: u8) -> u8 {
        if self.zones == 0 {
            return 0x09;
        }
        // Ten equal zones across the travel; the effect parameters are named in the same units.
        let zone = (u16::from(pos) * 10 / 256) as u8;
        let start = self.zones.trailing_zeros() as u8;
        let stop = (15 - self.zones.leading_zeros()) as u8;
        match self.mode {
            trig_mode::WEAPON => {
                if zone < start {
                    self.fired = false;
                } else if zone >= stop {
                    self.fired = true;
                }
                let status = if self.fired {
                    2
                } else if zone >= start {
                    1
                } else {
                    0
                };
                status << 4 | stop
            }
            // Resisting or vibrating only inside an active zone, so read the mask, not the span.
            trig_mode::FEEDBACK | trig_mode::VIBRATION => {
                let inside = self.zones & (1 << zone.min(15)) != 0;
                u8::from(inside) << 4 | start
            }
            _ => 0,
        }
    }
}

fn pack_touch(dst: &mut [u8], t: &Touch) {
    // byte0: bit7 = NOT active (1 = no contact), bits0-6 = contact id.
    dst[0] = (t.id & 0x7F) | if t.active { 0 } else { 0x80 };
    // The kernel advertises ABS_MT ranges 0..=W-1 / 0..=H-1 — never emit the size itself.
    let (x, y) = (t.x.min(DS_TOUCH_W - 1), t.y.min(DS_TOUCH_H - 1));
    dst[1] = (x & 0xFF) as u8;
    dst[2] = (((x >> 8) & 0x0F) as u8) | (((y & 0x0F) as u8) << 4);
    dst[3] = ((y >> 4) & 0xFF) as u8;
}

/// One output-report pass. Lightbar / player LEDs / triggers ride HID-output 0xCD; rumble
/// rides the universal 0xCA plane so a non-DualSense client still feels it.
#[derive(Default)]
pub struct DsFeedback {
    pub hidout: Vec<HidOutput>,
    /// `(low, high)` motor levels when the report carried them. This parser widens 8-bit motors
    /// by `<< 8` (`0..=0xFF00`). The Windows backend uses `× 257` and reaches `0xFFFF`. Both
    /// narrow with `>> 8` to 255 — do not "fix" one to match the other
    /// ([`crate::uhid_manager::PadFeedback::rumble`] sees both).
    pub rumble: Option<(u16, u16)>,
    /// Output-report ring overflowed this poll: pending reports were discarded and feedback is
    /// unknown. The [`UhidManager`](crate::uhid_manager) must resync. Set by the backend drain,
    /// never by this parser.
    pub resync: bool,
}

/// DualSense **output** report field offsets, including the leading report id at `[0]`.
/// The payload block is shared; the header in front of it is not:
///
/// - USB (these constants, this parser): base 0, first payload byte `[1]`
/// - SDL `DS5EffectsState_t` / `pf-client-core` `Ds5Feedback`: base −1, no report id
/// - Bluetooth report `0x31`: base +2 (id, sequence, magic), CRC32 in the last 4 bytes
///
/// Kotlin (`DsDevice.kt`) and Swift (`DualSenseHID.swift`) cannot import this module —
/// keep those copies in step by hand.
pub mod out_report {
    /// `valid_flag0`: BIT0 compat vibration, BIT1 haptics select, BIT2 R2, BIT3 L2.
    pub const VALID_FLAG0: usize = 1;
    /// `valid_flag1`: BIT0 mic-mute LED, BIT2 lightbar, BIT4 player indicators.
    pub const VALID_FLAG1: usize = 2;
    /// High-frequency (small / right) motor.
    pub const MOTOR_RIGHT: usize = 3;
    /// Low-frequency (big / left) motor.
    pub const MOTOR_LEFT: usize = 4;
    /// Mic-mute LED mode: 0 off, 1 on, 2 pulse (`mute_button_led`).
    pub const MIC_LED: usize = 9;
    /// First byte of the RIGHT trigger's parameter block — it precedes the left one in the report.
    pub const RIGHT_TRIGGER: usize = 11;
    /// First byte of the LEFT trigger's parameter block.
    pub const LEFT_TRIGGER: usize = 22;
    /// One adaptive-trigger parameter block: a mode byte plus 10 parameters.
    pub const TRIGGER_LEN: usize = 11;
    /// `valid_flag2`: BIT2 = `COMPATIBLE_VIBRATION2` (the firmware ≥ 2.24 rumble signal).
    pub const VALID_FLAG2: usize = 39;
    /// Lit player-indicator bits (low 5).
    pub const PLAYER_LEDS: usize = 44;
    /// Lightbar red; green and blue follow.
    pub const LED_RGB: usize = 45;
}

/// Parse output report `0x02` into [`DsFeedback`], indexed off [`out_report`]. Rumble,
/// lightbar, player LEDs and the mic-mute LED are typed; trigger blocks and audio-control are
/// forwarded raw.
///
/// Bluetooth `0x31` is accepted too: libScePad writes it to any pad whose output report
/// exceeds 48 bytes, as the Edge's does. Its sequence and tag bytes are skipped; the CRC is
/// not checked.
///
/// Gated on valid-flags: writers set only the bits they mean to change and zero the rest, so
/// an ungated parse would turn a rumble write into lightbar-off + triggers-off.
///
/// A report with no valid-flag bit at all is a rumble stop. SDL stops rumble that way, and
/// the pad leaves rumble emulation for audio haptics, where the motor bytes are ignored. An
/// LED-only report says nothing about rumble, as `hid-playstation` writes it.
pub fn parse_ds_output(pad: u8, data: &[u8], fb: &mut DsFeedback) {
    use out_report as o;
    let data = match data.first() {
        Some(0x02) => data,
        Some(0x31) => data.get(2..).unwrap_or_default(),
        _ => return,
    };
    if data.len() < 48 {
        return;
    }
    let flag0 = data[o::VALID_FLAG0];
    let flag1 = data[o::VALID_FLAG1];
    let flag2 = data[o::VALID_FLAG2];
    if flag0 == 0 && flag1 == 0 && flag2 == 0 {
        fb.rumble = Some((0, 0));
    }
    // Rumble on flag0 BIT0/BIT1 or valid_flag2 COMPATIBLE_VIBRATION2 (fw ≥ 2.24). Both must
    // land: a dropped stop is silent here and the 500 ms refresh then re-sends stale motors.
    // Widen `<< 8` to 0..=0xFF00 (see `DsFeedback::rumble`); (low, high) = (left, right).
    if flag0 & 0x03 != 0 || flag2 & 0x04 != 0 {
        let high = (data[o::MOTOR_RIGHT] as u16) << 8;
        let low = (data[o::MOTOR_LEFT] as u16) << 8;
        fb.rumble = Some((low, high));
    }
    if flag1 & 0x04 != 0 {
        let (r, g, b) = (data[o::LED_RGB], data[o::LED_RGB + 1], data[o::LED_RGB + 2]);
        fb.hidout.push(HidOutput::Led { pad, r, g, b });
    }
    if flag1 & 0x10 != 0 {
        fb.hidout.push(HidOutput::PlayerLeds {
            pad,
            bits: data[o::PLAYER_LEDS] & 0x1F,
        });
    }
    if flag1 & 0x01 != 0 {
        fb.hidout.push(HidOutput::MicLed {
            pad,
            mode: data[o::MIC_LED],
        });
    }
    // Right trigger block first (SDL `DS5EffectsState_t` / inputtino). Wire `which`: 0 = L2, 1 = R2.
    if data.len() >= o::LEFT_TRIGGER + o::TRIGGER_LEN {
        if flag0 & 0x04 != 0 {
            fb.hidout.push(HidOutput::Trigger {
                pad,
                which: 1,
                effect: data[o::RIGHT_TRIGGER..o::RIGHT_TRIGGER + o::TRIGGER_LEN].to_vec(),
            });
        }
        if flag0 & 0x08 != 0 {
            fb.hidout.push(HidOutput::Trigger {
                pad,
                which: 0,
                effect: data[o::LEFT_TRIGGER..o::LEFT_TRIGGER + o::TRIGGER_LEN].to_vec(),
            });
        }
    }
    // Audio region bytes 5..=10. Flags: bit0 = haptics-select (flag0 BIT1, also set on every
    // SDL rumble — so it alone must not emit), bits1..4 = flag0 bits 4..7. Emit if an
    // audio-valid bit is set or the region is non-zero; [`crate::hidout_dedup`] collapses repeats.
    let raw: [u8; 6] = data[5..11].try_into().unwrap();
    if flag0 & 0xF0 != 0 || raw != [0u8; 6] {
        let flags = ((flag0 >> 1) & 0x01) | ((flag0 >> 3) & 0x1E);
        fb.hidout.push(HidOutput::AudioCtl {
            pad: pad.into(),
            flags,
            raw,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feature blobs match hid-playstation request sizes: calibration 41, pairing 20, firmware 64.
    /// A USB backend rejects a longer reply as a malicious URB and tears down the device.
    #[test]
    fn feature_reports_are_exactly_the_size_the_driver_requests() {
        assert_eq!(
            DS_FEATURE_CALIBRATION.len(),
            41,
            "calibration (report 0x05)"
        );
        assert_eq!(DS_FEATURE_PAIRING.len(), 20, "pairing (report 0x09)");
        assert_eq!(DS_FEATURE_FIRMWARE.len(), 64, "firmware (report 0x20)");
        assert_eq!(ds_pairing_reply(0).len(), 20, "pairing reply");

        // Report id is byte 0; a wrong id is answered to the wrong GET_REPORT.
        assert_eq!(DS_FEATURE_CALIBRATION[0], 0x05);
        assert_eq!(DS_FEATURE_PAIRING[0], 0x09);
        assert_eq!(DS_FEATURE_FIRMWARE[0], 0x20);
    }

    /// Steam surfaces split the DualSense pad: left → contact 0 / left half, right → contact 1
    /// / right half; y is screen-top = 0 (no flip); a pad click sets the serialized click bit.
    #[test]
    fn steam_surfaces_split_the_touchpad() {
        let mut s = DsState::neutral();
        // Left pad, centre → middle of the LEFT half.
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 1,
                finger: 0,
                touch: true,
                click: false,
                x: 0,
                y: 0,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert!(s.touch[0].active);
        assert_eq!(s.touch[0].id, 0);
        assert_eq!(s.touch[0].x, (DS_TOUCH_W / 2 - 1) / 2); // centre of 0..=959
        assert_eq!(s.touch[0].y, (DS_TOUCH_H - 1) / 2);
        // Right pad, top-right corner → right edge of the RIGHT half, y = 0 (screen top).
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 2,
                finger: 0,
                touch: true,
                click: true,
                x: i16::MAX,
                y: i16::MIN,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert!(s.touch[1].active);
        assert_eq!(s.touch[1].id, 1);
        assert_eq!(s.touch[1].x, DS_TOUCH_W - 1);
        assert_eq!(s.touch[1].y, 0);
        assert!(s.touch_click[1]);
        assert_eq!(s.buttons2_with_click() & btn2::TOUCHPAD, btn2::TOUCHPAD);
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &s, 0, 0);
        assert_eq!(r[10] & btn2::TOUCHPAD, btn2::TOUCHPAD);
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 2,
                finger: 0,
                touch: true,
                click: false,
                x: 0,
                y: 0,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(s.buttons2_with_click() & btn2::TOUCHPAD, 0);
    }

    #[test]
    fn single_surface_spans_full_pad() {
        let mut s = DsState::neutral();
        s.apply_rich(
            RichInput::Touchpad {
                pad: 0,
                finger: 0,
                active: true,
                x: 65535,
                y: 65535,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(
            (s.touch[0].x, s.touch[0].y),
            (DS_TOUCH_W - 1, DS_TOUCH_H - 1)
        );
        s.apply_rich(
            RichInput::TouchpadEx {
                pad: 0,
                surface: 0,
                finger: 1,
                touch: true,
                click: false,
                x: i16::MAX,
                y: i16::MAX,
                pressure: 0,
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(
            (s.touch[1].x, s.touch[1].y),
            (DS_TOUCH_W - 1, DS_TOUCH_H - 1)
        );
        // Motion is unit-passthrough (wire is already DualSense convention).
        s.apply_rich(
            RichInput::Motion {
                pad: 0,
                gyro: [100, -200, 300],
                accel: [-1000, 2000, -3000],
            },
            DS_TOUCH_W,
            DS_TOUCH_H,
        );
        assert_eq!(s.gyro, [100, -200, 300]);
        assert_eq!(s.accel, [-1000, 2000, -3000]);
    }

    /// Full valid-flags `0x02` → rumble, lightbar, player LEDs, both trigger blocks.
    /// Report right-trigger-first maps to wire `which` 0 = L2, 1 = R2.
    #[test]
    fn parse_output_report() {
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x0F; // valid_flag0: vibration + haptics + R2 + L2
        data[2] = 0x14; // valid_flag1: lightbar + player indicators
        data[3] = 0x80; // right (high-freq) motor
        data[4] = 0x40; // left (low-freq) motor
        data[11] = 0x21; // right-trigger mode
        data[22] = 0x26; // left-trigger mode
        data[44] = 0x03; // player LEDs
        data[45] = 10;
        data[46] = 20;
        data[47] = 30;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        // (low, high) = (left<<8, right<<8).
        assert_eq!(fb.rumble, Some((0x4000, 0x8000)));
        assert!(fb.hidout.contains(&HidOutput::Led {
            pad: 0,
            r: 10,
            g: 20,
            b: 30
        }));
        assert!(fb
            .hidout
            .contains(&HidOutput::PlayerLeds { pad: 0, bits: 3 }));
        // The report's FIRST block (bytes 11..22) is the RIGHT trigger → wire which = 1.
        let triggers: Vec<_> = fb
            .hidout
            .iter()
            .filter_map(|h| match h {
                HidOutput::Trigger { which, effect, .. } => Some((*which, effect[0])),
                _ => None,
            })
            .collect();
        assert_eq!(triggers, vec![(1, 0x21), (0, 0x26)]);
    }

    /// The whole path a game drives: arm a Weapon effect on R2, pull through the stop, ease off.
    /// Status rides the HIGH nibble (0 before the effect, 1 inside, 2 past the stop) over the stop
    /// zone. Zero here is the bug this models — the game never sees the shot.
    #[test]
    fn weapon_effect_reports_the_shot_in_the_trigger_status_nibble() {
        // Weapon (0x25), start zone 2, stop zone 8 — Nielk1's factory packs both as a zone mask.
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x04; // valid_flag0: R2 block only
        data[11] = 0x25;
        data[12..14].copy_from_slice(&((1u16 << 2) | (1 << 8)).to_le_bytes());
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        let mut trig = DsTriggers::default();
        trig.observe(&fb.hidout);

        let byte = |t: &mut DsTriggers, r2| {
            let mut r = [0u8; DS_INPUT_REPORT_LEN];
            serialize_state(&mut r, &DsState::neutral(), 0, 0);
            t.stamp(&mut r, 0, r2);
            (r[42], r[43])
        };
        assert_eq!(
            byte(&mut trig, 0),
            (0x08, 0x09),
            "released: before the effect"
        );
        assert_eq!(
            byte(&mut trig, 0x60),
            (0x18, 0x09),
            "held inside the effect"
        );
        assert_eq!(
            byte(&mut trig, 0xFF),
            (0x28, 0x09),
            "past the stop: shot fired"
        );
        assert_eq!(
            byte(&mut trig, 0x60),
            (0x28, 0x09),
            "eased off: still fired"
        );
        assert_eq!(byte(&mut trig, 0x10), (0x08, 0x09), "released: re-armed");
        // Re-arming the same effect every frame must not clear the latch mid-pull.
        let _ = byte(&mut trig, 0xFF);
        trig.observe(&fb.hidout);
        assert_eq!(
            byte(&mut trig, 0x60),
            (0x28, 0x09),
            "re-sent effect keeps the shot"
        );
    }

    /// No effect armed reads the end-of-travel rest value a real pad sends; a mode the firmware
    /// does not report on reads zero.
    #[test]
    fn only_the_official_modes_move_the_status_nibble() {
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &DsState::neutral(), 0, 0);
        DsTriggers::default().stamp(&mut r, 0xFF, 0xFF);
        assert_eq!((r[42], r[43]), (0x09, 0x09), "nothing armed");

        let mut trig = DsTriggers::default();
        trig.observe(&[HidOutput::Trigger {
            pad: 0,
            which: 0,
            effect: vec![0x02, 0x20, 0xFF, 0x00, 0, 0, 0, 0, 0, 0, 0], // Simple_Weapon
        }]);
        trig.stamp(&mut r, 0xFF, 0);
        assert_eq!(r[43], 0, "simple modes never move it");
    }

    /// Arming a Weapon effect under a trigger that is ALREADY past the stop reports the shot at
    /// once. The firmware's rule is positional — "2 after until again before the start position"
    /// — so there is no edge to wait for, and adding one would swallow the first shot of every
    /// game that arms its effect while the player is holding the trigger down.
    #[test]
    fn a_weapon_effect_armed_under_a_held_trigger_reports_the_shot() {
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x04; // valid_flag0: R2 block
        data[11] = 0x25;
        data[12..14].copy_from_slice(&((1u16 << 2) | (1 << 8)).to_le_bytes());
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        let mut trig = DsTriggers::default();
        trig.observe(&fb.hidout);

        let byte = |t: &mut DsTriggers, r2| {
            let mut r = [0u8; DS_INPUT_REPORT_LEN];
            serialize_state(&mut r, &DsState::neutral(), 0, 0);
            t.stamp(&mut r, 0, r2);
            r[42]
        };
        assert_eq!(byte(&mut trig, 0xFF), 0x28, "armed under a held trigger");
        assert_eq!(byte(&mut trig, 0x10), 0x08, "released past the start zone");
        assert_eq!(byte(&mut trig, 0xFF), 0x28, "and it fires again");
    }

    /// Valid-flags gate: rumble-only must not emit hidout; LED-only must not surface rumble.
    #[test]
    fn parse_output_respects_valid_flags() {
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x03; // compatible vibration + haptics select
        data[3] = 0xFF;
        data[4] = 0xFF;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert_eq!(fb.rumble, Some((0xFF00, 0xFF00)));
        assert!(fb.hidout.is_empty(), "rumble write must not emit hidout");

        // Lightbar-only: no rumble (would otherwise spam rumble-stops).
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[2] = 0x04; // lightbar control enable
        data[45] = 1;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert!(fb.rumble.is_none());
        assert_eq!(fb.hidout.len(), 1);
        assert!(matches!(fb.hidout[0], HidOutput::Led { r: 1, .. }));

        // Vibration flag set, motors zero → `Some((0, 0))`, not absence. Distinguishes "game
        // stopped the motors" from "this report says nothing about rumble"; `rumble_drove`
        // keys on that.
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0x03; // compatible vibration + haptics select
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert_eq!(fb.rumble, Some((0, 0)));
    }

    /// `hid-playstation` lights the mic LED with `valid_flag1` BIT0 and `mute_button_led` at
    /// byte 9. Without the flag, the same byte is stale audio state, not an LED write.
    #[test]
    fn mic_led_rides_valid_flag1_bit0() {
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[2] = 0x01; // valid_flag1: mic-mute LED
        data[9] = 0x02; // pulse
        let mut fb = DsFeedback::default();
        parse_ds_output(3, &data, &mut fb);
        assert!(
            fb.rumble.is_none(),
            "an LED write says nothing about rumble"
        );
        assert!(fb.hidout.contains(&HidOutput::MicLed { pad: 3, mode: 2 }));

        data[2] = 0;
        let mut fb = DsFeedback::default();
        parse_ds_output(3, &data, &mut fb);
        assert!(!fb
            .hidout
            .iter()
            .any(|h| matches!(h, HidOutput::MicLed { .. })));
    }

    /// SDL's `RumbleJoystick(0, 0)` sends report `0x02` with every byte zero: no vibration
    /// flag, no LED flag (`SDL_hidapi_ps5.c` `UpdateEffects`). The pad drops rumble emulation
    /// on it, so it must read as a stop, or the motors run until the idle force-off.
    #[test]
    fn an_sdl_stop_report_stops_the_motors() {
        let mut stop = vec![0u8; 48];
        stop[0] = 0x02;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &stop, &mut fb);
        assert_eq!(fb.rumble, Some((0, 0)));
        assert!(fb.hidout.is_empty(), "a stop is not an LED or audio write");

        // The Edge's 63-byte output report carries the same header.
        let mut edge = vec![0u8; 63];
        edge[0] = 0x02;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &edge, &mut fb);
        assert_eq!(fb.rumble, Some((0, 0)));

        // SDL's enhanced-rumble write (fw ≥ 2.24): valid_flag2 BIT2 + haptics select.
        let mut on = vec![0u8; 48];
        on[0] = 0x02;
        on[1] = 0x02;
        on[39] = 0x04;
        on[3] = 0x40;
        on[4] = 0x80;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &on, &mut fb);
        assert_eq!(fb.rumble, Some((0x8000, 0x4000)));
    }

    /// A kernel lightbar or player-LED update carries no vibration flag. It must not read as
    /// a stop, or every LED change mid-effect would cut the rumble.
    #[test]
    fn an_led_only_report_leaves_rumble_alone() {
        for (flag1, what) in [(0x04, "lightbar"), (0x10, "player LEDs")] {
            let mut data = vec![0u8; 48];
            data[0] = 0x02;
            data[2] = flag1;
            let mut fb = DsFeedback::default();
            parse_ds_output(0, &data, &mut fb);
            assert_eq!(fb.rumble, None, "{what}-only report");
        }
        // hid-playstation's probe-time lightbar setup rides valid_flag2 BIT1 alone.
        let mut setup = vec![0u8; 48];
        setup[0] = 0x02;
        setup[39] = 0x02;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &setup, &mut fb);
        assert_eq!(fb.rumble, None, "lightbar-setup report");
    }

    /// Sensor/touch bytes match `struct dualsense_input_report` (gyro 15, accel 21, timestamp
    /// 27, touch 32; report byte = struct offset + 1). A one-byte slip is noise / phantom touch.
    #[test]
    fn input_report_layout_matches_hid_playstation() {
        let mut st = DsState::neutral();
        st.gyro = [0x1122, 0x3344, 0x5566];
        st.accel = [0x778, 0x99A, 0xBBC];
        st.touch[0] = Touch {
            active: true,
            id: 5,
            x: 0x123,
            y: 0x356,
        };
        // touch[1] stays inactive — its NOT-active bit must be set.
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &st, 7, 0xAABBCCDD);
        assert_eq!(r[0], 0x01);
        assert_eq!(r[7], 7); // seq_number (struct off 6)
        assert_eq!(&r[16..22], &[0x22, 0x11, 0x44, 0x33, 0x66, 0x55]); // gyro LE
        assert_eq!(&r[22..28], &[0x78, 0x07, 0x9A, 0x09, 0xBC, 0x0B]); // accel LE
        assert_eq!(&r[28..32], &[0xDD, 0xCC, 0xBB, 0xAA]); // sensor_timestamp LE
                                                           // Touch point 1 at struct off 32 = r[33..37]: contact byte (active → bit7 clear),
                                                           // then 12-bit x / 12-bit y packed.
        assert_eq!(r[33], 5);
        assert_eq!(r[34], 0x23);
        assert_eq!(r[35], 0x61); // x_hi nibble 0x1 | (y & 0xF) << 4 (y=0x356 → 0x6 << 4)
        assert_eq!(r[36], 0x35); // y >> 4
        assert_eq!(r[37] & 0x80, 0x80); // touch point 2 inactive
        assert_eq!(r[32], 0x14, "IMU temperature");
        assert_eq!(
            (r[53], r[54]),
            (0x2A, 0x18),
            "charge complete, USB data + power"
        );
    }

    /// Centre encodes as `DsState::neutral` on both axes. `255 - v` after quantise puts Y at
    /// 0x7F; invert in i16 first. Extremes stay exact.
    #[test]
    fn centred_sticks_encode_as_neutral_on_every_axis() {
        let n = DsState::neutral();
        let s = DsState::from_gamepad(0, 0, 0, 0, 0, 0, 0);
        assert_eq!((s.lx, s.ly), (n.lx, n.ly), "left stick centre");
        assert_eq!((s.rx, s.ry), (n.rx, n.ry), "right stick centre");

        // Y is still inverted (XInput +y = up, DualSense 0 = up) and both ends stay exact.
        let up = DsState::from_gamepad(0, 0, i16::MAX, 0, i16::MAX, 0, 0);
        assert_eq!((up.ly, up.ry), (0, 0), "full up = 0");
        let down = DsState::from_gamepad(0, 0, i16::MIN, 0, i16::MIN, 0, 0);
        assert_eq!((down.ly, down.ry), (255, 255), "full down = 255");

        let right = DsState::from_gamepad(0, i16::MAX, 0, i16::MAX, 0, 0, 0);
        assert_eq!((right.lx, right.rx), (255, 255));
        let left = DsState::from_gamepad(0, i16::MIN, 0, i16::MIN, 0, 0, 0);
        assert_eq!((left.lx, left.rx), (0, 0));
    }

    /// Wire touchpad-click / guide / mute land in `buttons[2]`.
    #[test]
    fn from_gamepad_maps_touchpad_click() {
        use punktfunk_core::input::gamepad as gs;
        let s = DsState::from_gamepad(gs::BTN_TOUCHPAD | gs::BTN_GUIDE, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], btn2::PS | btn2::TOUCHPAD);
        let s = DsState::from_gamepad(gs::BTN_MISC1, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], btn2::MUTE);
        let s = DsState::from_gamepad(gs::BTN_A, 0, 0, 0, 0, 0, 0);
        assert_eq!(s.buttons[2], 0);
    }

    /// PADDLE1/2 (R4/L4) → BACK right/left, PADDLE3/4 (R5/L5) → Fn right/left
    /// (`DS_EDGE_BUTTONS_*` / `SDL_GAMEPAD_BUTTON_PS5_*`). Serialized at report byte 10.
    #[test]
    fn edge_paddles_map_to_native_bits() {
        use punktfunk_core::input::gamepad as gs;
        assert_eq!(edge_paddle_bits(0), 0);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE1), btn2::EDGE_BACK_RIGHT);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE2), btn2::EDGE_BACK_LEFT);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE3), btn2::EDGE_FN_RIGHT);
        assert_eq!(edge_paddle_bits(gs::BTN_PADDLE4), btn2::EDGE_FN_LEFT);
        // Exact kernel/SDL bit values (a one-bit slip ships dead paddles).
        assert_eq!(btn2::EDGE_FN_LEFT, 0x10);
        assert_eq!(btn2::EDGE_FN_RIGHT, 0x20);
        assert_eq!(btn2::EDGE_BACK_LEFT, 0x40);
        assert_eq!(btn2::EDGE_BACK_RIGHT, 0x80);
        // All four + a non-paddle bit: paddles map, the rest is ignored here.
        let all = gs::BTN_PADDLE1 | gs::BTN_PADDLE2 | gs::BTN_PADDLE3 | gs::BTN_PADDLE4 | gs::BTN_A;
        assert_eq!(edge_paddle_bits(all), 0xF0);
        // Merge ORs into buttons[2]; byte 10 carries paddles and PS together.
        let mut s = DsState::from_gamepad(gs::BTN_GUIDE, 0, 0, 0, 0, 0, 0);
        s.buttons[2] |= edge_paddle_bits(gs::BTN_PADDLE2 | gs::BTN_PADDLE3);
        let mut r = [0u8; DS_INPUT_REPORT_LEN];
        serialize_state(&mut r, &s, 0, 0);
        assert_eq!(r[10], btn2::PS | btn2::EDGE_BACK_LEFT | btn2::EDGE_FN_RIGHT);
    }

    /// Edge descriptor length and the three deltas vs [`DUALSENSE_RDESC`]: output `0x02` count
    /// 63, feature `0xF2` count 52, appended profile reports. Input `0x01` prefix is identical.
    #[test]
    fn edge_descriptor_shape() {
        assert_eq!(DUALSENSE_RDESC.len(), 273);
        assert_eq!(DUALSENSE_EDGE_RDESC.len(), 389);
        // First delta: output `0x02` Report Count at offset 109 (47 → 63 payload bytes).
        assert_eq!(DUALSENSE_EDGE_RDESC[..109], DUALSENSE_RDESC[..109]);
        assert_eq!(DUALSENSE_RDESC[109], 0x2F);
        assert_eq!(DUALSENSE_EDGE_RDESC[109], 0x3F);
        assert_eq!(*DUALSENSE_EDGE_RDESC.last().unwrap(), 0xC0);
    }

    /// Audio-valid flags + volume bytes surface `AudioCtl` with condensed flags. A non-zero
    /// region with no audio-valid bits still surfaces (dedup collapses repeats).
    #[test]
    fn parse_output_surfaces_audio_ctl() {
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[1] = 0xB2; // flag0: haptics-select (BIT1) + audio-valid bits 4/5/7
        data[5] = 0x50; // headphone volume
        data[6] = 0x60; // speaker volume
        data[7] = 0x70; // mic volume
        data[8] = 0x05; // audio routing / enable bits
        let mut fb = DsFeedback::default();
        parse_ds_output(3, &data, &mut fb);
        // flags: bit0 = flag0 bit1, bits1..4 = flag0 bits 4..7 (0b1011 → 0b10110).
        assert_eq!(
            fb.hidout,
            vec![HidOutput::AudioCtl {
                pad: 3,
                flags: 0b1_0111,
                raw: [0x50, 0x60, 0x70, 0x05, 0x00, 0x00],
            }]
        );
        // Non-zero audio region, no audio-valid flags: still surface. Writers leave stale
        // volumes gated off; the host wants the bytes. Dedup collapses repeats.
        let mut data = vec![0u8; 48];
        data[0] = 0x02;
        data[9] = 0x01;
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &data, &mut fb);
        assert_eq!(
            fb.hidout,
            vec![HidOutput::AudioCtl {
                pad: 0,
                flags: 0,
                raw: [0, 0, 0, 0, 0x01, 0],
            }]
        );
    }

    /// A Bluetooth `0x31` frame, built at literal offsets, parses like the matching `0x02`.
    /// 64 bytes is what libScePad writes to the Edge; 49 is one short of the lightbar.
    #[test]
    fn bluetooth_output_report_matches_usb() {
        let mut usb = vec![0u8; 48];
        usb[0] = 0x02;
        let mut bt = vec![0u8; 64];
        bt[0] = 0x31;
        bt[1] = 0x30; // sequence 3, tag nibble 0
        bt[2] = 0x10; // tag
        for (u, b, v) in [
            (1, 3, 0xFF),  // valid_flag0: everything
            (2, 4, 0x14),  // valid_flag1: lightbar + player indicators
            (3, 5, 0x80),  // right motor
            (4, 6, 0x40),  // left motor
            (9, 11, 0x02), // audio region
            (11, 13, 0x21),
            (22, 24, 0x26),
            (44, 46, 0x05),
            (45, 47, 10),
            (46, 48, 20),
            (47, 49, 30),
        ] {
            usb[u] = v;
            bt[b] = v;
        }
        let (mut from_usb, mut from_bt) = (DsFeedback::default(), DsFeedback::default());
        parse_ds_output(0, &usb, &mut from_usb);
        parse_ds_output(0, &bt, &mut from_bt);
        assert_eq!(from_bt.rumble, Some((0x4000, 0x8000)));
        assert_eq!(from_bt.rumble, from_usb.rumble);
        assert_eq!(from_bt.hidout.len(), 5);
        assert_eq!(from_bt.hidout, from_usb.hidout);

        let mut fb = DsFeedback::default();
        parse_ds_output(0, &bt[..49], &mut fb);
        assert!(fb.rumble.is_none());
        assert!(fb.hidout.is_empty());
    }

    #[test]
    fn parse_output_rejects_garbage() {
        let mut fb = DsFeedback::default();
        parse_ds_output(0, &[0x01, 0, 0], &mut fb);
        assert!(fb.rumble.is_none());
        assert!(fb.hidout.is_empty());
    }

    /// Pairing replies keep report id `0x09` and differ only in the MAC low octet (SDL/Steam uniq).
    #[test]
    fn pairing_reply_mac_is_per_pad() {
        assert_eq!(ds_pairing_reply(0).as_slice(), DS_FEATURE_PAIRING);
        let (a, b) = (ds_pairing_reply(1), ds_pairing_reply(2));
        assert_eq!(a[0], 0x09);
        assert_eq!(a[1], DS_FEATURE_PAIRING[1].wrapping_add(1));
        assert_eq!(b[1], DS_FEATURE_PAIRING[1].wrapping_add(2));
        assert_eq!(a[2..], b[2..]);
    }
}
