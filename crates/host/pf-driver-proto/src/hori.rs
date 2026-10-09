//! Wireless HORIPAD for Steam (`0F0D:01AB`, wired identity): report id, descriptor and serial
//! SDL's and Steam's `steam_hori` driver read. The Linux UHID pad and the Windows driver serve
//! these bytes; the state codec is pf-inject's `hori_proto`. No output reports: no rumble.
//!
//! The descriptor is written, not captured: a Game Pad collection declaring input `0x07` of
//! [`REPORT_LEN`] bytes, the length SDL reads the serial out of.

use crate::gamepad::with_proof;

pub const VENDOR: u16 = 0x0F0D;
pub const PRODUCT: u16 = 0x01AB;
pub const NAME: &str = "Wireless HORIPAD For Steam";
pub const REPORT_ID: u8 = 0x07;
pub const REPORT_LEN: usize = 64;

#[rustfmt::skip]
pub static RDESC: [u8; 91] = [
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x05, // Usage (Game Pad)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x07, //   Report ID (7)
    0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35, // X, Y, Z, Rz
    0x15, 0x00, 0x26, 0xFF, 0x00, // Logical 0..255
    0x75, 0x08, 0x95, 0x04, 0x81, 0x02, // 4 × u8
    0x09, 0x39, // Usage (Hat switch)
    0x15, 0x00, 0x25, 0x07, // Logical 0..7
    0x35, 0x00, 0x46, 0x3B, 0x01, // Physical 0..315
    0x65, 0x14, // Unit (degrees)
    0x75, 0x04, 0x95, 0x01, 0x81, 0x42, // Input (Data,Var,Abs,Null)
    0x65, 0x00, // Unit (None)
    0x05, 0x09, // Usage Page (Button)
    0x19, 0x01, 0x29, 0x14, // Buttons 1..20
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x14, 0x81, 0x02,
    0x05, 0x02, // Usage Page (Simulation Controls)
    0x09, 0xC4, 0x09, 0xC5, // Accelerator (RT), Brake (LT)
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
    0x06, 0x00, 0xFF, // Usage Page (Vendor 0xFF00)
    0x09, 0x20, 0x95, 0x36, 0x81, 0x02, // clock, IMU, battery, serial: 54 bytes
    0xC0,
];
pub static RDESC_WITH_PROOF: [u8; 109] = with_proof(&RDESC);

/// Per-pad serial, bytes 38–43 of every report. SDL prints them in order.
pub const fn serial(pad: u8) -> [u8; 6] {
    [0x50, 0x46, 0x48, 0x52, 0x00, pad]
}

/// Report `0x07` of a pad at rest: centred, flat (gravity on `-accel[1]`), charged.
pub const NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = REPORT_ID;
    r[1] = 0x80;
    r[2] = 0x80;
    r[3] = 0x80;
    r[4] = 0x80;
    r[5] = 0x0F;
    let y = (-4096i16).to_le_bytes();
    r[20] = y[0];
    r[21] = y[1];
    r[24] = 10;
    r
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_variant_extends_the_descriptor() {
        assert_eq!(RDESC_WITH_PROOF[..90], RDESC[..90]);
        assert_eq!(*RDESC_WITH_PROOF.last().unwrap(), 0xC0);
    }
}
