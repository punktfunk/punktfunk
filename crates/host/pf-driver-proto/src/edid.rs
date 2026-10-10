//! The 256-byte EDID the `pf-vdisplay` driver hands IddCx for each virtual monitor: an EDID 1.4
//! base block plus a CTA-861.3 extension carrying a BT.2020 Colorimetry block and an HDR Static
//! Metadata block declaring the SMPTE ST 2084 (PQ) EOTF. Windows reads a display's HDR capability
//! from that CTA block; without it the monitor is SDR-only whatever the IddCx adapter's FP16 /
//! wide-gamut / 10-bit caps say.
//!
//! Identity: manufacturer "PNK", product name "Punktfunk" (the 0xFC descriptor Windows shows), and
//! a per-monitor serial at base offset 0x0C that [`get_serial`] reads back out of the EDID the OS
//! hands to the mode callbacks. No HDMI Vendor-Specific Data Block: a VSDB carries physical-sink
//! facts (CEC address, TMDS limits) a virtual display does not have, and Windows drives the
//! monitor without one.
//!
//! Lives here, not in the driver: the driver only builds under the WDK, and one wrong byte drops
//! HDR silently. `no_std` + integer-only, so it drops into the driver unchanged.

/// `2^(k/32)` for `k = 0..32` in Q16 fixed point (`round(2^(k/32) * 65536)`) — the fractional
/// step table for the CTA-861.3 luminance exponent.
const POW2_Q16: [u32; 32] = [
    65536, 66971, 68438, 69936, 71468, 73032, 74632, 76266, 77936, 79642, 81386, 83169, 84990,
    86851, 88752, 90696, 92682, 94711, 96785, 98905, 101070, 103283, 105545, 107856, 110218,
    112631, 115098, 117618, 120194, 122825, 125515, 128263,
];

/// Decode a CTA-861.3 max / frame-average luminance code to MILLI-nits:
/// `L = 50 * 2^(CV/32)` cd/m², so `L_millinits = 50_000 * 2^(CV/32)`.
/// (`CV = 255` ≈ 12_525 nits — comfortably inside u64 at Q16.)
pub const fn cta_max_millinits(code: u8) -> u64 {
    let whole = code as u32 / 32;
    let frac = code as u32 % 32;
    ((50_000u64 << whole) * POW2_Q16[frac as usize] as u64) >> 16
}

/// Largest CTA-861.3 code whose decoded luminance does not exceed `nits` — never advertise
/// brighter than the glass. Clamped to `1..=255`: `0` is "no data" on the wire; callers gate
/// on `nits > 0`. A sub-51-nit request (no real HDR panel) still codes as 1.
pub fn cta_max_luminance_code(nits: u32) -> u8 {
    let target = nits as u64 * 1000;
    let mut code = 1u8;
    while code < 255 && cta_max_millinits(code + 1) <= target {
        code += 1;
    }
    code
}

/// Floor integer square root (Newton). `u64::isqrt` needs Rust 1.84, above this crate's 1.82
/// MSRV. Converges in ≤ 6 iterations from the power-of-two seed.
fn isqrt_u64(x: u64) -> u64 {
    if x == 0 {
        return 0;
    }
    // Seed strictly above sqrt(x): 2^(ceil(bits/2)).
    let mut r = 1u64 << (64 - x.leading_zeros()).div_ceil(2);
    loop {
        let next = (r + x / r) / 2;
        if next >= r {
            return r;
        }
        r = next;
    }
}

