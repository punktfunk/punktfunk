//! 8BitDo pads in their own HID mode (VID `2DC8`): the input report SDL's and Steam's `8bitdo`
//! driver parse and its rumble output. Identities, descriptors and the capability feature are
//! [`pf_driver_proto::eightbitdo`]'s. Shared by Linux UHID and Windows UMDF.
//!
//! Report `0x01`, [`REPORT_LEN`] bytes: hat (`0x0F` = centre), four sticks and two triggers as
//! `u8` (sticks centre `0x7F`), three button bytes, battery, accel then gyro as LE `i16`
//! (4096 LSB/g, ±2000 °/s full scale), and a µs IMU clock on the models that declare one.
//! Offsets follow `SDL_hidapi_8bitdo.c`; `tests/motion_contract.rs` pins the units.

use pf_driver_proto::eightbitdo as wire;
pub use pf_driver_proto::eightbitdo::{
    caps_reply, FEATURE_CAPS, REPORT_ID, REPORT_LEN, RUMBLE_ID, VENDOR,
};
use pf_driver_proto::gamepad::{
    identity_vid_pid, report_period_us, DEVTYPE_8BITDO_PRO2, DEVTYPE_8BITDO_PRO3,
    DEVTYPE_8BITDO_ULTIMATE2,
};
use punktfunk_core::input::{gamepad as gs, GamepadFrame};
use punktfunk_core::quic::RichInput;
use std::time::Duration;

const ACCEL_LSB_PER_G: i32 = 4096;
/// `INT16_MAX` is 2000 °/s.
const GYRO_FULL_SCALE_DPS: i32 = 2000;

/// Byte 8. Face bits are the driver's positional slots; the Pro models swap them (see
/// [`Model::nintendo_labels`]).
mod b8 {
    pub const SOUTH: u8 = 0x01;
    pub const EAST: u8 = 0x02;
    pub const PR: u8 = 0x04;
    pub const WEST: u8 = 0x08;
    pub const NORTH: u8 = 0x10;
    pub const PL: u8 = 0x20;
    pub const LB: u8 = 0x40;
    pub const RB: u8 = 0x80;
}

/// Byte 9.
mod b9 {
    pub const BACK: u8 = 0x04;
    pub const START: u8 = 0x08;
    pub const GUIDE: u8 = 0x10;
    pub const LS: u8 = 0x20;
    pub const RS: u8 = 0x40;
}

/// Byte 10.
mod b10 {
    pub const L4: u8 = 0x01;
    pub const R4: u8 = 0x02;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Ultimate2,
    Pro2,
    Pro3,
}

impl Model {
    /// The driver's identity for this model; product id, descriptor and clock follow from it.
    pub const fn devtype(self) -> u8 {
        match self {
            Model::Ultimate2 => DEVTYPE_8BITDO_ULTIMATE2,
            Model::Pro2 => DEVTYPE_8BITDO_PRO2,
            Model::Pro3 => DEVTYPE_8BITDO_PRO3,
        }
    }

    pub const fn product(self) -> u16 {
        match identity_vid_pid(self.devtype()) {
            Some((_, pid)) => pid,
            None => 0,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Model::Ultimate2 => "8BitDo Ultimate 2 Wireless",
            Model::Pro2 => "8BitDo Pro 2",
            Model::Pro3 => "8BitDo Pro 3",
        }
    }

    /// The Ultimate 2 presents over Bluetooth: Steam binds its paddles there, and SDL assumes
    /// 120 Hz for a pad with no IMU clock.
    pub const fn bluetooth(self) -> bool {
        matches!(self, Model::Ultimate2)
    }

    /// Pro 2 and Pro 3 answer [`FEATURE_CAPS`] and carry the IMU clock.
    pub const fn timestamps(self) -> bool {
        wire::has_caps(self.devtype())
    }

    /// Fixed report rate for a model without the IMU clock: SDL stamps each sample one step
    /// apart whatever the arrival rate, so it gets the driver's period.
    pub const fn report_period(self) -> Option<Duration> {
        if self.timestamps() {
            None
        } else {
            Some(Duration::from_micros(report_period_us(self.devtype())))
        }
    }

    /// SDL's mapping for the Pro models is `a:b1,b:b0,x:b3,y:b2`: they report by label, so
    /// wire south lands on the east bit and wire west on the north bit.
    const fn nintendo_labels(self) -> bool {
        !matches!(self, Model::Ultimate2)
    }

    /// Wire `PADDLE1/2/3/4` (R4/L4/R5/L5) → `(byte, bit)`. SDL maps the Pro 2's back pair as
    /// paddle 1/2; the others carry L4/R4 there and the back pair as paddle 3/4.
    const fn paddles(self) -> [Option<(usize, u8)>; 4] {
        match self {
            Model::Pro2 => [Some((8, b8::PR)), Some((8, b8::PL)), None, None],
            Model::Ultimate2 | Model::Pro3 => [
                Some((10, b10::R4)),
                Some((10, b10::L4)),
                Some((8, b8::PR)),
                Some((8, b8::PL)),
            ],
        }
    }

