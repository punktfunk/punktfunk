//! Motion unit contract: every backend's calibration or rescale lands on the wire constants.
//!
//! Gyro aim integrates angular velocity, so a scale error is every rotation the wrong
//! size. The wire carries DualSense `i16` LSBs ([`MOTION_GYRO_LSB_PER_DEG_S`] /
//! [`MOTION_ACCEL_LSB_PER_G`]). Sony pads *declare* that scale in a USB calibration
//! feature report; Steam Deck and Switch Pro *rescale* into their driver's native
//! resolution.
//!
//! This file applies the consumer's arithmetic to each declaration and asserts the
//! result is the wire constant. A new motion-capable backend belongs here.

use pf_inject::dualsense_proto::{
    serialize_state as ds_serialize, DsState, DS_FEATURE_CALIBRATION, DS_INPUT_REPORT_LEN,
    DS_TOUCH_H, DS_TOUCH_W,
};
use pf_inject::dualshock4_proto::{
    serialize_state as ds4_serialize, DS4_FEATURE_CALIBRATION, DS4_INPUT_REPORT_LEN, DS4_TOUCH_H,
    DS4_TOUCH_W,
};
use pf_inject::eightbitdo_proto::EightBitDoState;
use pf_inject::hori_proto::HoriState;
use pf_inject::steam_proto::SteamState;
use pf_inject::steam_remap::motion_wire_to_deck;
use pf_inject::switch_proto::SwitchState;
use punktfunk_core::input::gamepad::{
    MOTION_ACCEL_LSB_PER_G, MOTION_GYRO_LSB_PER_DEG_S, MOTION_NEUTRAL_ACCEL,
};
use punktfunk_core::quic::RichInput;

/// Sony IMU-calibration feature report (DualSense `0x05`, USB DualShock 4 `0x02`):
/// report id, three signed bias words, six **interleaved** per-axis `plus`/`minus`
/// words, two gyro `speed` words, six accel `plus`/`minus` words, all LE `i16`.
///
/// Interleaved is USB order. Bluetooth DualShock 4 groups pluses then minuses;
/// consumers switch on transport. Virtual pads declare `BUS_USB`.
#[derive(Debug, PartialEq, Eq)]
struct SonyImuCalibration {
    gyro_bias: [i16; 3],
    gyro_plus: [i16; 3],
    gyro_minus: [i16; 3],
    /// `speed_plus` + `speed_minus`: reference rate the plus/minus span was measured at.
    gyro_speed: [i16; 2],
    accel_plus: [i16; 3],
    accel_minus: [i16; 3],
}

impl SonyImuCalibration {
    fn parse(blob: &[u8], report_id: u8, who: &str) -> SonyImuCalibration {
        assert_eq!(blob.first().copied(), Some(report_id), "{who}: report id");
        assert!(
            blob.len() >= 35,
            "{who}: {} bytes, too short to carry the calibration fields (need 35)",
            blob.len()
        );
        let w = |i: usize| i16::from_le_bytes([blob[i], blob[i + 1]]);
        SonyImuCalibration {
            gyro_bias: [w(1), w(3), w(5)],
            gyro_plus: [w(7), w(11), w(15)],
            gyro_minus: [w(9), w(13), w(17)],
            gyro_speed: [w(19), w(21)],
            accel_plus: [w(23), w(27), w(31)],
            accel_minus: [w(25), w(29), w(33)],
        }
    }