/// Code a display's min luminance (MILLI-nits) as the CTA-861.3 min-luminance value, which is
/// relative to the block's coded max: `L_min = L_max * (CV/255)^2 / 100`, so
/// `CV = 255 * sqrt(100 * L_min / L_max)` — rounded to nearest. `max_code` is the byte
/// produced by [`cta_max_luminance_code`]; a result of `0` (a true-black panel, or
/// `millinits = 0` = unknown) is valid on the wire.
pub fn cta_min_luminance_code(millinits: u32, max_code: u8) -> u8 {
    let max_millinits = cta_max_millinits(max_code);
    if millinits == 0 || max_millinits == 0 {
        return 0;
    }
    // CV = sqrt(100 * 255^2 * L_min / L_max); round to nearest by comparing the two flanking
    // squares (the integer sqrt floors).
    let x = (100u64 * 255 * 255).saturating_mul(millinits as u64) / max_millinits;
    let floor = isqrt_u64(x);
    let cv = if (floor + 1) * (floor + 1) - x <= x - floor * floor {
        floor + 1
    } else {
        floor
    };
    cv.min(255) as u8
}

/// Fixed reduced-blanking geometry for [`dtd`] (CVT-RBv2-shaped): 80 px of horizontal and 45
/// lines of vertical blanking, front-porch/sync splits within them. A virtual display has no
/// real scan-out, so the blanking only has to be self-consistent — the pixel clock is derived
/// from these same totals.
const DTD_H_BLANK: u32 = 80;
const DTD_V_BLANK: u32 = 45;
const DTD_H_SYNC_OFFSET: u32 = 8;
const DTD_H_SYNC_WIDTH: u32 = 32;
const DTD_V_SYNC_OFFSET: u32 = 3;
const DTD_V_SYNC_WIDTH: u32 = 5;

/// 18-byte EDID detailed timing descriptor for `width`×`height`@`refresh_hz` with the fixed
/// reduced blanking above. `None` when the mode does not fit: pixel clock above 655.35 MHz
/// (u16 10 kHz field — 4K120-class) or active dimensions above the 12-bit fields. Flags byte
/// 0x1E: digital separate sync, +H/+V.
pub fn dtd(width: u32, height: u32, refresh_hz: u32) -> Option<[u8; 18]> {
    if width == 0 || height == 0 || refresh_hz == 0 || width > 4095 || height > 4095 {
        return None;
    }
    let h_total = u64::from(width + DTD_H_BLANK);
    let v_total = u64::from(height + DTD_V_BLANK);
    let clock_10khz = h_total * v_total * u64::from(refresh_hz) / 10_000;
    let clock_10khz = u16::try_from(clock_10khz).ok()?;
    let mut d = [0u8; 18];
    d[0..2].copy_from_slice(&clock_10khz.to_le_bytes());
    d[2] = (width & 0xFF) as u8;
    d[3] = (DTD_H_BLANK & 0xFF) as u8;
    d[4] = (((width >> 8) & 0x0F) << 4) as u8 | ((DTD_H_BLANK >> 8) & 0x0F) as u8;
    d[5] = (height & 0xFF) as u8;
    d[6] = (DTD_V_BLANK & 0xFF) as u8;
    d[7] = (((height >> 8) & 0x0F) << 4) as u8 | ((DTD_V_BLANK >> 8) & 0x0F) as u8;
    d[8] = (DTD_H_SYNC_OFFSET & 0xFF) as u8;
    d[9] = (DTD_H_SYNC_WIDTH & 0xFF) as u8;
    d[10] = (((DTD_V_SYNC_OFFSET & 0x0F) << 4) | (DTD_V_SYNC_WIDTH & 0x0F)) as u8;
    d[11] = ((((DTD_H_SYNC_OFFSET >> 8) & 0x03) << 6)
        | (((DTD_H_SYNC_WIDTH >> 8) & 0x03) << 4)
        | (((DTD_V_SYNC_OFFSET >> 4) & 0x03) << 2)
        | ((DTD_V_SYNC_WIDTH >> 4) & 0x03)) as u8;
    // Bytes 12..16 (image size mm, borders) stay 0 = undefined.
    d[17] = 0x1E;
    Some(d)
}

/// Per-monitor serial number: base-block offset 0x0C, little-endian u32.
const SERIAL_OFFSET: usize = 0x0C;