    /// The UHID descriptor; the Windows driver serves the same with its channel feature.
    pub fn rdesc(self) -> &'static [u8] {
        wire::rdesc(self.devtype(), false)
    }
}

/// One pad's report state. Motion is raw driver units, driver axis order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EightBitDoState {
    pub hat: u8,
    /// LX, LY, RX, RY; `0x7F` is centre, Y grows downward.
    pub sticks: [u8; 4],
    pub rt: u8,
    pub lt: u8,
    /// Bytes 8, 9 and 10.
    pub buttons: [u8; 3],
    pub accel: [i16; 3],
    pub gyro: [i16; 3],
}

impl EightBitDoState {
    /// Centred, unpressed, 1 g on +Z (the pad flat on a table).
    pub fn neutral() -> EightBitDoState {
        EightBitDoState {
            hat: 0x0F,
            sticks: [0x7F; 4],
            rt: 0,
            lt: 0,
            buttons: [0; 3],
            accel: [0, 0, ACCEL_LSB_PER_G as i16],
            gyro: [0; 3],
        }
    }

    /// A button/stick frame over `prev`, keeping its motion from the rich plane.
    pub fn merge_frame(model: Model, prev: &EightBitDoState, f: &GamepadFrame) -> EightBitDoState {
        let on = |bit: u32| f.buttons & bit != 0;
        let mut b = [0u8; 3];
        let (south, east, west, north) = if model.nintendo_labels() {
            (b8::EAST, b8::SOUTH, b8::NORTH, b8::WEST)
        } else {
            (b8::SOUTH, b8::EAST, b8::WEST, b8::NORTH)
        };
        for (bit, byte, mask) in [
            (gs::BTN_A, 0, south),
            (gs::BTN_B, 0, east),
            (gs::BTN_X, 0, west),
            (gs::BTN_Y, 0, north),
            (gs::BTN_LB, 0, b8::LB),
            (gs::BTN_RB, 0, b8::RB),
            (gs::BTN_BACK, 1, b9::BACK),
            (gs::BTN_START, 1, b9::START),
            (gs::BTN_GUIDE, 1, b9::GUIDE),
            (gs::BTN_LS_CLICK, 1, b9::LS),
            (gs::BTN_RS_CLICK, 1, b9::RS),
        ] {
            if on(bit) {
                b[byte] |= mask;
            }
        }
        let paddles = [
            gs::BTN_PADDLE1,
            gs::BTN_PADDLE2,
            gs::BTN_PADDLE3,
            gs::BTN_PADDLE4,
        ];
        for (bit, slot) in paddles.into_iter().zip(model.paddles()) {
            if let (true, Some((byte, mask))) = (on(bit), slot) {
                b[byte - 8] |= mask;
            }
        }
        EightBitDoState {
            hat: hat(f.buttons),
            // SDL passes Y through: down is positive. The wire is up-positive.
            sticks: [
                stick(f.ls_x as i32),
                stick(-(f.ls_y as i32)),
                stick(f.rs_x as i32),
                stick(-(f.rs_y as i32)),
            ],
            rt: f.right_trigger,
            lt: f.left_trigger,
            buttons: b,
            accel: prev.accel,
            gyro: prev.gyro,
        }
    }

