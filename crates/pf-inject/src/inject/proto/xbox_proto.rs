//! Xbox Wireless Controller HID codec — the byte-exact input report `pf-gamepad` serves under
//! `device_type = 4` ([`pf_driver_proto::gamepad::DEVTYPE_XBOX`]).
//!
//! `pf-xusb` registers only `GUID_DEVINTERFACE_XUSB` and has no HID collection, so Steam hidapi,
//! DirectInput, `joy.cpl`, and WGI/GameInput never see it — only classic `XInputGetState` does.
//!
//! The report is `XBOX_RDESC` in order: two 16-bit stick pairs (`X`/`Y`, `Rx`/`Ry`, 0..65535),
//! two 16-bit Simulation-page triggers (`Brake`/`Accelerator`, 0..1023), a 4-bit null-state hat
//! plus 4 pad bits, then 15 buttons plus 1 pad bit. [`serialize_xbox_state`] writes that layout;
//! [`tests`] pin every field.
//!
//! Button numbers are the real Xbox-Bluetooth layout, gaps included. We enumerate as Microsoft
//! `045E:0B13`; SDL / Steam / Windows stock mappings key off that VID/PID. Renumbering silently
//! lands every control on the wrong action. Reserved slots 3, 6, 9, 10 are Microsoft's — leave
//! them empty.

use punktfunk_core::input::gamepad as gs;

/// Report id included. Must equal the driver's `XBOX_INPUT_REPORT_LEN`: hidclass sizes
/// READ_REPORT from the descriptor, and `copy_to_output` refuses a longer source rather than
/// truncating.
pub const XBOX_REPORT_LEN: usize = 16;

const REPORT_ID: u8 = 0x01;

/// Midpoint of the descriptor's 0..65535 stick axis.
const STICK_CENTRE: u16 = 0x8000;

/// Descriptor 10-bit trigger full scale (0..1023).
const TRIGGER_MAX: u32 = 1023;

// HID button N is bit (N-1), LSB-first in bytes 14..15. Real Xbox-Bluetooth numbers; slots
// 3, 6, 9, 10 stay empty (Microsoft reserved).
const BIT_A: u8 = 0;
const BIT_B: u8 = 1;
const BIT_X: u8 = 3;
const BIT_Y: u8 = 4;
const BIT_LB: u8 = 6;
const BIT_RB: u8 = 7;
const BIT_VIEW: u8 = 10; // Back/Select
const BIT_MENU: u8 = 11; // Start
const BIT_GUIDE: u8 = 12; // Xbox button
const BIT_LS: u8 = 13;
const BIT_RS: u8 = 14;

/// Wire conventions: sticks −32768..32767 with **+y = up**, triggers 0..255, [`gs`] `BTN_*`.
/// [`serialize_xbox_state`] converts to HID.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XboxState {
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

impl XboxState {
    #[allow(clippy::too_many_arguments)]
    pub fn from_gamepad(
        buttons: u32,
        left_trigger: u8,
        right_trigger: u8,
        ls_x: i16,
        ls_y: i16,
        rs_x: i16,
        rs_y: i16,
    ) -> XboxState {
        XboxState {
            buttons,
            left_trigger,
            right_trigger,
            ls_x,
            ls_y,
            rs_x,
            rs_y,
        }
    }
}

fn axis_x(v: i16) -> u16 {
    (v as i32 + 32768) as u16
}

/// Wire +y is UP; HID `Y`/`Ry` grow down. Invert, or the look stick aims the wrong way.
///
/// Signed i16 has no exact midpoint: `axis_x(0)` is 32768 and `axis_y(0)` is 32767. Endpoints
/// stay exact (full up → 0, full down → 65535). Do not recentre on 32768 — that costs an endpoint.
fn axis_y(v: i16) -> u16 {
    65535 - axis_x(v)
}

/// 0..255 → 0..1023, rounded (`+ 127`) so a fully-held trigger is exactly full scale, not 1020.
fn trigger(v: u8) -> u16 {
    ((v as u32 * TRIGGER_MAX + 127) / 255) as u16
}

