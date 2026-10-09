//! Nintendo Switch Pro Controller (wired, `057E:2009`) and Joy-Con (`2006`/`2007`) report tables
//! and handshake replies, pinned to `hid-nintendo.c` and SDL's `SDL_hidapi_switch.c`. A Joy-Con
//! speaks the Pro protocol under its own device type. The UMDF driver answers the handshake from
//! [`reply`] with no host round trip; Linux UHID calls the same function, so both serve identical
//! bytes.
//!
//! USB: output `0x80 <cmd>` → input `0x81 <cmd>`. Subcommand `0x01` → `0x21`, whose 13-byte
//! header is the latest `0x30` state report's. SPI `0x10` reads are served by address range.

use alloc::vec::Vec;

/// Wired Pro Controller USB HID report descriptor. Report ids: in 0x30/0x21/0x81, out
/// 0x01/0x10/0x80/0x82. Not the Bluetooth descriptor, which declares another report set.
#[rustfmt::skip]
pub static RDESC: [u8; 203] = [
    0x05, 0x01, 0x15, 0x00, 0x09, 0x04, 0xA1, 0x01, 0x85, 0x30, 0x05, 0x01, 0x05, 0x09, 0x19, 0x01,
    0x29, 0x0A, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0A, 0x55, 0x00, 0x65, 0x00, 0x81, 0x02,
    0x05, 0x09, 0x19, 0x0B, 0x29, 0x0E, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04, 0x81, 0x02,
    0x75, 0x01, 0x95, 0x02, 0x81, 0x03, 0x0B, 0x01, 0x00, 0x01, 0x00, 0xA1, 0x00, 0x0B, 0x30, 0x00,
    0x01, 0x00, 0x0B, 0x31, 0x00, 0x01, 0x00, 0x0B, 0x32, 0x00, 0x01, 0x00, 0x0B, 0x35, 0x00, 0x01,
    0x00, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x04, 0x81, 0x02, 0xC0, 0x0B,
    0x39, 0x00, 0x01, 0x00, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46, 0x3B, 0x01, 0x65, 0x14, 0x75,
    0x04, 0x95, 0x01, 0x81, 0x02, 0x05, 0x09, 0x19, 0x0F, 0x29, 0x12, 0x15, 0x00, 0x25, 0x01, 0x75,
    0x01, 0x95, 0x04, 0x81, 0x02, 0x75, 0x08, 0x95, 0x34, 0x81, 0x03, 0x06, 0x00, 0xFF, 0x85, 0x21,
    0x09, 0x01, 0x75, 0x08, 0x95, 0x3F, 0x81, 0x03, 0x85, 0x81, 0x09, 0x02, 0x75, 0x08, 0x95, 0x3F,
    0x81, 0x03, 0x85, 0x01, 0x09, 0x03, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, 0x85, 0x10, 0x09, 0x04,
    0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, 0x85, 0x80, 0x09, 0x05, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83,
    0x85, 0x82, 0x09, 0x06, 0x75, 0x08, 0x95, 0x3F, 0x91, 0x83, 0xC0,
];

/// What the Windows driver serves: [`RDESC`] with the channel-proof feature
/// ([`crate::gamepad::with_proof`]). The Linux UHID pad has no channel and serves [`RDESC`].
pub static RDESC_WITH_PROOF: [u8; 221] = crate::gamepad::with_proof(&RDESC);

/// Every USB input report, id included. `hid-nintendo` rejects a `0x21` under 49 bytes.
pub const REPORT_LEN: usize = 64;
/// 12-bit factory stick calibration: `center ± range` is full deflection.
pub const STICK_CENTER: u16 = 2048;
pub const STICK_RANGE: u16 = 1400;
/// Header byte 2: full, charging, wired. Suppresses low-battery warnings.
pub const BAT_CON_FULL_WIRED: u8 = 0x91;
/// Header byte 12. Zero stops `hid-nintendo`'s rumble queue.
pub const VIBRATOR_READY: u8 = 0x70;

/// Two 12-bit values in `hid_field_extract` little-endian bitfield order.
pub fn pack12(a: u16, b: u16) -> [u8; 3] {
    [
        (a & 0xFF) as u8,
        ((a >> 8) & 0x0F) as u8 | ((b & 0x0F) << 4) as u8,
        ((b >> 4) & 0xFF) as u8,
    ]
}

