//! Virtual pointer + keyboard layout (host ↔ UMDF HID minidriver `pf_mouse`).
//!
//! The device keeps a pointer present (`SM_MOUSEPRESENT`), so DWM draws the cursor on a headless
//! host, and carries the host's mouse and keyboard input as HID reports. A HID report reaches raw
//! input with a device handle and no injected flag, which kernel anti-cheat accepts.
//!
//! Same sealed-pad handshake as [`gamepad`](crate::gamepad) (`design/gamepad-channel-sealing.md`):
//! [`gamepad::PadBootstrap`](crate::gamepad::PadBootstrap), [`mouse_boot_name`], mouse DATA
//! magic. Reports travel in order through [`MouseShm::ring`]; a write to the doorbell collection
//! ([`DOORBELL_REPORT_ID`]) makes the driver drain it at once.

use alloc::string::String;
use bytemuck::{Pod, Zeroable};

/// Mouse DATA-section magic ("PFMO" LE) — distinct from the pad magics so a cross-wire fails.
pub const MOUSE_MAGIC: u32 = 0x4F4D_4650;

/// `Global\pfmouse-boot-<index>` — mouse bootstrap mailbox ([`crate::gamepad::PadBootstrap`]).
pub fn mouse_boot_name(index: u8) -> String {
    alloc::format!("Global\\pfmouse-boot-{index}")
}

/// HID identity ("PF" / "MO") — obviously virtual; no software matches on it, unlike the
/// pads' cloned Sony/Valve ids.
pub const MOUSE_VID: u16 = 0x5046;
pub const MOUSE_PID: u16 = 0x4D4F;
pub const MOUSE_VER: u16 = 0x0100;

/// Absolute pointer: `[id, buttons, x_lo, x_hi, y_lo, y_hi, wheel, pan]`, X/Y over
/// `0..=`[`MOUSE_ABS_MAX`]. Windows maps it onto the primary monitor. Buttons and wheel travel
/// the relative collection, so the host leaves those bytes zero.
pub const MOUSE_REPORT_ID: u8 = 0x01;
pub const MOUSE_REPORT_LEN: usize = 8;
/// Logical maximum of the absolute X/Y axes (15-bit, HID-descriptor convention).
pub const MOUSE_ABS_MAX: u16 = 0x7FFF;

/// Relative pointer, its own collection: `[id, buttons, dx, dy, wheel, pan]`, axes i16 LE.
/// Games read raw relative motion; mouhid makes a collection all-absolute or all-relative.
pub const MOUSE_REL_REPORT_ID: u8 = 0x02;
pub const MOUSE_REL_REPORT_LEN: usize = 10;
/// The relative collection's feature report `[id, multipliers]`: bits 0–1 wheel, 2–3 pan.
/// Windows writes 1 to read [`WHEEL_MULTIPLIER`] counts per notch; 0 is one count per notch.
pub const MOUSE_REL_FEATURE_LEN: usize = 2;
/// Counts per notch once Windows enables the multiplier: one count is the wire's 1/120 notch.
pub const WHEEL_MULTIPLIER: u32 = 120;

/// Keyboard, NKRO: `[id, bitmap]`; bit `n` (LSB first) holds HID usage `n` of page 7.
pub const KEYBOARD_REPORT_ID: u8 = 0x03;
pub const KEYBOARD_BITMAP_LEN: usize = 29;
pub const KEYBOARD_REPORT_LEN: usize = 1 + KEYBOARD_BITMAP_LEN;

/// The vendor collection's output report `[id, 0]`. It carries nothing: the driver only drains
/// the ring, so any writer can at most make it drain sooner.
pub const DOORBELL_REPORT_ID: u8 = 0x04;
/// The doorbell collection's usage page (vendor-defined); its usage is 1.
pub const DOORBELL_USAGE_PAGE: u16 = 0xFF00;
pub const DOORBELL_REPORT_LEN: usize = 2;

/// Input report length for `id`, `None` for an id [`MOUSE_RDESC`] does not declare.
#[must_use]
pub fn input_report_len(id: u8) -> Option<usize> {
    match id {
        MOUSE_REPORT_ID => Some(MOUSE_REPORT_LEN),
        MOUSE_REL_REPORT_ID => Some(MOUSE_REL_REPORT_LEN),
        KEYBOARD_REPORT_ID => Some(KEYBOARD_REPORT_LEN),
        _ => None,
    }
}