/// `0` is NULL (logical min is 1), then 1..8 clockwise from North. Opposing presses cancel.
fn hat(buttons: u32) -> u8 {
    let up = buttons & gs::BTN_DPAD_UP != 0;
    let down = buttons & gs::BTN_DPAD_DOWN != 0;
    let left = buttons & gs::BTN_DPAD_LEFT != 0;
    let right = buttons & gs::BTN_DPAD_RIGHT != 0;
    let (up, down) = if up && down {
        (false, false)
    } else {
        (up, down)
    };
    let (left, right) = if left && right {
        (false, false)
    } else {
        (left, right)
    };
    match (up, right, down, left) {
        (true, false, false, false) => 1, // N
        (true, true, false, false) => 2,  // NE
        (false, true, false, false) => 3, // E
        (false, true, true, false) => 4,  // SE
        (false, false, true, false) => 5, // S
        (false, false, true, true) => 6,  // SW
        (false, false, false, true) => 7, // W
        (true, false, false, true) => 8,  // NW
        _ => 0,                           // nothing held → NULL
    }
}

fn button_bits(buttons: u32) -> (u8, u8) {
    let mut bits: u16 = 0;
    for (mask, bit) in [
        (gs::BTN_A, BIT_A),
        (gs::BTN_B, BIT_B),
        (gs::BTN_X, BIT_X),
        (gs::BTN_Y, BIT_Y),
        (gs::BTN_LB, BIT_LB),
        (gs::BTN_RB, BIT_RB),
        (gs::BTN_BACK, BIT_VIEW),
        (gs::BTN_START, BIT_MENU),
        (gs::BTN_GUIDE, BIT_GUIDE),
        (gs::BTN_LS_CLICK, BIT_LS),
        (gs::BTN_RS_CLICK, BIT_RS),
    ] {
        if buttons & mask != 0 {
            bits |= 1 << bit;
        }
    }
    (bits as u8, (bits >> 8) as u8)
}

pub fn serialize_xbox_state(s: &XboxState) -> [u8; XBOX_REPORT_LEN] {
    let mut r = [0u8; XBOX_REPORT_LEN];
    r[0] = REPORT_ID;
    r[1..3].copy_from_slice(&axis_x(s.ls_x).to_le_bytes());
    r[3..5].copy_from_slice(&axis_y(s.ls_y).to_le_bytes());
    r[5..7].copy_from_slice(&axis_x(s.rs_x).to_le_bytes());
    r[7..9].copy_from_slice(&axis_y(s.rs_y).to_le_bytes());
    r[9..11].copy_from_slice(&trigger(s.left_trigger).to_le_bytes());
    r[11..13].copy_from_slice(&trigger(s.right_trigger).to_le_bytes());
    r[13] = hat(s.buttons); // low nibble; the high nibble is descriptor padding
    let (lo, hi) = button_bits(s.buttons);
    r[14] = lo;
    r[15] = hi;
    r
}

/// At-rest pose. Must match the driver's `XBOX_NEUTRAL_REPORT` (see `neutral_matches_a_zeroed_state`).
pub fn neutral_xbox_report() -> [u8; XBOX_REPORT_LEN] {
    serialize_xbox_state(&XboxState::default())
}