/// Input report `0x30`: the 13-byte header (timer, battery, 24 button bits, packed
/// `[lx, ly, rx, ry]`, vibrator), then three IMU frames of accel and gyro repeating one
/// sample, i16 LE.
pub fn state_report(
    timer: u8,
    buttons: u32,
    sticks: [u16; 4],
    accel: [i16; 3],
    gyro: [i16; 3],
) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0] = 0x30;
    r[1] = timer;
    r[2] = BAT_CON_FULL_WIRED;
    r[3..6].copy_from_slice(&buttons.to_le_bytes()[..3]);
    r[6..9].copy_from_slice(&pack12(sticks[0], sticks[1]));
    r[9..12].copy_from_slice(&pack12(sticks[2], sticks[3]));
    r[12] = VIBRATOR_READY;
    for frame in r[13..49].chunks_exact_mut(12) {
        for (i, v) in accel.iter().chain(gyro.iter()).enumerate() {
            frame[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
    r
}

/// At rest: sticks centred, nothing held, 1 g on +Z. Zero accel reads as free fall. A right
/// Joy-Con's IMU is mounted turned over, so its gravity reads on −Z.
pub fn neutral_report(device_type: u8) -> [u8; REPORT_LEN] {
    let z = match device_type {
        crate::gamepad::DEVTYPE_JOYCON_RIGHT => -4096,
        _ => 4096,
    };
    state_report(0, 0, [STICK_CENTER; 4], [0, 0, z], [0; 3])
}

/// Whether `device_type` speaks this protocol: the Pro Controller or a Joy-Con half.
pub const fn is_switch(device_type: u8) -> bool {
    use crate::gamepad::{DEVTYPE_JOYCON_LEFT, DEVTYPE_JOYCON_RIGHT, DEVTYPE_SWITCH_PRO};
    matches!(
        device_type,
        DEVTYPE_SWITCH_PRO | DEVTYPE_JOYCON_LEFT | DEVTYPE_JOYCON_RIGHT
    )
}

/// The controller type SDL reads from device info: 1 left Joy-Con, 2 right, 3 Pro.
pub const fn controller_type(device_type: u8) -> u8 {
    match device_type {
        crate::gamepad::DEVTYPE_JOYCON_LEFT => 1,
        crate::gamepad::DEVTYPE_JOYCON_RIGHT => 2,
        _ => 3,
    }
}

/// Nintendo OUI, 0 for a Pro or the Joy-Con's controller type, then the pad index: the MAC
/// `hid-nintendo` keys `uniq` off and SDL reads. The two halves of a pair never share one.
pub const fn mac(device_type: u8, index: u8) -> [u8; 6] {
    let kind = match controller_type(device_type) {
        3 => 0,
        t => t,
    };
    [0x7C, 0xBB, 0x8A, 0xDF, kind, index]
}

/// `0x81 <cmd>` handshake ack; `hid-nintendo` matches those two bytes. SDL reads the status
/// ack (`cmd` 0x01) for the controller type and the MAC, least significant first.
pub fn usb_ack(cmd: u8, device_type: u8, index: u8) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0] = 0x81;
    r[1] = cmd;
    if cmd == 0x01 {
        r[3] = controller_type(device_type);
        for (slot, b) in r[4..10]
            .iter_mut()
            .zip(mac(device_type, index).iter().rev())
        {
            *slot = *b;
        }
    }
    r
}

/// `0x21` reply on `state`'s header. `hid-nintendo` matches only the echoed id (byte 14);
/// `ack` has its MSB set, as on hardware.
pub fn subcmd_reply(
    state: &[u8; REPORT_LEN],
    ack: u8,
    subcmd: u8,
    payload: &[u8],
) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[..13].copy_from_slice(&state[..13]);
    r[0] = 0x21;
    r[13] = ack;
    r[14] = subcmd;
    let n = payload.len().min(REPORT_LEN - 15);
    r[15..15 + n].copy_from_slice(&payload[..n]);
    r
}

/// Subcommand `0x02` payload: firmware 4.33, the controller type, then the MAC.
pub fn device_info_payload(device_type: u8, index: u8) -> [u8; 12] {
    let kind = controller_type(device_type);
    let mut p = [0x04, 0x21, kind, 0x02, 0, 0, 0, 0, 0, 0, 0x01, 0x01];
    p[4..10].copy_from_slice(&mac(device_type, index));
    p
}

