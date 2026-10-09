//! Steam Deck controller interface, served by `pf-gamepad` for device type 3 and by `pf-inject`
//! over the Linux gadget and usbip transports. Its one feature report is unnumbered and Steam
//! drives it as command and response.

use crate::triton::{ID_GET_ATTRIBUTES_VALUES, ID_GET_STRING_ATTRIBUTE};

/// Captured controller-interface report descriptor (interface 2 of a real `28DE:1205`): one
/// vendor page `0xFFFF` collection with a 64-byte input and a 64-byte feature report.
#[rustfmt::skip]
pub static RDESC: [u8; 38] = [
    0x06, 0xff, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x02, 0x09, 0x03, 0x15, 0x00, 0x26, 0xff, 0x00,
    0x75, 0x08, 0x95, 0x40, 0x81, 0x02, 0x09, 0x06, 0x09, 0x07, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75,
    0x08, 0x95, 0x40, 0xb1, 0x02, 0xc0,
];

/// Unnumbered input frame at rest: header `[0x01, 0x00, ID_CONTROLLER_DECK_STATE, 64]`, every
/// control released. SDL drops a Deck frame whose length byte is not 64.
pub const NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01;
    r[2] = 0x09;
    r[3] = 0x40;
    r
};

/// GET_FEATURE reply to the latched SET_FEATURE `last_set` (command byte first). `0x83` answers
/// the captured attribute table, `0xAE` answers `serial` under the attribute asked for, and any
/// other command echoes `last_set`. An empty `last_set` reads as a unit-serial query.
///
/// Steam accepts the pad only when the reply answers the command it last set. `serial` is
/// [`crate::gamepad::pad_serial`]: Steam rejects a `PF`-leading unit serial and mangles the
/// pad's name, so ours start `FVPF`.
pub fn feature_reply(last_set: &[u8], serial: &str) -> [u8; 64] {
    const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;
    let cmd = last_set.first().copied().unwrap_or(ID_GET_STRING_ATTRIBUTE);
    let mut r = [0u8; 64];
    match cmd {
        ID_GET_ATTRIBUTES_VALUES => {
            // [0x83, 0x2D, then 9 × (attribute id, u32 LE)]. Ids per SDL's
            // controller_constants.h; 0x04 and 0x0A are build times that must look like real
            // dates. Per-pad uniqueness rides the serial.
            r[0] = ID_GET_ATTRIBUTES_VALUES;
            r[1] = 0x2D;
            let attrs: [(u8, u32); 9] = [
                (0x01, 0x1205),      // ATTRIB_PRODUCT_ID
                (0x02, 0),           // ATTRIB_CAPABILITIES
                (0x0A, 0x6408_9000), // ATTRIB_BOOTLOADER_BUILD_TIME (2023-03-08)
                (0x04, 0x66A8_C000), // ATTRIB_FIRMWARE_BUILD_TIME (2024-07-30)
                (0x09, 0x2E),        // ATTRIB_BOARD_REVISION (captured)
                (0x0B, 0x0FA0),      // ATTRIB_CONNECTION_INTERVAL_IN_US (4 ms)
                (0x0D, 0),
                (0x0C, 0),
                (0x0E, 0),
            ];
            let mut o = 2;
            for (id, val) in attrs {
                r[o] = id;
                r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                o += 5;
            }
        }
        ID_GET_STRING_ATTRIBUTE => {
            // [0xAE, len, attr, ascii…]. Steam asks for the board (0x00) and unit (0x01)
            // serials; both get the unit serial. Steam logs any board serial as invalid, and
            // that line is benign.
            let attr = last_set.get(2).copied().unwrap_or(ATTRIB_STR_UNIT_SERIAL);
            let b = serial.as_bytes();
            let len = b.len().clamp(1, 20);
            r[0] = ID_GET_STRING_ATTRIBUTE;
            r[1] = len as u8;
            r[2] = attr;
            r[3..3 + len].copy_from_slice(&b[..len]);
        }
        _ => {
            let n = last_set.len().min(64);
            r[..n].copy_from_slice(&last_set[..n]);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad;

    /// The Deck answers the command Steam last set: `0x83` with build-time attributes, `0xAE`
    /// with the unit serial under the requested attribute, anything else as an echo.
    #[test]
    fn deck_feature_reply_answers_the_latched_command() {
        let serial = gamepad::pad_serial(gamepad::DEVTYPE_STEAMDECK, 0);
        let attr = |r: &[u8; 64], slot: usize| {
            let o = 2 + slot * 5;
            (
                r[o],
                u32::from_le_bytes(r[o + 1..o + 5].try_into().unwrap()),
            )
        };
        let r = feature_reply(&[0x83], &serial);
        assert_eq!(r[..2], [0x83, 0x2D]);
        assert_eq!(attr(&r, 0), (0x01, 0x1205));
        assert_eq!(attr(&r, 2), (0x0A, 0x6408_9000), "bootloader build time");
        assert_eq!(attr(&r, 3), (0x04, 0x66A8_C000), "firmware build time");

        for requested in [0x00, 0x01] {
            let r = feature_reply(&[0xAE, 0x00, requested], &serial);
            assert_eq!(r[..3], [0xAE, serial.len() as u8, requested]);
            assert_eq!(&r[3..3 + serial.len()], serial.as_bytes());
        }
        assert_eq!(feature_reply(&[], &serial)[..3], [0xAE, 12, 0x01]);

        // The driver latches a zeroed 64-byte buffer; before any SET it echoes zeros.
        assert_eq!(feature_reply(&[0u8; 64], &serial), [0u8; 64]);
        let settings = [0x87, 0x03, 0x08, 0x07, 0x00];
        assert_eq!(feature_reply(&settings, &serial)[..5], settings);
    }
}