    /// LSB/°·s the kernel derives for axis `i`. `hid-playstation` sets
    /// `sens_numer = (speed_plus + speed_minus) * GYRO_RES_PER_DEG_S` and
    /// `sens_denom = |plus - bias| + |minus - bias|`, then reports
    /// `raw * sens_numer / sens_denom` in 1/`GYRO_RES_PER_DEG_S` °/s.
    /// `GYRO_RES_PER_DEG_S` cancels; advertised resolution is `denom / speed_2x`.
    /// Integer because a fraction cannot round-trip.
    fn kernel_gyro_lsb_per_deg_s(&self, i: usize, who: &str) -> i64 {
        let speed_2x = self.gyro_speed[0] as i64 + self.gyro_speed[1] as i64;
        assert!(
            speed_2x != 0,
            "{who}: gyro speed_plus + speed_minus is zero"
        );
        let denom = (self.gyro_plus[i] as i64 - self.gyro_bias[i] as i64).abs()
            + (self.gyro_minus[i] as i64 - self.gyro_bias[i] as i64).abs();
        assert_eq!(
            denom % speed_2x,
            0,
            "{who} axis {i}: declares a fractional {denom}/{speed_2x} LSB per °/s"
        );
        denom / speed_2x
    }

    /// SDL's form (`SDL_hidapi_ps4` / `SDL_hidapi_ps5`): `plus - minus` over the
    /// speed sum, ignoring bias. Agrees with the kernel only for a symmetric
    /// zero-bias blob — both consumers read the same pad, so disagreement is a bug.
    fn sdl_gyro_lsb_per_deg_s(&self, i: usize) -> f64 {
        (self.gyro_plus[i] as f64 - self.gyro_minus[i] as f64)
            / (self.gyro_speed[0] as f64 + self.gyro_speed[1] as f64)
    }

    /// LSB/g for axis `i`: consumers take `plus - minus` as 2 g, so 1 g is half.
    fn accel_lsb_per_g(&self, i: usize, who: &str) -> i64 {
        let range_2g = self.accel_plus[i] as i64 - self.accel_minus[i] as i64;
        assert_eq!(
            range_2g % 2,
            0,
            "{who} axis {i}: odd accel range {range_2g} has no exact 1 g"
        );
        range_2g / 2
    }

    /// Raw value a consumer treats as zero g (`plus - range_2g / 2`). Wire zero is 0;
    /// a non-zero bias is a constant phantom acceleration.
    fn accel_zero_point(&self, i: usize) -> i64 {
        let range_2g = self.accel_plus[i] as i64 - self.accel_minus[i] as i64;
        self.accel_plus[i] as i64 - range_2g / 2
    }
}

#[test]
fn sony_calibration_blobs_declare_the_wire_units() {
    let wire_gyro = MOTION_GYRO_LSB_PER_DEG_S as i64;
    let wire_accel = MOTION_ACCEL_LSB_PER_G as i64;

    for (who, blob, report_id) in [
        ("DualSense 0x05", DS_FEATURE_CALIBRATION, 0x05u8),
        ("DualShock 4 0x02", DS4_FEATURE_CALIBRATION, 0x02u8),
    ] {
        let cal = SonyImuCalibration::parse(blob, report_id, who);
        for axis in 0..3 {
            assert_eq!(
                cal.kernel_gyro_lsb_per_deg_s(axis, who),
                wire_gyro,
                "{who} axis {axis}: gyro resolution the kernel derives"
            );
            assert_eq!(
                cal.sdl_gyro_lsb_per_deg_s(axis),
                wire_gyro as f64,
                "{who} axis {axis}: gyro resolution SDL derives"
            );
            assert_eq!(
                cal.accel_lsb_per_g(axis, who),
                wire_accel,
                "{who} axis {axis}: accel resolution"
            );
            assert_eq!(
                cal.accel_zero_point(axis),
                0,
                "{who} axis {axis}: accel zero point must be the wire's 0"
            );
        }
    }
}

