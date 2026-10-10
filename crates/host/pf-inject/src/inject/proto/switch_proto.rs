//! Nintendo Switch Pro Controller and Joy-Con pair state mapping and feedback parsing for the
//! host backends (Linux UHID, Windows UMDF). The descriptor, `0x30` layout and handshake replies
//! live in `pf_driver_proto::switch`, which the Windows driver serves from too. A pair is one
//! [`SwitchState`] served as two halves ([`serialize_joycon_0x30`]).
//!
//! Face buttons are positional (wire south → report B). Wire motion is SDL's frame in
//! DualSense units (20 LSB/°·s, 10000 LSB/g); the report is raw Pro axes and units
//! (14.247 LSB/°·s, 4096 LSB/g) via the factory-calibration identity. Evidence: this module's
//! tests and hid-nintendo.c.

use pf_driver_proto::switch::{self as wire, STICK_CENTER, STICK_RANGE};
use punktfunk_core::input::{gamepad as gs, GamepadFrame};
use punktfunk_core::quic::RichInput;

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
    /// Right Joy-Con's rail buttons.
    pub const SR_R: u32 = 1 << 4;
    pub const SL_R: u32 = 1 << 5;
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
    /// Left Joy-Con's rail buttons.
    pub const SR_L: u32 = 1 << 20;
    pub const SL_L: u32 = 1 << 21;
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

    /// Fold a button/stick frame over `prev`, keeping its motion from the rich plane.
    /// `buttons` is the frame's after the paddle fold.
    pub fn merge_frame(prev: &SwitchState, f: &GamepadFrame, buttons: u32) -> SwitchState {
        SwitchState {
            gyro: prev.gyro,
            accel: prev.accel,
            ..SwitchState::from_gamepad(
                buttons,
                f.ls_x,
                f.ls_y,
                f.rs_x,
                f.rs_y,
                f.left_trigger,
                f.right_trigger,
            )
        }
    }

    /// [`SwitchState::merge_frame`] for a Joy-Con pair, whose rail buttons carry the paddles.
    pub fn merge_joycon_frame(prev: &SwitchState, f: &GamepadFrame) -> SwitchState {
        let mut st = SwitchState::merge_frame(prev, f, f.buttons);
        for (wire, bit) in JOYCON_PADDLES {
            if f.buttons & wire != 0 {
                st.buttons |= bit;
            }
        }
        st
    }

    /// IMU samples only; a Pro Controller has no touchpad.
    pub fn apply_rich(&mut self, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            self.apply_motion(gyro, accel);
        }
    }

    /// Zero gyro only. Gravity stays. True iff the sample changed (`PadState::neutralize_gyro`).
    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    /// Motion only — this pad has no touchpad (`PadState::clear_rich`).
    pub fn clear_rich(&mut self) {
        let fresh = SwitchState::neutral();
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// Wire sample (SDL's frame: pitch, yaw, roll) → the pad's raw axes. SDL reads a Switch
    /// IMU as `(-y, z, -x)` for both sensors; this is its inverse, and the raw frame
    /// `hid-nintendo` reads too.
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        let gyro_den = 1000 * gs::MOTION_GYRO_LSB_PER_DEG_S;
        let g = |v: i32| (v * JC_IMU_GYRO_MILLI_RES_PER_DPS / gyro_den) as i16;
        let a = |v: i32| (v * JC_IMU_ACCEL_RES_PER_G / gs::MOTION_ACCEL_LSB_PER_G) as i16;
        let [p, y, r] = gyro.map(i32::from);
        self.gyro = [g(-r), g(-p), g(y)];
        let [ax, ay, az] = accel.map(i32::from);
        self.accel = [a(-az), a(-ax), a(ay)];
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

/// Wire paddles → rail buttons, in SDL's pair order: PADDLE1/3 are the right half's SR/SL,
/// PADDLE2/4 the left half's SL/SR.
const JOYCON_PADDLES: [(u32, u32); 4] = [
    (gs::BTN_PADDLE1, btn::SR_R),
    (gs::BTN_PADDLE2, btn::SL_L),
    (gs::BTN_PADDLE3, btn::SL_R),
    (gs::BTN_PADDLE4, btn::SR_L),
];

/// One half of a Joy-Con pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    Left,
    Right,
}

