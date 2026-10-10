//! 8BitDo pads in their own HID mode (VID `2DC8`): report ids, descriptors and the capability
//! feature SDL's and Steam's `8bitdo` driver read. The Linux UHID pad and the Windows driver
//! serve these bytes; the state codec is pf-inject's `eightbitdo_proto`.
//!
//! The descriptors are written, not captured: a Game Pad collection declaring the report sizes
//! SDL reads (input `0x01` of [`REPORT_LEN`], output `0x05`, feature `0x06` on the Pro models).

use crate::gamepad::{
    with_proof, DEVTYPE_8BITDO_PRO2, DEVTYPE_8BITDO_PRO3, DEVTYPE_8BITDO_ULTIMATE2,
};

pub const VENDOR: u16 = 0x2DC8;
pub const REPORT_ID: u8 = 0x01;
pub const REPORT_LEN: usize = 34;
/// Output `[0x05, low, high, left trigger, right trigger]`, motors as `u8`.
pub const RUMBLE_ID: u8 = 0x05;
/// Read by SDL from the Pro 2 and Pro 3 at init. Any reply turns on gyro, rumble and battery;
/// byte 13 = `0xAA` declares the µs IMU clock at bytes 27–30.
pub const FEATURE_CAPS: u8 = 0x06;

macro_rules! rdesc_body {
    ($($tail:expr),*) => {
        [
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x05, // Usage (Game Pad)
            0xA1, 0x01, // Collection (Application)
            0x85, 0x01, //   Report ID (1)
            0x09, 0x39, //   Usage (Hat switch)
            0x15, 0x00, //   Logical Minimum (0)
            0x25, 0x07, //   Logical Maximum (7)
            0x35, 0x00, //   Physical Minimum (0)
            0x46, 0x3B, 0x01, // Physical Maximum (315)
            0x65, 0x14, //   Unit (degrees)
            0x75, 0x04, //   Report Size (4)
            0x95, 0x01, //   Report Count (1)
            0x81, 0x42, //   Input (Data,Var,Abs,Null)
            0x65, 0x00, //   Unit (None)
            0x81, 0x03, //   Input (Const) — hat byte's high nibble
            0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, // X, Y, Z, Rz
            0x15, 0x00, 0x26, 0xFF, 0x00, // Logical 0..255
            0x35, 0x00, 0x46, 0xFF, 0x00, // Physical 0..255
            0x75, 0x08, 0x95, 0x04, 0x81, 0x02, // 4 × u8
            0x05, 0x02, // Usage Page (Simulation Controls)
            0x09, 0xC4, 0x09, 0xC5, // Accelerator (RT), Brake (LT)
            0x95, 0x02, 0x81, 0x02, // 2 × u8
            0x05, 0x09, // Usage Page (Button)
            0x19, 0x01, 0x29, 0x18, // Buttons 1..24
            0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x18, 0x81, 0x02,
            0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
            0x09, 0x20, 0x15, 0x00, 0x26, 0xFF, 0x00,
            0x75, 0x08, 0x95, 0x17, 0x81, 0x02, // battery, IMU, clock: 23 bytes
            0x85, 0x05, 0x09, 0x21, 0x95, 0x04, 0x91, 0x02, // Output 0x05: 4 bytes
            $($tail,)*
            0xC0,
        ]
    };
}

/// Ultimate 2: input `0x01`, output `0x05`.
pub static RDESC: [u8; 106] = rdesc_body!();
/// Pro 2 and Pro 3: also feature `0x06`, 13 bytes after the id.
pub static RDESC_CAPS: [u8; 114] = rdesc_body!(0x85, 0x06, 0x09, 0x22, 0x95, 0x0D, 0xB1, 0x02);
pub static RDESC_WITH_PROOF: [u8; 124] = with_proof(&RDESC);
pub static RDESC_CAPS_WITH_PROOF: [u8; 132] = with_proof(&RDESC_CAPS);

/// Whether this identity answers [`FEATURE_CAPS`] and stamps the IMU clock.
pub const fn has_caps(device_type: u8) -> bool {
    matches!(device_type, DEVTYPE_8BITDO_PRO2 | DEVTYPE_8BITDO_PRO3)
}

/// The descriptor a pad of `device_type` presents; `proof` adds the Windows channel feature.
pub fn rdesc(device_type: u8, proof: bool) -> &'static [u8] {
    match (has_caps(device_type), proof) {
        (false, false) => &RDESC,
        (true, false) => &RDESC_CAPS,
        (false, true) => &RDESC_WITH_PROOF,
        (true, true) => &RDESC_CAPS_WITH_PROOF,
    }
}

/// Per-pad MAC, most significant octet first. The identity byte keeps an Ultimate 2 and a
/// Pro 2 at the same index apart.
pub const fn mac(device_type: u8, pad: u8) -> [u8; 6] {
    [0x50, 0x46, 0x38, 0x42, device_type, pad]
}

/// [`FEATURE_CAPS`] reply. SDL reads bytes 5–10 as the MAC, least significant octet first, and
/// `0xAA` at 13 as the clock declaration.
pub const fn caps_reply(device_type: u8, pad: u8) -> [u8; 14] {
    let m = mac(device_type, pad);
    [
        FEATURE_CAPS,
        0,
        0,
        0,
        0,
        m[5],
        m[4],
        m[3],
        m[2],
        m[1],
        m[0],
        0,
        0,
        0xAA,
    ]
}

/// Report `0x01` of a pad at rest: centred, unpressed, charged, 1 g on accel Z.
pub const NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = REPORT_ID;
    r[1] = 0x0F;
    r[2] = 0x7F;
    r[3] = 0x7F;
    r[4] = 0x7F;
    r[5] = 0x7F;
    r[14] = 100;
    let z = 4096i16.to_le_bytes();
    r[19] = z[0];
    r[20] = z[1];
    r
};

/// Every model this module names, for tables that must cover all of them.
pub const DEVTYPES: [u8; 3] = [
    DEVTYPE_8BITDO_ULTIMATE2,
    DEVTYPE_8BITDO_PRO2,
    DEVTYPE_8BITDO_PRO3,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The proof variants are the plain descriptors plus the one feature `0x85`.
    #[test]
    fn proof_variants_extend_the_plain_descriptors() {
        for (plain, proof) in [
            (&RDESC[..], &RDESC_WITH_PROOF[..]),
            (&RDESC_CAPS[..], &RDESC_CAPS_WITH_PROOF[..]),
        ] {
            assert_eq!(proof[..plain.len() - 1], plain[..plain.len() - 1]);
            assert_eq!(*proof.last().unwrap(), 0xC0);
            let ids = proof.windows(2).filter(|w| w == &[0x85, 0x85]).count();
            assert_eq!(ids, 1, "one Report ID (0x85)");
        }
    }

    #[test]
    fn caps_reply_carries_the_mac_and_the_clock() {
        let r = caps_reply(DEVTYPE_8BITDO_PRO2, 3);
        assert_eq!(r[0], FEATURE_CAPS);
        // SDL prints data[10]..data[5] as the serial: the MAC, most significant first.
        assert_eq!(
            [r[10], r[9], r[8], r[7], r[6], r[5]],
            mac(DEVTYPE_8BITDO_PRO2, 3)
        );
        assert_eq!(r[13], 0xAA);
    }
}
