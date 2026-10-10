//! Steam Controller 2 (Triton) wire tables: UMDF driver (answers Steam synchronously) and
//! host/inject (Linux usbip + tests). Pure byte-packing so it tests on any host.

/// Feature-1 command bytes of the Valve query dance.
pub const ID_GET_ATTRIBUTES_VALUES: u8 = 0x83;
pub const ID_GET_STRING_ATTRIBUTE: u8 = 0xAE;
pub const ID_GET_FIRMWARE_INFO: u8 = 0xF2;
/// A Puck slot's config store, read by key string (`esb/bond`, `user/wireless_transport`).
pub const ID_GET_CONFIG_VALUE: u8 = 0xED;
/// A Puck slot's live state: never answered from a recording.
pub const ID_GET_SLOT_STATE: u8 = 0xB4;
/// Lizard mode off, lizard mode on, and the factory settings — what hid-steam writes when it
/// binds a pad and again whenever Steam lets go of it.
pub const ID_CLEAR_DIGITAL_MAPPINGS: u8 = 0x81;
pub const ID_SET_DEFAULT_DIGITAL_MAPPINGS: u8 = 0x85;
pub const ID_LOAD_DEFAULT_SETTINGS: u8 = 0x8E;

/// Whether a feature SET from the host's stack goes on to the physical pad. Lizard mode and the
/// factory settings stay on the virtual pad: the client holds lizard mode off while it forwards
/// and restores it on release, and a reset would undo the settings Steam wrote. `set` is id-first
/// or already stripped (a command is `0x80` or above).
pub fn forwards_to_pad(set: &[u8]) -> bool {
    let cmd = match set {
        [first, ..] if *first >= 0x80 => *first,
        [_, cmd, ..] => *cmd,
        _ => return true,
    };
    !matches!(
        cmd,
        ID_CLEAR_DIGITAL_MAPPINGS | ID_SET_DEFAULT_DIGITAL_MAPPINGS | ID_LOAD_DEFAULT_SETTINGS
    )
}

/// Devnode property `{783BFBEF-EBC2-4159-80FB-4737ABA2F523}`, pid 2: a virtual SC2's
/// [`identity_blob`]. The host sets it at `SwDeviceCreate`; the driver reads it at
/// `EvtDeviceAdd`, before Steam asks for the serial.
pub const IDENTITY_PROPKEY_FMTID: u128 = 0x783B_FBEF_EBC2_4159_80FB_4737_ABA2_F523;
pub const IDENTITY_PROPKEY_PID: u32 = 2;

/// A virtual SC2's identity as the host hands it to the driver: `[n][serial]`, then
/// `[len][request][len][reply]…`, each part at most 64 bytes.
pub fn identity_blob<'a>(
    serial: &str,
    pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
) -> alloc::vec::Vec<u8> {
    let serial = &serial.as_bytes()[..serial.len().min(64)];
    let mut b = alloc::vec![serial.len() as u8];
    b.extend_from_slice(serial);
    for (req, reply) in pairs {
        for part in [req, reply] {
            let part = &part[..part.len().min(64)];
            b.push(part.len() as u8);
            b.extend_from_slice(part);
        }
    }
    b
}

/// An [`identity_blob`], read back. The serial is empty when the client sent none.
#[derive(Clone, Copy, Debug)]
pub struct Identity<'a> {
    pub serial: &'a str,
    pairs: &'a [u8],
}

impl<'a> Identity<'a> {
    /// `None` when a length runs past the end, a reply has no request, or the serial is not UTF-8.
    pub fn parse(blob: &'a [u8]) -> Option<Identity<'a>> {
        let (&n, rest) = blob.split_first()?;
        let serial = core::str::from_utf8(rest.get(..usize::from(n))?).ok()?;
        let pairs = rest.get(usize::from(n)..)?;
        let (mut cur, mut parts) = (pairs, 0usize);
        while let Some((&len, tail)) = cur.split_first() {
            cur = tail.get(usize::from(len)..).filter(|_| len <= 64)?;
            parts += 1;
        }
        (parts % 2 == 0).then_some(Identity { serial, pairs })
    }

    /// The recorded `(request, reply)` pairs, both id-first.
    pub fn pairs(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
        let mut cur = self.pairs;
        let mut part = move || {
            let (&n, tail) = cur.split_first()?;
            let (p, rest) = (tail.get(..usize::from(n))?, tail.get(usize::from(n)..)?);
            cur = rest;
            Some(p)
        };
        core::iter::from_fn(move || Some((part()?, part()?)))
    }

    pub fn reply(&self, last_set: &[u8]) -> Option<[u8; 64]> {
        recorded_reply(self.pairs(), last_set)
    }
}

/// The recorded reply to the request `last_set` makes, zero-padded to 64 bytes.
pub fn recorded_reply<'a>(
    pairs: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    last_set: &[u8],
) -> Option<[u8; 64]> {
    let want = request_key(last_set);
    if want.1 == ID_GET_SLOT_STATE {
        return None;
    }
    let (_, rep) = pairs
        .into_iter()
        .find(|(req, rep)| !rep.is_empty() && request_key(req) == want)?;
    let mut reply = [0u8; 64];
    let n = rep.len().min(64);
    reply[..n].copy_from_slice(&rep[..n]);
    Some(reply)
}

