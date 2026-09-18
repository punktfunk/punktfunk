//! HDR10 static-metadata helpers shared by capture (source mastering) and encode
//! (in-band SEI). Platform-independent so the byte-level logic is unit-tested on
//! every target.
//!
//! Units follow HDR10 so values pass through:
//! - chromaticities in 1/50000 (SMPTE ST.2086 / `DXGI_HDR_METADATA_HDR10`),
//! - mastering luminance in 0.0001 cd/m²,
//! - MaxCLL/MaxFALL in cd/m² (nits).
//!
//! SEI and AV1 metadata builders feed the NVENC and Vulkan Video encoders; display
//! conversion the Windows capturers; the PQ cursor re-encode the Linux HDR blends.

/// SMPTE ST.2086 mastering volume + CEA-861.3 content light level, in HDR10
/// SEI fixed-point units. Field-for-field the wire `punktfunk_core::quic::HdrMeta`;
/// this copy keeps the encoders free of the QUIC crate. `pf_encode` converts.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HdrMeta {
    /// Primaries G, B, R as (x, y) in 1/50000 units (ST.2086 order is G, B, R).
    pub display_primaries: [[u16; 2]; 3],
    /// White point (x, y) in 1/50000 units.
    pub white_point: [u16; 2],
    /// Max display mastering luminance, 0.0001 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Min display mastering luminance, 0.0001 cd/m².
    pub min_display_mastering_luminance: u32,
    /// MaxCLL, nits. `0` = unknown.
    pub max_cll: u16,
    /// MaxFALL, nits. `0` = unknown.
    pub max_fall: u16,
}

// 12 + 4 + 4 + 4 + 2 + 2: the same 28 bytes the 0xCE datagram body carries.
const _: () = assert!(std::mem::size_of::<HdrMeta>() == 28);

/// HEVC/H.264 SEI payload type `mastering_display_colour_volume` (ST.2086). Same
/// code point in AVC and HEVC.
pub const SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME: u32 = 137;
/// HEVC/H.264 SEI payload type `content_light_level_info` (CEA-861.3).
pub const SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO: u32 = 144;

fn xy_to_2086(v: f32) -> u16 {
    (v * 50000.0).round().clamp(0.0, 65535.0) as u16
}

/// Build [`HdrMeta`] from a source display's measured colour volume (CIE xy,
/// cd/m²). `max_cll`/`max_fall` are nits; pass `0` when unknown — GetDesc1 does
/// not expose them, and `0` lets the display fall back to mastering luminance.
#[allow(clippy::too_many_arguments)]
pub fn hdr_meta_from_display(
    red: (f32, f32),
    green: (f32, f32),
    blue: (f32, f32),
    white: (f32, f32),
    max_mastering_nits: f32,
    min_mastering_nits: f32,
    max_cll: u16,
    max_fall: u16,
) -> HdrMeta {
    HdrMeta {
        // ST.2086 stores primaries in G, B, R order.
        display_primaries: [
            [xy_to_2086(green.0), xy_to_2086(green.1)],
            [xy_to_2086(blue.0), xy_to_2086(blue.1)],
            [xy_to_2086(red.0), xy_to_2086(red.1)],
        ],
        white_point: [xy_to_2086(white.0), xy_to_2086(white.1)],
        max_display_mastering_luminance: (max_mastering_nits.max(0.0) * 10_000.0).round() as u32,
        min_display_mastering_luminance: (min_mastering_nits.max(0.0) * 10_000.0).round() as u32,
        max_cll,
        max_fall,
    }
}

/// [`HdrMeta`] volume → pf-vdisplay `AddRequest` luminance:
/// `(max nits, max frame-average nits, min milli-nits)`.
///
/// Mastering luminance is 0.0001 cd/m² (nits = /10_000, milli-nits = /10).
/// MaxFALL is already nits and is the display's frame-average ceiling.
pub fn vdisplay_luminance_fields(m: &HdrMeta) -> (u32, u32, u32) {
    (
        m.max_display_mastering_luminance / 10_000,
        m.max_fall as u32,
        m.min_display_mastering_luminance / 10,
    )
}