/// Modelled SPI flash as `(start, bytes)`. Anything else reads as zero.
///
/// `0x6020` IMU: offsets 0, accel scale 16384, gyro scale 13371 (the driver's identity).
/// Stick cal: [`STICK_CENTER`] ± [`STICK_RANGE`]. Left = max ++ center ++ min; right =
/// center ++ min ++ max (`joycon_read_stick_calibration`). User magics at
/// `0x8010`/`0x801B`/`0x8026` are not `0xB2 0xA1`, so consumers take factory.
fn flash_blocks() -> [(u32, Vec<u8>); 6] {
    let cal_pair = pack12(STICK_RANGE, STICK_RANGE);
    let center_pair = pack12(STICK_CENTER, STICK_CENTER);
    let mut imu = Vec::with_capacity(24);
    imu.extend_from_slice(&[0u8; 6]);
    for _ in 0..3 {
        imu.extend_from_slice(&16384u16.to_le_bytes());
    }
    imu.extend_from_slice(&[0u8; 6]);
    for _ in 0..3 {
        imu.extend_from_slice(&13371u16.to_le_bytes());
    }
    [
        (0x6020, imu),
        (0x603D, [cal_pair, center_pair, cal_pair].concat()),
        (0x6046, [center_pair, cal_pair, cal_pair].concat()),
        (0x8010, alloc::vec![0xFF, 0xFF]),
        (0x801B, alloc::vec![0xFF, 0xFF]),
        (0x8026, alloc::vec![0xFF, 0xFF]),
    ]
}

/// SPI `0x10` reply payload: echoed LE addr + len + `len` bytes at `addr`.
///
/// Served by range, never by exact `(addr, len)`: `hid-nintendo` reads two 9-byte stick
/// blocks, SDL 18 bytes at `0x603D` and 22 at `0x8010`. Exact matching zero-fills SDL's
/// reads and pins both sticks to a corner.
pub fn spi_flash_read(addr: u32, len: u8) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + len as usize);
    payload.extend_from_slice(&addr.to_le_bytes());
    payload.push(len);
    payload.resize(5 + len as usize, 0);
    for (start, bytes) in flash_blocks() {
        for (i, slot) in payload[5..].iter_mut().enumerate() {
            let a = addr.saturating_add(i as u32);
            if let Some(b) = a.checked_sub(start).and_then(|o| bytes.get(o as usize)) {
                *slot = *b;
            }
        }
    }
    payload
}