impl Half {
    /// The half `device_type` names, `None` for the Pro Controller.
    pub const fn of(device_type: u8) -> Option<Half> {
        match device_type {
            pf_driver_proto::gamepad::DEVTYPE_JOYCON_LEFT => Some(Half::Left),
            pf_driver_proto::gamepad::DEVTYPE_JOYCON_RIGHT => Some(Half::Right),
            _ => None,
        }
    }

    pub const fn device_type(self) -> u8 {
        match self {
            Half::Left => pf_driver_proto::gamepad::DEVTYPE_JOYCON_LEFT,
            Half::Right => pf_driver_proto::gamepad::DEVTYPE_JOYCON_RIGHT,
        }
    }

    pub const fn product(self) -> u32 {
        match self {
            Half::Left => 0x2006,
            Half::Right => 0x2007,
        }
    }

    /// This half's rumble level from a decoded `(side 0, side 1)` packet: each half drives the
    /// side it sits on, as `hid-nintendo` and SDL both write them.
    pub const fn rumble(self, (left, right): (u16, u16)) -> u16 {
        match self {
            Half::Left => left,
            Half::Right => right,
        }
    }
}

/// Report `0x30` as `half` sends it: its own buttons and stick, the other stick centred. The
/// right half's IMU is mounted turned over, so its y and z read negated; SDL flips them back.
pub fn serialize_joycon_0x30(st: &SwitchState, half: Half, timer: u8) -> [u8; wire::REPORT_LEN] {
    const LEFT: u32 = btn::MINUS | btn::LSTICK | btn::CAPTURE | 0xFF_0000;
    const RIGHT: u32 = 0xFF | btn::PLUS | btn::RSTICK | btn::HOME;
    let c = STICK_CENTER;
    let over = |v: [i16; 3]| [v[0], v[1].saturating_neg(), v[2].saturating_neg()];
    let (buttons, sticks, accel, gyro) = match half {
        Half::Left => (st.buttons & LEFT, [st.lx, st.ly, c, c], st.accel, st.gyro),
        Half::Right => (
            st.buttons & RIGHT,
            [c, c, st.rx, st.ry],
            over(st.accel),
            over(st.gyro),
        ),
    };
    wire::state_report(timer, buttons, sticks, accel, gyro)
}