/// BT.2020 / D65 / 1000-nit HDR10 default until the source display is read.
pub fn generic_hdr10() -> HdrMeta {
    HdrMeta {
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]], // BT.2020 G, B, R
        white_point: [15635, 16450],                                      // D65
        max_display_mastering_luminance: 10_000_000,                      // 1000 nits
        min_display_mastering_luminance: 1,                               // 0.0001 nits
        max_cll: 1000,
        max_fall: 400,
    }
}

/// `mastering_display_colour_volume` SEI payload: 24 bytes, big-endian, G/B/R
/// per ST.2086. Pass the raw bytes to NVENC `NV_ENC_SEI_PAYLOAD`.
pub fn hevc_mastering_display_sei(m: &HdrMeta) -> [u8; 24] {
    let mut b = [0u8; 24];
    let mut o = 0;
    let mut put16 = |v: u16| {
        b[o..o + 2].copy_from_slice(&v.to_be_bytes());
        o += 2;
    };
    for p in m.display_primaries.iter() {
        put16(p[0]);
        put16(p[1]);
    }
    put16(m.white_point[0]);
    put16(m.white_point[1]);
    let mut put32 = |v: u32| {
        b[o..o + 4].copy_from_slice(&v.to_be_bytes());
        o += 4;
    };
    put32(m.max_display_mastering_luminance);
    put32(m.min_display_mastering_luminance);
    debug_assert_eq!(o, 24);
    b
}

/// `content_light_level_info` SEI payload: 4 bytes, big-endian, MaxCLL then MaxFALL.
pub fn hevc_content_light_level_sei(m: &HdrMeta) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[0..2].copy_from_slice(&m.max_cll.to_be_bytes());
    b[2..4].copy_from_slice(&m.max_fall.to_be_bytes());
    b
}

/// Both HDR10 messages as one Annex-B HEVC prefix SEI NAL (type 39), for encoders that write
/// their own NAL units. Emulation prevention included.
pub fn hevc_hdr_sei_nal(m: &HdrMeta) -> Vec<u8> {
    let mut rbsp = vec![SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME as u8, 24];
    rbsp.extend_from_slice(&hevc_mastering_display_sei(m));
    rbsp.extend_from_slice(&[SEI_TYPE_CONTENT_LIGHT_LEVEL_INFO as u8, 4]);
    rbsp.extend_from_slice(&hevc_content_light_level_sei(m));
    rbsp.push(0x80); // rbsp_trailing_bits
    let mut nal = vec![0, 0, 0, 1, 39 << 1, 1];
    let mut zeros = 0;
    for b in rbsp {
        if zeros >= 2 && b <= 3 {
            nal.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        nal.push(b);
    }
    nal
}

/// [`HdrMeta`] in AV1 metadata units (spec 6.7.4): primaries R, G, B and the white point in
/// 0.16 fixed point, maximum luminance 24.8 and minimum 18.14 fixed point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Av1Mdcv {
    pub primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    pub luminance_max: u32,
    pub luminance_min: u32,
}

pub fn av1_mdcv(m: &HdrMeta) -> Av1Mdcv {
    let q16 = |v: u16| ((u32::from(v) * 65536 + 25000) / 50000).min(65535) as u16;
    let xy = |[x, y]: [u16; 2]| [q16(x), q16(y)];
    let fixed = |v: u32, frac: u64| {
        (((u64::from(v) << frac) + 5_000) / 10_000).min(u64::from(u32::MAX)) as u32
    };
    Av1Mdcv {
        primaries: [
            xy(m.display_primaries[2]),
            xy(m.display_primaries[0]),
            xy(m.display_primaries[1]),
        ],
        white_point: xy(m.white_point),
        luminance_max: fixed(m.max_display_mastering_luminance, 8),
        luminance_min: fixed(m.min_display_mastering_luminance, 14),
    }
}