/// EDID 1.4 base block. Differs from a plain SDR virtual EDID by revision 1.4 (byte 19),
/// 10-bit digital video input (byte 20 = 0xB0) and one extension present (byte 126 = 0x01).
/// The checksum (byte 127), the serial at 0x0C and the preferred DTD are patched in
/// [`generate`], so editing the name here needs no hand-computed checksum.
#[rustfmt::skip]
const BASE: [u8; 128] = [
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, // fixed header
    0x41, 0xCB, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, // mfr "PNK", product code 1 (0 = "unset" to EDID tooling), serial (patched)
    0xFF, 0x21, 0x01, 0x04, 0xB0, 0x32, 0x1F, 0x78, // week/year, EDID 1.4, 10-bit digital, size, gamma
    0x03, 0x78, 0xB1, 0xB5, 0x4A, 0x2B, 0xCC, 0x21, // feature (sRGB-default CLEARED), BT.2020 primaries...
    0x0B, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, // ...BT.2020 primaries, established timings, std timings
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3A, // std timings, DTD 1 (placeholder preferred timing)
    0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C,
    0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E,
    0x00, 0x00, 0x00, 0xFD, 0x08, 0x17, 0xF0, 0x0F, // range-limits: offsets H-max+255, 23-240 Hz, min-H 15 kHz...
    0xFF, 0xFF, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20, // ...max-H 510 kHz, max clock 2550 MHz (150 was below the driver's own 1080p120 default)
    0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x50, // name descriptor "Punktfunk"
    0x75, 0x6E, 0x6B, 0x74, 0x66, 0x75, 0x6E, 0x6B,
    0x0A, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00, // empty 4th descriptor...
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, // ...byte 126 = 1 extension, byte 127 = checksum
];

/// CTA-861.3 extension block header (block 1, bytes 0..4). What follows is a Data Block
/// Collection holding the Colorimetry and HDR Static Metadata blocks; the rest of the block is
/// padding up to the checksum at byte 255.
#[rustfmt::skip]
const CTA_HEADER: [u8; 4] = [
    0x02, // CTA Extension tag
    0x03, // revision 3 (CTA-861.3 — required for the extended-tag data blocks below)
    0x0F, // D = 15: the (empty) DTD region starts at block byte 15, i.e. data blocks occupy bytes 4..15
    0x00, // 0 native DTDs; no basic audio; no YCbCr 4:4:4/4:2:2 (RGB-only, matching the wire format)
];

/// Colorimetry Data Block (CTA extended tag 0x05): declare BT.2020 RGB. YCbCr variants stay
/// clear — the IddCx wire format is RGB-only — and the gamut-metadata flags are 0.
#[rustfmt::skip]
const COLORIMETRY_DB: [u8; 4] = [
    0xE3, // tag 0b111 (use-extended-tag) | length 3
    0x05, // extended tag: Colorimetry
    0x80, // BT2020RGB (bit 7); xvYCC/sYCC/opRGB/BT2020 YCC/cYCC all clear
    0x00, // gamut metadata profiles MD0..MD3: none
];

/// HDR Static Metadata Data Block (CTA extended tag 0x06): EOTFs = Traditional SDR (ET_0) plus
/// SMPTE ST 2084 / PQ (ET_2), Static Metadata Type 1 (SM_0). The desired-content luminance tail
/// holds the BUILT-IN defaults, used when the host reported no client volume; [`generate`]
/// overwrites bytes 4..7 with the client display's coded volume otherwise.
#[rustfmt::skip]
const HDR_STATIC_METADATA_DB: [u8; 7] = [
    0xE6, // tag 0b111 (use-extended-tag) | length 6
    0x06, // extended tag: HDR Static Metadata
    0x05, // Supported EOTFs: ET_0 (traditional SDR) | ET_2 (SMPTE ST 2084 / PQ)
    0x01, // Supported Static Metadata Descriptors: SM_0 (Static Metadata Type 1)
    0x8A, // Desired Content Max Luminance      (code 138 ≈ 993 nits)
    0x60, // Desired Content Max Frame-avg Lum. (code  96 = 400 nits)
    0x12, // Desired Content Min Luminance      (code  18 ≈ 0.05 nits)
];

