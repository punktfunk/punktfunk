//! Transport-independent DualShock 4 HID report codec — shared by the Linux UHID backend
//! ([`super::dualshock4`]) and the Windows UMDF backend ([`super::dualshock4_windows`]).
//!
//! PS4 sibling of [`super::dualsense_proto`]: same [`DsState`] model and
//! [`DsState::from_gamepad`] mapper; only the report byte layout, touchpad resolution, and
//! output report (`0x05`) differ. Offsets match kernel `struct dualshock4_input_report_usb` /
//! `_output_report_common`. Pin via `tests` here and `crates/host/pf-inject/tests/motion_contract.rs`.

use super::dualsense_proto::{pack_touch, DsState};
use crate::sensor_clock::SensorClock;
use std::time::Instant;

pub const DS4_VENDOR: u16 = 0x054C;
pub const DS4_PRODUCT: u16 = 0x09CC;
/// USB input report `0x01`: report id + 63-byte body.
pub const DS4_INPUT_REPORT_LEN: usize = 64;
/// Kernel ABS_MT range is 0..=1919 / 0..=941 — one less than DualSense's 1920×1080.
pub const DS4_TOUCH_W: u16 = 1920;
pub const DS4_TOUCH_H: u16 = 942;

// USB report descriptor and the GET_REPORT blobs read at DS4 init (`0x12` pairing, `0x02` IMU
// calibration, `0xA3` firmware). The bytes live in `pf_driver_proto::dualshock4`, which the
// Windows driver serves from too.
pub const DS4_RDESC: &[u8] = &pf_driver_proto::dualshock4::RDESC;
pub const DS4_FEATURE_PAIRING: &[u8] = &pf_driver_proto::dualshock4::FEATURE_PAIRING;
pub const DS4_FEATURE_CALIBRATION: &[u8] = &pf_driver_proto::dualshock4::FEATURE_CALIBRATION;
pub const DS4_FEATURE_FIRMWARE: &[u8] = &pf_driver_proto::dualshock4::FEATURE_FIRMWARE;
pub use pf_driver_proto::dualshock4::pairing_reply as ds4_pairing_reply;

/// Pack input report `0x01`. Offsets match kernel `struct dualshock4_input_report_usb`;
/// a `common` field at struct offset N sits at report byte N+1 (byte 0 is the report id).
/// Unwritten bytes (temp, reserved, extra touch frames) stay as the caller left them.
pub fn serialize_state(r: &mut [u8; DS4_INPUT_REPORT_LEN], st: &DsState, counter: u8, ts: u16) {
    r[0] = 0x01;
    r[1] = st.lx;
    r[2] = st.ly;
    r[3] = st.rx;
    r[4] = st.ry;
    r[5] = (st.dpad & 0x0F) | (st.buttons[0] & 0xF0); // dpad hat (low nibble) + face (high)
    r[6] = st.buttons[1];
    r[7] = (st.buttons2_with_click() & 0x03) | ((counter & 0x3F) << 2); // PS + pad-click + report counter
    r[8] = st.l2;
    r[9] = st.r2;
    r[10..12].copy_from_slice(&ts.to_le_bytes()); // sensor_timestamp (struct off 9)
    for (i, v) in st.gyro.iter().enumerate() {
        r[13 + i * 2..15 + i * 2].copy_from_slice(&v.to_le_bytes()); // gyro (struct off 12)
    }
    for (i, v) in st.accel.iter().enumerate() {
        r[19 + i * 2..21 + i * 2].copy_from_slice(&v.to_le_bytes()); // accel (struct off 18)
    }
    // status[0] (struct off 29 → r[30]): bit4 cable, low nibble battery. Wired + full
    // (0x1B) so the kernel never warns "low battery" on a virtual pad.
    r[30] = 0x10 | 0x0B;
    r[33] = 1; // one touch frame; a real DS4 always sends one
    r[34] = ts as u8;
    pack_touch(&mut r[35..39], &st.touch[0], DS4_TOUCH_W, DS4_TOUCH_H);
    pack_touch(&mut r[39..43], &st.touch[1], DS4_TOUCH_W, DS4_TOUCH_H);
}

