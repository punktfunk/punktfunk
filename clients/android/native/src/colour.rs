//! The colour rules `decode` hands the platform: which dataspace a stream or a decoder's report
//! maps to, and the HDR static metadata for a codec and for a layer. Plain values in and out; the
//! NDK `DataSpace`, `MediaFormat` and transaction are built in `decode/`.

use punktfunk_core::quic::{ColorInfo, HdrMeta};

/// The `ADataSpace` tags a surface takes, named as the NDK's `DataSpace` names them. The `Itu`
/// variants are studio range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Space {
    Srgb,
    Bt709,
    Bt2020Pq,
    Bt2020ItuPq,
    Bt2020Hlg,
    Bt2020ItuHlg,
}

/// The tag the presenters put on buffers, from the *negotiated* colour ([`ColorInfo`], carried
/// on Welcome): the wire contract's source, not what the decoder echoes (many omit
/// `color-transfer`). Never untagged: an untagged ASurfaceControl layer defaults to sRGB, which
/// shows studio-range black (16) as grey.
///
/// Full-range SDR is sRGB: a desktop's pixels are sRGB-encoded, and SurfaceFlinger treats
/// BT.709's transfer the same way. Studio-range SDR (an older host) is BT.709.
pub(crate) fn negotiated_space(color: &ColorInfo) -> Space {
    let full = color.full_range != 0;
    match (color.transfer, full) {
        (ColorInfo::TRC_PQ, true) => Space::Bt2020Pq,
        (ColorInfo::TRC_PQ, false) => Space::Bt2020ItuPq,
        (ColorInfo::TRC_HLG, true) => Space::Bt2020Hlg,
        (ColorInfo::TRC_HLG, false) => Space::Bt2020ItuHlg,
        (_, true) => Space::Srgb,
        (_, false) => Space::Bt709,
    }
}

/// The tag for the colour a decoder reports: BT.2020 PQ or HLG, BT.709 for SDR (sRGB when full
/// range), `None` without a transfer. `transfer` and `range` are the output format's
/// `color-transfer` and `color-range`: SDR_VIDEO 3, ST2084 6 (PQ), HLG 7; FULL 1, LIMITED 2.
/// Codec2 fills a decoder that reports nothing with its BT.709 default, so SDR here does not
/// prove the stream is SDR.
pub(crate) fn reported_space(transfer: Option<i32>, range: Option<i32>) -> Option<Space> {
    let full = range == Some(1);
    match transfer {
        Some(6) => Some(if full {
            Space::Bt2020Pq
        } else {
            Space::Bt2020ItuPq
        }),
        Some(7) => Some(if full {
            Space::Bt2020Hlg
        } else {
            Space::Bt2020ItuHlg
        }),
        Some(3) => Some(if full { Space::Srgb } else { Space::Bt709 }),
        _ => None,
    }
}

/// [`negotiated_space`] as an ASurfaceControl layer on `sdk` takes it. Android 10 aborts the
/// process and 11 ignores anything but sRGB, scRGB, P3 and full-range PQ, so `None` there sends
/// a studio-range session to the SurfaceView, whose decoder tags its own buffers.
pub(crate) fn layer_space(color: &ColorInfo, sdk: u32) -> Option<Space> {
    let space = negotiated_space(color);
    (sdk >= 31 || matches!(space, Space::Srgb | Space::Bt2020Pq)).then_some(space)
}

