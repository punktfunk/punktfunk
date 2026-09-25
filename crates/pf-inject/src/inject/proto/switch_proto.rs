//! Nintendo Switch Pro Controller state mapping and feedback parsing for the host backends
//! (Linux UHID, Windows UMDF). The descriptor, `0x30` layout and handshake replies live in
//! `pf_driver_proto::switch`, which the Windows driver serves from too.
//!
//! Face buttons are positional (wire south → report B). Wire motion is DualSense units
//! (20 LSB/°·s, 10000 LSB/g); the report is raw Pro units (14.247 LSB/°·s, 4096 LSB/g)
//! via the factory-calibration identity. Evidence: this module's tests and hid-nintendo.c.

use pf_driver_proto::switch::{self as wire, STICK_CENTER, STICK_RANGE};
use punktfunk_core::input::gamepad as gs;

pub const SWITCH_VENDOR: u32 = 0x057E; // Nintendo Co., Ltd
pub const SWITCH_PRODUCT: u32 = 0x2009; // Pro Controller

/// `JC_IMU_GYRO_RES_PER_DPS` in thousandths so 14.247 stays exact. Factory IMU cal is
/// the driver's identity default, so reports are consumed at this ratio.
const JC_IMU_GYRO_MILLI_RES_PER_DPS: i32 = 14_247;
/// `JC_IMU_ACCEL_RES_PER_G`. Same identity-cal path as gyro.
const JC_IMU_ACCEL_RES_PER_G: i32 = 4096;

// 24-bit LE button field (report bytes 3..6), `JC_BTN_*` in hid-nintendo.c.
pub mod btn {
    pub const Y: u32 = 1 << 0;
    pub const X: u32 = 1 << 1;
    pub const B: u32 = 1 << 2;
    pub const A: u32 = 1 << 3;
    pub const R: u32 = 1 << 6;
    pub const ZR: u32 = 1 << 7;
    pub const MINUS: u32 = 1 << 8;
    pub const PLUS: u32 = 1 << 9;
    pub const RSTICK: u32 = 1 << 10;
    pub const LSTICK: u32 = 1 << 11;
    pub const HOME: u32 = 1 << 12;
    pub const CAPTURE: u32 = 1 << 13;
    pub const DOWN: u32 = 1 << 16;
    pub const UP: u32 = 1 << 17;
    pub const RIGHT: u32 = 1 << 18;
    pub const LEFT: u32 = 1 << 19;
    pub const L: u32 = 1 << 22;
    pub const ZL: u32 = 1 << 23;
}

/// Raw 12-bit sticks ([`STICK_CENTER`]-based) and raw IMU units for report `0x30` / `0x21`.
#[derive(Clone, Copy)]
pub struct SwitchState {
    pub buttons: u32,
    pub lx: u16,
    pub ly: u16,
    pub rx: u16,
    pub ry: u16,
    /// Raw gyro (~14.247 LSB/°·s) and accel (4096 LSB/g), driver axis order x/y/z.
    pub gyro: [i16; 3],
    pub accel: [i16; 3],
}

impl SwitchState {
    /// Centered, unpressed, 1 g on +Z (pad at rest). Zero accel looks like free-fall.
    pub fn neutral() -> SwitchState {
        SwitchState {
            buttons: 0,
            lx: STICK_CENTER,
            ly: STICK_CENTER,
            rx: STICK_CENTER,
            ry: STICK_CENTER,
            gyro: [0; 3],
            accel: [0, 0, 4096],
        }
    }

