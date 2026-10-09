//! Xbox Bluetooth HID report descriptors, served by `pf-gamepad` for device types 4–6 and packed
//! by `pf-inject`'s `xbox_proto`.
//!
//! [`SERIES_RDESC`] is the Series X|S pad (`045E:0B13`). Its input report matches the Linux
//! capture of a real one (xpadneo `docs/descriptors/xbxs.md`) byte for byte; the triggers are
//! declared as 16-bit fields where the pad declares 10 bits plus 6 of padding. One S and Elite
//! have no Share button, so [`NO_SHARE_RDESC`] is the same bytes with [`SHARE_ITEMS`] cut out.
//! Identity beyond that is VID/PID, not descriptor shape.
//!
//! Two blocks the capture lacks. Output `0x03` is the Bluetooth rumble report
//! (`[id][enable][lt][rt][left][right][duration][delay][loop]`, magnitudes 0..100). Feature
//! `0x85` is the sealed channel's proof transport: without it hidclass refuses the host's
//! `HidD_GetFeature` and the pad serves neutral forever. Both come after the last Input item and
//! restate every global they use, so they cannot shift the input layout.

/// Offset of [`SHARE_ITEMS`] in [`SERIES_RDESC`]: right after the button padding bit.
pub const SHARE_AT: usize = 131;
/// Consumer `Record` (0x0C/0xB2), one bit plus seven of padding: Share is bit 0 of byte 16.
#[rustfmt::skip]
pub const SHARE_ITEMS: [u8; 25] = [
    0x05, 0x0C, 0x0A, 0xB2, 0x00, 0x15, 0x00, 0x25, 0x01, 0x95, 0x01, 0x75, 0x01, 0x81, 0x02,
    0x15, 0x00, 0x25, 0x00, 0x75, 0x07, 0x95, 0x01, 0x81, 0x03,
];
/// Input report 0x01 on the wire, id included: sticks 8, triggers 4, hat 1, buttons 2, Share 1.
pub const SERIES_INPUT_LEN: usize = 17;
/// [`SERIES_INPUT_LEN`] without the Share byte.
pub const NO_SHARE_INPUT_LEN: usize = 16;

/// Whether `device_type` is the Series pad, the one identity with a Share button.
pub const fn has_share(device_type: u8) -> bool {
    device_type == crate::gamepad::DEVTYPE_XBOX
}

/// Wire length of the input report the identity's descriptor declares.
pub const fn input_len(device_type: u8) -> usize {
    if has_share(device_type) {
        SERIES_INPUT_LEN
    } else {
        NO_SHARE_INPUT_LEN
    }
}

/// Input report `0x01` at rest: sticks centred, triggers 0, hat 0 (the NULL state; the logical
/// range starts at 1), no buttons or Share. A pad serves only its [`input_len`] bytes.
pub const NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01;
    // LX 0x8000 LE. The Y axes are inverted (+y is up on the wire, down in HID), and mirroring
    // an even-sized range centres one unit low: LY = RY = 0x7FFF.
    r[2] = 0x80;
    r[3] = 0xFF;
    r[4] = 0x7F;
    r[6] = 0x80; // RX
    r[7] = 0xFF;
    r[8] = 0x7F;
    r
};