/// The client display's luminance volume for the CTA HDR block — the
/// [`crate::control::AddRequest`] luminance tail, same units. `max_nits == 0` means unknown (an
/// SDR client, or an un-upgraded host whose short ADD zero-fills the tail) and keeps the
/// built-in defaults.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientLuminance {
    /// Peak luminance, nits. `0` = unknown → keep the built-in default block.
    pub max_nits: u32,
    /// Max frame-average luminance, nits. `0` = unknown ("no data" on the wire).
    pub max_frame_avg_nits: u32,
    /// Min luminance, milli-nits. `0` = unknown/true black ("no data" on the wire).
    pub min_millinits: u32,
}

/// EDID screen-size bytes 0x15/0x16 hold the image size in CENTIMETRES, and Windows derives a
/// display's DPI from resolution over that size. A FIXED size therefore makes the virtual
/// display's scale ride its mode: the 50 cm this EDID used to declare reads as ~97 DPI at
/// 1080p but ~260 DPI at 5120 px wide, so the OS quite correctly scales the desktop up and
/// hands out a cursor to match. Sizing from the mode pins the display near 96 DPI, which
/// leaves scaling where it belongs — the client's own choice, not an artefact of our EDID.
///
/// One byte each, so `1..=255`: 0 means "undefined" (projectors) and would put the DPI
/// decision back with the OS.
fn size_cm(px: u32) -> u8 {
    (u64::from(px) * 254 / 9600).clamp(1, 255) as u8
}

/// Base-block offsets of the horizontal and vertical image size.
const H_SIZE_CM_OFFSET: usize = 0x15;
const V_SIZE_CM_OFFSET: usize = 0x16;

/// Build the 256-byte EDID for the monitor identified by `serial`, with both block checksums
/// recomputed — the serial patch at 0x0C and the CTA edits below both invalidate them.
///
/// `lum` is the CLIENT display's luminance volume, coded into the HDR block's desired-content
/// bytes so apps tone-map to the panel the stream lands on; all-zero keeps the built-in
/// ~993-nit defaults. `preferred` is the session's `(width, height, refresh)`: it replaces the
/// hard-coded 1080p60 preferred-timing DTD when it fits the encoding (4K120-class does not).
/// The modes the OS OFFERS still come from the IddCx mode list, not this descriptor.
#[must_use]
pub fn generate(
    serial: u32,
    lum: ClientLuminance,
    preferred: Option<(u32, u32, u32)>,
) -> [u8; 256] {
    let mut edid = [0u8; 256];
    edid[..128].copy_from_slice(&BASE);
    edid[SERIAL_OFFSET..SERIAL_OFFSET + 4].copy_from_slice(&serial.to_le_bytes());
    if let Some(d) = preferred.and_then(|(w, h, r)| dtd(w, h, r)) {
        edid[54..72].copy_from_slice(&d);
    }
    // Declare a size that puts this mode near 96 DPI, or the OS scales the desktop for a
    // panel we only claimed to be.
    if let Some((w, h, _)) = preferred {
        edid[H_SIZE_CM_OFFSET] = size_cm(w);
        edid[V_SIZE_CM_OFFSET] = size_cm(h);
    }
    edid[128..132].copy_from_slice(&CTA_HEADER);
    edid[132..136].copy_from_slice(&COLORIMETRY_DB);
    let mut hdr_db = HDR_STATIC_METADATA_DB;
    if lum.max_nits > 0 {
        let max_code = cta_max_luminance_code(lum.max_nits);
        hdr_db[4] = max_code;
        hdr_db[5] = if lum.max_frame_avg_nits > 0 {
            cta_max_luminance_code(lum.max_frame_avg_nits)
        } else {
            0 // "no data" — valid per CTA-861.3
        };
        hdr_db[6] = cta_min_luminance_code(lum.min_millinits, max_code);
    }
    edid[136..143].copy_from_slice(&hdr_db);
    fix_block_checksum(&mut edid, 0);
    fix_block_checksum(&mut edid, 128);
    edid
}