    /// IMU samples only; no touchpad.
    pub fn apply_rich(&mut self, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            self.apply_motion(gyro, accel);
        }
    }

    /// Wire sample (SDL's frame: pitch, yaw, roll) → the driver's axes. SDL reads the pad as
    /// `(-y, z, -x)` for both sensors; this is its inverse.
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        let g = |v: i32| {
            (v * i16::MAX as i32 / (GYRO_FULL_SCALE_DPS * gs::MOTION_GYRO_LSB_PER_DEG_S))
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16
        };
        let a = |v: i32| {
            (v * ACCEL_LSB_PER_G / gs::MOTION_ACCEL_LSB_PER_G)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16
        };
        let [p, y, r] = gyro.map(i32::from);
        self.gyro = [g(-r), g(-p), g(y)];
        let [ax, ay, az] = accel.map(i32::from);
        self.accel = [a(-az), a(-ax), a(ay)];
    }

    /// Zero gyro only. Gravity stays. True iff the sample changed.
    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    /// Motion only — this pad has no touchpad.
    pub fn clear_rich(&mut self) {
        let fresh = EightBitDoState::neutral();
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// The input report. `clock` is the IMU µs tick, for the models that declare it.
    pub fn serialize(&self, clock: Option<u32>) -> [u8; REPORT_LEN] {
        let mut r = [0u8; REPORT_LEN];
        r[0] = REPORT_ID;
        r[1] = self.hat;
        r[2..6].copy_from_slice(&self.sticks);
        r[6] = self.rt;
        r[7] = self.lt;
        r[8..11].copy_from_slice(&self.buttons);
        // Battery: bit 7 charging, low 7 bits percent. 100 reads as charged.
        r[14] = 100;
        for (i, v) in self.accel.iter().chain(&self.gyro).enumerate() {
            r[15 + 2 * i..17 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        if let Some(t) = clock {
            r[27..31].copy_from_slice(&t.to_le_bytes());
        }
        r
    }
}

/// D-pad bits → hat 0..7 clockwise from up, `0x0F` centred. Opposite directions cancel.
/// The HORIPAD uses the same encoding.
pub(crate) fn hat(buttons: u32) -> u8 {
    let up = buttons & gs::BTN_DPAD_UP != 0;
    let down = buttons & gs::BTN_DPAD_DOWN != 0;
    let left = buttons & gs::BTN_DPAD_LEFT != 0;
    let right = buttons & gs::BTN_DPAD_RIGHT != 0;
    let v = up && !down;
    let d = down && !up;
    let l = left && !right;
    let r = right && !left;
    match (v, r, d, l) {
        (true, false, _, false) => 0,
        (true, true, _, _) => 1,
        (false, true, false, _) => 2,
        (_, true, true, _) => 3,
        (false, false, true, false) => 4,
        (_, _, true, true) => 5,
        (false, _, false, true) => 6,
        (true, _, _, true) => 7,
        _ => 0x0F,
    }
}

/// Wire axis → the `u8` SDL reads back as `raw * 257 - 32768`. 0 lands on `0x7F`, which SDL
/// treats as exact centre.
fn stick(v: i32) -> u8 {
    ((v.clamp(-32768, 32767) + 32768) * 255 / 65535) as u8
}

/// Output `0x05` → `(low, high)` on the 0xCA plane's `0..=0xFFFF` scale.
pub fn parse_rumble(data: &[u8]) -> Option<(u16, u16)> {
    match data {
        [RUMBLE_ID, low, high, ..] => Some((*low as u16 * 257, *high as u16 * 257)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(buttons: u32) -> GamepadFrame {
        GamepadFrame {
            buttons,
            ..Default::default()
        }
    }

    fn state(model: Model, buttons: u32) -> EightBitDoState {
        EightBitDoState::merge_frame(model, &EightBitDoState::neutral(), &frame(buttons))
    }

    /// The idle report matches a real Ultimate 2's: `01 0F 7F 7F 7F 7F`, then zeros to the battery.
    #[test]
    fn neutral_report_matches_the_pad_at_rest() {
        let r = state(Model::Ultimate2, 0).serialize(None);
        assert_eq!(r.len(), 34);
        assert_eq!(r[..8], [0x01, 0x0F, 0x7F, 0x7F, 0x7F, 0x7F, 0, 0]);
        assert_eq!(r[8..14], [0; 6]);
        // Accel Z = 4096 (1 g), everything else still.
        assert_eq!(r[19..21], 4096i16.to_le_bytes());
    }

    /// Face buttons land where SDL's mapping reads south/east/west/north per model.
    #[test]
    fn face_buttons_follow_each_models_mapping() {
        let u = |b| state(Model::Ultimate2, b).buttons[0];
        assert_eq!(u(gs::BTN_A), b8::SOUTH);
        assert_eq!(u(gs::BTN_B), b8::EAST);
        assert_eq!(u(gs::BTN_X), b8::WEST);
        assert_eq!(u(gs::BTN_Y), b8::NORTH);
        // Pro 2: `a:b1,b:b0,x:b3,y:b2`.
        let p = |b| state(Model::Pro2, b).buttons[0];
        assert_eq!(p(gs::BTN_A), b8::EAST);
        assert_eq!(p(gs::BTN_B), b8::SOUTH);
        assert_eq!(p(gs::BTN_X), b8::NORTH);
        assert_eq!(p(gs::BTN_Y), b8::WEST);
    }

    /// Ultimate 2 / Pro 3: `paddle1:b12 (R4), paddle2:b11 (L4), paddle3:b14 (PR), paddle4:b13 (PL)`.
    /// Pro 2: `paddle1:b14 (PR), paddle2:b13 (PL)`.
    #[test]
    fn paddles_land_on_the_bits_sdl_names() {
        let s = |m, b| state(m, b).buttons;
        assert_eq!(s(Model::Ultimate2, gs::BTN_PADDLE1), [0, 0, b10::R4]);
        assert_eq!(s(Model::Ultimate2, gs::BTN_PADDLE2), [0, 0, b10::L4]);
        assert_eq!(s(Model::Pro3, gs::BTN_PADDLE3), [b8::PR, 0, 0]);
        assert_eq!(s(Model::Pro3, gs::BTN_PADDLE4), [b8::PL, 0, 0]);
        assert_eq!(s(Model::Pro2, gs::BTN_PADDLE1), [b8::PR, 0, 0]);
        assert_eq!(s(Model::Pro2, gs::BTN_PADDLE2), [b8::PL, 0, 0]);
        assert_eq!(
            s(Model::Pro2, gs::BTN_PADDLE3 | gs::BTN_PADDLE4),
            [0; 3],
            "the Pro 2 has two paddles"
        );
    }

    #[test]
    fn menu_buttons_and_hat() {
        let s = state(
            Model::Ultimate2,
            gs::BTN_BACK | gs::BTN_START | gs::BTN_GUIDE | gs::BTN_DPAD_UP | gs::BTN_DPAD_RIGHT,
        );
        assert_eq!(s.buttons[1], b9::BACK | b9::START | b9::GUIDE);
        assert_eq!(s.hat, 1);
        assert_eq!(hat(gs::BTN_DPAD_LEFT), 6);
        assert_eq!(hat(gs::BTN_DPAD_UP | gs::BTN_DPAD_DOWN), 0x0F);
    }

    /// SDL reads a stick byte as `raw * 257 - 32768`, Y down-positive.
    #[test]
    fn sticks_invert_what_sdl_reads() {
        let f = GamepadFrame {
            ls_x: 32767,
            ls_y: 32767,
            rs_x: -32767,
            ..Default::default()
        };
        let s = EightBitDoState::merge_frame(Model::Ultimate2, &EightBitDoState::neutral(), &f);
        assert_eq!(s.sticks, [0xFF, 0x00, 0x00, 0x7F]);
        let sdl = |raw: u8| raw as i32 * 257 - 32768;
        assert!(sdl(s.sticks[1]) < -32000, "wire up must read as SDL up");
    }

    /// SDL: gyro `(-Y, Z, -X) × 2000°/s / 32767`, accel `(-Y, Z, -X) / 4096`.
    #[test]
    fn motion_is_the_inverse_of_sdls_rotation() {
        let mut s = EightBitDoState::neutral();
        // 100 °/s pitch, 1 g along SDL +y (flat, face up).
        s.apply_motion(
            [100 * gs::MOTION_GYRO_LSB_PER_DEG_S as i16, 0, 0],
            [0, gs::MOTION_ACCEL_LSB_PER_G as i16, 0],
        );
        let pitch = -(s.gyro[1] as f64) * 2000.0 / 32767.0;
        assert!((pitch - 100.0).abs() < 0.1, "{pitch} °/s");
        assert_eq!(s.accel, [0, 0, 4096]);
    }

    #[test]
    fn rumble_and_caps() {
        assert_eq!(
            parse_rumble(&[0x05, 0xFF, 0x80, 0, 0]),
            Some((0xFFFF, 0x8080))
        );
        assert_eq!(parse_rumble(&[0x04, 1, 2]), None);
        let c = caps_reply(Model::Pro2.devtype(), 3);
        assert_eq!((c[0], c[10], c[13]), (FEATURE_CAPS, 0x50, 0xAA));
    }

    /// The clock rides bytes 27–30 only when given.
    #[test]
    fn clock_is_stamped_when_declared() {
        let s = EightBitDoState::neutral();
        assert_eq!(s.serialize(None)[27..31], [0; 4]);
        assert_eq!(s.serialize(Some(0x0102_0304))[27..31], [4, 3, 2, 1]);
    }

    /// Each report's declared length is what the codec writes and SDL reads: hidclass sizes
    /// its buffers from the descriptor, and SDL enables gyro only for a 34-byte report.
    #[test]
    fn descriptor_declares_the_report_sizes() {
        use pf_driver_proto::rdesc::{report_lens, FEATURE, INPUT, OUTPUT};
        for m in [Model::Ultimate2, Model::Pro2, Model::Pro3] {
            let d = m.rdesc();
            assert_eq!(
                d[..6],
                [0x05, 0x01, 0x09, 0x05, 0xA1, 0x01],
                "Game Pad collection"
            );
            let lens = report_lens(d);
            assert_eq!(lens[&(INPUT, REPORT_ID)], REPORT_LEN);
            assert_eq!(lens[&(OUTPUT, RUMBLE_ID)], 5);
        }
        let caps = report_lens(Model::Pro2.rdesc())[&(FEATURE, FEATURE_CAPS)];
        assert_eq!(caps, caps_reply(Model::Pro2.devtype(), 0).len());
        assert_eq!(
            report_lens(Model::Ultimate2.rdesc()).get(&(FEATURE, FEATURE_CAPS)),
            None
        );
    }
}