#[test]
fn rescaling_backends_convert_the_wire_into_their_native_units() {
    // 100 °/s and 1 g on the wire.
    let wire_gyro = (100 * MOTION_GYRO_LSB_PER_DEG_S) as i16;
    let wire_accel = MOTION_ACCEL_LSB_PER_G as i16;

    // hid-steam: STEAM_DECK_GYRO_RES_PER_DPS = 16, ACCEL_RES_PER_G = 16384.
    let (gyro, accel) = motion_wire_to_deck([wire_gyro; 3], [wire_accel; 3]);
    assert_eq!(gyro, [100 * 16; 3], "Deck gyro: 100 °/s at 16 LSB/°·s");
    assert_eq!(accel, [16384; 3], "Deck accel: 1 g at 16384 LSB/g");

    // hid-nintendo: JC_IMU_GYRO_RES_PER_DPS = 14.247, ACCEL_RES_PER_G = 4096.
    // Identity factory-calibration blob, so report is 1:1. 100 °/s × 14.247 = 1424.7, truncated.
    // Signs are the codec tests' job: the pad's axes are SDL's permutation of the wire's.
    let mut st = SwitchState::neutral();
    st.apply_motion([wire_gyro; 3], [wire_accel; 3]);
    assert_eq!(
        st.gyro.map(i16::abs),
        [1424; 3],
        "Switch gyro: 100 °/s at 14.247 LSB/°·s"
    );
    assert_eq!(
        st.accel.map(i16::abs),
        [4096; 3],
        "Switch accel: 1 g at 4096 LSB/g"
    );

    // SDL 8bitdo: INT16_MAX = 2000 °/s, accel 4096 LSB/g. Signs are the codec tests' job.
    let mut st = EightBitDoState::neutral();
    st.apply_motion([wire_gyro; 3], [wire_accel; 3]);
    assert_eq!(
        st.gyro.map(i16::abs),
        [1638; 3],
        "8BitDo gyro: 100 °/s at 32767/2000"
    );
    assert_eq!(
        st.accel.map(i16::abs),
        [4096; 3],
        "8BitDo accel: 1 g at 4096 LSB/g"
    );

    // SDL steam_hori: the i16 range spans ±2048 °/s, 16 LSB/°·s; accel 4096 LSB/g.
    let mut st = HoriState::neutral();
    st.apply_motion([wire_gyro; 3], [wire_accel; 3]);
    assert_eq!(
        st.gyro.map(i16::abs),
        [1600; 3],
        "HORIPAD gyro: 100 °/s at 16 LSB/°·s"
    );
    assert_eq!(
        st.accel.map(i16::abs),
        [4096; 3],
        "HORIPAD accel: 1 g at 4096 LSB/g"
    );
}

/// Sony backends pass the wire sample unscaled. Correct only because the blobs above
/// declare the wire's units; a rescale here must move the blobs with it.
#[test]
fn sony_backends_pass_the_wire_sample_through_unscaled() {
    let gyro = [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16, -640, 7];
    let accel = [0, 0, MOTION_ACCEL_LSB_PER_G as i16];
    let motion = RichInput::Motion {
        pad: 0,
        gyro,
        accel,
    };

    for (who, w, h) in [
        ("DualSense", DS_TOUCH_W, DS_TOUCH_H),
        ("DualShock 4", DS4_TOUCH_W, DS4_TOUCH_H),
    ] {
        let mut st = DsState::neutral();
        st.apply_rich(motion, w, h);
        assert_eq!(st.gyro, gyro, "{who} rescaled the wire gyro");
        assert_eq!(st.accel, accel, "{who} rescaled the wire accel");
    }
}

/// Proto tests pin the HID offsets; this pins the VALUE arriving unscaled — the
/// other half of the calibration-blob promise.
#[test]
fn a_wire_motion_sample_reaches_the_report_bytes_unchanged() {
    let g = (100 * MOTION_GYRO_LSB_PER_DEG_S) as i16; // 100 °/s = 2000 = 0x07D0
    let a = MOTION_ACCEL_LSB_PER_G as i16; // 1 g = 10000 = 0x2710
    let motion = RichInput::Motion {
        pad: 0,
        gyro: [g, -g, 0],
        accel: [0, 0, a],
    };
    let gyro_le = [0xD0, 0x07, 0x30, 0xF8, 0x00, 0x00]; // 2000, −2000, 0
    let accel_le = [0x00, 0x00, 0x00, 0x00, 0x10, 0x27]; // 0, 0, 10000

    // DualSense report 0x01: gyro at bytes 16..22, accel at 22..28.
    let mut st = DsState::neutral();
    st.apply_rich(motion, DS_TOUCH_W, DS_TOUCH_H);
    let mut r = [0u8; DS_INPUT_REPORT_LEN];
    ds_serialize(&mut r, &st, 0, 0);
    assert_eq!(&r[16..22], &gyro_le, "DualSense report gyro");
    assert_eq!(&r[22..28], &accel_le, "DualSense report accel");

    // DualShock 4 report 0x01: gyro at 13..19, accel at 19..25.
    let mut st = DsState::neutral();
    st.apply_rich(motion, DS4_TOUCH_W, DS4_TOUCH_H);
    let mut r = [0u8; DS4_INPUT_REPORT_LEN];
    ds4_serialize(&mut r, &st, 0, 0);
    assert_eq!(&r[13..19], &gyro_le, "DualShock 4 report gyro");
    assert_eq!(&r[19..25], &accel_le, "DualShock 4 report accel");
}