    /// Positional face map (wire south → Switch B). Analog triggers become ZL/ZR.
    /// Fold paddles through [`super::steam_remap`] first — they have no Switch slot.
    pub fn from_gamepad(
        buttons: u32,
        lx: i16,
        ly: i16,
        rx: i16,
        ry: i16,
        lt: u8,
        rt: u8,
    ) -> SwitchState {
        let on = |bit: u32| buttons & bit != 0;
        let mut b = 0u32;
        if on(gs::BTN_A) {
            b |= btn::B; // south
        }
        if on(gs::BTN_B) {
            b |= btn::A; // east
        }
        if on(gs::BTN_X) {
            b |= btn::Y; // west
        }
        if on(gs::BTN_Y) {
            b |= btn::X; // north
        }
        if on(gs::BTN_LB) {
            b |= btn::L;
        }
        if on(gs::BTN_RB) {
            b |= btn::R;
        }
        if lt > 0 {
            b |= btn::ZL;
        }
        if rt > 0 {
            b |= btn::ZR;
        }
        if on(gs::BTN_BACK) {
            b |= btn::MINUS;
        }
        if on(gs::BTN_START) {
            b |= btn::PLUS;
        }
        if on(gs::BTN_LS_CLICK) {
            b |= btn::LSTICK;
        }
        if on(gs::BTN_RS_CLICK) {
            b |= btn::RSTICK;
        }
        if on(gs::BTN_GUIDE) {
            b |= btn::HOME;
        }
        if on(gs::BTN_MISC1) {
            b |= btn::CAPTURE;
        }
        if on(gs::BTN_DPAD_UP) {
            b |= btn::UP;
        }
        if on(gs::BTN_DPAD_DOWN) {
            b |= btn::DOWN;
        }
        if on(gs::BTN_DPAD_LEFT) {
            b |= btn::LEFT;
        }
        if on(gs::BTN_DPAD_RIGHT) {
            b |= btn::RIGHT;
        }
        SwitchState {
            buttons: b,
            lx: stick_raw(lx),
            ly: stick_raw(ly),
            rx: stick_raw(rx),
            ry: stick_raw(ry),
            ..SwitchState::neutral()
        }
    }

    /// Zero gyro only. Gravity stays. True iff the sample changed (`PadProto::neutralize_gyro`).
    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    /// Motion only — this pad has no touchpad (`PadProto::clear_rich`).
    pub fn clear_rich(&mut self) {
        let fresh = SwitchState::neutral();
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// DualSense-convention sample → raw IMU. No axis flip: the Pro path does not negate.
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        let gyro_den = 1000 * gs::MOTION_GYRO_LSB_PER_DEG_S;
        self.gyro = gyro.map(|v| ((v as i32 * JC_IMU_GYRO_MILLI_RES_PER_DPS) / gyro_den) as i16);
        self.accel = accel
            .map(|v| ((v as i32 * JC_IMU_ACCEL_RES_PER_G) / gs::MOTION_ACCEL_LSB_PER_G) as i16);
    }
}

/// Wire i16 (+ = right/up) → 12-bit raw. Driver Y-negates both conventions, so +y
/// is above-center, same as x.
pub fn stick_raw(v: i16) -> u16 {
    let raw = STICK_CENTER as i32 + (v as i32 * STICK_RANGE as i32) / 32767;
    raw.clamp(0, 0xFFF) as u16
}

/// Report `0x30` for `st`. The same IMU sample fills all three frames.
pub fn serialize_report_0x30(st: &SwitchState, timer: u8) -> [u8; wire::REPORT_LEN] {
    wire::state_report(
        timer,
        st.buttons,
        [st.lx, st.ly, st.rx, st.ry],
        st.accel,
        st.gyro,
    )
}

/// What an output report asks of the host. The pad's own answer is `wire::reply`.
pub enum SwitchOutput {
    /// `0x80 <cmd>`, a handshake command.
    UsbCmd(u8),
    /// `0x01`, rumble plus a subcommand.
    Subcmd {
        id: u8,
        args: Vec<u8>,
        rumble: (u16, u16),
    },
    /// `0x10` rumble-only — no reply.
    Rumble((u16, u16)),
}

