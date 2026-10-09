//! Nintendo Switch 2 Pro Controller (`057E:2069`) and GameCube controller (`057E:2073`): state,
//! input report, flash and vendor-command replies for [`super::switch2_usbip`], pinned to SDL's
//! `SDL_hidapi_switch2.c`.
//!
//! A Switch 2 pad is USB-only to SDL and Steam. They claim its vendor bulk interface, read
//! factory flash (serial, stick calibration), send an init sequence, then read 64-byte input
//! report `0x05` from the HID interface at 250 Hz. Commands are `[cmd, 0x91, 0, sub, 0, len, 0,
//! 0, payload…]`; the pad echoes the header with `0x01` and `0xF8`. Wire motion is SDL's frame
//! in DualSense units; the report carries the pad's raw axes, which SDL reads as `(x, z, -y)`.

use pf_driver_proto::switch::{pack12, STICK_CENTER, STICK_RANGE};
use punktfunk_core::input::{gamepad as gs, GamepadFrame};
use punktfunk_core::quic::RichInput;

pub const VENDOR: u16 = 0x057E;
/// Input report id SDL's init selects ("set report format 5").
pub const INPUT_REPORT_ID: u8 = 0x05;
pub const REPORT_LEN: usize = 64;
/// SDL assumes one report per 4 ms when it calibrates the sensor clock.
pub const REPORT_INTERVAL_MS: u8 = 4;

/// Which Switch 2 pad a device presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    Pro,
    GameCube,
}

impl Model {
    pub const fn product(self) -> u16 {
        match self {
            Model::Pro => 0x2069,
            Model::GameCube => 0x2073,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Model::Pro => "Nintendo Switch Pro Controller",
            Model::GameCube => "Nintendo GameCube Controller",
        }
    }

    /// Output report id SDL writes rumble with.
    pub const fn rumble_report_id(self) -> u8 {
        match self {
            Model::Pro => 0x02,
            Model::GameCube => 0x03,
        }
    }

    /// Model byte in the serial, so two models at one index never share one.
    const fn tag(self) -> char {
        match self {
            Model::Pro => 'P',
            Model::GameCube => 'G',
        }
    }
}

/// HID report descriptor: a Game Pad collection (SDL's hidapi only-controllers filter keeps it)
/// with vendor input report `0x05` and the model's vendor rumble output report.
pub const fn rdesc(model: Model) -> [u8; 33] {
    [
        0x05,
        0x01,
        0x09,
        0x05,
        0xA1,
        0x01, // Generic Desktop / Game Pad / Application
        0x85,
        INPUT_REPORT_ID,
        0x06,
        0x00,
        0xFF,
        0x09,
        0x01, // id 5, vendor usage
        0x15,
        0x00,
        0x26,
        0xFF,
        0x00,
        0x75,
        0x08,
        0x95,
        0x3F,
        0x81,
        0x02, // 63 × u8 input
        0x85,
        model.rumble_report_id(),
        0x09,
        0x02,
        0x95,
        0x3F,
        0x91,
        0x02, // 63 × u8 output
        0xC0,
    ]
}

/// Pad state in report units: raw buttons (report bytes 5–8), 12-bit sticks, GameCube trigger
/// travel, raw IMU in report order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Switch2State {
    pub buttons: [u8; 4],
    pub sticks: [u16; 4],
    pub triggers: [u8; 2],
    pub accel: [i16; 3],
    pub gyro: [i16; 3],
}

/// Accel: SDL reads `raw × 8 g / 32767`.
const ACCEL_RAW_PER_G: i32 = 32767 / 8;
/// Gyro: SDL reads `raw × 34.8 / 32767` rad/s, 16.4334 raw per °/s, here in ten-thousandths.
const GYRO_RAW_PER_DPS_E4: i32 = 164_334;
/// GameCube trigger travel SDL maps to full scale (`MapTriggerAxis`), zero point 0 in flash.
const TRIGGER_FULL: u32 = 232;

impl Switch2State {
    pub fn neutral() -> Switch2State {
        Switch2State {
            buttons: [0; 4],
            sticks: [STICK_CENTER; 4],
            triggers: [0; 2],
            // At rest gravity is SDL's +y, which the pad reports on raw z.
            accel: [0, 0, ACCEL_RAW_PER_G as i16],
            gyro: [0; 3],
        }
    }