/// The `0x30` report `device_type` serves for `st`: a Joy-Con half's or the Pro's.
pub fn serialize_for(device_type: u8, st: &SwitchState, timer: u8) -> [u8; wire::REPORT_LEN] {
    match Half::of(device_type) {
        Some(half) => serialize_joycon_0x30(st, half, timer),
        None => serialize_report_0x30(st, timer),
    }
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

/// Invert one side, taking the louder band. High band: the even bits of byte 1 are the table
/// index × 2 (freq is bit 0 only). Low band: byte 3 is `0x40` + index / 2, byte 2's top bit the
/// odd step. SDL writes a Joy-Con's one motor into a single band, so reading one band loses it.
fn side_amplitude(side: &[u8]) -> u16 {
    let high = (side[1] & 0xFE) / 2;
    let low = side[3].saturating_sub(0x40).saturating_mul(2) | (side[2] >> 7);
    let idx = high.max(low) as usize;
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

    /// Wire 20 LSB/°·s, 10000 LSB/g → raw 14.247 LSB/°·s, 4096 LSB/g, on the axes SDL reads
    /// back as the wire's: raw `(-roll, -pitch, yaw)`. Gravity at rest (+y) lands on raw +z.
    #[test]
    fn motion_units() {
        let mut st = SwitchState::neutral();
        // 100 °/s = wire 2000 → raw ≈ 1424; 1 g = wire 10000 → raw 4096.
        st.apply_motion([2000, 0, -2000], [10000, -10000, 0]);
        assert_eq!(st.gyro, [1424, -1424, 0]);
        assert_eq!(st.accel, [0, -4096, -4096]);
        st.apply_motion([0; 3], [0, 10000, 0]);
        assert_eq!(st.accel, SwitchState::neutral().accel);
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

    /// SDL rumbles a pair's left half on the low band only and its right half on the high band
    /// only, the same bytes on both sides of each packet.
    #[test]
    fn joycon_rumble_reads_either_band() {
        // Low band at index 100: byte 3 = 0x40 + 50, byte 2 = freq 0x3D with no odd step.
        let left = [0x74, 0x00, 0x3D, 0x72, 0x74, 0x00, 0x3D, 0x72];
        assert_eq!(Half::Left.rumble(decode_rumble(&left)), 65535);
        // Odd low-band step: index 17 → 123.
        let odd = [0x74, 0x00, 0xBD, 0x48, 0x74, 0x00, 0xBD, 0x48];
        assert_eq!(decode_rumble(&odd).0, (123u32 * 65535 / 1003) as u16);
        let right = [0x74, 0xC8, 0x3D, 0x40, 0x74, 0xC8, 0x3D, 0x40];
        assert_eq!(Half::Right.rumble(decode_rumble(&right)), 65535);
    }

    /// Each half carries only its own controls. The paddles land on the rail buttons SDL reads as
    /// the pair's paddles, and the right half's IMU reads turned over.
    #[test]
    fn joycon_halves_split_one_state() {
        let f = GamepadFrame {
            buttons: gs::BTN_A
                | gs::BTN_DPAD_UP
                | gs::BTN_BACK
                | gs::BTN_START
                | gs::BTN_PADDLE1
                | gs::BTN_PADDLE2
                | gs::BTN_PADDLE3
                | gs::BTN_PADDLE4,
            ls_x: 32767,
            rs_y: -32767,
            ..GamepadFrame::default()
        };
        let mut st = SwitchState::merge_joycon_frame(&SwitchState::neutral(), &f);
        st.apply_motion([2000, 1000, -500], [0, 10000, 0]);
        let l = serialize_joycon_0x30(&st, Half::Left, 1);
        let r = serialize_joycon_0x30(&st, Half::Right, 1);
        let bits = |rep: &[u8; 64]| u32::from_le_bytes([rep[3], rep[4], rep[5], 0]);
        assert_eq!(bits(&l), btn::UP | btn::MINUS | btn::SL_L | btn::SR_L);
        assert_eq!(bits(&r), btn::B | btn::PLUS | btn::SR_R | btn::SL_R);
        assert_eq!(
            l[6..9],
            wire::pack12(STICK_CENTER + STICK_RANGE, STICK_CENTER)
        );
        assert_eq!(l[9..12], wire::pack12(STICK_CENTER, STICK_CENTER));
        assert_eq!(r[6..9], wire::pack12(STICK_CENTER, STICK_CENTER));
        assert_eq!(
            r[9..12],
            wire::pack12(STICK_CENTER, STICK_CENTER - STICK_RANGE)
        );
        let imu = |rep: &[u8; 64], at: usize| i16::from_le_bytes([rep[at], rep[at + 1]]);
        // accel x/y/z at 13/15/17, gyro at 19/21/23.
        let axes = |rep: &[u8; 64]| [17, 19, 21, 23].map(|at| imu(rep, at));
        assert_eq!(axes(&l), [4096, 356, -1424, 712]);
        assert_eq!(axes(&r), [-4096, 356, 1424, -712]);
        assert_eq!(serialize_for(Half::Right.device_type(), &st, 1), r);
    }

    /// Solid and flashing nibbles both count as lit.
    #[test]
    fn player_lights() {
        assert_eq!(player_leds_bits(0x01), 0b0001);
        assert_eq!(player_leds_bits(0x10), 0b0001); // flashing LED 1
        assert_eq!(player_leds_bits(0x23), 0b0011 | 0b0010);
    }
}