pub fn parse_output(data: &[u8]) -> Option<SwitchOutput> {
    match *data.first()? {
        0x80 => Some(SwitchOutput::UsbCmd(*data.get(1)?)),
        0x01 if data.len() >= 11 => Some(SwitchOutput::Subcmd {
            id: data[10],
            args: data.get(11..).map(|s| s.to_vec()).unwrap_or_default(),
            rumble: decode_rumble(&data[2..10]),
        }),
        0x10 if data.len() >= 10 => Some(SwitchOutput::Rumble(decode_rumble(&data[2..10]))),
        _ => None,
    }
}

/// `joycon_rumble_amplitudes` amplitude column, indexed by `amp_high / 2`.
/// Last entry is `joycon_max_rumble_amp` (1003).
#[rustfmt::skip]
const RUMBLE_AMPS: [u16; 101] = [
       0,   10,   12,   14,   17,   20,   24,   28,   33,   40,
      47,   56,   67,   80,   95,  112,  117,  123,  128,  134,
     140,  146,  152,  159,  166,  173,  181,  189,  198,  206,
     215,  225,  230,  235,  240,  245,  251,  256,  262,  268,
     273,  279,  286,  292,  298,  305,  311,  318,  325,  332,
     340,  347,  355,  362,  370,  378,  387,  395,  404,  413,
     422,  431,  440,  450,  460,  470,  480,  491,  501,  512,
     524,  535,  547,  559,  571,  584,  596,  609,  623,  636,
     650,  665,  679,  694,  709,  725,  741,  757,  773,  790,
     808,  825,  843,  862,  881,  900,  920,  940,  960,  981,
    1003,
];

/// Invert one side: even bits of byte 1 are the table index × 2
/// (`data[1] = freq_high_lo + amp.high`; freq is bit 0 only).
fn side_amplitude(side: &[u8]) -> u16 {
    let idx = ((side[1] & 0xFE) / 2) as usize;
    let amp = RUMBLE_AMPS[idx.min(RUMBLE_AMPS.len() - 1)] as u32;
    // Driver: amp = magnitude * 1003 / 65535 — invert, saturating at full scale.
    ((amp * 65535) / 1003).min(65535) as u16
}

/// 8 rumble bytes → (low, high). Left = strong/low, right = weak/high (`joycon_play_effect`).
pub fn decode_rumble(bytes: &[u8]) -> (u16, u16) {
    if bytes.len() < 8 {
        return (0, 0);
    }
    (side_amplitude(&bytes[..4]), side_amplitude(&bytes[4..8]))
}