/// Xbox Bluetooth rumble report → `(low, high, left_trigger, right_trigger)` on 0..65535.
///
/// Report `0x03`: `[id, enable, left_trigger, right_trigger, strong, weak, duration, delay,
/// loop]`. Magnitudes are 0..100, not 0..255; reading them as 0..255 costs 60 % of the range.
/// `enable` bit 0 = weak (right handle, `high`), bit 1 = strong (left handle, `low`), bit 2 =
/// right trigger, bit 3 = left trigger, as Linux `hid-microsoft` and xpadneo write them. A clear
/// bit reads as a stopped motor. `None` for anything but a rumble report.
pub fn parse_xbox_output(bytes: &[u8]) -> Option<(u16, u16, u16, u16)> {
    // The driver republishes output reports report-id-prefixed, like the PS backends.
    if bytes.len() < 6 || bytes[0] != 0x03 {
        return None;
    }
    let enable = bytes[1];
    let scale = |v: u8| -> u16 { (v.min(100) as u32 * 65535 / 100) as u16 };
    let gated = |bit: u8, v: u8| if enable & bit != 0 { scale(v) } else { 0 };
    Some((
        gated(0x02, bytes[4]),
        gated(0x01, bytes[5]),
        gated(0x08, bytes[2]),
        gated(0x04, bytes[3]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rumble_scales_off_the_zero_to_hundred_protocol_range() {
        let full = [0x03, 0x0F, 0, 0, 100, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&full), Some((65535, 65535, 0, 0)));
        let half = [0x03, 0x03, 0, 0, 50, 100, 0, 0, 1];
        assert_eq!(parse_xbox_output(&half), Some((32767, 65535, 0, 0)));
    }

    /// Trigger magnitudes share the handles' 0..100 range. A full-scale `100`
    /// is `65535`, not `25700` (the 0..255 misread).
    #[test]
    fn trigger_magnitudes_are_not_a_zero_to_255_range() {
        let full = [0x03, 0xFF, 100, 100, 0, 0, 0, 0, 1];
        assert_eq!(parse_xbox_output(&full), Some((0, 0, 65535, 65535)));
        let half = [0x03, 0xFF, 50, 25, 0, 0, 0, 0, 1];
        assert_eq!(parse_xbox_output(&half), Some((0, 0, 32767, 16383)));
    }

    /// Values above 0..100 clamp, not wrap — all four actuators share the
    /// scale closure.
    #[test]
    fn out_of_range_magnitudes_clamp() {
        let over = [0x03, 0xFF, 255, 255, 255, 255, 0, 0, 1];
        assert_eq!(parse_xbox_output(&over), Some((65535, 65535, 65535, 65535)));
    }

    /// One bit per actuator, as `hid-microsoft` (`ENABLE_WEAK` = bit 0, `ENABLE_STRONG` =
    /// bit 1) and xpadneo (`XBOX_RUMBLE_RIGHT` = bit 2, `XBOX_RUMBLE_LEFT` = bit 3) define them.
    /// Every actuator carries a distinct magnitude, so a swapped bit names the wrong motor.
    #[test]
    fn each_enable_bit_drives_its_own_actuator() {
        let report = |enable: u8| [0x03, enable, 10, 20, 30, 40, 0, 0, 1];
        let s = |v: u32| (v * 65535 / 100) as u16;
        assert_eq!(
            parse_xbox_output(&report(0x01)),
            Some((0, s(40), 0, 0)),
            "weak"
        );
        assert_eq!(
            parse_xbox_output(&report(0x02)),
            Some((s(30), 0, 0, 0)),
            "strong"
        );
        assert_eq!(
            parse_xbox_output(&report(0x04)),
            Some((0, 0, 0, s(20))),
            "right trigger"
        );
        assert_eq!(
            parse_xbox_output(&report(0x08)),
            Some((0, 0, s(10), 0)),
            "left trigger"
        );
        assert_eq!(parse_xbox_output(&report(0x00)), Some((0, 0, 0, 0)), "none");
    }

    /// `hid-microsoft`'s `ms_ff_worker` sends `enable = 0x03` with only the handle magnitudes.
    /// That is a handle-only rumble, never a trigger one.
    #[test]
    fn the_kernel_handle_report_leaves_the_triggers_silent() {
        let kernel = [0x03, 0x03, 0, 0, 80, 20, 0xFF, 0, 0xFF];
        assert_eq!(
            parse_xbox_output(&kernel),
            Some((
                (80u32 * 65535 / 100) as u16,
                (20u32 * 65535 / 100) as u16,
                0,
                0
            ))
        );
    }

    #[test]
    fn non_rumble_reports_are_ignored() {
        assert_eq!(parse_xbox_output(&[0x01, 0x0F, 0, 0, 100, 100]), None);
        assert_eq!(parse_xbox_output(&[0x03, 0x0F, 0]), None);
        assert_eq!(parse_xbox_output(&[]), None);
    }

    fn le(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }

    /// Offsets `XBOX_RDESC` declares. Fail means one side moved.
    #[test]
    fn the_report_matches_the_descriptor_layout() {
        let s = XboxState::from_gamepad(0, 0, 0, 0, 0, 0, 0);
        let r = serialize_xbox_state(&s);
        assert_eq!(r.len(), XBOX_REPORT_LEN, "16 bytes: id + 8 + 4 + 1 + 2");
        assert_eq!(r[0], 0x01, "report id");
    }

    #[test]
    fn sticks_span_the_full_unsigned_axis() {
        let full = XboxState::from_gamepad(0, 0, 0, i16::MIN, i16::MIN, i16::MAX, i16::MAX);
        let r = serialize_xbox_state(&full);
        assert_eq!(le(&r[1..3]), 0, "LX at hard left = 0");
        assert_eq!(le(&r[5..7]), 65535, "RX at hard right = 65535");
    }

    #[test]
    fn the_y_axes_are_inverted_into_hid_convention() {
        let up = XboxState::from_gamepad(0, 0, 0, 0, i16::MAX, 0, i16::MAX);
        let r = serialize_xbox_state(&up);
        assert_eq!(le(&r[3..5]), 0, "stick fully UP is 0 in HID");
        assert_eq!(le(&r[7..9]), 0, "right stick too");

        let down = XboxState::from_gamepad(0, 0, 0, 0, i16::MIN, 0, i16::MIN);
        let r = serialize_xbox_state(&down);
        assert_eq!(le(&r[3..5]), 65535, "stick fully DOWN is full scale");
        assert_eq!(le(&r[7..9]), 65535);
    }

    /// Upright axes sit on `STICK_CENTRE`; inverted ones one unit below (even-length range; see `axis_y`).
    #[test]
    fn a_centred_stick_reads_centred() {
        let r = neutral_xbox_report();
        assert_eq!(le(&r[1..3]), STICK_CENTRE, "LX");
        assert_eq!(le(&r[5..7]), STICK_CENTRE, "RX");
        assert_eq!(le(&r[3..5]), STICK_CENTRE - 1, "LY (inverted)");
        assert_eq!(le(&r[7..9]), STICK_CENTRE - 1, "RY (inverted)");
    }

    /// Truncating division stops at 1020; games with a "fully pressed" threshold then never fire.
    #[test]
    fn triggers_scale_to_full_ten_bit_range() {
        let none = serialize_xbox_state(&XboxState::default());
        assert_eq!(le(&none[9..11]), 0);
        assert_eq!(le(&none[11..13]), 0);

        let held = XboxState::from_gamepad(0, 255, 255, 0, 0, 0, 0);
        let r = serialize_xbox_state(&held);
        assert_eq!(le(&r[9..11]), 1023, "LT fully held = full scale");
        assert_eq!(le(&r[11..13]), 1023, "RT fully held = full scale");

        let half = XboxState::from_gamepad(0, 128, 0, 0, 0, 0, 0);
        let r = serialize_xbox_state(&half);
        assert_eq!(le(&r[9..11]), 514, "128/255 rounds to 514, not 513");
    }

    #[test]
    fn the_hat_walks_clockwise_from_north() {
        let cases = [
            (0, 0u8),
            (gs::BTN_DPAD_UP, 1),
            (gs::BTN_DPAD_UP | gs::BTN_DPAD_RIGHT, 2),
            (gs::BTN_DPAD_RIGHT, 3),
            (gs::BTN_DPAD_RIGHT | gs::BTN_DPAD_DOWN, 4),
            (gs::BTN_DPAD_DOWN, 5),
            (gs::BTN_DPAD_DOWN | gs::BTN_DPAD_LEFT, 6),
            (gs::BTN_DPAD_LEFT, 7),
            (gs::BTN_DPAD_UP | gs::BTN_DPAD_LEFT, 8),
        ];
        for (buttons, want) in cases {
            let r = serialize_xbox_state(&XboxState::from_gamepad(buttons, 0, 0, 0, 0, 0, 0));
            assert_eq!(r[13] & 0x0F, want, "buttons {buttons:#x}");
        }
    }

    /// A physical hat cannot report both; a game that sees "up" on up+down drifts.
    #[test]
    fn opposing_dpad_presses_cancel() {
        let ud = gs::BTN_DPAD_UP | gs::BTN_DPAD_DOWN;
        let r = serialize_xbox_state(&XboxState::from_gamepad(ud, 0, 0, 0, 0, 0, 0));
        assert_eq!(r[13] & 0x0F, 0);
        let lr = gs::BTN_DPAD_LEFT | gs::BTN_DPAD_RIGHT;
        let r = serialize_xbox_state(&XboxState::from_gamepad(lr, 0, 0, 0, 0, 0, 0));
        assert_eq!(r[13] & 0x0F, 0);
    }

    #[test]
    fn buttons_land_on_the_real_xbox_bluetooth_positions() {
        let cases: [(u32, usize, u8); 11] = [
            (gs::BTN_A, 14, 0),
            (gs::BTN_B, 14, 1),
            (gs::BTN_X, 14, 3),
            (gs::BTN_Y, 14, 4),
            (gs::BTN_LB, 14, 6),
            (gs::BTN_RB, 14, 7),
            (gs::BTN_BACK, 15, 2),
            (gs::BTN_START, 15, 3),
            (gs::BTN_GUIDE, 15, 4),
            (gs::BTN_LS_CLICK, 15, 5),
            (gs::BTN_RS_CLICK, 15, 6),
        ];
        for (mask, byte, bit) in cases {
            let r = serialize_xbox_state(&XboxState::from_gamepad(mask, 0, 0, 0, 0, 0, 0));
            assert_eq!(
                r[byte] & (1u8 << bit),
                1u8 << bit,
                "mask {mask:#x} should set byte {byte} bit {bit}"
            );
            let shift = bit as u16 + (byte as u16 - 14) * 8;
            let others = (r[14] as u16 | (r[15] as u16) << 8) & !(1u16 << shift);
            assert_eq!(others, 0, "mask {mask:#x} set a second button bit");
        }
    }

    /// Buttons 3, 6, 9, 10 and the trailing pad bit: a stray bit reads as a button the real pad lacks.
    #[test]
    fn reserved_button_slots_stay_empty() {
        let all = gs::BTN_A
            | gs::BTN_B
            | gs::BTN_X
            | gs::BTN_Y
            | gs::BTN_LB
            | gs::BTN_RB
            | gs::BTN_BACK
            | gs::BTN_START
            | gs::BTN_GUIDE
            | gs::BTN_LS_CLICK
            | gs::BTN_RS_CLICK;
        let r = serialize_xbox_state(&XboxState::from_gamepad(all, 0, 0, 0, 0, 0, 0));
        let bits = r[14] as u16 | (r[15] as u16) << 8;
        for reserved_bit in [2u8, 5, 8, 9, 15] {
            assert_eq!(
                bits & (1 << reserved_bit),
                0,
                "bit {reserved_bit} is reserved/padding and must stay clear"
            );
        }
    }

    /// Touchpad / capture / paddles have no Xbox HID slot — drop them, do not collide with a real button.
    #[test]
    fn unmappable_wire_buttons_are_dropped() {
        let extra = gs::BTN_TOUCHPAD | gs::BTN_MISC1 | gs::BTN_PADDLE1;
        let r = serialize_xbox_state(&XboxState::from_gamepad(extra, 0, 0, 0, 0, 0, 0));
        assert_eq!(r[14], 0);
        assert_eq!(r[15], 0);
        assert_eq!(r[13] & 0x0F, 0);
    }

    #[test]
    fn neutral_matches_a_zeroed_state() {
        assert_eq!(
            neutral_xbox_report(),
            serialize_xbox_state(&XboxState::default())
        );
        let r = neutral_xbox_report();
        assert_eq!(r[13], 0, "hat NULL");
        assert_eq!(r[14], 0);
        assert_eq!(r[15], 0);
        // Must match the driver's `XBOX_NEUTRAL_REPORT`; a drift is a different at-rest pose before the first frame.
        assert_eq!(r[0], 0x01);
        assert_eq!([r[1], r[2]], [0x00, 0x80], "LX = 0x8000");
        assert_eq!([r[3], r[4]], [0xFF, 0x7F], "LY = 0x7FFF (inverted centre)");
        assert_eq!([r[5], r[6]], [0x00, 0x80], "RX = 0x8000");
        assert_eq!([r[7], r[8]], [0xFF, 0x7F], "RY = 0x7FFF (inverted centre)");
    }
}