/// Right stick is `Z`/`Rz`: `xinputhid` maps those to the right stick and ignores `Rx`/`Ry`.
#[rustfmt::skip]
pub static SERIES_RDESC: [u8; 248] = [
    0x05, 0x01,                    // Usage Page (Generic Desktop)
    0x09, 0x05,                    // Usage (Game Pad)
    0xA1, 0x01,                    // Collection (Application)
    0x85, 0x01,                    //   Report ID (1)
    0x09, 0x01,                    //   Usage (Pointer)
    0xA1, 0x00,                    //   Collection (Physical)
    0x09, 0x30,                    //     Usage (X)          — left stick X
    0x09, 0x31,                    //     Usage (Y)          — left stick Y
    0x15, 0x00,                    //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00,  //     Logical Maximum (65535)
    0x95, 0x02,                    //     Report Count (2)
    0x75, 0x10,                    //     Report Size (16)
    0x81, 0x02,                    //     Input (Data,Var,Abs)
    0xC0,                          //   End Collection
    0x09, 0x01,                    //   Usage (Pointer)
    0xA1, 0x00,                    //   Collection (Physical)
    0x09, 0x32,                    //     Usage (Z)          — right stick X
    0x09, 0x35,                    //     Usage (Rz)         — right stick Y
    0x15, 0x00,                    //     Logical Minimum (0)
    0x27, 0xFF, 0xFF, 0x00, 0x00,  //     Logical Maximum (65535)
    0x95, 0x02,                    //     Report Count (2)
    0x75, 0x10,                    //     Report Size (16)
    0x81, 0x02,                    //     Input (Data,Var,Abs)
    0xC0,                          //   End Collection
    0x05, 0x02,                    //   Usage Page (Simulation Controls)
    0x09, 0xC5,                    //   Usage (Brake)        — left trigger
    0x15, 0x00,                    //   Logical Minimum (0)
    0x26, 0xFF, 0x03,              //   Logical Maximum (1023)
    0x95, 0x01,                    //   Report Count (1)
    0x75, 0x10,                    //   Report Size (16)
    0x81, 0x02,                    //   Input (Data,Var,Abs)
    0x09, 0xC4,                    //   Usage (Accelerator)  — right trigger
    0x15, 0x00,                    //   Logical Minimum (0)
    0x26, 0xFF, 0x03,              //   Logical Maximum (1023)
    0x95, 0x01,                    //   Report Count (1)
    0x75, 0x10,                    //   Report Size (16)
    0x81, 0x02,                    //   Input (Data,Var,Abs)
    0x05, 0x01,                    //   Usage Page (Generic Desktop)
    0x09, 0x39,                    //   Usage (Hat switch)
    0x15, 0x01,                    //   Logical Minimum (1)
    0x25, 0x08,                    //   Logical Maximum (8)
    0x35, 0x00,                    //   Physical Minimum (0)
    0x46, 0x3B, 0x01,              //   Physical Maximum (315)
    0x65, 0x14,                    //   Unit (Eng Rot: Degrees)
    0x75, 0x04,                    //   Report Size (4)
    0x95, 0x01,                    //   Report Count (1)
    0x81, 0x42,                    //   Input (Data,Var,Abs,Null State)
    0x65, 0x00,                    //   Unit (None)
    0x75, 0x04,                    //   Report Size (4)
    0x95, 0x01,                    //   Report Count (1)
    0x81, 0x03,                    //   Input (Cnst,Var,Abs) — pad the hat byte
    0x05, 0x09,                    //   Usage Page (Button)
    0x19, 0x01,                    //   Usage Minimum (Button 1)
    0x29, 0x0F,                    //   Usage Maximum (Button 15)
    0x15, 0x00,                    //   Logical Minimum (0)
    0x25, 0x01,                    //   Logical Maximum (1)
    0x75, 0x01,                    //   Report Size (1)
    0x95, 0x0F,                    //   Report Count (15)
    0x81, 0x02,                    //   Input (Data,Var,Abs)
    0x75, 0x01,                    //   Report Size (1)
    0x95, 0x01,                    //   Report Count (1)
    0x81, 0x03,                    //   Input (Cnst,Var,Abs) — pad to a byte boundary
    0x05, 0x0C,                    //   Usage Page (Consumer)          ┐ SHARE_ITEMS
    0x0A, 0xB2, 0x00,              //   Usage (Record)       — Share   │
    0x15, 0x00,                    //   Logical Minimum (0)            │
    0x25, 0x01,                    //   Logical Maximum (1)            │
    0x95, 0x01,                    //   Report Count (1)               │
    0x75, 0x01,                    //   Report Size (1)                │
    0x81, 0x02,                    //   Input (Data,Var,Abs)           │
    0x15, 0x00,                    //   Logical Minimum (0)            │
    0x25, 0x00,                    //   Logical Maximum (0)            │
    0x75, 0x07,                    //   Report Size (7)                │
    0x95, 0x01,                    //   Report Count (1)               │
    0x81, 0x03,                    //   Input (Cnst,Var,Abs) — pad     ┘
    0x05, 0x0F,                    //   Usage Page (Physical Interface Device)
    0x09, 0x21,                    //   Usage (Set Effect Report)
    0x85, 0x03,                    //   Report ID (3)
    0xA1, 0x02,                    //   Collection (Logical)
    0x09, 0x97,                    //     Usage (DC Enable Actuators)
    0x15, 0x00,                    //     Logical Minimum (0)
    0x25, 0x01,                    //     Logical Maximum (1)
    0x75, 0x04,                    //     Report Size (4)
    0x95, 0x01,                    //     Report Count (1)
    0x91, 0x02,                    //     Output (Data,Var,Abs) — the enable mask, low nibble
    0x15, 0x00,                    //     Logical Minimum (0)
    0x25, 0x00,                    //     Logical Maximum (0)
    0x75, 0x04,                    //     Report Size (4)
    0x95, 0x01,                    //     Report Count (1)
    0x91, 0x03,                    //     Output (Cnst,Var,Abs) — pad the enable byte
    0x09, 0x70,                    //     Usage (Magnitude)
    0x15, 0x00,                    //     Logical Minimum (0)
    0x25, 0x64,                    //     Logical Maximum (100) — percent, NOT 255
    0x75, 0x08,                    //     Report Size (8)
    0x95, 0x04,                    //     Report Count (4) — LT, RT, left handle, right handle
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0x09, 0x50,                    //     Usage (Duration)
    0x66, 0x01, 0x10,              //     Unit (SI Linear: seconds)
    0x55, 0x0E,                    //     Unit Exponent (-2) — centiseconds
    0x15, 0x00,                    //     Logical Minimum (0)
    0x26, 0xFF, 0x00,              //     Logical Maximum (255)
    0x75, 0x08,                    //     Report Size (8)
    0x95, 0x01,                    //     Report Count (1)
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0x09, 0xA7,                    //     Usage (Start Delay) — same unit and range as Duration
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0x65, 0x00,                    //     Unit (None)
    0x55, 0x00,                    //     Unit Exponent (0)
    0x09, 0x7C,                    //     Usage (Loop Count)
    0x91, 0x02,                    //     Output (Data,Var,Abs)
    0xC0,                          //   End Collection
    0x06, 0x00, 0xFF,              //   Usage Page (Vendor Defined 0xFF00)
    0x85, 0x85,                    //   Report ID (0x85)
    0x09, 0x2D,                    //   Usage (0x2D) — the id the PS descriptors use for it
    0x15, 0x00,                    //   Logical Minimum (0)
    0x26, 0xFF, 0x00,              //   Logical Maximum (255)
    0x75, 0x08,                    //   Report Size (8)
    0x95, 0x3F,                    //   Report Count (63) — 1 id + 63 = 64 = FeatureReportByteLength
    0xB1, 0x02,                    //   Feature (Data,Var,Abs)
    0xC0,                          // End Collection
];