/// The input report pad `index` of `device_type` answers `output` with, given its latest
/// `0x30` `state`: `0x81` for a `0x80` command, `0x21` for a `0x01` subcommand. Device info and
/// SPI reads carry data; every other subcommand is acked. `None` for rumble-only `0x10`.
pub fn reply(
    state: &[u8; REPORT_LEN],
    output: &[u8],
    device_type: u8,
    index: u8,
) -> Option<[u8; REPORT_LEN]> {
    match *output.first()? {
        0x80 => Some(usb_ack(*output.get(1)?, device_type, index)),
        0x01 if output.len() >= 11 => {
            let (id, args) = (output[10], &output[11..]);
            Some(match id {
                0x02 => subcmd_reply(state, 0x82, id, &device_info_payload(device_type, index)),
                0x10 => {
                    let addr = args
                        .get(..4)
                        .map_or(0, |a| u32::from_le_bytes([a[0], a[1], a[2], a[3]]));
                    let len = args.get(4).copied().unwrap_or(0);
                    subcmd_reply(state, 0x90, id, &spi_flash_read(addr, len))
                }
                _ => subcmd_reply(state, 0x80, id, &[]),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `0x30` header, packed sticks and three identical IMU frames (`struct
    /// joycon_input_report` + `joycon_imu_data`).
    #[test]
    fn switch_state_report_layout() {
        let buttons = (1 << 2) | (1 << 8) | (1 << 23); // B, Minus, ZL
        let c = STICK_CENTER;
        let r = state_report(7, buttons, [c; 4], [-1, 0x3344, 5], [0x1122, -2, 3]);
        assert_eq!(r[..3], [0x30, 7, BAT_CON_FULL_WIRED]);
        assert_eq!(r[3..6], [0x04, 0x01, 0x80]);
        assert_eq!(r[6..9], pack12(c, c));
        assert_eq!(r[9..12], pack12(c, c));
        assert_eq!(r[12], VIBRATOR_READY);
        assert_eq!(r[13..15], (-1i16).to_le_bytes());
        assert_eq!(r[15..17], 0x3344u16.to_le_bytes());
        assert_eq!(r[19..21], 0x1122u16.to_le_bytes());
        assert_eq!(r[13..25], r[25..37]);
        assert_eq!(r[13..25], r[37..49]);
        let n = neutral_report(crate::gamepad::DEVTYPE_SWITCH_PRO);
        assert_eq!(
            n[13 + 4..13 + 6],
            4096i16.to_le_bytes(),
            "1 g on +Z at rest"
        );
    }

    /// The Windows descriptor is the capture plus the proof feature, closed by the capture's own
    /// `End Collection`, and it declares Feature report `0x85` exactly once.
    #[test]
    fn switch_windows_rdesc_adds_only_the_proof_feature() {
        assert_eq!(RDESC[202], 0xC0);
        assert_eq!(RDESC_WITH_PROOF[..202], RDESC[..202]);
        assert_eq!(RDESC_WITH_PROOF[220], 0xC0);
        let ids = RDESC_WITH_PROOF
            .windows(2)
            .filter(|w| w == &[0x85, 0x85])
            .count();
        assert_eq!(ids, 1, "one Report ID (0x85)");
        assert!(RDESC_WITH_PROOF[202..]
            .windows(2)
            .any(|w| w == [0xB1, 0x02]));
    }

    /// A at bit 0, B at bit 12 (`hid_field_extract` LE bitfield).
    #[test]
    fn switch_pack12_layout() {
        assert_eq!(pack12(0x578, 0x578), [0x78, 0x85, 0x57]); // 1400/1400, the cal pair
        assert_eq!(pack12(0x800, 0x800), [0x00, 0x08, 0x80]); // 2048/2048, the center pair
        let p = pack12(0xABC, 0x123);
        let a = p[0] as u16 | ((p[1] as u16 & 0xF) << 8);
        let b = ((p[1] as u16) >> 4) | ((p[2] as u16) << 4);
        assert_eq!((a, b), (0xABC, 0x123));
    }

    /// The handshake as `hid-nintendo` and SDL drive it: `0x80` commands get `0x81` acks (the
    /// status ack names a Pro pad and its MAC), subcommands get `0x21` on the latched header.
    #[test]
    fn switch_reply_answers_the_handshake() {
        use crate::gamepad::DEVTYPE_SWITCH_PRO as PRO;
        let state = state_report(9, 1 << 3, [STICK_CENTER; 4], [0, 0, 4096], [0; 3]);
        let ack = reply(&state, &[0x80, 0x02], PRO, 3).expect("handshake ack");
        assert_eq!(ack[..2], [0x81, 0x02]);
        assert_eq!(ack[2..], [0u8; 62]);
        let status = reply(&state, &[0x80, 0x01], PRO, 3).expect("status ack");
        assert_eq!(status[3], 0x03, "controller type Pro");
        let mut mac_back = [0u8; 6];
        mac_back.copy_from_slice(&status[4..10]);
        mac_back.reverse();
        assert_eq!(mac_back, mac(PRO, 3), "SDL reverses the status MAC");

        let subcmd = |id: u8, args: &[u8]| {
            let mut out = alloc::vec![0x01, 0x05, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, id];
            out.extend_from_slice(args);
            reply(&state, &out, PRO, 3).expect("subcommand reply")
        };
        let info = subcmd(0x02, &[]);
        assert_eq!(info[..13], [&[0x21][..], &state[1..13]].concat()[..]);
        assert_eq!(info[13..15], [0x82, 0x02]);
        assert_eq!(info[15..27], device_info_payload(PRO, 3));
        let spi = subcmd(0x10, &[0x3D, 0x60, 0, 0, 9]);
        assert_eq!(spi[13..15], [0x90, 0x10]);
        assert_eq!(spi[15..29], spi_flash_read(0x603D, 9)[..]);
        let mode = subcmd(0x03, &[0x30]);
        assert_eq!(mode[13..16], [0x80, 0x03, 0]);

        let rumble_only = [0x10, 0x06, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40];
        assert!(reply(&state, &rumble_only, PRO, 3).is_none());
        assert!(
            reply(&state, &[0x01, 0x05], PRO, 3).is_none(),
            "short subcommand"
        );
        assert!(reply(&state, &[], PRO, 3).is_none());
    }

    /// Each Joy-Con half names its own type in device info, as SDL reads it over Bluetooth, and
    /// the two halves of one pad never share a MAC.
    #[test]
    fn joycon_halves_name_their_side() {
        use crate::gamepad::{DEVTYPE_JOYCON_LEFT as L, DEVTYPE_JOYCON_RIGHT as R};
        let state = neutral_report(L);
        let info = |dt| {
            let out = [0x01, 0x05, 0, 1, 0x40, 0x40, 0, 1, 0x40, 0x40, 0x02];
            reply(&state, &out, dt, 4).expect("device info")
        };
        assert_eq!(info(L)[17], 1, "left Joy-Con");
        assert_eq!(info(R)[17], 2, "right Joy-Con");
        assert_eq!(info(L)[19..25], mac(L, 4));
        assert_ne!(mac(L, 4), mac(R, 4));
        assert_eq!(usb_ack(0x01, R, 4)[3], 2);
        assert!(is_switch(L) && is_switch(R) && !is_switch(crate::gamepad::DEVTYPE_XBOX));
    }

    /// User magics absent; stick min < center < max in per-side byte order; replies echo addr+len.
    #[test]
    fn switch_spi_blobs_are_valid() {
        for addr in [0x8010u32, 0x801B, 0x8026] {
            let p = spi_flash_read(addr, 2);
            assert_eq!(p[..4], addr.to_le_bytes());
            assert_eq!(p[4], 2);
            assert!(!(p[5] == 0xB2 && p[6] == 0xA1));
        }
        let unpack = |b: &[u8]| b[0] as u16 | ((b[1] as u16 & 0xF) << 8);
        // Left: max-above ++ center ++ min-below.
        let l = spi_flash_read(0x603D, 9);
        assert_eq!(l[..5], [0x3D, 0x60, 0, 0, 9]);
        let (max_above, center, min_below) =
            (unpack(&l[5..8]), unpack(&l[8..11]), unpack(&l[11..14]));
        assert_eq!(center, STICK_CENTER);
        assert!(center - min_below < center && center < center + max_above);
        // Right: center ++ min-below ++ max-above.
        assert_eq!(unpack(&spi_flash_read(0x6046, 9)[5..8]), STICK_CENTER);
        let imu = spi_flash_read(0x6020, 24);
        assert_eq!(imu[5..11], [0; 6]);
        assert_eq!(imu[11..13], 16384u16.to_le_bytes());
        assert_eq!(imu[17..23], [0; 6]);
        assert_eq!(imu[23..25], 13371u16.to_le_bytes());
        let gap = spi_flash_read(0x6050, 12);
        assert_eq!(gap[..5], [0x50, 0x60, 0, 0, 12]);
        assert_eq!(gap[5..], [0u8; 12]);
    }

    /// SDL reads 18 factory bytes at `0x603D` and 22 user bytes at `0x8010`, shapes
    /// `hid-nintendo` never asks. Exact `(addr, len)` matching would zero-fill them.
    #[test]
    fn switch_spi_serves_sdl_read_shapes() {
        let f = spi_flash_read(0x603D, 18);
        assert_eq!(f[..5], [0x3D, 0x60, 0, 0, 18]);
        assert_eq!(f[5..14], spi_flash_read(0x603D, 9)[5..]);
        assert_eq!(f[14..], spi_flash_read(0x6046, 9)[5..]);
        let cal = &f[5..];
        let cx = (((cal[4] as u16) << 8) & 0xF00) | cal[3] as u16;
        let cy = ((cal[5] as u16) << 4) | ((cal[4] as u16) >> 4);
        assert_eq!((cx, cy), (STICK_CENTER, STICK_CENTER));
        let u = spi_flash_read(0x8010, 22);
        assert_eq!(u[..5], [0x10, 0x80, 0, 0, 22]);
        assert_eq!(u[5..7], [0xFF, 0xFF]); // left magic  @ 0x8010
        assert_eq!(u[16..18], [0xFF, 0xFF]); // right magic @ 0x801B
    }

    /// The timer byte advances per served report; a `0x81` ack keeps its echoed command.
    #[test]
    fn switch_timer_advances_per_report() {
        use crate::gamepad::*;
        let mut r = neutral_report(DEVTYPE_SWITCH_PRO);
        assert!(stamp_report_clock(DEVTYPE_SWITCH_PRO, &mut r, 0x1FF, 0));
        assert_eq!(r[1], 0xFF);
        let mut ack = usb_ack(0x02, DEVTYPE_SWITCH_PRO, 0);
        assert!(!stamp_report_clock(DEVTYPE_SWITCH_PRO, &mut ack, 5, 0));
        assert_eq!(ack[1], 0x02);
        assert_eq!(pad_serial(DEVTYPE_SWITCH_PRO, 2), "7CBB8ADF0002");
        assert_eq!(pad_serial(DEVTYPE_JOYCON_RIGHT, 2), "7CBB8ADF0202");
        assert_eq!(identity_vid_pid(DEVTYPE_SWITCH_PRO), Some((0x057E, 0x2009)));
        assert_eq!(
            identity_vid_pid(DEVTYPE_JOYCON_LEFT),
            Some((0x057E, 0x2006))
        );
    }
}