/// What picks a feature reply out of a set of recorded ones: the report id, the command, and
/// its argument — the attribute of `0xAE`, the index of `0xF2`, the key string of `0xED`.
/// `set` is the SET frame, id first.
pub fn request_key(set: &[u8]) -> (u8, u8, &[u8]) {
    let rid = set.first().copied().unwrap_or(0);
    let cmd = set.get(1).copied().unwrap_or(0);
    let tail = set.get(3..).unwrap_or(&[]);
    let arg = match cmd {
        ID_GET_STRING_ATTRIBUTE | ID_GET_FIRMWARE_INFO => tail.get(..1).unwrap_or(&[]),
        ID_GET_CONFIG_VALUE => &tail[..tail.iter().position(|&b| b == 0).unwrap_or(tail.len())],
        _ => &[],
    };
    (rid, cmd, arg)
}
/// Output report id Steam rumbles with (`80 | type | intensity16 | Lspeed16 Lgain | Rspeed16 Rgain`).
pub const ID_OUT_REPORT_HAPTIC_RUMBLE: u8 = 0x80;

/// Wired Steam Controller 2 identity (`28DE:1302`) — Triton half of the `0x83` attributes reply.
const WIRED_PRODUCT: u32 = 0x1302;

/// Firmware build time (unix epoch) as attribute tag `4` (`ATTRIB_FIRMWARE_BUILD_TIME`) in
/// the `0x83` reply, mirrored at bytes 4..8 of the `0xF2` firmware-info reply — the two must
/// agree. `0x6A6D_3700` = 2026-08-01T00:00:00Z. An older synthetic epoch (Feb 2016) made
/// Steam offer to "update" the virtual pad, forwarding SET_REPORTs toward a real controller.
/// Bump when Steam learns a newer shipping firmware and starts prompting again.
pub const FW_BUILD_TIME: u32 = 0x6A6D_3700;

/// Bit 31 of an out-ring slot's `len` marks a FEATURE set (vs interrupt/output). Only Triton's
/// producer/consumer interpret it; other devtypes write plain lengths, so the bit is additive.
pub const OUT_FEATURE_BIT: u32 = 0x8000_0000;
#[inline]
pub const fn out_len(raw: u32) -> u32 {
    raw & !OUT_FEATURE_BIT
}
#[inline]
pub const fn out_is_feature(raw: u32) -> bool {
    raw & OUT_FEATURE_BIT != 0
}

/// Wire length (id byte included) of each input report the wired descriptor declares.
/// hidclass sizes its read buffer from the largest (0x42 → 54) and refuses over-long
/// completions. `None` = undeclared id, drop it (0x47 is BLE-only, not in the 372-byte descriptor).
pub const fn input_len(report_id: u8) -> Option<usize> {
    match report_id {
        0x42 => Some(54),
        0x45 => Some(46),
        0x43 => Some(15),
        0x44 => Some(6),
        0x79 => Some(2),
        0x7B => Some(13),
        _ => None,
    }
}

/// Wired `0x42` state report at rest: the id and an all-zero payload, of which a pad serves
/// `input_len(0x42)` bytes.
pub const NEUTRAL_REPORT: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x42;
    r
};

/// Declared wire length (id byte included) of each OUTPUT report. hidclass pads every write
/// to `OutputReportByteLength` (64), so the host trims before forwarding — a 0x80 rumble is
/// 10 bytes on GATT, not 64. Unknown id returns 64: no trim, never guess a length.
/// The Apple and Android `Sc2Device.strippedOutputLen` tables are this one without the id
/// byte; `clients/shared/sc2-vectors.json` holds all three to the same rows.
pub const fn out_report_len(id: u8) -> usize {
    match id {
        0x80 => 10,
        0x81 => 8,
        0x82 => 4,
        0x83 => 10,
        0x84 => 9,
        0x85 => 4,
        0x86 => 4,
        // 0x87/0x88/0x89 are declared full-length (63-byte payload) blobs.
        _ => 64,
    }
}