/// Serialize [`HdrMeta`] into Android's `KEY_HDR_STATIC_INFO` (`hdr-static-info`) layout: a
/// 25-byte CTA-861.3 / `HDRStaticInfo.Type1` blob — descriptor id 0, then primaries in **R, G, B**
/// order, white point, max/min display luminance, MaxCLL, MaxFALL, all **little-endian** `u16`.
/// Two conversions vs our wire form: HdrMeta stores primaries in ST.2086 **G, B, R** order
/// (reorder to R, G, B), and `max_display_mastering_luminance` is in 0.0001-cd/m² units while
/// Android wants **whole nits** (min stays 0.0001-nit). Chromaticities (1/50000) and
/// MaxCLL/MaxFALL (nits) match 1:1.
pub(crate) fn android_hdr_static_info(m: &HdrMeta) -> [u8; 25] {
    let [g, b_, r] = m.display_primaries; // ST.2086 G, B, R
    let max_nits = (m.max_display_mastering_luminance / 10_000).min(u16::MAX as u32) as u16;
    let min_units = m.min_display_mastering_luminance.min(u16::MAX as u32) as u16;
    let fields: [u16; 12] = [
        r[0],
        r[1],
        g[0],
        g[1],
        b_[0],
        b_[1], // R, G, B primaries
        m.white_point[0],
        m.white_point[1], // white point
        max_nits,
        min_units, // max (nits) / min (0.0001-nit) display luminance
        m.max_cll,
        m.max_fall, // MaxCLL / MaxFALL (nits)
    ];
    let mut out = [0u8; 25]; // out[0] = 0 (Type 1 descriptor id), already zero
    for (i, v) in fields.iter().enumerate() {
        out[1 + i * 2..3 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// `AColorXy`: a chromaticity as floats.
#[repr(C)]
#[derive(Debug, PartialEq)]
struct AColorXy {
    x: f32,
    y: f32,
}

/// `AHdrMetadata_smpte2086`: chromaticities as floats, luminance in nits.
#[repr(C)]
#[derive(Debug, PartialEq)]
pub(crate) struct AHdrMetadataSmpte2086 {
    red: AColorXy,
    green: AColorXy,
    blue: AColorXy,
    white: AColorXy,
    max_luminance: f32,
    min_luminance: f32,
}

/// `AHdrMetadata_cta861_3`, in nits.
#[repr(C)]
#[derive(Debug, PartialEq)]
pub(crate) struct AHdrMetadataCta8613 {
    max_content_light_level: f32,
    max_frame_average_light_level: f32,
}

/// [`HdrMeta`] (ST.2086 G, B, R in 1/50000; mastering luminance in 0.0001 nits) as the NDK's
/// float structs. A block with a zero field means "unknown" and is `None`, as Codec2 treats it for
/// the decoder's own Surface: SurfaceFlinger takes a sent MaxCLL as the layer's peak, so a zero
/// one declares a 0-nit picture.
pub(crate) fn hdr_metadata(
    m: &HdrMeta,
) -> (Option<AHdrMetadataSmpte2086>, Option<AHdrMetadataCta8613>) {
    let xy = |[x, y]: [u16; 2]| AColorXy {
        x: f32::from(x) / 50_000.0,
        y: f32::from(y) / 50_000.0,
    };
    let [g, b, r] = m.display_primaries;
    let mastering = (m.max_display_mastering_luminance > 0
        && m.min_display_mastering_luminance > 0)
        .then(|| AHdrMetadataSmpte2086 {
            red: xy(r),
            green: xy(g),
            blue: xy(b),
            white: xy(m.white_point),
            max_luminance: m.max_display_mastering_luminance as f32 / 10_000.0,
            min_luminance: m.min_display_mastering_luminance as f32 / 10_000.0,
        });
    let light_level = (m.max_cll > 0 && m.max_fall > 0).then(|| AHdrMetadataCta8613 {
        max_content_light_level: f32::from(m.max_cll),
        max_frame_average_light_level: f32::from(m.max_fall),
    });
    (mastering, light_level)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PQ: u8 = ColorInfo::TRC_PQ;
    const HLG: u8 = ColorInfo::TRC_HLG;
    const SDR: u8 = ColorInfo::TRC_BT709;

    fn color(transfer: u8, full_range: u8) -> ColorInfo {
        ColorInfo {
            primaries: ColorInfo::CP_BT709,
            transfer,
            matrix: ColorInfo::MC_BT709,
            full_range,
        }
    }

    #[test]
    fn the_negotiated_tag_follows_transfer_and_range() {
        // (name, colour, tag)
        let rows = [
            ("full-range PQ", color(PQ, 1), Space::Bt2020Pq),
            ("studio-range PQ", color(PQ, 0), Space::Bt2020ItuPq),
            ("full-range HLG", color(HLG, 1), Space::Bt2020Hlg),
            ("studio-range HLG", color(HLG, 0), Space::Bt2020ItuHlg),
            ("full-range SDR is sRGB", color(SDR, 1), Space::Srgb),
            ("studio-range SDR is BT.709", color(SDR, 0), Space::Bt709),
            ("an unknown transfer reads as SDR", color(0, 1), Space::Srgb),
            ("an unknown studio transfer", color(99, 0), Space::Bt709),
            (
                "any non-zero range byte is full",
                color(PQ, 2),
                Space::Bt2020Pq,
            ),
            (
                "primaries and matrix do not change the tag",
                ColorInfo {
                    primaries: ColorInfo::CP_BT2020,
                    matrix: ColorInfo::MC_BT2020_NCL,
                    ..color(SDR, 1)
                },
                Space::Srgb,
            ),
            ("the host's SDR constant", ColorInfo::SDR_BT709, Space::Srgb),
            (
                "an older host's default",
                ColorInfo::SDR_BT709_STUDIO,
                Space::Bt709,
            ),
            (
                "the host's HDR10 constant",
                ColorInfo::HDR10_BT2020_PQ,
                Space::Bt2020Pq,
            ),
        ];
        for (name, c, want) in rows {
            assert_eq!(negotiated_space(&c), want, "{name}");
        }
    }

    #[test]
    fn a_decoders_report_maps_its_transfer_and_range() {
        // (name, color-transfer, color-range, tag)
        let rows = [
            ("PQ full", Some(6), Some(1), Some(Space::Bt2020Pq)),
            ("PQ limited", Some(6), Some(2), Some(Space::Bt2020ItuPq)),
            (
                "PQ without a range",
                Some(6),
                None,
                Some(Space::Bt2020ItuPq),
            ),
            ("HLG full", Some(7), Some(1), Some(Space::Bt2020Hlg)),
            ("HLG limited", Some(7), Some(2), Some(Space::Bt2020ItuHlg)),
            (
                "a range other than 1",
                Some(7),
                Some(0),
                Some(Space::Bt2020ItuHlg),
            ),
            ("no transfer is no tag", None, Some(1), None),
            ("no transfer, no range", None, None, None),
            ("linear is not named", Some(1), Some(1), None),
            (
                "a CICP PQ number is not a MediaFormat one",
                Some(16),
                Some(1),
                None,
            ),
            (
                "a CICP HLG number is not a MediaFormat one",
                Some(18),
                Some(2),
                None,
            ),
        ];
        for (name, transfer, range, want) in rows {
            assert_eq!(reported_space(transfer, range), want, "{name}");
        }
    }

    /// R1-3, an open item (design D4): the sinks apply a decoder's report over the negotiated
    /// tag, and a decoder that reports SDR (Codec2's default for one that reports nothing)
    /// yields BT.709 or sRGB. An HDR session can lose its PQ or HLG tag to it. Pinned as is.
    #[test]
    fn r1_3_open_a_reported_sdr_transfer_yields_an_sdr_tag() {
        assert_eq!(reported_space(Some(3), Some(2)), Some(Space::Bt709));
        assert_eq!(reported_space(Some(3), Some(1)), Some(Space::Srgb));
        assert_eq!(reported_space(Some(3), None), Some(Space::Bt709));
        let negotiated = negotiated_space(&ColorInfo::HDR10_BT2020_PQ);
        assert_ne!(reported_space(Some(3), Some(1)), Some(negotiated));
    }

    #[test]
    fn a_layer_takes_a_tag_by_android_release() {
        let sdr = color(SDR, 1);
        let studio = color(SDR, 0);
        // (name, colour, sdk, tag)
        let rows = [
            ("full-range SDR on 10", sdr, 29, Some(Space::Srgb)),
            ("full-range SDR on 11", sdr, 30, Some(Space::Srgb)),
            ("full-range SDR on 12", sdr, 31, Some(Space::Srgb)),
            ("studio SDR on 10 has no layer tag", studio, 29, None),
            ("studio SDR on 11 has no layer tag", studio, 30, None),
            ("studio SDR on 12", studio, 31, Some(Space::Bt709)),
            ("studio SDR on 15", studio, 35, Some(Space::Bt709)),
            (
                "full-range PQ on 10",
                color(PQ, 1),
                29,
                Some(Space::Bt2020Pq),
            ),
            ("studio PQ on 11 has no layer tag", color(PQ, 0), 30, None),
            (
                "studio PQ on 12",
                color(PQ, 0),
                31,
                Some(Space::Bt2020ItuPq),
            ),
            (
                "full-range HLG on 11 has no layer tag",
                color(HLG, 1),
                30,
                None,
            ),
            (
                "full-range HLG on 12",
                color(HLG, 1),
                31,
                Some(Space::Bt2020Hlg),
            ),
            ("an unreadable sdk takes the old gate", studio, 0, None),
            (
                "an unreadable sdk still takes sRGB",
                sdr,
                0,
                Some(Space::Srgb),
            ),
        ];
        for (name, c, sdk, want) in rows {
            assert_eq!(layer_space(&c, sdk), want, "{name}");
        }
    }

    /// Each field holds a distinct byte pair, so a swapped field or byte shows.
    fn meta() -> HdrMeta {
        HdrMeta {
            display_primaries: [[0x0506, 0x0708], [0x090A, 0x0B0C], [0x0102, 0x0304]],
            white_point: [0x0D0E, 0x0F10],
            max_display_mastering_luminance: 0x1112 * 10_000,
            min_display_mastering_luminance: 0x1314,
            max_cll: 0x1516,
            max_fall: 0x1718,
        }
    }

    #[test]
    fn the_hdr_blob_is_red_green_blue_in_little_endian() {
        // The wire form is G, B, R; Android wants R, G, B.
        assert_eq!(
            android_hdr_static_info(&meta()),
            [
                0, // Type 1 descriptor
                0x02, 0x01, 0x04, 0x03, // red
                0x06, 0x05, 0x08, 0x07, // green
                0x0A, 0x09, 0x0C, 0x0B, // blue
                0x0E, 0x0D, 0x10, 0x0F, // white point
                0x12, 0x11, // max luminance, nits
                0x14, 0x13, // min luminance, 0.0001 nits
                0x16, 0x15, // MaxCLL
                0x18, 0x17, // MaxFALL
            ]
        );
    }

    #[test]
    fn the_hdr_blob_converts_the_luminances() {
        // (name, max in 0.0001 nits, min in 0.0001 nits, max in nits, min)
        let rows = [
            ("1000 nits", 10_000_000, 50, 1000, 50),
            ("a fraction of a nit is cut", 9_999, 1, 0, 1),
            ("the max saturates", 700_000_000, 0, u16::MAX, 0),
            ("the min saturates", 0, 70_000, 0, u16::MAX),
            ("the largest max", u32::MAX, u32::MAX, u16::MAX, u16::MAX),
        ];
        for (name, max_lum, min_lum, want_max, want_min) in rows {
            let m = HdrMeta {
                max_display_mastering_luminance: max_lum,
                min_display_mastering_luminance: min_lum,
                ..meta()
            };
            let blob = android_hdr_static_info(&m);
            assert_eq!(
                (
                    u16::from_le_bytes([blob[17], blob[18]]),
                    u16::from_le_bytes([blob[19], blob[20]])
                ),
                (want_max, want_min),
                "{name}"
            );
        }
    }

    #[test]
    fn an_empty_hdr_meta_is_an_all_zero_blob() {
        assert_eq!(android_hdr_static_info(&HdrMeta::default()), [0; 25]);
    }

    /// BT.2020 primaries, a D65 white, a 1000-nit master with a 0.005-nit floor.
    fn hdr10() -> HdrMeta {
        HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        }
    }

    fn xy(x: f32, y: f32) -> AColorXy {
        AColorXy { x, y }
    }

    fn mastering() -> AHdrMetadataSmpte2086 {
        AHdrMetadataSmpte2086 {
            red: xy(0.708, 0.292),
            green: xy(0.17, 0.797),
            blue: xy(0.131, 0.046),
            white: xy(0.3127, 0.329),
            max_luminance: 1000.0,
            min_luminance: 0.005,
        }
    }

    fn light_level(cll: f32, fall: f32) -> AHdrMetadataCta8613 {
        AHdrMetadataCta8613 {
            max_content_light_level: cll,
            max_frame_average_light_level: fall,
        }
    }

    #[test]
    fn a_layer_gets_floats_in_red_green_blue_order() {
        assert_eq!(
            hdr_metadata(&hdr10()),
            (Some(mastering()), Some(light_level(1000.0, 400.0)))
        );
    }

    #[test]
    fn a_block_with_a_zero_field_is_unknown() {
        // (name, edit to the HDR10 meta, mastering block kept, light-level block kept)
        type Edit = fn(&mut HdrMeta);
        let rows: [(&str, Edit, bool, bool); 7] = [
            (
                "nothing is known",
                |m| *m = HdrMeta::default(),
                false,
                false,
            ),
            (
                "no max luminance drops the mastering block",
                |m| m.max_display_mastering_luminance = 0,
                false,
                true,
            ),
            (
                "no min luminance drops it too",
                |m| m.min_display_mastering_luminance = 0,
                false,
                true,
            ),
            (
                "no MaxCLL drops the light level",
                |m| m.max_cll = 0,
                true,
                false,
            ),
            ("no MaxFALL drops it too", |m| m.max_fall = 0, true, false),
            (
                "a zero white point keeps the mastering block",
                |m| m.white_point = [0, 0],
                true,
                true,
            ),
            (
                "zero primaries keep the mastering block",
                |m| m.display_primaries = [[0; 2]; 3],
                true,
                true,
            ),
        ];
        for (name, edit, want_mastering, want_light) in rows {
            let mut m = hdr10();
            edit(&mut m);
            let (mastering, light) = hdr_metadata(&m);
            assert_eq!(
                (mastering.is_some(), light.is_some()),
                (want_mastering, want_light),
                "{name}"
            );
        }
    }
}