    /// Fold a frame over `prev`, keeping its motion.
    pub fn merge_frame(model: Model, prev: &Switch2State, f: &GamepadFrame) -> Switch2State {
        let on = |bit: u32| f.buttons & bit != 0;
        let mut b = [0u8; 4];
        let mut set = |byte: usize, mask: u8, pressed: bool| {
            if pressed {
                b[byte] |= mask;
            }
        };
        // Faces by position. The Pro's byte 5 is west, north, south, east; SDL's GameCube mapping
        // (`a:b1,b:b3,x:b0,y:b2`) puts A on 0x08, B on 0x02, X on 0x04, Y on 0x01.
        let [south, east, west, north] = match model {
            Model::Pro => [0x04, 0x08, 0x01, 0x02],
            Model::GameCube => [0x08, 0x02, 0x04, 0x01],
        };
        set(0, south, on(gs::BTN_A));
        set(0, east, on(gs::BTN_B));
        set(0, west, on(gs::BTN_X));
        set(0, north, on(gs::BTN_Y));
        set(1, 0x02, on(gs::BTN_START));
        set(1, 0x10, on(gs::BTN_GUIDE));
        set(1, 0x20, on(gs::BTN_MISC1));
        set(2, 0x01, on(gs::BTN_DPAD_DOWN));
        set(2, 0x02, on(gs::BTN_DPAD_UP));
        set(2, 0x04, on(gs::BTN_DPAD_RIGHT));
        set(2, 0x08, on(gs::BTN_DPAD_LEFT));
        match model {
            Model::Pro => {
                set(0, 0x40, on(gs::BTN_RB));
                set(0, 0x80, f.right_trigger > 0);
                set(1, 0x01, on(gs::BTN_BACK));
                set(1, 0x04, on(gs::BTN_RS_CLICK));
                set(1, 0x08, on(gs::BTN_LS_CLICK));
                set(2, 0x40, on(gs::BTN_LB));
                set(2, 0x80, f.left_trigger > 0);
                // GR is SDL's right paddle, GL its left one.
                set(3, 0x01, on(gs::BTN_PADDLE1) || on(gs::BTN_PADDLE3));
                set(3, 0x02, on(gs::BTN_PADDLE2) || on(gs::BTN_PADDLE4));
            }
            Model::GameCube => {
                // Z and ZL on the shoulders; L and R click at the end of their travel.
                set(0, 0x80, on(gs::BTN_RB));
                set(2, 0x80, on(gs::BTN_LB));
                set(0, 0x40, f.right_trigger == u8::MAX);
                set(2, 0x40, f.left_trigger == u8::MAX);
            }
        }
        let travel = |v: u8| (v as u32 * TRIGGER_FULL / 255) as u8;
        Switch2State {
            buttons: b,
            sticks: [f.ls_x, f.ls_y, f.rs_x, f.rs_y].map(stick_raw),
            triggers: match model {
                Model::GameCube => [travel(f.left_trigger), travel(f.right_trigger)],
                Model::Pro => [0; 2],
            },
            ..*prev
        }
    }

