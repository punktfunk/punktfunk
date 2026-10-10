//! SEI payload parsing — the piece vendored cros-codecs lacks (it classifies SEI
//! NALUs but never reads a payload). Only recovery-point SEI is parsed; every
//! other payload type is skipped by its declared size.
//!
//! Both codecs put recovery point at payload type 6 with the same D.1 framing;
//! payload syntax differs. H.264 (D.1.8/D.2.8): `recovery_frame_cnt` in
//! `frame_num` increments, ue(v), plus a slice-group bit pair. H.265 (D.2.8/D.3.8):
//! `recovery_poc_cnt` in picture order, se(v), may be negative, no slice-group
//! field. Two parsers, one shared message walk.

/// Recovery point SEI (D.2.8).
///
/// `recovery_frame_cnt` is in `frame_num` increments, not POC. `broken_link` means
/// pictures before the recovery point must not be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPoint {
    pub recovery_frame_cnt: u32,
    pub exact_match: bool,
    pub broken_link: bool,
}

/// Recovery point SEI, H.265 (D.3.8).
///
/// `recovery_poc_cnt` is a POC delta, se(v), so it can be negative (a recovery
/// point among leading pictures). `exact_match` / `broken_link` match H.264.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPointHevc {
    pub recovery_poc_cnt: i32,
    pub exact_match: bool,
    pub broken_link: bool,
}

/// First recovery-point SEI in an H.264 SEI NALU.
///
/// `sei_payload` starts after the one-byte NAL header; emulation-prevention bytes
/// are still in place (stripped here, 7.4.1). `Ok(None)`: parsed, no recovery point.
pub fn parse_recovery_point(sei_payload: &[u8]) -> Result<Option<RecoveryPoint>, String> {
    let rbsp = strip_emulation_prevention(sei_payload);
    let Some(payload) = first_recovery_point_payload(&rbsp)? else {
        return Ok(None);
    };

    let mut r = BitCursor::new(payload);
    let recovery_frame_cnt = r.read_ue()?;
    let exact_match = r.read_bit()? != 0;
    let broken_link = r.read_bit()? != 0;
    // changing_slice_group_idc u(2): consume so the cursor stays aligned; hosts never emit slice groups.
    let _changing_slice_group_idc = r.read_bits(2)?;
    Ok(Some(RecoveryPoint {
        recovery_frame_cnt,
        exact_match,
        broken_link,
    }))
}

/// First recovery-point SEI in an H.265 prefix SEI NALU (type 39).
///
/// `sei_payload` starts after the two-byte NAL header; emulation-prevention still
/// in place. D.2.1 lists recovery point as prefix-only; type-40 suffix must not
/// reach here.
pub fn parse_recovery_point_hevc(sei_payload: &[u8]) -> Result<Option<RecoveryPointHevc>, String> {
    let rbsp = strip_emulation_prevention(sei_payload);
    let Some(payload) = first_recovery_point_payload(&rbsp)? else {
        return Ok(None);
    };

    let mut r = BitCursor::new(payload);
    let recovery_poc_cnt = r.read_se()?;
    let exact_match = r.read_bit()? != 0;
    let broken_link = r.read_bit()? != 0;
    Ok(Some(RecoveryPointHevc {
        recovery_poc_cnt,
        exact_match,
        broken_link,
    }))
}

/// First recovery-point payload (type 6) from D.1 message framing, shared by
/// H.264 and H.265. `rbsp` is already emulation-prevention-stripped.
fn first_recovery_point_payload(rbsp: &[u8]) -> Result<Option<&[u8]>, String> {
    let mut i = 0usize;
    while i < rbsp.len() && !is_rbsp_trailing(rbsp, i) {
        // D.1 ff-coding: type saturates so a huge 0xFF run cannot overflow (a
        // saturated type never matches 6). Size is a bounds-checked usize.
        let mut payload_type = 0u32;
        while i < rbsp.len() && rbsp[i] == 0xFF {
            payload_type = payload_type.saturating_add(255);
            i += 1;
        }
        if i >= rbsp.len() {
            return Err("truncated SEI payload type".into());
        }
        payload_type = payload_type.saturating_add(u32::from(rbsp[i]));
        i += 1;

        let mut payload_size = 0usize;
        while i < rbsp.len() && rbsp[i] == 0xFF {
            payload_size += 255;
            i += 1;
        }
        if i >= rbsp.len() {
            return Err("truncated SEI payload size".into());
        }
        payload_size += usize::from(rbsp[i]);
        i += 1;

        let end = i
            .checked_add(payload_size)
            .filter(|&end| end <= rbsp.len())
            .ok_or_else(|| "SEI payload overruns the NALU".to_string())?;

        if payload_type == 6 {
            return Ok(Some(&rbsp[i..end]));
        }

        i = end;
    }

    Ok(None)
}

/// 7.4.1: `00 00 03` encodes two zeros; drop the `03`.
fn strip_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0usize;
    for &byte in data {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Byte-aligned `more_rbsp_data()`: stop bit 0x80, then only zeros.
fn is_rbsp_trailing(rbsp: &[u8], i: usize) -> bool {
    rbsp[i] == 0x80 && rbsp[i + 1..].iter().all(|&b| b == 0)
}

/// MSB-first bit reader over an unescaped RBSP slice. Vendored `BitReader` is
/// `pub(crate)` to that crate, so this one is local.
struct BitCursor<'a> {
    data: &'a [u8],
    /// Bit offset into `data` (not a byte index).
    pos: usize,
}

