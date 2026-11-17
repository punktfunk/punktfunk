//! The HID side the minidrivers share: the IOCTL table, descriptor and string helpers, and the
//! **channel proof** (`pf_driver_proto::gamepad::ChannelProof`) transport, measured on Win11
//! build 26200 so nobody re-derives it.
//!
//! The proof names the process the host duplicates a pad's DATA section into. The bootstrap
//! mailbox cannot carry it: any LocalService principal could name its own process there. Only
//! the driver bound to the devnode answers its I/O, so the device stack is asked:
//!
//! * ❌ `IOCTL_HID_GET_INDEXED_STRING`: hidclass never forwards it to a UMDF HID minidriver.
//! * ❌ A private device interface (the `pf_xusb` shape): enumerates, but `CreateFile` is
//!   refused — hidclass owns `IRP_MJ_CREATE` on a devnode it is the FDO for.
//! * ✅ The serial-number string: `HidD_GetSerialNumberString` works on a zero-access handle
//!   (`pf_mouse` answered `PFCP:3:0:7296`, a real WUDFHost).
//! * ✅ A feature report: `HidD_GetFeature` on the pad's HID handle (`pf_gamepad`).
//!
//! `pf_mouse` and `pf_touchpad` serve their proof as their serial. The pads do not: SDL and
//! Steam dedup controllers on the serial, so `pf_gamepad` answers GET_FEATURE (`on_get_feature`).

/// `CTL_CODE(FILE_DEVICE_KEYBOARD, id, METHOD_NEITHER, FILE_ANY_ACCESS)`: the HID minidriver
/// IOCTL numbering hidclass and mshidumdf speak.
#[must_use]
pub const fn ioctl(id: u32) -> u32 {
    (0x0000_000b << 16) | (id << 2) | 3
}
pub const IOCTL_HID_GET_DEVICE_DESCRIPTOR: u32 = ioctl(0);
pub const IOCTL_HID_GET_REPORT_DESCRIPTOR: u32 = ioctl(1);
pub const IOCTL_HID_READ_REPORT: u32 = ioctl(2);
pub const IOCTL_HID_WRITE_REPORT: u32 = ioctl(3);
pub const IOCTL_HID_GET_STRING: u32 = ioctl(4);
pub const IOCTL_HID_GET_DEVICE_ATTRIBUTES: u32 = ioctl(9);
pub const IOCTL_UMDF_HID_SET_FEATURE: u32 = ioctl(20);
pub const IOCTL_UMDF_HID_GET_FEATURE: u32 = ioctl(21);
pub const IOCTL_UMDF_HID_SET_OUTPUT_REPORT: u32 = ioctl(22);
pub const IOCTL_UMDF_HID_GET_INPUT_REPORT: u32 = ioctl(23);

/// The report-descriptor length a 9-byte HID descriptor declares (bytes 7..9, little-endian).
/// Every driver pins `declared_len(&HID_DESC) == RDESC.len()` at compile time.
#[must_use]
pub const fn declared_len(hid_desc: &[u8; 9]) -> usize {
    (hid_desc[7] as usize) | ((hid_desc[8] as usize) << 8)
}

/// `IOCTL_HID_GET_STRING`'s input: the low 16 bits of a little-endian u32 name the string
/// (`0`/`0x0E` manufacturer, `2`/`0x10` serial, else product). `(raw, id)`; a short input is `0`.
#[must_use]
pub fn string_request_id(bytes: &[u8]) -> (u32, u32) {
    let raw = match bytes {
        [a, b, c, d, ..] => u32::from_le_bytes([*a, *b, *c, *d]),
        _ => 0,
    };
    (raw, raw & 0xFFFF)
}

/// `s` as the NUL-terminated little-endian UTF-16 bytes a `GET_STRING` reply carries.
#[must_use]
pub fn utf16z(s: &str) -> Vec<u8> {
    let mut wide: Vec<u8> = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        wide.extend_from_slice(&u.to_le_bytes());
    }
    wide.extend_from_slice(&[0, 0]);
    wide
}

/// The first `max` bytes as `"01 02 03 "` for a log line. Build it inside the log gate: a
/// release driver used to format 48 bytes per output report for a line it never wrote.
#[must_use]
pub fn hex_dump(bytes: &[u8], max: usize) -> String {
    let mut hex = String::with_capacity(bytes.len().min(max) * 3);
    for b in bytes.iter().take(max) {
        hex.push_str(&format!("{b:02x} "));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_ioctls_are_the_keyboard_device_neither_method_codes() {
        assert_eq!(IOCTL_HID_GET_DEVICE_DESCRIPTOR, 0x000B_0003);
        assert_eq!(IOCTL_HID_GET_REPORT_DESCRIPTOR, 0x000B_0007);
        assert_eq!(IOCTL_HID_GET_STRING, 0x000B_0013);
        assert_eq!(IOCTL_UMDF_HID_GET_INPUT_REPORT, 0x000B_005F);
    }

    #[test]
    fn declared_len_reads_the_little_endian_tail() {
        assert_eq!(
            declared_len(&[0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, 0x11, 0x01]),
            273
        );
        assert_eq!(
            declared_len(&[0x09, 0x21, 0x11, 0x01, 0x00, 0x01, 0x22, 0x74, 0x01]),
            372
        );
    }

    #[test]
    fn get_string_input_is_the_low_word_and_short_input_is_zero() {
        assert_eq!(
            string_request_id(&[0x10, 0x00, 0x09, 0x04]),
            (0x0409_0010, 0x10)
        );
        assert_eq!(string_request_id(&[0x02]), (0, 0));
    }

    #[test]
    fn utf16z_is_little_endian_with_a_double_nul() {
        assert_eq!(utf16z("A"), vec![0x41, 0x00, 0x00, 0x00]);
        assert_eq!(utf16z(""), vec![0x00, 0x00]);
        assert_eq!(utf16z("é").len(), 4);
    }

    #[test]
    fn hex_dump_caps_and_spaces() {
        assert_eq!(hex_dump(&[0x01, 0xAB, 0xFF], 8), "01 ab ff ");
        assert_eq!(hex_dump(&[1, 2, 3], 2), "01 02 ");
        assert_eq!(hex_dump(&[], 4), "");
    }
}