/// Absolute report ([`MOUSE_REPORT_ID`]); axes clamp to [`MOUSE_ABS_MAX`].
#[must_use]
pub fn abs_report(x: u16, y: u16) -> [u8; MOUSE_REPORT_LEN] {
    let [x_lo, x_hi] = x.min(MOUSE_ABS_MAX).to_le_bytes();
    let [y_lo, y_hi] = y.min(MOUSE_ABS_MAX).to_le_bytes();
    [MOUSE_REPORT_ID, 0, x_lo, x_hi, y_lo, y_hi, 0, 0]
}

/// Relative report ([`MOUSE_REL_REPORT_ID`]). Buttons 1..=5 are bits 0..=4 in HID order
/// (primary, secondary, middle, X1, X2); every axis clamps to the declared ±32767.
#[must_use]
pub fn relative_report(
    buttons: u8,
    dx: i16,
    dy: i16,
    wheel: i16,
    pan: i16,
) -> [u8; MOUSE_REL_REPORT_LEN] {
    let mut r = [0u8; MOUSE_REL_REPORT_LEN];
    r[0] = MOUSE_REL_REPORT_ID;
    r[1] = buttons & 0x1F;
    for (i, v) in [dx, dy, wheel, pan].into_iter().enumerate() {
        r[2 + 2 * i..4 + 2 * i].copy_from_slice(&v.max(-i16::MAX).to_le_bytes());
    }
    r
}

/// Keyboard report ([`KEYBOARD_REPORT_ID`]) for the held-usage bitmap.
#[must_use]
pub fn keyboard_report(bitmap: &[u8; KEYBOARD_BITMAP_LEN]) -> [u8; KEYBOARD_REPORT_LEN] {
    let mut r = [0u8; KEYBOARD_REPORT_LEN];
    r[0] = KEYBOARD_REPORT_ID;
    r[1..].copy_from_slice(bitmap);
    r
}

/// Ring slots. 64 reports is far more than one drain ever finds; a full ring means the driver
/// stopped taking reports.
pub const MOUSE_RING_LEN: usize = 64;
/// Bytes per slot: the largest report, [`KEYBOARD_REPORT_LEN`], rounded up.
pub const MOUSE_RING_SLOT: usize = 32;
/// `driver_features` bit: this driver drains [`MouseShm::ring`] and serves all four collections.
pub const MOUSE_FEATURE_RING: u32 = 1;
/// Section size of hosts that predate the ring. The driver still attaches to them.
pub const MOUSE_SHM_LEGACY_SIZE: usize = 64;

/// Section offset of ring slot `seq` (any wrapping sequence number).
#[must_use]
pub const fn ring_slot_off(seq: u32) -> usize {
    core::mem::offset_of!(MouseShm, ring) + (seq as usize % MOUSE_RING_LEN) * MOUSE_RING_SLOT
}

/// Virtual-device shared section. The host writes slot `ring_head`, then bumps `ring_head`
/// (Release); the driver hands each slot to a pended `READ_REPORT` and bumps `ring_tail`.
/// Idle generates no HID traffic — a constant report stream would read as user activity.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct MouseShm {
    pub magic: u32,
    /// The retired single-report slot. Zero, so a driver that predates the ring delivers nothing.
    pub _retired: [u8; 12],
    /// [`crate::gamepad::GAMEPAD_PROTO_VERSION`] while attached. `0` = no driver.
    pub driver_proto: u32,
    /// Bumped each timer tick — advances whether or not input flows.
    pub driver_heartbeat: u32,
    /// Device index (host-stamped before the magic); driver checks it against the devnode Location.
    pub pad_index: u32,
    /// Driver-stamped [`MOUSE_FEATURE_RING`]. `0` = a driver that predates the ring.
    pub driver_features: u32,
    /// Reports the host published (wrapping).
    pub ring_head: u32,
    /// Reports the driver handed to Windows (wrapping). The host writes only while
    /// `head - tail < MOUSE_RING_LEN`.
    pub ring_tail: u32,
    /// Wheel counts per notch Windows set up: 1, or [`WHEEL_MULTIPLIER`]. `0` reads as 1.
    pub wheel_counts: u32,
    /// Horizontal-wheel counts per notch, as `wheel_counts`.
    pub pan_counts: u32,
    pub _reserved: [u8; 16],
    pub ring: [[u8; MOUSE_RING_SLOT]; MOUSE_RING_LEN],
}