/// Idle watchdog: gyro → 0 when the feed stops; accel does not — a still pad
/// still measures gravity, and blanking it reads as free-fall.
#[test]
fn neutralizing_motion_keeps_gravity() {
    let mut st = DsState::neutral();
    st.gyro = [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16; 3];
    st.accel = [0, 0, MOTION_ACCEL_LSB_PER_G as i16];

    assert!(st.neutralize_gyro(), "reported no change while rotating");
    assert_eq!(st.gyro, [0; 3]);
    assert_eq!(st.accel, [0, 0, MOTION_ACCEL_LSB_PER_G as i16]);
    assert!(!st.neutralize_gyro(), "a still pad must report no change");

    let mut deck = SteamState::neutral();
    deck.gyro = [(100 * MOTION_GYRO_LSB_PER_DEG_S) as i16; 3];
    deck.accel = [0, 0, 16384];
    assert!(deck.neutralize_gyro());
    assert_eq!(deck.gyro, [0; 3]);
    assert_eq!(deck.accel, [0, 0, 16384], "Deck gravity must survive too");
}

/// Neutral must read as still, not free-fall. `[0, 0, 0]` is zero proper
/// acceleration (falling). Each backend is checked in its own units.
#[test]
fn every_backend_neutral_reads_as_a_still_pad_not_a_falling_one() {
    // Wire convention: axis 1 is up.
    assert_eq!(MOTION_NEUTRAL_ACCEL, [0, MOTION_ACCEL_LSB_PER_G as i16, 0]);

    let ds = DsState::neutral();
    assert_eq!(
        ds.accel, MOTION_NEUTRAL_ACCEL,
        "a fresh DualSense/DS4 must report 1 g up, not free fall"
    );
    assert_eq!(ds.gyro, [0; 3], "and it must not be turning");

    // Deck neutral is the wire's, through the same rescale a live sample takes.
    // 16384 LSB/g is hid-steam's ACCEL_RES_PER_G.
    let deck = SteamState::neutral();
    assert_eq!(
        deck.accel,
        motion_wire_to_deck([0; 3], MOTION_NEUTRAL_ACCEL).1,
        "the Deck neutral must be the wire neutral, rescaled — not a second opinion about 1 g"
    );
    assert_eq!(deck.accel, [0, 16384, 0]);
    assert_eq!(deck.gyro, [0; 3]);

    // Switch Pro is a different device (hid-nintendo); its up axis is its own.
    // Do not align it to DualSense unmeasured.
    let sw = SwitchState::neutral();
    assert_eq!(
        sw.accel,
        [0, 0, 4096],
        "switch_proto's neutral is its own device's; do not align it to the DualSense unmeasured"
    );

    for (what, accel) in [
        ("dualsense", ds.accel),
        ("deck", deck.accel),
        ("switch", sw.accel),
    ] {
        assert_ne!(accel, [0; 3], "{what} neutral reads as free fall");
        let mag = accel
            .iter()
            .map(|&v| (v as f64).powi(2))
            .sum::<f64>()
            .sqrt();
        assert!(mag > 0.0, "{what} neutral has no gravity at all");
    }
}