/// AV1 `OBU_METADATA` for the same volume: `METADATA_TYPE_HDR_MDCV` then `HDR_CLL`.
pub fn av1_hdr_metadata_obus(m: &HdrMeta) -> Vec<u8> {
    let a = av1_mdcv(m);
    let mut mdcv = vec![2u8]; // metadata_type HDR_MDCV
    for [x, y] in a.primaries {
        mdcv.extend_from_slice(&x.to_be_bytes());
        mdcv.extend_from_slice(&y.to_be_bytes());
    }
    mdcv.extend_from_slice(&a.white_point[0].to_be_bytes());
    mdcv.extend_from_slice(&a.white_point[1].to_be_bytes());
    mdcv.extend_from_slice(&a.luminance_max.to_be_bytes());
    mdcv.extend_from_slice(&a.luminance_min.to_be_bytes());
    let mut cll = vec![1u8]; // metadata_type HDR_CLL
    cll.extend_from_slice(&m.max_cll.to_be_bytes());
    cll.extend_from_slice(&m.max_fall.to_be_bytes());
    let mut out = Vec::new();
    for payload in [mdcv, cll] {
        // obu_header: type OBU_METADATA (5), has_size_field; payload + trailing byte < 128.
        out.extend_from_slice(&[0x2a, payload.len() as u8 + 1]);
        out.extend_from_slice(&payload);
        out.push(0x80); // trailing_bits
    }
    out
}

/// Insert `obus` ahead of the first frame, frame-header or tile-group OBU of a temporal unit,
/// after any temporal delimiter and sequence header. `false` if the unit could not be walked.
pub fn av1_insert_before_frame(au: &mut Vec<u8>, obus: &[u8]) -> bool {
    let mut pos = 0;
    while pos < au.len() {
        let h = au[pos];
        let obu_type = (h >> 3) & 0xf;
        if matches!(obu_type, 3 | 4 | 6) {
            au.splice(pos..pos, obus.iter().copied());
            return true;
        }
        if h & 0x02 == 0 {
            return false; // no obu_size: cannot find the next OBU
        }
        let mut p = pos + 1 + usize::from((h >> 2) & 1);
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let Some(&b) = au.get(p) else { return false };
            size |= usize::from(b & 0x7f) << shift;
            p += 1;
            shift += 7;
            if b & 0x80 == 0 || shift > 56 {
                break;
            }
        }
        pos = p + size;
    }
    false
}