    pub fn apply_rich(&mut self, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            self.apply_motion(gyro, accel);
        }
    }

    /// Wire sample (SDL's frame) → the pad's raw axes, the inverse of SDL's `(x, z, -y)`.
    pub fn apply_motion(&mut self, gyro: [i16; 3], accel: [i16; 3]) {
        let g = |v: i32| {
            (v as i64 * GYRO_RAW_PER_DPS_E4 as i64
                / (10_000 * gs::MOTION_GYRO_LSB_PER_DEG_S as i64))
                .clamp(i16::MIN as i64, i16::MAX as i64) as i16
        };
        let a = |v: i32| {
            (v * ACCEL_RAW_PER_G / gs::MOTION_ACCEL_LSB_PER_G)
                .clamp(i16::MIN as i32, i16::MAX as i32) as i16
        };
        let [gx, gy, gz] = gyro.map(i32::from);
        self.gyro = [g(gx), g(-gz), g(gy)];
        let [ax, ay, az] = accel.map(i32::from);
        self.accel = [a(ax), a(-az), a(ay)];
    }

    pub fn neutralize_gyro(&mut self) -> bool {
        let changed = self.gyro != [0; 3];
        self.gyro = [0; 3];
        changed
    }

    pub fn clear_rich(&mut self) {
        let fresh = Switch2State::neutral();
        self.gyro = fresh.gyro;
        self.accel = fresh.accel;
    }

    /// Input report `0x05` as the pad serves it: `seq` at byte 1, the µs sensor clock at `0x2B`.
    pub fn report(&self, seq: u8, clock_us: u32) -> [u8; REPORT_LEN] {
        let mut r = [0u8; REPORT_LEN];
        r[0] = INPUT_REPORT_ID;
        r[1] = seq;
        r[2] = 0x20;
        r[5..9].copy_from_slice(&self.buttons);
        r[11..14].copy_from_slice(&pack12(self.sticks[0], self.sticks[1]));
        r[14..17].copy_from_slice(&pack12(self.sticks[2], self.sticks[3]));
        r[0x2B..0x2F].copy_from_slice(&clock_us.to_le_bytes());
        for (i, v) in self.accel.iter().enumerate() {
            r[0x31 + 2 * i..0x33 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in self.gyro.iter().enumerate() {
            r[0x37 + 2 * i..0x39 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        r[61..63].copy_from_slice(&self.triggers);
        r
    }
}

/// Wire i16 (+ = right/up) → 12-bit raw around the flash calibration's centre.
fn stick_raw(v: i16) -> u16 {
    super::switch_proto::stick_raw(v)
}

/// Serial SDL reads from flash `0x13002` and dedups pads by: model, index, 16 characters.
pub fn serial(model: Model, index: u8) -> String {
    format!("PFS2{}{index:011}", model.tag())
}

/// Factory flash block (64 bytes) at `address`. Erased flash reads `0xFF`; the IMU bias blocks
/// read zero so SDL subtracts nothing.
pub fn flash_block(model: Model, index: u8, address: u32) -> [u8; 0x40] {
    let mut data = [0xFFu8; 0x40];
    match address {
        0x13000 => {
            data[..2].copy_from_slice(&[0x01, 0x00]);
            let s = serial(model, index);
            data[2..18].copy_from_slice(&s.as_bytes()[..16]);
            data[18..20].copy_from_slice(&VENDOR.to_le_bytes());
            data[20..22].copy_from_slice(&model.product().to_le_bytes());
        }
        0x13040 | 0x13100 => data = [0; 0x40],
        // Stick calibration at +0x28: neutral, then the reach above and below it.
        0x13080 | 0x130C0 => {
            data[0x28..0x2B].copy_from_slice(&pack12(STICK_CENTER, STICK_CENTER));
            data[0x2B..0x2E].copy_from_slice(&pack12(STICK_RANGE, STICK_RANGE));
            data[0x2E..0x31].copy_from_slice(&pack12(STICK_RANGE, STICK_RANGE));
        }
        // GameCube trigger zero points.
        0x13140 => data[..2].copy_from_slice(&[0, 0]),
        _ => {}
    }
    data
}

/// What a vendor bulk command asks of the host.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `0x09`: player lights, SDL's pattern bits.
    PlayerLeds(u8),
    Other,
}

/// The reply to one bulk-OUT command and what it asks of the host. A flash read (`0x02`) carries
/// a 16-byte header and the block at `0x10`; everything else is the 8-byte echo.
pub fn bulk_reply(model: Model, index: u8, cmd: &[u8]) -> Option<(Vec<u8>, Command)> {
    let (&c0, _) = cmd.split_first()?;
    let at = |i: usize| cmd.get(i).copied().unwrap_or(0);
    let mut reply = vec![c0, 0x01, at(2), at(3), at(4), 0xF8, 0, 0];
    let what = match c0 {
        0x02 if cmd.len() >= 16 => {
            let address = u32::from_le_bytes([cmd[12], cmd[13], cmd[14], cmd[15]]);
            reply.resize(0x10, 0);
            reply[8] = 0x40;
            reply[12..16].copy_from_slice(&cmd[12..16]);
            reply.extend_from_slice(&flash_block(model, index, address));
            Command::Other
        }
        0x09 => Command::PlayerLeds(at(8) & 0x0F),
        _ => Command::Other,
    };
    Some((reply, what))
}

/// Rumble output → `(low, high)` on 0..65535, or `None` for another report. The Pro carries SDL's
/// HD encoding scaled to its 29000 ceiling; the GameCube motor is on/off.
pub fn parse_rumble(model: Model, report: &[u8]) -> Option<(u16, u16)> {
    if report.first() != Some(&model.rumble_report_id()) {
        return None;
    }
    match model {
        Model::GameCube => {
            let on = report.get(2) == Some(&1);
            let level = if on { u16::MAX } else { 0 };
            Some((level, level))
        }
        Model::Pro => {
            let d = report.get(2..7)?;
            let high = ((d[1] as u32 & 0xFC) << 4) | ((d[2] as u32 & 0x0F) << 12);
            let low = (d[3] as u32 & 0xC0) | ((d[4] as u32) << 8);
            let unscale = |v: u32| (v * u16::MAX as u32 / 29_000).min(u16::MAX as u32) as u16;
            Some((unscale(low), unscale(high)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(buttons: u32) -> GamepadFrame {
        GamepadFrame {
            buttons,
            ..GamepadFrame::default()
        }
    }

    /// SDL's `HandleSwitchProState` bits: faces, shoulders, the C row, the d-pad, GR/GL.
    #[test]
    fn pro_buttons_land_where_sdl_reads_them() {
        let n = Switch2State::neutral();
        let st = |b| Switch2State::merge_frame(Model::Pro, &n, &frame(b)).buttons;
        assert_eq!(st(gs::BTN_A), [0x04, 0, 0, 0], "south");
        assert_eq!(st(gs::BTN_B), [0x08, 0, 0, 0], "east");
        assert_eq!(st(gs::BTN_X), [0x01, 0, 0, 0], "west");
        assert_eq!(st(gs::BTN_Y), [0x02, 0, 0, 0], "north");
        assert_eq!(st(gs::BTN_RB | gs::BTN_LB), [0x40, 0, 0x40, 0]);
        assert_eq!(
            st(gs::BTN_BACK | gs::BTN_START | gs::BTN_GUIDE | gs::BTN_MISC1)[1],
            0x33
        );
        assert_eq!(st(gs::BTN_DPAD_UP | gs::BTN_DPAD_LEFT)[2], 0x0A);
        assert_eq!(st(gs::BTN_PADDLE1)[3], 0x01, "GR is SDL's right paddle");
        assert_eq!(st(gs::BTN_PADDLE2)[3], 0x02, "GL is SDL's left paddle");
        let mut f = frame(0);
        f.left_trigger = 1;
        f.right_trigger = 200;
        let b = Switch2State::merge_frame(Model::Pro, &n, &f).buttons;
        assert_eq!((b[0], b[2]), (0x80, 0x80), "ZR / ZL are digital");
    }

    /// The GameCube pad: faces as SDL's mapping names them, Z/ZL on the shoulders, analog
    /// triggers at 61/62 with a click at the end of their travel.
    #[test]
    fn gamecube_triggers_are_analog_with_a_click() {
        let n = Switch2State::neutral();
        let face = |b| Switch2State::merge_frame(Model::GameCube, &n, &frame(b)).buttons[0];
        assert_eq!(
            [
                face(gs::BTN_A),
                face(gs::BTN_B),
                face(gs::BTN_X),
                face(gs::BTN_Y)
            ],
            [0x08, 0x02, 0x04, 0x01]
        );
        let mut f = frame(gs::BTN_RB | gs::BTN_LB);
        f.left_trigger = 255;
        f.right_trigger = 128;
        let st = Switch2State::merge_frame(Model::GameCube, &n, &f);
        assert_eq!(st.buttons[0], 0x80, "Z, no R click at half travel");
        assert_eq!(st.buttons[2], 0x80 | 0x40, "ZL and the L click");
        let r = st.report(0, 0);
        assert_eq!((r[61], r[62]), (232, 116));
    }

    /// Report 0x05 offsets SDL reads: sticks at 11/14, clock at 0x2B, accel 0x31, gyro 0x37.
    #[test]
    fn report_layout() {
        let mut st = Switch2State::neutral();
        st.sticks = [0x123, 0x456, 0x789, 0xABC];
        let r = st.report(7, 0x0102_0304);
        assert_eq!(r[..3], [INPUT_REPORT_ID, 7, 0x20]);
        let x = |o: usize| r[o] as u16 | ((r[o + 1] as u16 & 0x0F) << 8);
        let y = |o: usize| (r[o + 1] as u16 >> 4) | ((r[o + 2] as u16) << 4);
        assert_eq!([x(11), y(11), x(14), y(14)], [0x123, 0x456, 0x789, 0xABC]);
        assert_eq!(r[0x2B..0x2F], [0x04, 0x03, 0x02, 0x01]);
        assert_eq!(
            i16::from_le_bytes([r[0x35], r[0x36]]),
            4095,
            "gravity on raw z"
        );
    }

    /// SDL reads accel `(r0, r2, -r1) × 8 g / 32767` and gyro `(r0, r2, -r1) × 34.8 / 32767`.
    /// Applying that to the raw sample gives the wire sample back.
    #[test]
    fn motion_reads_back_as_sent() {
        let mut st = Switch2State::neutral();
        // 100 °/s pitch, 50 °/s roll; 1 g on +y.
        st.apply_motion([2000, 0, 1000], [0, 10000, 0]);
        let sdl = |r: [i16; 3]| [r[0] as f64, r[2] as f64, -(r[1] as f64)];
        let gyro = sdl(st.gyro).map(|v| v * 34.8 / 32767.0 * 180.0 / std::f64::consts::PI);
        assert!(
            (gyro[0] - 100.0).abs() < 0.1 && gyro[1].abs() < 0.1,
            "{gyro:?}"
        );
        assert!((gyro[2] - 50.0).abs() < 0.1, "{gyro:?}");
        let accel = sdl(st.accel).map(|v| v * 8.0 / 32767.0);
        assert!(
            (accel[1] - 1.0).abs() < 0.001 && accel[0] == 0.0,
            "{accel:?}"
        );
    }

    /// Flash reads SDL makes: serial, neutral IMU bias, stick calibration at +0x28, no user cal.
    #[test]
    fn flash_serves_sdl_reads() {
        let (reply, _) = bulk_reply(
            Model::Pro,
            3,
            &[
                0x02, 0x91, 0, 0x01, 0, 0x08, 0, 0, 0, 0, 0, 0, 0x00, 0x30, 0x01, 0x00,
            ],
        )
        .unwrap();
        assert_eq!(reply.len(), 0x50, "SDL reads 0x50 and copies from 0x10");
        assert_eq!(&reply[0x12..0x22], serial(Model::Pro, 3).as_bytes());
        assert_ne!(serial(Model::Pro, 3), serial(Model::GameCube, 3));
        let cal = flash_block(Model::Pro, 0, 0x13080);
        let neutral = cal[0x28] as u16 | ((cal[0x29] as u16 & 0x0F) << 8);
        let reach = cal[0x2B] as u16 | ((cal[0x2C] as u16 & 0x0F) << 8);
        assert_eq!((neutral, reach), (STICK_CENTER, STICK_RANGE));
        assert_eq!(flash_block(Model::Pro, 0, 0x13040)[4..16], [0; 12]);
        assert_ne!(flash_block(Model::Pro, 0, 0x1FC040)[..2], [0xB2, 0xA1]);
    }

    /// Every other command gets the 8-byte echo; `0x09` carries the player lights.
    #[test]
    fn commands_echo_and_lights() {
        let (reply, what) = bulk_reply(
            Model::Pro,
            0,
            &[0x09, 0x91, 0, 0x07, 0, 0x08, 0, 0, 0x03, 0, 0, 0],
        )
        .unwrap();
        assert_eq!(reply, [0x09, 0x01, 0, 0x07, 0, 0xF8, 0, 0]);
        assert_eq!(what, Command::PlayerLeds(0x03));
        assert!(bulk_reply(Model::Pro, 0, &[]).is_none());
    }

    /// SDL's `EncodeHDRumble` at its 29000 ceiling decodes back to full scale.
    #[test]
    fn rumble_decodes_sdl_encoding() {
        let enc = |hi: u16, lo: u16| {
            let (hf, lf) = (0x187u16, 0x112u16);
            let (hi, lo) = (
                (hi as u32 * 29_000 / 65_535) as u16,
                (lo as u32 * 29_000 / 65_535) as u16,
            );
            let mut r = [0u8; 64];
            r[0] = 0x02;
            r[1] = 0x50;
            r[2] = hf as u8;
            r[3] = (((hi >> 4) & 0xFC) | ((hf >> 8) & 0x03)) as u8;
            r[4] = ((hi >> 12) | (lf << 4)) as u8;
            r[5] = ((lo & 0xC0) | ((lf >> 4) & 0x3F)) as u8;
            r[6] = (lo >> 8) as u8;
            r
        };
        let (low, high) = parse_rumble(Model::Pro, &enc(0, 65535)).unwrap();
        assert!(high == 0 && low > 64_000, "{low} {high}");
        let (low, high) = parse_rumble(Model::Pro, &enc(32768, 0)).unwrap();
        assert!(low == 0 && (32_000..33_600).contains(&high), "{low} {high}");
        assert_eq!(
            parse_rumble(Model::GameCube, &[0x03, 0x50, 1]),
            Some((u16::MAX, u16::MAX))
        );
        assert_eq!(
            parse_rumble(Model::GameCube, &[0x03, 0x51, 2]),
            Some((0, 0))
        );
        assert_eq!(parse_rumble(Model::Pro, &[0x01, 0]), None);
    }
}