/// Per-pad unit id (`"TRI\0" | index` — same value the Linux leg uses).
pub const fn unit_id(index: u8) -> u32 {
    0x5452_4900 | index as u32
}

/// ASCII serial `FVPF1302<idx:02>D03`. Steam rejects a "PF"-leading serial; the FVPF prefix
/// is what the host's physical-conflict gate excludes.
pub fn serial(index: u8, out: &mut [u8; 13]) {
    const D: &[u8; 10] = b"0123456789";
    out.copy_from_slice(b"FVPF130200D03");
    out[8] = D[(index / 10 % 10) as usize];
    out[9] = D[(index % 10) as usize];
}

/// Wired Triton's captured 372-byte report descriptor. Byte-identical to the sysfs capture;
/// do not re-derive.
#[rustfmt::skip]
pub static RDESC: [u8; 372] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x40, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x02, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01,
    0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02,
    0x81, 0x06, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, 0x05, 0x0C, 0x0A, 0x38,
    0x02, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0, 0x05, 0x01, 0x09, 0x06, 0xA1,
    0x01, 0x85, 0x41, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
    0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x81, 0x01, 0x19, 0x00, 0x29,
    0x65, 0x15, 0x00, 0x25, 0x65, 0x75, 0x08, 0x95, 0x06, 0x81, 0x00, 0xC0,
    0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x42, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x35, 0x09, 0x42, 0x81, 0x02, 0x85, 0x44,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x05, 0x09, 0x44, 0x81,
    0x02, 0x85, 0x79, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01,
    0x09, 0x79, 0x81, 0x02, 0x85, 0x43, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
    0x08, 0x95, 0x0E, 0x09, 0x43, 0x81, 0x02, 0x85, 0x7B, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x0C, 0x09, 0x7B, 0x81, 0x02, 0x85, 0x45,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x2D, 0x09, 0x45, 0x81,
    0x02, 0x85, 0x80, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09,
    0x09, 0x80, 0x91, 0x02, 0x85, 0x81, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
    0x08, 0x95, 0x07, 0x09, 0x81, 0x91, 0x02, 0x85, 0x82, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x09, 0x82, 0x91, 0x02, 0x85, 0x83,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09, 0x09, 0x83, 0x91,
    0x02, 0x85, 0x84, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x08,
    0x09, 0x84, 0x91, 0x02, 0x85, 0x85, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
    0x08, 0x95, 0x03, 0x09, 0x85, 0x91, 0x02, 0x85, 0x86, 0x15, 0x00, 0x26,
    0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x09, 0x86, 0x91, 0x02, 0x85, 0x87,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x3F, 0x09, 0x87, 0x91,
    0x02, 0x85, 0x89, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x3F,
    0x09, 0x89, 0x91, 0x02, 0x85, 0x88, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
    0x08, 0x95, 0x3F, 0x09, 0x88, 0x91, 0x02, 0x85, 0x01, 0x95, 0x3F, 0x09,
    0x01, 0xB1, 0x02, 0x85, 0x02, 0x95, 0x3F, 0x09, 0x01, 0xB1, 0x02, 0xC0,
];

