//! Wireless HORIPAD for Steam (`0F0D:01AB`, wired identity): the report SDL's and Steam's
//! `steam_hori` driver parse. No output reports: the pad has no rumble. Identity, descriptor
//! and serial are [`pf_driver_proto::hori`]'s. Shared by Linux UHID and Windows UMDF.
//!
//! Report `0x07`, [`REPORT_LEN`] bytes: four sticks (`0x80` = centre), the hat in the low nibble
//! of byte 5 with face buttons above it, two more button bytes, triggers, a u16 clock SDL does
//! not trust, gyro then accel as LE `i16`, battery, and a six-byte serial at 38.
//! Offsets follow `SDL_hidapi_steam_hori.c`; `tests/motion_contract.rs` pins the units.

use punktfunk_core::input::{gamepad as gs, GamepadFrame};
use punktfunk_core::quic::RichInput;
use std::time::Duration;

pub use pf_driver_proto::hori::{serial, NAME, PRODUCT, RDESC, REPORT_ID, REPORT_LEN, VENDOR};
/// SDL stamps a wired pad's samples 4 ms apart whatever the clock bytes say: the driver's period.
pub const REPORT_PERIOD: Duration = Duration::from_micros(
    pf_driver_proto::gamepad::report_period_us(pf_driver_proto::gamepad::DEVTYPE_HORIPAD_STEAM),
);

/// 16 LSB per °/s: `INT16_MAX` is 2048 °/s.
const GYRO_LSB_PER_DEG_S: i32 = 16;
const ACCEL_LSB_PER_G: i32 = 4096;

