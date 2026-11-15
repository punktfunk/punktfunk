//! Virtual Precision Touchpad layout (host ↔ UMDF HID minidriver `pf_touchpad`).
//!
//! One devnode per session presents the host's virtual pad (320 × 240 mm at 100 units/mm, the
//! host's `TOUCHPAD_PAD_*`) as a Windows Precision Touchpad: a Touch Pad collection with five
//! contacts, Scan Time, Contact Count and Button 1; a Configuration collection; and the
//! capability and certification features Windows reads before it accepts the device.
//!
//! Same sealed-pad handshake as [`mouse`](crate::mouse): [`touchpad_boot_name`], its own DATA
//! magic, reports in order through [`TouchpadShm::ring`], and a vendor doorbell collection
//! ([`DOORBELL_REPORT_ID`]) that makes the driver drain at once. The report shapes follow
//! Microsoft's PTP sample descriptor; `design/touchpad-passthrough.md` §3 is the contract.

use alloc::string::String;
use bytemuck::{Pod, Zeroable};

/// Touchpad DATA-section magic ("PFTP" LE), distinct from the pad and mouse magics.
pub const TOUCHPAD_MAGIC: u32 = u32::from_le_bytes(*b"PFTP");

/// `Global\pftouchpad-boot-<index>`, the touchpad's bootstrap mailbox.
pub fn touchpad_boot_name(index: u8) -> String {
    alloc::format!("Global\\pftouchpad-boot-{index}")
}

/// HID identity "PF" / "TP": obviously virtual, like the mouse's.
pub const TOUCHPAD_VID: u16 = 0x5046;
pub const TOUCHPAD_PID: u16 = 0x5450;
pub const TOUCHPAD_VER: u16 = 0x0100;

/// Contacts one report carries; more in a frame makes Windows drop the frame.
pub const TOUCHPAD_MAX_CONTACTS: usize = 5;
/// Logical X and Y maxima: the host's virtual pad in 0.01 mm.
pub const PAD_MAX_X: u16 = 32000;
pub const PAD_MAX_Y: u16 = 24000;

/// `[id] + 5 × [conf|tip<<1|contact_id<<2, x LE, y LE] + [scan_time LE, count, buttons]`.
pub const TOUCHPAD_REPORT_ID: u8 = 0x01;
pub const TOUCHPAD_REPORT_LEN: usize = 1 + TOUCHPAD_MAX_CONTACTS * CONTACT_LEN + 4;
const CONTACT_LEN: usize = 5;
/// Feature `[id, max_contacts | pad_type << 4]`: five contacts, a click-pad (type 0).
pub const CAPS_REPORT_ID: u8 = 0x02;
pub const CAPS_FEATURE: [u8; 2] = [CAPS_REPORT_ID, TOUCHPAD_MAX_CONTACTS as u8];
/// Feature `[id] + 256 bytes` of certification status. Windows ignores a touchpad without it.
pub const HQA_REPORT_ID: u8 = 0x03;
pub const HQA_FEATURE_LEN: usize = 1 + PTPHQA_BLOB.len();
/// Configuration collection, feature `[id, mode]`. Windows writes [`INPUT_MODE_PTP`].
pub const INPUT_MODE_REPORT_ID: u8 = 0x04;
pub const INPUT_MODE_PTP: u8 = 3;
/// Configuration collection, feature `[id, surface | button << 1]`.
pub const SWITCH_REPORT_ID: u8 = 0x05;
/// The vendor collection's output report `[id, 0]`. It carries nothing: the driver drains the
/// ring, so a writer can at most make it drain sooner.
pub const DOORBELL_REPORT_ID: u8 = 0x06;
pub const DOORBELL_USAGE_PAGE: u16 = 0xFF00;
pub const DOORBELL_REPORT_LEN: usize = 2;

/// One contact as the report carries it. `id` is the report's Contact ID, `0..5`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtpContact {
    pub id: u8,
    pub tip: bool,
    pub confident: bool,
    pub x: u16,
    pub y: u16,
}