/// Straight-alpha sRGB RGBA re-encoded for a BT.2020 PQ frame: linearised, converted to BT.2020
/// (BT.2087), put at 203-nit SDR white and PQ-encoded to 8 bits. Alpha is unchanged, so the
/// blend over PQ codes shows the pointer at UI white instead of the display's peak.
pub fn srgb_rgba_to_pq(rgba: &[u8]) -> Vec<u8> {
    fn linear(v: u8) -> f64 {
        let c = f64::from(v) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
    fn pq8(nits: f64) -> u8 {
        let y = (nits / 10000.0).clamp(0.0, 1.0).powf(0.159_301_757_812_5);
        let e = ((0.835_937_5 + 18.851_562_5 * y) / (1.0 + 18.6875 * y)).powf(78.843_75);
        (e * 255.0).round() as u8
    }
    let mut out = rgba.to_vec();
    for px in out.chunks_exact_mut(4) {
        let (r, g, b) = (linear(px[0]), linear(px[1]), linear(px[2]));
        px[0] = pq8(203.0 * (0.6274 * r + 0.3293 * g + 0.0433 * b));
        px[1] = pq8(203.0 * (0.0691 * r + 0.9195 * g + 0.0114 * b));
        px[2] = pq8(203.0 * (0.0164 * r + 0.0880 * g + 0.8956 * b));
    }
    out
}

/// [`srgb_rgba_to_pq`] of `rgba`, recomputed only when the bitmap changes. One slot: a
/// session has one pointer bitmap at a time.
pub fn pq_rgba_cached(rgba: &std::sync::Arc<Vec<u8>>) -> std::sync::Arc<Vec<u8>> {
    use std::sync::{Arc, Mutex, Weak};
    type Cached = Option<(Weak<Vec<u8>>, Arc<Vec<u8>>)>;
    static LAST: Mutex<Cached> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((src, pq)) = last.as_ref() {
        // A live Weak cannot alias a freed bitmap reallocated at the same address.
        if src.upgrade().is_some_and(|s| Arc::ptr_eq(&s, rgba)) {
            return pq.clone();
        }
    }
    let pq = Arc::new(srgb_rgba_to_pq(rgba));
    *last = Some((Arc::downgrade(rgba), pq.clone()));
    pq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hevc_hdr_sei_nal_escapes_and_carries_both_messages() {
        let m = generic_hdr10();
        let nal = hevc_hdr_sei_nal(&m);
        assert_eq!(&nal[..6], &[0, 0, 0, 1, 0x4e, 0x01]);
        // Un-escape and compare with the payload builders.
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for &b in &nal[6..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        assert_eq!(&rbsp[..2], &[137, 24]);
        assert_eq!(&rbsp[2..26], &hevc_mastering_display_sei(&m));
        assert_eq!(&rbsp[26..28], &[144, 4]);
        assert_eq!(&rbsp[28..32], &hevc_content_light_level_sei(&m));
        assert_eq!(rbsp[32..], [0x80]);
        for w in nal[6..].windows(3) {
            assert!(
                !(w[0] == 0 && w[1] == 0 && w[2] <= 3 && w[2] != 3),
                "unescaped {w:?}"
            );
        }
    }

    #[test]
    fn av1_metadata_obus_use_av1_order_and_fixed_point() {
        let m = generic_hdr10();
        let obus = av1_hdr_metadata_obus(&m);
        // MDCV: header, size 26, type 2, R G B primaries, white, max, min, trailing.
        assert_eq!(&obus[..3], &[0x2a, 26, 2]);
        let be16 = |o: usize| u16::from_be_bytes([obus[o], obus[o + 1]]);
        let be32 = |o: usize| u32::from_be_bytes(obus[o..o + 4].try_into().unwrap());
        // BT.2020 red (0.708, 0.292) in 0.16.
        assert_eq!((be16(3), be16(5)), (46399, 19137));
        // Green (0.170, 0.797), blue (0.131, 0.046), D65 (0.3127, 0.3290).
        assert_eq!((be16(7), be16(9)), (11141, 52232));
        assert_eq!((be16(11), be16(13)), (8585, 3015));
        assert_eq!((be16(15), be16(17)), (20493, 21561));
        assert_eq!(be32(19), 1000 << 8, "1000 nits in 24.8");
        assert_eq!(be32(23), 2, "0.0001 nits in 18.14");
        assert_eq!(obus[27], 0x80);
        assert_eq!(&obus[28..], &[0x2a, 6, 1, 0x03, 0xe8, 0x01, 0x90, 0x80]);
    }

    #[test]
    fn av1_metadata_goes_after_the_sequence_header() {
        // TD (type 2), sequence header (type 1, 3 payload bytes), frame (type 6).
        let mut au = vec![0x12, 0x00, 0x0a, 0x03, 1, 2, 3, 0x32, 0x02, 9, 9];
        assert!(av1_insert_before_frame(&mut au, &[0xaa, 0xbb]));
        assert_eq!(
            au,
            [0x12, 0x00, 0x0a, 0x03, 1, 2, 3, 0xaa, 0xbb, 0x32, 0x02, 9, 9]
        );
        let mut no_frame = vec![0x12, 0x00];
        assert!(!av1_insert_before_frame(&mut no_frame, &[0xaa]));
    }

    #[test]
    fn cursor_bitmap_lands_at_sdr_white_in_pq() {
        let px = [255, 255, 255, 255, 0, 0, 0, 128, 255, 0, 0, 200];
        let pq = srgb_rgba_to_pq(&px);
        // PQ(203 nits) = 0.5807 -> 148; black stays 0; alpha untouched.
        assert_eq!(&pq[0..4], &[148, 148, 148, 255]);
        assert_eq!(&pq[4..8], &[0, 0, 0, 128]);
        // sRGB red on the BT.709 primary inside BT.2020: PQ (0.5325, 0.3270, 0.2201).
        assert_eq!(&pq[8..12], &[136, 83, 56, 200]);

        let src = std::sync::Arc::new(px.to_vec());
        let a = pq_rgba_cached(&src);
        assert!(std::sync::Arc::ptr_eq(&a, &pq_rgba_cached(&src)));
        assert_eq!(*a, pq);
    }

    #[test]
    fn display_conversion_bt2020_1000nit() {
        let m = hdr_meta_from_display(
            (0.708, 0.292),   // red
            (0.170, 0.797),   // green
            (0.131, 0.046),   // blue
            (0.3127, 0.3290), // D65
            1000.0,
            0.0001,
            0,
            0,
        );
        // ST.2086 G, B, R order, 1/50000 units.
        assert_eq!(
            m.display_primaries,
            [[8500, 39850], [6550, 2300], [35400, 14600]]
        );
        assert_eq!(m.white_point, [15635, 16450]);
        assert_eq!(m.max_display_mastering_luminance, 10_000_000); // 1000 * 10000
        assert_eq!(m.min_display_mastering_luminance, 1); // 0.0001 * 10000
        assert_eq!((m.max_cll, m.max_fall), (0, 0));
    }

    #[test]
    fn mastering_sei_is_24_bytes_big_endian_gbr() {
        let m = generic_hdr10();
        let p = hevc_mastering_display_sei(&m);
        assert_eq!(p.len(), 24);
        // First field = green.x (ST.2086 G/B/R), big-endian.
        assert_eq!(&p[0..2], &8500u16.to_be_bytes());
        assert_eq!(&p[2..4], &39850u16.to_be_bytes());
        assert_eq!(&p[4..6], &6550u16.to_be_bytes());
        assert_eq!(&p[12..14], &15635u16.to_be_bytes());
        assert_eq!(&p[16..20], &10_000_000u32.to_be_bytes());
        assert_eq!(&p[20..24], &1u32.to_be_bytes());
    }

    #[test]
    fn cll_sei_is_4_bytes_big_endian() {
        let m = generic_hdr10();
        let p = hevc_content_light_level_sei(&m);
        assert_eq!(p, [0x03, 0xE8, 0x01, 0x90]); // 1000, 400 big-endian
    }

    #[test]
    fn vdisplay_luminance_fields_convert_units() {
        // 800 nits / 0.05 nits / 400 MaxFALL → (nits, nits, milli-nits).
        let m = hdr_meta_from_display(
            (0.680, 0.320),
            (0.265, 0.690),
            (0.150, 0.060),
            (0.3127, 0.3290),
            800.0,
            0.05,
            0,
            400,
        );
        assert_eq!(vdisplay_luminance_fields(&m), (800, 400, 50));
        // Unknown (all-zero) volume stays all-zero; the driver keeps EDID defaults.
        assert_eq!(vdisplay_luminance_fields(&HdrMeta::default()), (0, 0, 0));
    }

    #[test]
    fn clamps_out_of_range() {
        let m = hdr_meta_from_display(
            (2.0, 2.0),
            (0.0, 0.0),
            (0.0, 0.0),
            (0.5, 0.5),
            -5.0,
            0.0,
            0,
            0,
        );
        assert_eq!(m.display_primaries[2], [65535, 65535]); // red clamped
        assert_eq!(m.max_display_mastering_luminance, 0); // negative → 0
    }
}