/// `(byte, bit)` per wire button. Face bits are positional (`a:b0,b:b1,x:b2,y:b3`). The four rear
/// buttons follow SDL's `paddle1:b13 (FL), paddle2:b12 (FR), paddle3:b15 (M2), paddle4:b14 (M1)`
/// so a physical HORIPAD's buttons come back as themselves. Wire bits not listed are dropped.
const BUTTONS: [(u32, usize, u8); 16] = [
    (gs::BTN_A, 5, 0x10),
    (gs::BTN_B, 5, 0x20),
    (gs::BTN_MISC1, 5, 0x40), // QAM
    (gs::BTN_X, 5, 0x80),
    (gs::BTN_Y, 6, 0x01),
    (gs::BTN_PADDLE4, 6, 0x02), // M1
    (gs::BTN_LB, 6, 0x04),
    (gs::BTN_RB, 6, 0x08),
    (gs::BTN_BACK, 6, 0x40),
    (gs::BTN_START, 6, 0x80),
    (gs::BTN_GUIDE, 7, 0x01),
    (gs::BTN_LS_CLICK, 7, 0x02),
    (gs::BTN_RS_CLICK, 7, 0x04),
    (gs::BTN_PADDLE3, 7, 0x08), // M2
    (gs::BTN_PADDLE2, 7, 0x40), // FR
    (gs::BTN_PADDLE1, 7, 0x80), // FL
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HoriState {
    /// LX, LY, RX, RY; `0x80` is centre, Y grows downward.
    pub sticks: [u8; 4],
    pub hat: u8,
    /// Bytes 5 (upper nibble only), 6 and 7.
    pub buttons: [u8; 3],
    pub rt: u8,
    pub lt: u8,
    /// Report order: bytes 12, 14, 16.
    pub gyro: [i16; 3],
    /// Report order: bytes 18, 20, 22.
    pub accel: [i16; 3],
}

impl HoriState {
    /// Centred, unpressed, flat on a table: SDL reads `-accel[1]` as +y.
    pub fn neutral() -> HoriState {
        HoriState {
            sticks: [0x80; 4],
            hat: 0x0F,
            buttons: [0; 3],
            rt: 0,
            lt: 0,
            gyro: [0; 3],
            accel: [0, -(ACCEL_LSB_PER_G as i16), 0],
        }
    }

    /// A button/stick frame over `prev`, keeping its motion from the rich plane.
    pub fn merge_frame(prev: &HoriState, f: &GamepadFrame) -> HoriState {
        let mut b = [0u8; 3];
        for (bit, byte, mask) in BUTTONS {
            if f.buttons & bit != 0 {
                b[byte - 5] |= mask;
            }
        }
        HoriState {
            sticks: [
                stick(f.ls_x as i32),
                stick(-(f.ls_y as i32)),
                stick(f.rs_x as i32),
                stick(-(f.rs_y as i32)),
            ],
            hat: crate::dpad::dpad_octant(f.buttons).unwrap_or(0x0F),
            buttons: b,
            rt: f.right_trigger,
            lt: f.left_trigger,
            gyro: prev.gyro,
            accel: prev.accel,
        }
    }

    pub fn apply_rich(&mut self, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            self.apply_motion(gyro, accel);
        }
    }

    /// Wire sample (SDL's frame: pitch, yaw, roll) → report order. SDL reads gyro as
    /// `(-g16, -g12, -g14)` and accel as `(a22, -a20, a18)`; this is its inverse.
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        let g = |v: i32| {
            (-v * GYRO_LSB_PER_DEG_S / gs::MOTION_GYRO_LSB_PER_DEG_S)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16
        };
        let a = |v: i32| {
            (v * ACCEL_LSB_PER_G / gs::MOTION_ACCEL_LSB_PER_G)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16
        };
        let [p, y, r] = gyro.map(i32::from);
        self.gyro = [g(y), g(r), g(p)];
        let [ax, ay, az] = accel.map(i32::from);
        self.accel = [a(az), a(-ay), a(ax)];
    }

    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    pub fn clear_rich(&mut self) {
        let fresh = HoriState::neutral();
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// The input report. `clock` is the u16 µs field SDL reads and then replaces with its own
    /// fixed step; `serial` becomes the pad's SDL serial.
    pub fn serialize(&self, clock: u16, serial: [u8; 6]) -> [u8; REPORT_LEN] {
        let mut r = [0u8; REPORT_LEN];
        r[0] = REPORT_ID;
        r[1..5].copy_from_slice(&self.sticks);
        r[5] = self.buttons[0] | (self.hat & 0x0F);
        r[6] = self.buttons[1];
        r[7] = self.buttons[2];
        r[8] = self.rt;
        r[9] = self.lt;
        r[10..12].copy_from_slice(&clock.to_le_bytes());
        for (i, v) in self.gyro.iter().chain(&self.accel).enumerate() {
            r[12 + 2 * i..14 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        // Low nibble × 10 %, bit 4 charging. A wired pad not charging reads as charged.
        r[24] = 10;
        r[38..44].copy_from_slice(&serial);
        r
    }
}

/// Wire axis → the `u8` SDL reads back as `raw * 257 - 32768`, `0x80` exact centre.
fn stick(v: i32) -> u8 {
    ((v.clamp(-32768, 32767) + 32768) * 255 + 32767).div_euclid(65535) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(buttons: u32) -> HoriState {
        HoriState::merge_frame(
            &HoriState::neutral(),
            &GamepadFrame {
                buttons,
                ..Default::default()
            },
        )
    }

    #[test]
    fn neutral_report() {
        let r = state(0).serialize(0, serial(2));
        assert_eq!(r[..6], [0x07, 0x80, 0x80, 0x80, 0x80, 0x0F]);
        assert_eq!(r[20..22], (-4096i16).to_le_bytes(), "gravity on -accel[1]");
        assert_eq!(r[38..44], [0x50, 0x46, 0x48, 0x52, 0x00, 2]);
    }

    /// Face, QAM and the rear buttons land where SDL's driver and mapping read them.
    #[test]
    fn buttons_land_on_sdls_bits() {
        let r = |b| state(b).serialize(0, serial(0));
        assert_eq!(r(gs::BTN_A)[5] & 0xF0, 0x10);
        assert_eq!(r(gs::BTN_Y)[6], 0x01);
        assert_eq!(r(gs::BTN_MISC1)[5] & 0xF0, 0x40);
        assert_eq!(r(gs::BTN_PADDLE1)[7], 0x80);
        assert_eq!(r(gs::BTN_PADDLE2)[7], 0x40);
        assert_eq!(r(gs::BTN_PADDLE3)[7], 0x08);
        assert_eq!(r(gs::BTN_PADDLE4)[6], 0x02);
        assert_eq!(r(gs::BTN_GUIDE)[7], 0x01);
        // The hat keeps its nibble under the face bits.
        assert_eq!(r(gs::BTN_A | gs::BTN_DPAD_DOWN)[5], 0x14);
    }

    #[test]
    fn sticks_center_on_0x80() {
        assert_eq!(stick(0), 0x80);
        assert_eq!(stick(32767), 0xFF);
        assert_eq!(stick(-32768), 0x00);
        let f = GamepadFrame {
            ls_y: 32767,
            ..Default::default()
        };
        assert_eq!(
            HoriState::merge_frame(&HoriState::neutral(), &f).sticks[1],
            0x00,
            "up"
        );
    }

    /// SDL: gyro `(-g16, -g12, -g14) × 2048°/s / 32768`, accel `(a22, -a20, a18) / 4096`.
    #[test]
    fn motion_is_the_inverse_of_sdls_rotation() {
        let mut s = HoriState::neutral();
        s.apply_motion(
            [0, 50 * gs::MOTION_GYRO_LSB_PER_DEG_S as i16, 0],
            [0, gs::MOTION_ACCEL_LSB_PER_G as i16, 0],
        );
        let yaw = -(s.gyro[0] as f64) * 2048.0 / 32768.0;
        assert!((yaw - 50.0).abs() < 0.1, "{yaw} °/s");
        assert_eq!(s.accel, [0, -4096, 0]);
    }

    #[test]
    fn descriptor_declares_the_report() {
        use pf_driver_proto::rdesc::{report_lens, INPUT, OUTPUT};
        let lens = report_lens(&RDESC);
        assert_eq!(lens[&(INPUT, REPORT_ID)], REPORT_LEN);
        assert_eq!(lens.get(&(OUTPUT, REPORT_ID)), None);
    }
}