/// Read the per-monitor serial (base offset 0x0C, little-endian) out of an EDID the OS handed
/// back, so a monitor-description callback can find the monitor it belongs to. Takes the full
/// 256-byte EDID or just the 128-byte base block, and errors rather than panics on a short
/// buffer so the caller can reject a malformed descriptor.
pub fn get_serial(edid: &[u8]) -> Result<u32, core::array::TryFromSliceError> {
    let bytes: [u8; 4] = edid
        .get(SERIAL_OFFSET..SERIAL_OFFSET + 4)
        .unwrap_or(&[])
        .try_into()?;
    Ok(u32::from_le_bytes(bytes))
}

/// Set the trailing byte of the 128-byte block at `start` so the block's bytes sum to 0
/// (mod 256) — the standard EDID block checksum, without which a parser rejects the block.
fn fix_block_checksum(edid: &mut [u8], start: usize) {
    let sum = edid[start..start + 127]
        .iter()
        .fold(0u8, |acc, &b| acc.wrapping_add(b));
    edid[start + 127] = 0u8.wrapping_sub(sum);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtd_encodes_the_session_mode() {
        // 1920×1080@60 with the fixed RB blanking: totals 2000×1125 → 135.00 MHz = 13500 × 10 kHz.
        let d = dtd(1920, 1080, 60).expect("1080p60 fits the DTD encoding");
        assert_eq!(u16::from_le_bytes([d[0], d[1]]), 13_500);
        // Active dimensions round-trip through the split 8+4-bit fields.
        assert_eq!(u32::from(d[2]) | (u32::from(d[4] >> 4) << 8), 1920);
        assert_eq!(u32::from(d[5]) | (u32::from(d[7] >> 4) << 8), 1080);
        // Blanking fields carry the fixed geometry; flags match the legacy descriptor.
        assert_eq!(u32::from(d[3]) | (u32::from(d[4] & 0x0F) << 8), 80);
        assert_eq!(u32::from(d[6]) | (u32::from(d[7] & 0x0F) << 8), 45);
        assert_eq!(d[17], 0x1E);
    }

    #[test]
    fn dtd_rejects_what_the_encoding_cannot_carry() {
        // 4K120: (3840+80)·(2160+45)·120 ≈ 1.037 GHz — past the u16 10 kHz pixel-clock field.
        assert_eq!(dtd(3840, 2160, 120), None);
        // 4K60 fits (≈518 MHz).
        assert!(dtd(3840, 2160, 60).is_some());
        // Degenerate and over-wide modes are refused, not mis-encoded.
        assert_eq!(dtd(0, 1080, 60), None);
        assert_eq!(dtd(5000, 1080, 10), None);
    }

    /// Sum of a 128-byte EDID block: the trailing checksum byte is what drives this to 0.
    fn block_sum(block: &[u8]) -> u8 {
        block.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
    }

    #[test]
    fn edid_matches_the_golden_bytes() {
        // Byte-for-byte capture of what the driver shipped before this code moved here. One byte
        // out of place and Windows drops HDR, so nothing below may drift silently. Two bytes DO
        // differ from that capture on purpose: 0x15/0x16 now size the panel from the mode
        // (2560x1440 -> 67x38 cm), which moves the checksum at 0x7F by the same 24.
        #[rustfmt::skip]
        const GOLDEN: [u8; 256] = [
            0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
            0x41, 0xCB, 0x01, 0x00, 0x07, 0x00, 0x00, 0x00,
            0xFF, 0x21, 0x01, 0x04, 0xB0, 0x43, 0x26, 0x78,
            0x03, 0x78, 0xB1, 0xB5, 0x4A, 0x2B, 0xCC, 0x21,
            0x0B, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0xC4, 0xB7,
            0x00, 0x50, 0xA0, 0xA0, 0x2D, 0x50, 0x08, 0x20,
            0x35, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E,
            0x00, 0x00, 0x00, 0xFD, 0x08, 0x17, 0xF0, 0x0F,
            0xFF, 0xFF, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x50,
            0x75, 0x6E, 0x6B, 0x74, 0x66, 0x75, 0x6E, 0x6B,
            0x0A, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x27,
            0x02, 0x03, 0x0F, 0x00, 0xE3, 0x05, 0x80, 0x00,
            0xE6, 0x06, 0x05, 0x01, 0x72, 0x52, 0x0F, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xBF,
        ];
        let lum = ClientLuminance {
            max_nits: 600,
            max_frame_avg_nits: 300,
            min_millinits: 20,
        };
        assert_eq!(generate(7, lum, Some((2560, 1440, 120))), GOLDEN);
    }

    /// The declared panel size must track the mode, or the display's DPI rides its resolution:
    /// a fixed 50 cm reads as ~97 DPI at 1080p but ~260 at 5120 wide, and the OS then scales the
    /// desktop (and the cursor) for a panel we only claimed to be.
    #[test]
    fn the_declared_panel_size_keeps_every_mode_near_96_dpi() {
        for (w, h) in [(1920u32, 1080u32), (2560, 1440), (3840, 2160), (5120, 1440)] {
            let e = generate(1, ClientLuminance::default(), Some((w, h, 60)));
            let (cm_w, cm_h) = (u32::from(e[0x15]), u32::from(e[0x16]));
            assert!(cm_w > 0 && cm_h > 0, "{w}x{h}: zero means undefined");
            // dpi = px / (cm / 2.54). Rounding to whole centimetres is the only error here.
            let dpi_w = w * 254 / (cm_w * 100);
            let dpi_h = h * 254 / (cm_h * 100);
            assert!(
                (92..=100).contains(&dpi_w) && (92..=100).contains(&dpi_h),
                "{w}x{h} declared {cm_w}x{cm_h} cm = {dpi_w}x{dpi_h} DPI, wanted ~96"
            );
        }
    }

    #[test]
    fn edid_blocks_checksum_to_zero() {
        let lum = ClientLuminance {
            max_nits: 1000,
            max_frame_avg_nits: 400,
            min_millinits: 50,
        };
        for (serial, l, mode) in [
            (0, ClientLuminance::default(), None),
            (1, lum, Some((1920, 1080, 60))),
            (u32::MAX, lum, Some((3840, 2160, 120))),
        ] {
            let e = generate(serial, l, mode);
            assert_eq!(block_sum(&e[..128]), 0, "base block, serial {serial}");
            assert_eq!(block_sum(&e[128..]), 0, "CTA block, serial {serial}");
        }
    }

    #[test]
    fn edid_serial_lands_at_0x0c_and_rechecksums() {
        for serial in [0u32, 1, 7, 0x00FF_00FF, u32::MAX] {
            let e = generate(serial, ClientLuminance::default(), None);
            assert_eq!(e[0x0C..0x10], serial.to_le_bytes());
            assert_eq!(get_serial(&e).unwrap(), serial);
            // The base block alone is what the mode callbacks sometimes get handed back.
            assert_eq!(get_serial(&e[..128]).unwrap(), serial);
            assert_eq!(block_sum(&e[..128]), 0);
        }
        // A short descriptor is rejected, not read out of bounds.
        assert!(get_serial(&[0u8; 8]).is_err());
    }

    #[test]
    fn edid_swaps_the_preferred_dtd_only_when_the_mode_fits() {
        let lum = ClientLuminance::default();
        let placeholder = generate(1, lum, None);
        // The stock descriptor is the 148.50 MHz 1080p60 timing baked into the base block.
        assert_eq!(
            u16::from_le_bytes([placeholder[54], placeholder[55]]),
            14_850
        );
        // A mode that fits replaces all 18 bytes; 4K120 does not fit, so the stock one stays.
        let swapped = generate(1, lum, Some((2560, 1440, 120)));
        assert_eq!(swapped[54..72], dtd(2560, 1440, 120).unwrap());
        let too_fast = generate(1, lum, Some((3840, 2160, 120)));
        assert_eq!(too_fast[54..72], placeholder[54..72]);
    }

    #[test]
    fn edid_hdr_block_tracks_the_client_volume() {
        // No client volume reported: the built-in ~993 / 400 / 0.05 nit defaults stay.
        let stock = generate(1, ClientLuminance::default(), None);
        assert_eq!(stock[136..143], [0xE6, 0x06, 0x05, 0x01, 0x8A, 0x60, 0x12]);
        // A known peak overrides all three; the EOTF and descriptor bytes never move.
        let lum = ClientLuminance {
            max_nits: 400,
            max_frame_avg_nits: 400,
            min_millinits: 50,
        };
        let coded = generate(1, lum, None);
        assert_eq!(coded[136..140], [0xE6, 0x06, 0x05, 0x01]);
        assert_eq!(coded[140], cta_max_luminance_code(400));
        // Unknown frame-average and unknown min both code as 0 = "no data" on the wire.
        let partial = generate(
            1,
            ClientLuminance {
                max_nits: 400,
                ..Default::default()
            },
            None,
        );
        assert_eq!(partial[141], 0);
        assert_eq!(partial[142], 0);
    }

    #[test]
    fn cta_luminance_codes_hit_the_reference_points() {
        // Historical built-in EDID block: 0x8A ≈ 993 nits, 0x60 = 400 nits (exact), 0x12 ≈ 0.05 nit.
        assert_eq!(cta_max_millinits(0x60), 400_000); // 50·2^3 exactly
        assert_eq!(cta_max_millinits(0x8A) / 1000, 993);
        assert_eq!(cta_max_luminance_code(400), 0x60);
        // 0x8A decodes to 993.481 nits; 994 is the smallest whole-nit input that reaches it.
        assert_eq!(cta_max_luminance_code(994), 0x8A);
        assert_eq!(cta_min_luminance_code(50, 0x8A), 0x12); // 0.05 nits @ a 993-nit max
                                                            // Never advertise brighter than the panel. 1000 nits sits between 138 (993) and 139 (~1015).
        assert_eq!(cta_max_luminance_code(1000), 138);
        assert!(cta_max_millinits(cta_max_luminance_code(1000)) <= 1_000_000);
        // Every real code decodes at or below its input, within one step (~2.2%).
        // Starts above code 1's 51.094 nits — beneath that the documented clamp-to-1 wins.
        for nits in [52u32, 80, 120, 250, 400, 604, 800, 1_499, 4_000, 10_000] {
            let c = cta_max_luminance_code(nits);
            let dec = cta_max_millinits(c);
            assert!(dec <= nits as u64 * 1000, "{nits} → {c} decoded {dec}");
            assert!(
                dec * 1023 / 1000 >= nits as u64 * 1000,
                "{nits} → {c} more than a step low"
            );
        }
        // 0/tiny stays a valid on-wire code (callers gate on nits > 0); the ceiling saturates at 255.
        assert_eq!(cta_max_luminance_code(0), 1);
        assert_eq!(cta_max_luminance_code(u32::MAX), 255);
        // Min-luminance: 0 = unknown/true black stays 0; a floor brighter than the max clamps.
        assert_eq!(cta_min_luminance_code(0, 0x8A), 0);
        assert_eq!(cta_min_luminance_code(u32::MAX, 1), 255);
        // HDR400: max 400 nits / min 0.4 nits.
        let max_c = cta_max_luminance_code(400);
        let min_c = cta_min_luminance_code(400, max_c);
        // L_min = L_max·(cv/255)²/100 — must come back within ~10% of 0.4 nits.
        let back = cta_max_millinits(max_c) * (min_c as u64 * min_c as u64) / (255 * 255) / 100;
        assert!((360..=440).contains(&back), "min decoded {back} millinits");
    }
}
