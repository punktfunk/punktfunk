//! The codec-facing rules `decode` computes before it touches the NDK: the MIME, the picture size
//! a decoder reports and the low-latency keys per decoder name. Plain values in and out;
//! `MediaFormat` and `MediaCodec` are read and written in `decode/`.

/// The MediaCodec MIME for the codec the host resolved (`Welcome.codec`). Shared by the decode
/// thread and `nativeVideoMime` (which tells Kotlin what to rank decoders for). AV1 uses the
/// AOSP `video/av01` type; anything not H.264/AV1 is treated as HEVC (every pre-negotiation host
/// emitted HEVC).
pub(crate) fn codec_mime(codec: u8) -> &'static str {
    match codec {
        punktfunk_core::quic::CODEC_H264 => "video/avc",
        punktfunk_core::quic::CODEC_AV1 => "video/av01",
        _ => "video/hevc",
    }
}

/// The picture size from a decoder's output format: the crop rect when the format carries one
/// (1080 rows decode into a 1088-row buffer), else `width` × `height`. The crop is one inclusive
/// rect, `(left, top, right, bottom)`, under `crop`: only Java's `MediaFormat` splits it into
/// `crop-left`…`crop-bottom`, so those keys never exist in the NDK's.
pub(crate) fn picture_dims(
    width: i32,
    height: i32,
    crop: Option<(i32, i32, i32, i32)>,
) -> (i32, i32) {
    match crop {
        Some((l, t, r, b)) if r >= l && b >= t => (r - l + 1, b - t + 1),
        _ => (width, height),
    }
}

/// The Qualcomm output-fence keys fit a Codec2 Qualcomm decoder on Android 13+. Only
/// `CCodecBufferChannel` since 13 forwards the codec's fence to the consumer; earlier releases
/// queue the buffer unfenced, and SurfaceFlinger would show it unfinished.
pub(crate) fn qti_fence_eligible(codec_name: &str, sdk: u32) -> bool {
    codec_name.to_ascii_lowercase().starts_with("c2.qti") && sdk >= 33
}

/// The low-latency profile to apply: `forced` when it names one (`standard`, `off`, `mtk-tv`,
/// `mtk-boost`, `qti-fence`), else `mtk-tv` for a `c2.mtk` decoder when `half_rate_tv` says the
/// platform is one, else `standard`. `half_rate_tv` runs only for a `c2.mtk` decoder.
pub(crate) fn low_latency_profile<'a>(
    forced: Option<&'a str>,
    codec_name: &str,
    half_rate_tv: impl FnOnce() -> bool,
) -> &'a str {
    match forced {
        Some(p @ ("standard" | "off" | "mtk-tv" | "mtk-boost" | "qti-fence")) => p,
        _ if codec_name.to_ascii_lowercase().starts_with("c2.mtk") && half_rate_tv() => "mtk-tv",
        _ => "standard",
    }
}

/// The low-latency keys for `codec_name` under `profile`, in the order they are set.
/// `refresh_hz` is the stream rate (`mtk-tv`'s `operating-rate`), `sdk` the Android release
/// (`qti-fence`'s gate).
///
/// `aggressive` is the "Low-latency mode" master toggle. Off, the set is the plain profile every
/// device once streamed with: the Qualcomm vendor twin set blind (other vendors ignore an unknown
/// key), `priority = 0` and `operating-rate = MAX` together. That is the per-device escape hatch.
/// On (the default), the set is Moonlight-Android's: MediaTek's `vdec-lowlatency`, the vendor keys
/// of the decoder's name prefix (a key one vendor honours means nothing to another) and one clock
/// hint. NVIDIA Tegra, Rockchip and Realtek have no such key; the standard key, the clock hint
/// and a first rank in `VideoDecoders` cover them.
pub(crate) fn low_latency_keys(
    profile: &str,
    codec_name: &str,
    aggressive: bool,
    refresh_hz: u32,
    sdk: u32,
) -> Vec<(&'static str, i32)> {
    let mut keys = Vec::new();
    // The standard key asks the platform decoder for its no-reorder path.
    if matches!(profile, "standard" | "mtk-boost" | "qti-fence") {
        keys.push(("low-latency", 1));
    }
    keys.extend(vendor_keys(codec_name, aggressive));
    match profile {
        "mtk-tv" => keys.extend([
            ("vendor.mtk-codec2.game-mode", 1),
            ("vendor.mtk-codec2.low-latency-mode", 1),
            ("operating-rate", refresh_hz as i32),
        ]),
        "mtk-boost" => keys.push(("vendor.mtk.vdec.cpu.boost.mode.value", 1)),
        "qti-fence" if qti_fence_eligible(codec_name, sdk) => keys.extend([
            // Decode-order output is the fence's prerequisite. The sw-fence key is the 8 Gen 2
            // spelling, the output-fence pair the 8 Gen 3 / 8 Elite one (type 1: the driver
            // signals). A codec without a key ignores it.
            ("vendor.qti-ext-dec-picture-order.enable", 1),
            ("vendor.qti-ext-output-sw-fence-enable.value", 1),
            ("vendor.qti-ext-output-fence.enable", 1),
            ("vendor.qti-ext-output-fence.fence_type", 1),
        ]),
        _ => {}
    }
    keys
}