/// `(flash << 4) | on` → wire bits. A flashing LED counts as on.
pub fn player_leds_bits(arg: u8) -> u8 {
    (arg & 0x0F) | (arg >> 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire south/east/west/north → Switch B/A/Y/X (`JC_BTN_*` for the rest).
    #[test]
    fn positional_swap_and_button_bits() {
        let st = SwitchState::from_gamepad(gs::BTN_A, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::B);
        let st = SwitchState::from_gamepad(gs::BTN_B, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::A);
        let st = SwitchState::from_gamepad(gs::BTN_X, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::Y);
        let st = SwitchState::from_gamepad(gs::BTN_Y, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::X);
        let st = SwitchState::from_gamepad(
            gs::BTN_LB | gs::BTN_RB | gs::BTN_BACK | gs::BTN_START | gs::BTN_GUIDE | gs::BTN_MISC1,
            0,
            0,
            0,
            0,
            255,
            1,
        );
        assert_eq!(
            st.buttons,
            btn::L | btn::R | btn::MINUS | btn::PLUS | btn::HOME | btn::CAPTURE | btn::ZL | btn::ZR
        );
        let st = SwitchState::from_gamepad(gs::BTN_DPAD_UP | gs::BTN_DPAD_LEFT, 0, 0, 0, 0, 0, 0);
        assert_eq!(st.buttons, btn::UP | btn::LEFT);
    }

    /// Full deflection → `center ± range`. Driver Y-negation restores evdev negative-up.
    #[test]
    fn stick_scaling() {
        assert_eq!(stick_raw(0), STICK_CENTER);
        assert_eq!(stick_raw(32767), STICK_CENTER + STICK_RANGE);
        assert_eq!(stick_raw(-32767), STICK_CENTER - STICK_RANGE);
        assert!(stick_raw(i16::MIN) <= 0xFFF);
    }

    /// Wire 20 LSB/°·s, 10000 LSB/g → raw 14.247 LSB/°·s, 4096 LSB/g.
    #[test]
    fn motion_units() {
        let mut st = SwitchState::neutral();
        // 100 °/s = wire 2000 → raw ≈ 1424; 1 g = wire 10000 → raw 4096.
        st.apply_motion([2000, 0, -2000], [10000, -10000, 0]);
        assert_eq!(st.gyro, [1424, 0, -1424]);
        assert_eq!(st.accel, [4096, -4096, 0]);
    }

    /// Neutral → 0; max amp → 65535; left = low/strong, right = high/weak.
    #[test]
    fn rumble_decode() {
        // Neutral per the driver's tables: freq defaults + amp 0.
        let neutral = [0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40];
        assert_eq!(decode_rumble(&neutral), (0, 0));
        // Max amp (0xC8 → index 100 → 1003 → 65535) on the LEFT only → (low=full, high=0).
        let left_max = [0x00, 0xC8, 0x40, 0x72, 0x00, 0x01, 0x40, 0x40];
        assert_eq!(decode_rumble(&left_max), (65535, 0));
        // Mid-table on the right: amp_high 0x20 → index 16 → 117 → 117*65535/1003 = 7644.
        let right_mid = [0x00, 0x01, 0x40, 0x40, 0x00, 0x20, 0x48, 0x40];
        assert_eq!(decode_rumble(&right_mid), (0, 7644));
        // The freq bit riding data[1] bit0 must not disturb the amplitude index.
        let with_freq_bit = [0x00, 0x21, 0x48, 0x40, 0x00, 0x01, 0x40, 0x40];
        assert_eq!(decode_rumble(&with_freq_bit).0, 7644);
        // Short slice → silence, not a panic.
        assert_eq!(decode_rumble(&[0x10; 4]), (0, 0));
    }

    #[test]
    fn parse_output_shapes() {
        assert!(matches!(
            parse_output(&[0x80, 0x02]),
            Some(SwitchOutput::UsbCmd(0x02))
        ));
        let mut sub = vec![0x01, 0x05];
        sub.extend_from_slice(&[0x00, 0x01, 0x40, 0x40, 0x00, 0x01, 0x40, 0x40]);
        sub.push(0x10);
        sub.extend_from_slice(&[0x3D, 0x60, 0x00, 0x00, 0x09]);
        match parse_output(&sub) {
            Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                assert_eq!(id, 0x10);
                assert_eq!(&args[..5], &[0x3D, 0x60, 0x00, 0x00, 0x09]);
                assert_eq!(rumble, (0, 0));
            }
            _ => panic!("expected subcmd"),
        }
        let mut rum = vec![0x10, 0x06];
        rum.extend_from_slice(&[0x00, 0xC8, 0x40, 0x72, 0x00, 0x01, 0x40, 0x40]);
        assert!(matches!(
            parse_output(&rum),
            Some(SwitchOutput::Rumble((65535, 0)))
        ));
        assert!(parse_output(&[0x21]).is_none());
        assert!(parse_output(&[]).is_none());
    }

    /// Solid and flashing nibbles both count as lit.
    #[test]
    fn player_lights() {
        assert_eq!(player_leds_bits(0x01), 0b0001);
        assert_eq!(player_leds_bits(0x10), 0b0001); // flashing LED 1
        assert_eq!(player_leds_bits(0x23), 0b0011 | 0b0010);
    }
}
