//! The colour rules `decode` hands the platform: which dataspace a stream or a decoder's report
//! maps to. Plain values in and out; the NDK `DataSpace` is built in `decode/display.rs`.

use punktfunk_core::quic::ColorInfo;

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
}