impl<'a> BitCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn read_bit(&mut self) -> Result<u32, String> {
        let byte = *self
            .data
            .get(self.pos / 8)
            .ok_or("SEI payload out of bits")?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, String> {
        debug_assert!(count <= 31);
        let mut out = 0u32;
        for _ in 0..count {
            out = (out << 1) | self.read_bit()?;
        }
        Ok(out)
    }

    /// ue(v), spec 9.1.
    fn read_ue(&mut self) -> Result<u32, String> {
        let mut leading_zeros = 0usize;
        while self.read_bit()? == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return Err("invalid exp-Golomb code in SEI payload".into());
            }
        }
        let suffix = self.read_bits(leading_zeros)?;
        ((1u32 << leading_zeros) - 1)
            .checked_add(suffix)
            .ok_or_else(|| "exp-Golomb value overflows u32".to_string())
    }

    /// se(v), spec 9.1.1: k → (−1)^(k+1) · ⌈k/2⌉.
    fn read_se(&mut self) -> Result<i32, String> {
        let k = self.read_ue()?;
        let magnitude = k.div_ceil(2);
        let magnitude =
            i32::try_from(magnitude).map_err(|_| "exp-Golomb value overflows i32".to_string())?;
        Ok(if k % 2 == 1 { magnitude } else { -magnitude })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_recovery_point_message_parses_to_its_field_values() {
        // type 6, size 1; ue(0)='1', exact=0, broken=0, csg=00, align → 0x84; trailing 0x80.
        let sei = [0x06, 0x01, 0x84, 0x80];
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );
    }

    #[test]
    fn recovery_frame_cnt_and_both_flags_round_trip_through_the_bit_reader() {
        // ue(5)='00110', exact=1, broken=1, csg=00, alignment -> 0b0011_0110 0b0100_0000.
        let sei = [0x06, 0x02, 0x36, 0x40, 0x80];
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 5,
                exact_match: true,
                broken_link: true
            })
        );
    }

    #[test]
    fn earlier_messages_and_ff_coded_types_are_skipped_to_reach_the_recovery_point() {
        let sei = [
            0xFF, 0x00, 0x01, 0x55, // type 255
            0x05, 0x03, 0xAA, 0xBB, 0xCC, // type 5
            0x06, 0x01, 0x84, // recovery point
            0x80,
        ];
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );
    }

    #[test]
    fn emulation_prevention_bytes_inside_the_payload_are_removed_before_reading() {
        // 22 leading zeros → recovery_frame_cnt = 2^22-1. Unescaped 00 00 02 becomes
        // 00 00 03 02 on the wire.
        let sei = [
            0x06, 0x07, 0x00, 0x00, 0x03, 0x02, 0x00, 0x00, 0x04, 0x40, 0x80,
        ];
        assert!(sei.windows(3).any(|w| w == [0x00, 0x00, 0x03]));
        assert_eq!(
            parse_recovery_point(&sei).unwrap(),
            Some(RecoveryPoint {
                recovery_frame_cnt: 4194303,
                exact_match: true,
                broken_link: false
            })
        );
    }

    #[test]
    fn a_sei_nalu_without_a_recovery_point_yields_none_not_an_error() {
        let sei = [0x05, 0x01, 0x00, 0x80];
        assert_eq!(parse_recovery_point(&sei).unwrap(), None);
    }

    #[test]
    fn a_payload_size_overrunning_the_nalu_is_a_parse_error() {
        let sei = [0x06, 0x0A, 0x00];
        assert!(parse_recovery_point(&sei).is_err());
    }

    #[test]
    fn the_hevc_recovery_point_parses_its_se_coded_poc_count() {
        // se(0)='1', exact=0, broken=0, align → 0x90.
        let sei = [0x06, 0x01, 0x90, 0x80];
        assert_eq!(
            parse_recovery_point_hevc(&sei).unwrap(),
            Some(RecoveryPointHevc {
                recovery_poc_cnt: 0,
                exact_match: false,
                broken_link: false
            })
        );

        // se(-1)='011' (ue k=2), exact=1, broken=0, align → 0x74. H.264 ue(v) cannot encode this.
        let sei = [0x06, 0x01, 0x74, 0x80];
        assert_eq!(
            parse_recovery_point_hevc(&sei).unwrap(),
            Some(RecoveryPointHevc {
                recovery_poc_cnt: -1,
                exact_match: true,
                broken_link: false
            })
        );
    }

    #[test]
    fn the_hevc_parser_skips_earlier_messages_and_reports_absence_as_none() {
        // type 5 then recovery: se(3) k=5 → 0x37.
        let sei = [
            0x05, 0x02, 0xAA, 0xBB, // type 5
            0x06, 0x01, 0x37, // recovery point
            0x80,
        ];
        assert_eq!(
            parse_recovery_point_hevc(&sei).unwrap(),
            Some(RecoveryPointHevc {
                recovery_poc_cnt: 3,
                exact_match: true,
                broken_link: true
            })
        );

        let sei = [0x05, 0x01, 0x00, 0x80];
        assert_eq!(parse_recovery_point_hevc(&sei).unwrap(), None);
    }
}