/// Feature GET_REPORT reply for Steam's `GetControllerInfo` query dance. The reply's command
/// byte must echo the last SET's command or Steam never adopts the pad. Frame is feature
/// report id 1 (`[0x01][cmd][len][payload…]`, matching SDL). `last_set` is id-first
/// (`[0x01, cmd, …]`); a stack that already stripped the id (`[cmd, …]`, cmd ≥ 0x80) works too.
pub fn feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> [u8; 64] {
    const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;

    let body = match last_set {
        [0x01, rest @ ..] => rest,
        d => d,
    };
    let cmd = body.first().copied().unwrap_or(ID_GET_STRING_ATTRIBUTE);

    let mut r = [0u8; 64];
    r[0] = 0x01;
    match cmd {
        ID_GET_ATTRIBUTES_VALUES => {
            // Captured controller response: 25-byte payload, five id/u32 attributes.
            r[1] = ID_GET_ATTRIBUTES_VALUES;
            r[2] = 0x19;
            let attrs = [
                (0x01, WIRED_PRODUCT),
                (0x02, 0),
                (0x0A, unit_id),
                // Tag 4 = ATTRIB_FIRMWARE_BUILD_TIME. See [`FW_BUILD_TIME`].
                (0x04, FW_BUILD_TIME),
                (0x09, 0x49),
            ];
            let mut o = 3;
            for (id, val) in attrs {
                r[o] = id;
                r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                o += 5;
            }
        }
        ID_GET_STRING_ATTRIBUTE => {
            // Captured replies always declare 20 bytes: attribute id plus a 19-byte padded string.
            let attr = body.get(2).copied().unwrap_or(ATTRIB_STR_UNIT_SERIAL);
            let b = serial.as_bytes();
            let len = b.len().min(19);
            r[..4].copy_from_slice(&[0x01, ID_GET_STRING_ATTRIBUTE, 0x14, attr]);
            r[4..4 + len].copy_from_slice(&b[..len]);
        }
        ID_GET_FIRMWARE_INFO => {
            let index = body.get(2).copied().unwrap_or(0);
            r[1] = ID_GET_FIRMWARE_INFO;
            r[3] = index;
            match index {
                0 => {
                    r[2] = 0x29;
                    // Must agree with the 0x83 reply's tag-4 attribute (Steam may cross-check).
                    r[4..8].copy_from_slice(&FW_BUILD_TIME.to_le_bytes());
                    r[8] = 0x49;
                    r[12..24].copy_from_slice(b"603f69218a85");
                    let b = serial.as_bytes();
                    let len = b.len().min(16);
                    r[28..28 + len].copy_from_slice(&b[..len]);
                }
                1 => {
                    r[2] = 0x22;
                    r[4..37].copy_from_slice(&[
                        0x00, 0x57, 0xD0, 0x18, 0x6A, 0x37, 0x30, 0x35, 0x34, 0x32, 0x35, 0x37,
                        0x64, 0x32, 0x64, 0x61, 0x37, 0x00, 0x00, 0x00, 0x00, 0x23, 0x00, 0x00,
                        0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x6D, 0x02, 0x00,
                    ]);
                }
                _ => {
                    r[2] = 0x09;
                    r[4..12].copy_from_slice(&[0x7C, 0x4F, 0x01, 0x00, 0x01, 0, 0, 0]);
                }
            }
        }
        _ => {
            let n = body.len().min(63);
            r[1..1 + n].copy_from_slice(&body[..n]);
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamepad;

    #[test]
    fn lizard_and_reset_writes_stay_on_the_virtual_pad() {
        assert!(!forwards_to_pad(&[0x01, 0x85, 0x00]));
        assert!(!forwards_to_pad(&[0x8E, 0x00]));
        assert!(!forwards_to_pad(&[0x01, 0x81]));
        assert!(
            forwards_to_pad(&[0x01, 0x87, 0x03, 0x08, 0x07]),
            "Steam's settings go through"
        );
        assert!(forwards_to_pad(&[0x01, 0xAE, 0x15, 0x01]));
    }

    /// The driver answers from the blob the host wrote; a torn blob is no identity.
    #[test]
    fn identity_blob_round_trips() {
        let serial_q = [0x01, 0xAE, 0x15, 0x01];
        let serial_r = [0x01, 0xAE, 0x15, 0x01, b'F', b'X', b'A'];
        let slot_q = [0x02, 0xB4, 0x00];
        let blob = identity_blob(
            "FXA9954800A07",
            [
                (&serial_q[..], &serial_r[..]),
                (&slot_q[..], &[0x02, 0xB4][..]),
            ],
        );
        let id = Identity::parse(&blob).expect("parses");
        assert_eq!(id.serial, "FXA9954800A07");
        assert_eq!(id.pairs().count(), 2);
        let mut asked = [0u8; 64];
        asked[..4].copy_from_slice(&serial_q);
        assert_eq!(id.reply(&asked).map(|r| r[4]), Some(b'F'));
        assert_eq!(id.reply(&slot_q), None, "slot state is live");
        assert!(Identity::parse(&blob[..blob.len() - 1]).is_none());
        assert!(Identity::parse(&identity_blob("", [(&serial_q[..], &[][..])])).is_some());
        assert!(Identity::parse(&[0]).is_some_and(|i| i.serial.is_empty()));
    }

    #[test]
    fn triton_devtype_is_the_next_free_slot() {
        assert_eq!(gamepad::DEVTYPE_TRITON, 7);
    }

    /// GET reply echoes the last SET's command; a mismatch makes Steam drop the pad.
    #[test]
    fn triton_feature_reply_echoes_the_queried_command() {
        // Settings write (lizard-off) reads back as a mirror.
        let set = [0x01, 0x87, 0x03, 0x09, 0x00, 0x00];
        let r = feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(r[0], 0x01);
        assert_eq!(&r[1..6], &[0x87, 0x03, 0x09, 0x00, 0x00]);
    }

    #[test]
    fn triton_feature_reply_synthesizes_attributes_for_0x83() {
        let set = [0x01, 0x83, 0x00];
        let r = feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(&r[..3], &[0x01, 0x83, 0x19]); // 25-byte TLV payload
        assert_eq!(r[3], 0x01); // first attribute id: product id
                                // Tag-4 TLV carries FW_BUILD_TIME; a stale epoch makes Steam prompt to update firmware.
        assert_eq!(r[18], 0x04);
        assert_eq!(r[19..23], FW_BUILD_TIME.to_le_bytes());
    }

    #[test]
    fn triton_firmware_info_build_time_agrees_with_the_attributes_reply() {
        let set = [0x01, 0xF2, 0x00, 0x00];
        let r = feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(&r[..4], &[0x01, 0xF2, 0x29, 0x00]);
        // Bytes 4..8 mirror the 0x83 reply's tag-4 build time — Steam may cross-check.
        assert_eq!(r[4..8], FW_BUILD_TIME.to_le_bytes());
    }

    #[test]
    fn request_key_keeps_only_what_selects_the_reply() {
        assert_eq!(request_key(&[0x01, 0x83, 0x00]), (0x01, 0x83, &[][..]));
        // Steam and hid-steam disagree on 0xAE's length byte; only the attribute counts.
        assert_eq!(
            request_key(&[0x01, 0xAE, 0x15, 0x00]),
            request_key(&[0x01, 0xAE, 0x14, 0x00, 0x00])
        );
        assert_ne!(
            request_key(&[0x01, 0xAE, 0x15, 0x00]),
            request_key(&[0x01, 0xAE, 0x15, 0x01])
        );
        assert_eq!(request_key(&[0x01, 0xF2, 0x01, 0x02]).2, &[0x02]);
        let mut bond = [0u8; 64];
        bond[..11].copy_from_slice(b"\x01\xED\x08esb/bond");
        assert_eq!(request_key(&bond), (0x01, 0xED, &b"esb/bond"[..]));
        assert_eq!(request_key(&[0x02, 0xA3, 0x00]), (0x02, 0xA3, &[][..]));
        assert_eq!(request_key(&[]), (0, 0, &[][..]));
    }

    #[test]
    fn triton_input_len_matches_the_descriptor() {
        assert_eq!(input_len(0x42), Some(54));
        assert_eq!(input_len(0x45), Some(46));
        assert_eq!(input_len(0x43), Some(15));
        assert_eq!(input_len(0x44), Some(6));
        assert_eq!(input_len(0x79), Some(2));
        assert_eq!(input_len(0x7B), Some(13));
        assert_eq!(input_len(0x47), None); // BLE-only id, not in the wired descriptor
        assert_eq!(input_len(0x01), None);
    }

    /// `clients/shared/sc2-vectors.json`: the Apple and Android `strippedOutputLen` tables replay
    /// the same rows one byte shorter.
    #[test]
    fn triton_out_report_len_matches_the_shared_vectors() {
        let raw = include_str!("../../../../clients/shared/sc2-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        for row in file["out_report_len"].as_array().expect("out_report_len") {
            let id = row["id"].as_u64().unwrap() as u8;
            // Undeclared ids stay whole (64 = no trim) — never guess a length.
            let want = row["len"].as_u64().unwrap_or(64) as usize;
            assert_eq!(out_report_len(id), want, "id {id:#x}");
        }
        assert_eq!(out_report_len(0x00), 64);
    }

    #[test]
    fn triton_serial_shape_dodges_the_pf_prefix_rejection() {
        let mut s = [0u8; 13];
        serial(3, &mut s);
        assert_eq!(&s, b"FVPF130203D03");
    }

    #[test]
    fn triton_rdesc_is_the_372_byte_capture() {
        assert_eq!(RDESC.len(), 372);
        // Mouse TLC opens it: Usage Page Generic Desktop, Usage Mouse, Collection App, Report ID 0x40.
        assert_eq!(
            &RDESC[..8],
            &[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x40]
        );
    }

    #[test]
    fn out_feature_bit_round_trips() {
        let tagged = 64u32 | OUT_FEATURE_BIT;
        assert_eq!(out_len(tagged), 64);
        assert!(out_is_feature(tagged));
        assert!(!out_is_feature(64));
        assert_eq!(out_len(64), 64);
    }
}