/// [`low_latency_keys`]' vendor and clock keys, before the profile's own.
fn vendor_keys(codec_name: &str, aggressive: bool) -> Vec<(&'static str, i32)> {
    if !aggressive {
        return vec![
            ("vendor.qti-ext-dec-low-latency.enable", 1),
            ("priority", 0),                     // 0 = realtime
            ("operating-rate", i16::MAX as i32), // 32767 = "as fast as possible"
        ];
    }
    // Set whatever the decoder name: an OEM rename keeps it working, other vendors ignore it.
    let mut keys = vec![("vdec-lowlatency", 1)];
    let name = codec_name.to_ascii_lowercase();
    let is = |prefix: &str| name.starts_with(prefix);
    // Qualcomm: picture-order forces decode-order output on decoders older than the standard
    // key; the low-latency key is the older vendor twin.
    if is("omx.qcom") || is("c2.qti") {
        keys.push(("vendor.qti-ext-dec-picture-order.enable", 1));
        keys.push(("vendor.qti-ext-dec-low-latency.enable", 1));
    }
    // Samsung Exynos, which includes Google Tensor (`c2.exynos.*`).
    if is("omx.exynos") || is("c2.exynos") {
        keys.push(("vendor.rtc-ext-dec-low-latency.enable", 1));
    }
    // Amlogic: the Android TV boxes (onn 4K, Chromecast with Google TV, Homatics).
    if is("omx.amlogic") || is("c2.amlogic") {
        keys.push(("vendor.low-latency.enable", 1));
    }
    // HiSilicon / Kirin (older Huawei): paired req/rdy keys.
    if is("omx.hisi") || is("c2.hisi") {
        keys.push((
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
            1,
        ));
        keys.push((
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy",
            -1,
        ));
    }
    // The clock hints exclude each other: the "unbounded" operating-rate sentinel (Short.MAX)
    // runs each frame at max clocks, at a power and heat cost; the rest get realtime priority.
    if decoder_supports_max_operating_rate(&name) {
        keys.push(("operating-rate", i16::MAX as i32)); // 32767 = "as fast as possible"
    } else {
        keys.push(("priority", 0)); // 0 = realtime
    }
    keys
}

/// Whether a decoder tolerates `operating-rate = Short.MAX` rather than regressing on it. Follows
/// Moonlight's allowlist: only Qualcomm decoders honour the sentinel. The Adreno 620 generation
/// is the known exception Moonlight excludes by GPU model, which native code cannot see; it rides
/// the master toggle as its escape hatch.
fn decoder_supports_max_operating_rate(name_lower: &str) -> bool {
    name_lower.starts_with("omx.qcom") || name_lower.starts_with("c2.qti")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qti_fence_needs_a_codec2_qualcomm_decoder_on_13() {
        assert!(qti_fence_eligible("c2.qti.hevc.decoder.low_latency", 33));
        assert!(qti_fence_eligible("C2.QTI.avc.decoder", 35));
        assert!(!qti_fence_eligible("c2.qti.hevc.decoder", 32));
        assert!(!qti_fence_eligible("omx.qcom.video.decoder.hevc", 34));
        assert!(!qti_fence_eligible("c2.mtk.hevc.decoder", 34));
    }

    #[test]
    fn the_picture_is_the_crop_when_the_format_has_one() {
        // (name, width, height, crop, size)
        let rows = [
            ("no crop is the buffer", 1920, 1080, None, (1920, 1080)),
            (
                "1080 rows in a 1088-row buffer",
                1920,
                1088,
                Some((0, 0, 1919, 1079)),
                (1920, 1080),
            ),
            (
                "an offset crop counts both edges",
                1936,
                1096,
                Some((8, 8, 1927, 1087)),
                (1920, 1080),
            ),
            (
                "one pixel is an inclusive rect",
                64,
                64,
                Some((5, 5, 5, 5)),
                (1, 1),
            ),
            (
                "a rect that ends before it starts",
                1920,
                1088,
                Some((10, 0, 9, 1079)),
                (1920, 1088),
            ),
            (
                "a rect that ends above its top",
                1920,
                1088,
                Some((0, 10, 1919, 9)),
                (1920, 1088),
            ),
        ];
        for (name, w, h, crop, want) in rows {
            assert_eq!(picture_dims(w, h, crop), want, "{name}");
        }
    }
}