/// Report-`0x01` encoder a DualShock 4 keeps across writes: its report counter and sensor
/// clock. Each transport holds one and only moves bytes.
pub struct Ds4Encoder {
    counter: u8,
    clock: SensorClock,
}

impl Default for Ds4Encoder {
    fn default() -> Ds4Encoder {
        Ds4Encoder {
            counter: 0,
            clock: SensorClock::dualshock4(),
        }
    }
}

impl Ds4Encoder {
    /// The next report `0x01` for `st`.
    pub fn encode(&mut self, st: &DsState) -> [u8; DS4_INPUT_REPORT_LEN] {
        self.counter = self.counter.wrapping_add(1);
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(
            &mut r,
            st,
            self.counter,
            self.clock.ds4_ticks(Instant::now()),
        );
        r
    }
}

/// One HID-output pass: rumble on the 0xCA plane, lightbar as a `Led` on 0xCD.
/// DS4 has no player LEDs or adaptive triggers.
#[derive(Default)]
pub struct Ds4Feedback {
    /// `(low, high)` motor levels, 0..=0xFF00, if a report carried them.
    pub rumble: Option<(u16, u16)>,
    /// Lightbar RGB, if the report carried it (deduped by the manager).
    pub led: Option<(u8, u8, u8)>,
    /// Output-report ring overflowed this poll: pending reports discarded, feedback unknown.
    /// [`UhidManager`](crate::uhid_manager) must resync. Set by the backend drain, never the parser.
    pub resync: bool,
}