/// Input report ([`TOUCHPAD_REPORT_ID`]). Contact Count is the number of `contacts`, at most
/// [`TOUCHPAD_MAX_CONTACTS`] (the rest are dropped); axes clamp to the pad. `scan_time` is in
/// 100 µs and wraps.
#[must_use]
pub fn ptp_report(
    contacts: &[PtpContact],
    scan_time: u16,
    button: bool,
) -> [u8; TOUCHPAD_REPORT_LEN] {
    let mut r = [0u8; TOUCHPAD_REPORT_LEN];
    r[0] = TOUCHPAD_REPORT_ID;
    let n = contacts.len().min(TOUCHPAD_MAX_CONTACTS);
    for (i, c) in contacts[..n].iter().enumerate() {
        let o = 1 + i * CONTACT_LEN;
        r[o] = u8::from(c.confident) | u8::from(c.tip) << 1 | (c.id & 0x3F) << 2;
        r[o + 1..o + 3].copy_from_slice(&c.x.min(PAD_MAX_X).to_le_bytes());
        r[o + 3..o + 5].copy_from_slice(&c.y.min(PAD_MAX_Y).to_le_bytes());
    }
    let t = 1 + TOUCHPAD_MAX_CONTACTS * CONTACT_LEN;
    r[t..t + 2].copy_from_slice(&scan_time.to_le_bytes());
    r[t + 2] = n as u8;
    r[t + 3] = u8::from(button);
    r
}

/// The certification feature: [`HQA_REPORT_ID`] then [`PTPHQA_BLOB`].
#[must_use]
pub fn hqa_feature() -> [u8; HQA_FEATURE_LEN] {
    let mut r = [0u8; HQA_FEATURE_LEN];
    r[0] = HQA_REPORT_ID;
    r[1..].copy_from_slice(&PTPHQA_BLOB);
    r
}

/// Ring slots; a full ring means the driver stopped taking reports.
pub const TOUCHPAD_RING_LEN: usize = 64;
/// Bytes per slot: [`TOUCHPAD_REPORT_LEN`] rounded up.
pub const TOUCHPAD_RING_SLOT: usize = 32;

/// Section offset of ring slot `seq` (any wrapping sequence number).
#[must_use]
pub const fn ring_slot_off(seq: u32) -> usize {
    core::mem::offset_of!(TouchpadShm, ring)
        + (seq as usize % TOUCHPAD_RING_LEN) * TOUCHPAD_RING_SLOT
}

/// The shared section. The host writes slot `ring_head`, then bumps `ring_head`; the driver
/// hands each slot to a pended `READ_REPORT` and bumps `ring_tail`. Idle sends nothing.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct TouchpadShm {
    pub magic: u32,
    /// [`crate::gamepad::GAMEPAD_PROTO_VERSION`] while attached. `0` = no driver.
    pub driver_proto: u32,
    /// Bumped each driver timer tick, whether or not input flows.
    pub driver_heartbeat: u32,
    /// Device index, host-stamped before the magic; the driver checks it against its Location.
    pub pad_index: u32,
    /// Reports the host published (wrapping).
    pub ring_head: u32,
    /// Reports the driver handed to Windows (wrapping). The host writes only while
    /// `head - tail < TOUCHPAD_RING_LEN`.
    pub ring_tail: u32,
    /// The Input Mode Windows last set; [`INPUT_MODE_PTP`] once it runs the device as a PTP.
    pub input_mode: u32,
    pub _reserved: u32,
    pub ring: [[u8; TOUCHPAD_RING_SLOT]; TOUCHPAD_RING_LEN],
}

// Offsets are the cross-process wire contract: pin every one.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<TouchpadShm>() == 32 + TOUCHPAD_RING_LEN * TOUCHPAD_RING_SLOT);
    assert!(offset_of!(TouchpadShm, magic) == 0);
    assert!(offset_of!(TouchpadShm, driver_proto) == 4);
    assert!(offset_of!(TouchpadShm, driver_heartbeat) == 8);
    assert!(offset_of!(TouchpadShm, pad_index) == 12);
    assert!(offset_of!(TouchpadShm, ring_head) == 16);
    assert!(offset_of!(TouchpadShm, ring_tail) == 20);
    assert!(offset_of!(TouchpadShm, input_mode) == 24);
    assert!(offset_of!(TouchpadShm, ring) == 32);
    assert!(TOUCHPAD_REPORT_LEN <= TOUCHPAD_RING_SLOT);
};

const fn lo(v: u16) -> u8 {
    v.to_le_bytes()[0]
}
const fn hi(v: u16) -> u8 {
    v.to_le_bytes()[1]
}