// Offsets are the cross-process wire contract — pin every one.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<MouseShm>() == 64 + MOUSE_RING_LEN * MOUSE_RING_SLOT);
    assert!(offset_of!(MouseShm, magic) == 0);
    assert!(offset_of!(MouseShm, driver_proto) == 16);
    assert!(offset_of!(MouseShm, driver_heartbeat) == 20);
    assert!(offset_of!(MouseShm, pad_index) == 24);
    assert!(offset_of!(MouseShm, driver_features) == 28);
    assert!(offset_of!(MouseShm, ring_head) == 32);
    assert!(offset_of!(MouseShm, ring_tail) == 36);
    assert!(offset_of!(MouseShm, wheel_counts) == 40);
    assert!(offset_of!(MouseShm, pan_counts) == 44);
    assert!(offset_of!(MouseShm, ring) == MOUSE_SHM_LEGACY_SIZE);
    assert!(KEYBOARD_REPORT_LEN <= MOUSE_RING_SLOT);
};

pub const MOUSE_RDESC_LEN: usize = 250;

/// HID report descriptor: four application collections.
/// 1. Absolute mouse (report 0x01), mapping 1:1 onto the primary monitor.
/// 2. Relative mouse (0x02) with 16-bit wheel and pan behind Resolution Multipliers (feature
///    0x02). Two mouse collections because mouhid treats a whole collection as absolute or not.
/// 3. NKRO keyboard (0x03).
/// 4. Vendor doorbell (output 0x04): the one collection user mode may open for write.
#[rustfmt::skip]
pub static MOUSE_RDESC: [u8; MOUSE_RDESC_LEN] = [
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x02,        // Usage (Mouse)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x01,        //   Report ID (1)
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x05, 0x09,        //     Usage Page (Button)
    0x19, 0x01,        //     Usage Minimum (1)
    0x29, 0x05,        //     Usage Maximum (5)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x05,        //     Report Count (5)
    0x81, 0x02,        //     Input (Data,Var,Abs) — buttons 1..5
    0x75, 0x03,        //     Report Size (3)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x03,        //     Input (Const) — pad
    0x05, 0x01,        //     Usage Page (Generic Desktop)
    0x09, 0x30,        //     Usage (X)
    0x09, 0x31,        //     Usage (Y)
    0x15, 0x00,        //     Logical Minimum (0)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x02,        //     Input (Data,Var,Abs) — absolute X/Y
    0x09, 0x38,        //     Usage (Wheel)
    0x15, 0x81,        //     Logical Minimum (-127)
    0x25, 0x7F,        //     Logical Maximum (127)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x06,        //     Input (Data,Var,Rel) — wheel
    0x05, 0x0C,        //     Usage Page (Consumer)
    0x0A, 0x38, 0x02,  //     Usage (AC Pan)
    0x15, 0x81,        //     Logical Minimum (-127)
    0x25, 0x7F,        //     Logical Maximum (127)
    0x75, 0x08,        //     Report Size (8)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x06,        //     Input (Data,Var,Rel) — horizontal wheel
    0xC0,              //   End Collection
    0xC0,              // End Collection
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x02,        // Usage (Mouse)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x02,        //   Report ID (2)
    0x09, 0x01,        //   Usage (Pointer)
    0xA1, 0x00,        //   Collection (Physical)
    0x05, 0x09,        //     Usage Page (Button)
    0x19, 0x01,        //     Usage Minimum (1)
    0x29, 0x05,        //     Usage Maximum (5)
    0x15, 0x00,        //     Logical Minimum (0)
    0x25, 0x01,        //     Logical Maximum (1)
    0x75, 0x01,        //     Report Size (1)
    0x95, 0x05,        //     Report Count (5)
    0x81, 0x02,        //     Input (Data,Var,Abs) — buttons 1..5
    0x75, 0x03,        //     Report Size (3)
    0x95, 0x01,        //     Report Count (1)
    0x81, 0x03,        //     Input (Const) — pad
    0x05, 0x01,        //     Usage Page (Generic Desktop)
    0x09, 0x30,        //     Usage (X)
    0x09, 0x31,        //     Usage (Y)
    0x16, 0x01, 0x80,  //     Logical Minimum (-32767)
    0x26, 0xFF, 0x7F,  //     Logical Maximum (32767)
    0x75, 0x10,        //     Report Size (16)
    0x95, 0x02,        //     Report Count (2)
    0x81, 0x06,        //     Input (Data,Var,Rel) — X/Y deltas
    0xA1, 0x02,        //     Collection (Logical)
    0x09, 0x48,        //       Usage (Resolution Multiplier)
    0x15, 0x00,        //       Logical Minimum (0)
    0x25, 0x01,        //       Logical Maximum (1)
    0x35, 0x01,        //       Physical Minimum (1)
    0x45, 0x78,        //       Physical Maximum (120)
    0x75, 0x02,        //       Report Size (2)
    0x95, 0x01,        //       Report Count (1)
    0xA4,              //       Push
    0xB1, 0x02,        //       Feature (Data,Var,Abs) — wheel multiplier
    0x09, 0x38,        //       Usage (Wheel)
    0x16, 0x01, 0x80,  //       Logical Minimum (-32767)
    0x26, 0xFF, 0x7F,  //       Logical Maximum (32767)
    0x35, 0x00,        //       Physical Minimum (0)
    0x45, 0x00,        //       Physical Maximum (0)
    0x75, 0x10,        //       Report Size (16)
    0x81, 0x06,        //       Input (Data,Var,Rel) — wheel
    0xC0,              //     End Collection
    0xA1, 0x02,        //     Collection (Logical)
    0x09, 0x48,        //       Usage (Resolution Multiplier)
    0xB4,              //       Pop
    0xB1, 0x02,        //       Feature (Data,Var,Abs) — pan multiplier
    0x35, 0x00,        //       Physical Minimum (0)
    0x45, 0x00,        //       Physical Maximum (0)
    0x75, 0x04,        //       Report Size (4)
    0xB1, 0x03,        //       Feature (Const) — pad
    0x05, 0x0C,        //       Usage Page (Consumer)
    0x0A, 0x38, 0x02,  //       Usage (AC Pan)
    0x16, 0x01, 0x80,  //       Logical Minimum (-32767)
    0x26, 0xFF, 0x7F,  //       Logical Maximum (32767)
    0x75, 0x10,        //       Report Size (16)
    0x81, 0x06,        //       Input (Data,Var,Rel) — pan
    0xC0,              //     End Collection
    0xC0,              //   End Collection
    0xC0,              // End Collection
    0x05, 0x01,        // Usage Page (Generic Desktop)
    0x09, 0x06,        // Usage (Keyboard)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x03,        //   Report ID (3)
    0x05, 0x07,        //   Usage Page (Keyboard/Keypad)
    0x19, 0x00,        //   Usage Minimum (0)
    0x29, 0xE7,        //   Usage Maximum (0xE7)
    0x15, 0x00,        //   Logical Minimum (0)
    0x25, 0x01,        //   Logical Maximum (1)
    0x75, 0x01,        //   Report Size (1)
    0x96, 0xE8, 0x00,  //   Report Count (232)
    0x81, 0x02,        //   Input (Data,Var,Abs) — one bit per usage
    0xC0,              // End Collection
    0x06, 0x00, 0xFF,  // Usage Page (Vendor 0xFF00)
    0x09, 0x01,        // Usage (1)
    0xA1, 0x01,        // Collection (Application)
    0x85, 0x04,        //   Report ID (4)
    0x09, 0x01,        //   Usage (1)
    0x15, 0x00,        //   Logical Minimum (0)
    0x26, 0xFF, 0x00,  //   Logical Maximum (255)
    0x75, 0x08,        //   Report Size (8)
    0x95, 0x01,        //   Report Count (1)
    0x91, 0x02,        //   Output (Data,Var,Abs) — doorbell
    0xC0,              // End Collection
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad;
    use alloc::collections::BTreeMap;

    /// Report bytes per `(main-item tag, report id)`, walked the way a HID parser does: global
    /// items persist (Push/Pop save and restore them), each main item adds size × count bits.
    fn report_lens(d: &[u8]) -> BTreeMap<(u8, u8), usize> {
        let (mut size, mut count, mut id) = (0u32, 0u32, 0u8);
        let mut stack = alloc::vec::Vec::new();
        let mut bits = BTreeMap::<(u8, u8), u32>::new();
        let mut i = 0;
        while i < d.len() {
            let prefix = d[i];
            let n = [0, 1, 2, 4][usize::from(prefix & 3)];
            let mut v = 0u32;
            for k in 0..n {
                v |= u32::from(d[i + 1 + k]) << (8 * k);
            }
            match prefix & 0xFC {
                0x74 => size = v,
                0x94 => count = v,
                0x84 => id = v as u8,
                0xA4 => stack.push((size, count, id)),
                0xB4 => (size, count, id) = stack.pop().unwrap(),
                tag @ (0x80 | 0x90 | 0xB0) => *bits.entry((tag, id)).or_default() += size * count,
                _ => {}
            }
            i += 1 + n;
        }
        bits.into_iter()
            .map(|(k, b)| (k, 1 + (b as usize).div_ceil(8)))
            .collect()
    }

    #[test]
    fn descriptor_declares_exactly_the_reports_the_builders_make() {
        const INPUT: u8 = 0x80;
        const OUTPUT: u8 = 0x90;
        const FEATURE: u8 = 0xB0;
        let lens = report_lens(&MOUSE_RDESC);
        assert_eq!(
            lens.into_iter().collect::<alloc::vec::Vec<_>>(),
            [
                ((INPUT, MOUSE_REPORT_ID), MOUSE_REPORT_LEN),
                ((INPUT, MOUSE_REL_REPORT_ID), MOUSE_REL_REPORT_LEN),
                ((INPUT, KEYBOARD_REPORT_ID), KEYBOARD_REPORT_LEN),
                ((OUTPUT, DOORBELL_REPORT_ID), DOORBELL_REPORT_LEN),
                ((FEATURE, MOUSE_REL_REPORT_ID), MOUSE_REL_FEATURE_LEN),
            ]
        );
        for id in [MOUSE_REPORT_ID, MOUSE_REL_REPORT_ID, KEYBOARD_REPORT_ID] {
            assert!(input_report_len(id).unwrap() <= MOUSE_RING_SLOT);
        }
        assert_eq!(input_report_len(DOORBELL_REPORT_ID), None);
    }

    #[test]
    fn reports_and_names_are_stable() {
        assert_eq!(mouse_boot_name(0), "Global\\pfmouse-boot-0");
        // "PFMO" LE, and never colliding with a pad magic.
        assert_eq!(MOUSE_MAGIC.to_le_bytes(), *b"PFMO");
        assert_ne!(MOUSE_MAGIC, gamepad::XUSB_MAGIC);
        assert_ne!(MOUSE_MAGIC, gamepad::PAD_MAGIC);
        assert_eq!(
            abs_report(0x1234, 0xFFFF),
            [0x01, 0, 0x34, 0x12, 0xFF, 0x7F, 0, 0]
        );
        let r = relative_report(0xFF, -2, 300, 120, -1);
        assert_eq!(
            r,
            [0x02, 0x1F, 0xFE, 0xFF, 0x2C, 0x01, 0x78, 0x00, 0xFF, 0xFF]
        );
        // i16::MIN is outside the declared logical range; it clamps to -32767.
        assert_eq!(relative_report(0, i16::MIN, 0, 0, 0)[2..4], [0x01, 0x80]);
        let mut bitmap = [0u8; KEYBOARD_BITMAP_LEN];
        bitmap[0] = 1 << 4; // usage 0x04, "A"
        assert_eq!(keyboard_report(&bitmap)[..2], [0x03, 0x10]);
        assert_eq!(ring_slot_off(0), 64);
        assert_eq!(
            ring_slot_off(MOUSE_RING_LEN as u32 + 1),
            64 + MOUSE_RING_SLOT
        );
    }
}