/// Parse USB output report `0x05`. Gated on `valid_flag0`: bit0 motor, bit1 LED. Motor right
/// (weak) is at [4], left (strong) at [5] — inverted vs (low, high). A rumble-only write must
/// not look like a lightbar change.
pub fn parse_ds4_output(data: &[u8], fb: &mut Ds4Feedback) {
    if data.first() != Some(&0x05) || data.len() < 11 {
        return; // not USB 0x05 (BT 0x11 is shifted) / too short
    }
    let flag0 = data[1];
    if flag0 & 0x01 != 0 {
        // motor_left (strong/low) at [5], motor_right (weak/high) at [4];
        // scale 0..255 → 0..0xFF00, same (low, high) as the other backends.
        let low = (data[5] as u16) << 8;
        let high = (data[4] as u16) << 8;
        fb.rumble = Some((low, high));
    }
    if flag0 & 0x02 != 0 {
        fb.led = Some((data[6], data[7], data[8]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dualsense_proto::Touch;

    /// The driver serves these until the host writes: [`DsState::neutral`] through each Sony
    /// encoder, the DualSense's with no trigger effect armed.
    #[test]
    fn the_drivers_neutral_reports_are_a_still_pad() {
        use crate::dualsense_proto as ds;
        let mut r = [0u8; ds::DS_INPUT_REPORT_LEN];
        ds::serialize_state(&mut r, &DsState::neutral(), 0, 0);
        ds::DsTriggers::default().stamp(&mut r, 0, 0);
        assert_eq!(r, pf_driver_proto::dualsense::NEUTRAL_REPORT);

        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, &DsState::neutral(), 0, 0);
        assert_eq!(r, pf_driver_proto::dualshock4::NEUTRAL_REPORT);
    }

    /// A contact past the pad clamps to the DS4 extent (1919 × 941), not the DualSense one.
    #[test]
    fn touch_clamps_to_the_ds4_extent() {
        let mut st = DsState::neutral();
        st.touch[0] = Touch {
            active: true,
            id: 0,
            x: u16::MAX,
            y: u16::MAX,
        };
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, &st, 0, 0);
        let x = u16::from(r[36]) | (u16::from(r[37] & 0x0F) << 8);
        let y = u16::from(r[37] >> 4) | (u16::from(r[38]) << 4);
        assert_eq!((x, y), (DS4_TOUCH_W - 1, DS4_TOUCH_H - 1));
    }

    /// L2 analog is byte 8, not DualSense's 5. Offsets otherwise match kernel DS4 USB.
    #[test]
    fn serialize_offsets() {
        use punktfunk_core::input::gamepad as gs;
        let mut st = DsState::from_gamepad(
            gs::BTN_A | gs::BTN_DPAD_UP | gs::BTN_LB,
            16384, // lx right of centre
            0,
            0,
            -32768, // ry down — Y inverted to 0xFF
            200,
            0,
        );
        st.gyro = [0x0102, 0x0304, 0x0506];
        st.accel = [0x1112, 0x1314, 0x1516];
        st.touch[0] = Touch {
            active: true,
            id: 0,
            x: 100,
            y: 200,
        };
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, &st, 0, 0);
        assert_eq!(r[0], 0x01);
        assert_eq!(r[8], 200); // L2 analog at byte 8 (not DualSense's 5)
        assert_eq!(r[5] & 0x0F, 0); // dpad hat 0 = N
        assert_eq!(r[5] & 0x20, 0x20); // Cross (A)
        assert_eq!(r[6] & 0x01, 0x01);
        assert_eq!(&r[13..19], &[0x02, 0x01, 0x04, 0x03, 0x06, 0x05]);
        assert_eq!(&r[19..25], &[0x12, 0x11, 0x14, 0x13, 0x16, 0x15]);
        assert_eq!(r[33], 1);
        assert_eq!(r[35] & 0x80, 0); // contact 0 active (bit7 clear)
        assert_eq!(r[35] & 0x7F, 0);
        assert_eq!(r[30] & 0x10, 0x10);

        // Rich-plane `touch_click` (no BTN_TOUCHPAD in the frame) still sets byte 7 bit 1 via
        // `buttons2_with_click`.
        assert_eq!(r[7] & 0x02, 0);
        st.touch_click[0] = true;
        serialize_state(&mut r, &st, 0, 0);
        assert_eq!(r[7] & 0x02, 0x02);
    }

    /// Flag-gated parse: MOTOR|LED fills rumble + lightbar; MOTOR-only leaves `led` untouched.
    #[test]
    fn parse_output_rumble_and_lightbar() {
        let mut report = [0u8; 32];
        report[0] = 0x05;
        report[1] = 0x01 | 0x02; // MOTOR | LED
        report[4] = 0x40; // motor_right (weak/high)
        report[5] = 0x80; // motor_left (strong/low)
        report[6] = 0x11;
        report[7] = 0x22;
        report[8] = 0x33;
        let mut fb = Ds4Feedback::default();
        parse_ds4_output(&report, &mut fb);
        assert_eq!(fb.rumble, Some((0x8000, 0x4000))); // (low=strong, high=weak)
        assert_eq!(fb.led, Some((0x11, 0x22, 0x33)));

        let mut motor_only = [0u8; 32];
        motor_only[0] = 0x05;
        motor_only[1] = 0x01; // MOTOR only
        motor_only[5] = 0x10;
        let mut fb2 = Ds4Feedback::default();
        parse_ds4_output(&motor_only, &mut fb2);
        assert!(fb2.rumble.is_some());
        assert_eq!(fb2.led, None);

        // LED-only: rumble stays `None` (`rumble_drove` keys on it — an LED stream must not
        // look like rumble). A MOTOR-flagged zero is `Some((0, 0))`, never absence.
        let mut led_only = [0u8; 32];
        led_only[0] = 0x05;
        led_only[1] = 0x02; // LED only
        led_only[6] = 0x11;
        let mut fb3 = Ds4Feedback::default();
        parse_ds4_output(&led_only, &mut fb3);
        assert!(fb3.rumble.is_none());
        let mut stop = [0u8; 32];
        stop[0] = 0x05;
        stop[1] = 0x01; // MOTOR flag, motors zero
        let mut fb4 = Ds4Feedback::default();
        parse_ds4_output(&stop, &mut fb4);
        assert_eq!(fb4.rumble, Some((0, 0)));
    }
}