/// One contact. Each resets the units it set, so the next main item starts clean.
#[rustfmt::skip]
const FINGER: [u8; 67] = [
    0x05, 0x0D,        // Usage Page (Digitizers)
    0x09, 0x22,        // Usage (Finger)
    0xA1, 0x02,        // Collection (Logical)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x95, 0x02,        //   Report Count (2)
    0x09, 0x47,        //   Usage (Confidence)
    0x09, 0x42,        //   Usage (Tip Switch)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x25, TOUCHPAD_MAX_CONTACTS as u8 - 1, // Logical Maximum (4)
    0x75, 0x06,        //   Report Size (6)
    0x95, 0x01,        //   Report Count (1)
    0x09, 0x51,        //   Usage (Contact Identifier)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x05, 0x01,        //   Usage Page (Generic Desktop)
    0x75, 0x10,        //   Report Size (16)
    0x55, 0x0E,        //   Unit Exponent (-2)
    0x65, 0x11,        //   Unit (SI Linear: cm) — physical in 0.1 mm
    0x35, 0x00,        //   Physical Minimum (0)
    0x26, lo(PAD_MAX_X), hi(PAD_MAX_X), // Logical Maximum (32000)
    0x46, 0x80, 0x0C,  //   Physical Maximum (3200 = 320 mm)
    0x09, 0x30,        //   Usage (X)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x26, lo(PAD_MAX_Y), hi(PAD_MAX_Y), // Logical Maximum (24000)
    0x46, 0x60, 0x09,  //   Physical Maximum (2400 = 240 mm)
    0x09, 0x31,        //   Usage (Y)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x45, 0x00,        //   Physical Maximum (0)
    0x55, 0x00,        //   Unit Exponent (0)
    0x65, 0x00,        //   Unit (None)
    0xC0,              // End Collection
];

#[rustfmt::skip]
const HEAD: [u8; 8] = [
    0x05, 0x0D,        // Usage Page (Digitizers)
    0x09, 0x05,        // Usage (Touch Pad)
    0xA1, 0x01,        // Collection (Application)
    0x85, TOUCHPAD_REPORT_ID,
];

/// The rest of the Touch Pad collection, then Configuration, then the vendor doorbell.
#[rustfmt::skip]
const TAIL: [u8; 155] = [
    0x55, 0x0C,        //   Unit Exponent (-4)
    0x66, 0x01, 0x10,  //   Unit (SI Linear: s) — 100 µs
    0x47, 0xFF, 0xFF, 0x00, 0x00, // Physical Maximum (65535)
    0x27, 0xFF, 0xFF, 0x00, 0x00, // Logical Maximum (65535)
    0x75, 0x10,        //   Report Size (16)
    0x95, 0x01,        //   Report Count (1)
    0x05, 0x0D,        //   Usage Page (Digitizers)
    0x09, 0x56,        //   Usage (Scan Time)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x45, 0x00,        //   Physical Maximum (0)
    0x55, 0x00,        //   Unit Exponent (0)
    0x65, 0x00,        //   Unit (None)
    0x25, TOUCHPAD_MAX_CONTACTS as u8, // Logical Maximum (5)
    0x75, 0x08,        //   Report Size (8)
    0x09, 0x54,        //   Usage (Contact Count)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x05, 0x09,        //   Usage Page (Button)
    0x09, 0x01,        //   Usage (Button 1)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x81, 0x02,        //   Input (Data,Var,Abs)
    0x95, 0x07,        //   Report Count (7)
    0x81, 0x03,        //   Input (Const)
    0x05, 0x0D,        //   Usage Page (Digitizers)
    0x85, CAPS_REPORT_ID,
    0x09, 0x55,        //   Usage (Contact Count Maximum)
    0x09, 0x59,        //   Usage (Pad Type)
    0x25, 0x0F,        //   Logical Maximum (15)
    0x75, 0x04,        //   Report Size (4)
    0x95, 0x02,        //   Report Count (2)
    0xB1, 0x02,        //   Feature (Data,Var,Abs)
    0x06, 0x00, 0xFF,  //   Usage Page (Vendor 0xFF00)
    0x85, HQA_REPORT_ID,
    0x09, 0xC5,        //   Usage (PTPHQA certification)
    0x26, 0xFF, 0x00,  //   Logical Maximum (255)
    0x75, 0x08,        //   Report Size (8)
    0x96, 0x00, 0x01,  //   Report Count (256)
    0xB1, 0x02,        //   Feature (Data,Var,Abs)
    0xC0,              // End Collection
    0x05, 0x0D,        // Usage Page (Digitizers)
    0x09, 0x0E,        // Usage (Configuration)
    0xA1, 0x01,        // Collection (Application)
    0x85, INPUT_MODE_REPORT_ID,
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x02,        //   Collection (Logical)
    0x09, 0x52,        //     Usage (Input Mode)
    0x25, 0x0A,        //     Logical Maximum (10)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0xB1, 0x02,        //     Feature (Data,Var,Abs)
    0xC0,              //   End Collection
    0x09, 0x22,        //   Usage (Finger)
    0xA1, 0x00,        //   Collection (Physical)
    0x85, SWITCH_REPORT_ID,
    0x09, 0x57,        //     Usage (Surface Switch)
    0x09, 0x58,        //     Usage (Button Switch)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x02,        //     Report Count (2)
    0xB1, 0x02,        //     Feature (Data,Var,Abs)
    0x95, 0x06,        //     Report Count (6)
    0xB1, 0x03,        //     Feature (Const)
    0xC0,              //   End Collection
    0xC0,              // End Collection
    0x06, 0x00, 0xFF,  // Usage Page (Vendor 0xFF00)
    0x09, 0x01,        // Usage (1)
    0xA1, 0x01,        // Collection (Application)
    0x85, DOORBELL_REPORT_ID,
    0x09, 0x01,        //   Usage (1)
    0x26, 0xFF, 0x00,  //   Logical Maximum (255)
    0x75, 0x08,        //   Report Size (8)
    0x95, 0x01,        //   Report Count (1)
    0x91, 0x02,        //   Output (Data,Var,Abs) — doorbell
    0xC0,              // End Collection
];