/// One S and Elite: [`SERIES_RDESC`] without [`SHARE_ITEMS`].
pub static NO_SHARE_RDESC: [u8; 223] = {
    let mut out = [0u8; 223];
    let mut i = 0;
    while i < out.len() {
        let from = if i < SHARE_AT {
            i
        } else {
            i + SHARE_ITEMS.len()
        };
        out[i] = SERIES_RDESC[from];
        i += 1;
    }
    out
};

const _: () = {
    let mut i = 0;
    while i < SHARE_ITEMS.len() {
        assert!(SERIES_RDESC[SHARE_AT + i] == SHARE_ITEMS[i]);
        i += 1;
    }
};

/// The descriptor `device_type` serves.
pub fn rdesc(device_type: u8) -> &'static [u8] {
    if has_share(device_type) {
        &SERIES_RDESC
    } else {
        &NO_SHARE_RDESC
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad;

    /// Cutting the Consumer `Record` block out of the Series descriptor leaves exactly the one
    /// One S and Elite serve.
    #[test]
    fn xbox_no_share_rdesc_is_the_series_one_without_share() {
        let series = SERIES_RDESC.to_vec();
        let at = series
            .windows(SHARE_ITEMS.len())
            .position(|w| w == SHARE_ITEMS)
            .expect("Share block in the Series descriptor");
        assert_eq!(at, SHARE_AT);
        let mut cut = series.clone();
        cut.drain(at..at + SHARE_ITEMS.len());
        assert_eq!(cut, NO_SHARE_RDESC.to_vec());
        assert_eq!(rdesc(gamepad::DEVTYPE_XBOX), &SERIES_RDESC[..]);
        for dt in [gamepad::DEVTYPE_XBOX_ONE_S, gamepad::DEVTYPE_XBOX_ELITE] {
            assert_eq!(rdesc(dt), &NO_SHARE_RDESC[..]);
        }
    }

    #[test]
    fn xbox_input_lengths_match_their_descriptors() {
        use crate::rdesc::{report_lens, INPUT};
        assert_eq!(report_lens(&SERIES_RDESC)[&(INPUT, 1)], SERIES_INPUT_LEN);
        assert_eq!(
            report_lens(&NO_SHARE_RDESC)[&(INPUT, 1)],
            NO_SHARE_INPUT_LEN
        );
        assert_eq!(input_len(gamepad::DEVTYPE_XBOX), 17);
        assert_eq!(input_len(gamepad::DEVTYPE_XBOX_ELITE), 16);
    }
}
