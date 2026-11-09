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