pub const TOUCHPAD_RDESC_LEN: usize = 498;
const _: () =
    assert!(TOUCHPAD_RDESC_LEN == HEAD.len() + TOUCHPAD_MAX_CONTACTS * FINGER.len() + TAIL.len());

/// HID report descriptor: Touch Pad (input 0x01, features 0x02 and 0x03), Configuration
/// (features 0x04 and 0x05) and the vendor doorbell (output 0x06).
pub static TOUCHPAD_RDESC: [u8; TOUCHPAD_RDESC_LEN] = {
    let mut d = [0u8; TOUCHPAD_RDESC_LEN];
    let mut i = 0;
    while i < HEAD.len() {
        d[i] = HEAD[i];
        i += 1;
    }
    let mut f = 0;
    while f < TOUCHPAD_MAX_CONTACTS * FINGER.len() {
        d[i] = FINGER[f % FINGER.len()];
        i += 1;
        f += 1;
    }
    let mut t = 0;
    while t < TAIL.len() {
        d[i] = TAIL[t];
        i += 1;
        t += 1;
    }
    d
};

/// Microsoft's default certification blob for a device without a signed one. Windows 10 and
/// later accept any 256 bytes, but only when the feature exists.
#[rustfmt::skip]
pub const PTPHQA_BLOB: [u8; 256] = [
    0xfc, 0x28, 0xfe, 0x84, 0x40, 0xcb, 0x9a, 0x87, 0x0d, 0xbe, 0x57, 0x3c, 0xb6, 0x70, 0x09, 0x88,
    0x07, 0x97, 0x2d, 0x2b, 0xe3, 0x38, 0x34, 0xb6, 0x6c, 0xed, 0xb0, 0xf7, 0xe5, 0x9c, 0xf6, 0xc2,
    0x2e, 0x84, 0x1b, 0xe8, 0xb4, 0x51, 0x78, 0x43, 0x1f, 0x28, 0x4b, 0x7c, 0x2d, 0x53, 0xaf, 0xfc,
    0x47, 0x70, 0x1b, 0x59, 0x6f, 0x74, 0x43, 0xc4, 0xf3, 0x47, 0x18, 0x53, 0x1a, 0xa2, 0xa1, 0x71,
    0xc7, 0x95, 0x0e, 0x31, 0x55, 0x21, 0xd3, 0xb5, 0x1e, 0xe9, 0x0c, 0xba, 0xec, 0xb8, 0x89, 0x19,
    0x3e, 0xb3, 0xaf, 0x75, 0x81, 0x9d, 0x53, 0xb9, 0x41, 0x57, 0xf4, 0x6d, 0x39, 0x25, 0x29, 0x7c,
    0x87, 0xd9, 0xb4, 0x98, 0x45, 0x7d, 0xa7, 0x26, 0x9c, 0x65, 0x3b, 0x85, 0x68, 0x89, 0xd7, 0x3b,
    0xbd, 0xff, 0x14, 0x67, 0xf2, 0x2b, 0xf0, 0x2a, 0x41, 0x54, 0xf0, 0xfd, 0x2c, 0x66, 0x7c, 0xf8,
    0xc0, 0x8f, 0x33, 0x13, 0x03, 0xf1, 0xd3, 0xc1, 0x0b, 0x89, 0xd9, 0x1b, 0x62, 0xcd, 0x51, 0xb7,
    0x80, 0xb8, 0xaf, 0x3a, 0x10, 0xc1, 0x8a, 0x5b, 0xe8, 0x8a, 0x56, 0xf0, 0x8c, 0xaa, 0xfa, 0x35,
    0xe9, 0x42, 0xc4, 0xd8, 0x55, 0xc3, 0x38, 0xcc, 0x2b, 0x53, 0x5c, 0x69, 0x52, 0xd5, 0xc8, 0x73,
    0x02, 0x38, 0x7c, 0x73, 0xb6, 0x41, 0xe7, 0xff, 0x05, 0xd8, 0x2b, 0x79, 0x9a, 0xe2, 0x34, 0x60,
    0x8f, 0xa3, 0x32, 0x1f, 0x09, 0x78, 0x62, 0xbc, 0x80, 0xe3, 0x0f, 0xbd, 0x65, 0x20, 0x08, 0x13,
    0xc1, 0xe2, 0xee, 0x53, 0x2d, 0x86, 0x7e, 0xa7, 0x5a, 0xc5, 0xd3, 0x7d, 0x98, 0xbe, 0x31, 0x48,
    0x1f, 0xfb, 0xda, 0xaf, 0xa2, 0xa8, 0x6a, 0x89, 0xd6, 0xbf, 0xf2, 0xd3, 0x32, 0x2a, 0x9a, 0xe4,
    0xcf, 0x17, 0xb7, 0xb8, 0xf4, 0xe1, 0x33, 0x08, 0x24, 0x8b, 0xc4, 0x43, 0xa5, 0xe5, 0x24, 0xc2,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdesc::{report_lens, FEATURE, INPUT, OUTPUT};
    use crate::{gamepad, mouse};

    #[test]
    fn descriptor_declares_exactly_the_reports_the_builders_make() {
        let lens = report_lens(&TOUCHPAD_RDESC);
        assert_eq!(
            lens.into_iter().collect::<alloc::vec::Vec<_>>(),
            [
                ((INPUT, TOUCHPAD_REPORT_ID), TOUCHPAD_REPORT_LEN),
                ((OUTPUT, DOORBELL_REPORT_ID), DOORBELL_REPORT_LEN),
                ((FEATURE, CAPS_REPORT_ID), CAPS_FEATURE.len()),
                ((FEATURE, HQA_REPORT_ID), HQA_FEATURE_LEN),
                ((FEATURE, INPUT_MODE_REPORT_ID), 2),
                ((FEATURE, SWITCH_REPORT_ID), 2),
            ]
        );
        assert_eq!(HQA_FEATURE_LEN, 257);
        assert_eq!(hqa_feature()[..3], [HQA_REPORT_ID, 0xfc, 0x28]);
    }

    #[test]
    fn a_report_packs_contacts_scan_time_count_and_button() {
        let a = PtpContact {
            id: 0,
            tip: true,
            confident: true,
            x: 0x1234,
            y: 24000,
        };
        let lifting = PtpContact {
            id: 4,
            tip: false,
            confident: false,
            x: u16::MAX,
            y: 1,
        };
        let r = ptp_report(&[a, lifting], 0xBEEF, true);
        assert_eq!(r.len(), 30);
        assert_eq!(r[..6], [0x01, 0b11, 0x34, 0x12, 0xC0, 0x5D]);
        assert_eq!(
            r[6..11],
            [4 << 2, 0x00, 0x7D, 0x01, 0x00],
            "x clamps to the pad"
        );
        assert!(
            r[11..26].iter().all(|b| *b == 0),
            "unused contacts stay zero"
        );
        assert_eq!(r[26..], [0xEF, 0xBE, 2, 1]);

        let six = [a; 6];
        assert_eq!(ptp_report(&six, 0, false)[28], 5, "never above the maximum");
        assert_eq!(ptp_report(&[], 7, false)[26..], [7, 0, 0, 0]);
    }

    #[test]
    fn names_magic_and_ring_are_stable() {
        assert_eq!(touchpad_boot_name(3), "Global\\pftouchpad-boot-3");
        assert_eq!(TOUCHPAD_MAGIC.to_le_bytes(), *b"PFTP");
        for other in [gamepad::XUSB_MAGIC, gamepad::PAD_MAGIC, mouse::MOUSE_MAGIC] {
            assert_ne!(TOUCHPAD_MAGIC, other);
        }
        assert_eq!(CAPS_FEATURE, [0x02, 0x05]);
        assert_eq!(ring_slot_off(0), 32);
        assert_eq!(
            ring_slot_off(TOUCHPAD_RING_LEN as u32 + 1),
            32 + TOUCHPAD_RING_SLOT
        );
    }
}
