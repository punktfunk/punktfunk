//! The codec-facing rules `decode` computes before it touches the NDK: the MIME, the picture size
//! a decoder reports, the low-latency keys per decoder name and the pts-keyed stamp queues behind
//! the HUD. Plain values in and out; `MediaFormat` and `MediaCodec` are read and written in
//! `decode/`.

use std::collections::VecDeque;

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

/// The buffer rect `[left, top, right, bottom]` for a crop in frame fractions on a `w` × `h`
/// buffer, at least one pixel on each axis.
pub(crate) fn crop_px([left, top, right, bottom]: [f32; 4], w: i32, h: i32) -> [i32; 4] {
    let px = |f: f32, size: i32| ((f * size as f32).round() as i32).clamp(0, size);
    let (l, t) = (px(left, w).min(w - 1), px(top, h).min(h - 1));
    [l, t, px(right, w).max(l + 1), px(bottom, h).max(t + 1)]
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
        "mtk-tv" => {
            keys.extend([
                ("vendor.mtk-codec2.game-mode", 1),
                ("vendor.mtk-codec2.low-latency-mode", 1),
            ]);
            // A mode with no stated rate names none here either: 0 is no rate to run at.
            if refresh_hz > 0 {
                keys.push(("operating-rate", refresh_hz as i32));
            }
        }
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

/// Whether the fed stream is recoded with a 16-bit POC: HEVC on an Amlogic decoder, whose
/// long-term references fail behind a POC lsb wrap. `forced` is the debug override.
pub(crate) fn widens_poc(codec: u8, codec_name: &str, forced: Option<bool>) -> bool {
    let name = codec_name.to_ascii_lowercase();
    let amlogic = name.starts_with("omx.amlogic") || name.starts_with("c2.amlogic");
    codec == punktfunk_core::quic::CODEC_HEVC && forced.unwrap_or(amlogic)
}

/// Whether each fed AU goes through the concealer first. HEVC on a Realtek decoder, which keeps
/// showing garbage past the host's anchor after a picture naming a lost reference. AV1 on a
/// `strict` decoder (Tensor G5, Realtek), which refuses a stale wave start: the concealer makes
/// an error-resilient one conformant, so the wave heals it. `forced` is the debug override.
pub(crate) fn conceals(codec: u8, codec_name: &str, strict: bool, forced: Option<bool>) -> bool {
    use punktfunk_core::quic::{CODEC_AV1, CODEC_HEVC};
    let name = codec_name.to_ascii_lowercase();
    let pick = match codec {
        CODEC_HEVC => name.starts_with("omx.realtek") || name.starts_with("c2.realtek"),
        CODEC_AV1 => strict,
        _ => return false,
    };
    forced.unwrap_or(pick)
}

/// Whether a decoder tolerates `operating-rate = Short.MAX` rather than regressing on it. Follows
/// Moonlight's allowlist: only Qualcomm decoders honour the sentinel. The Adreno 620 generation
/// is the known exception Moonlight excludes by GPU model, which native code cannot see; it rides
/// the master toggle as its escape hatch.
fn decoder_supports_max_operating_rate(name_lower: &str) -> bool {
    name_lower.starts_with("omx.qcom") || name_lower.starts_with("c2.qti")
}

/// The value parked for `pts_us` in a pts-ordered queue, popping it and every older entry.
/// Decode order == input order (low-latency, no B-frames), so an older entry was dropped inside
/// the codec or parked before a flush; a newer one stays for its own output. `None` on a miss.
pub(crate) fn take_by_pts<V: Copy>(q: &mut VecDeque<(u64, V)>, pts_us: u64) -> Option<V> {
    while let Some(&(p, v)) = q.front() {
        if p > pts_us {
            return None;
        }
        q.pop_front();
        if p == pts_us {
            return Some(v);
        }
    }
    None
}

/// The AU `user_flags` for a decoded output, keyed by the echoed `presentationTimeUs`. Recovery
/// signalling (the IDR marker, RECOVERY_ANCHOR, RECOVERY_POINT) rides the AU's flags, which are
/// only in scope at feed time: the feed side parks `(pts_us, flags)` and the present side reads
/// them back for core's freeze (`NativeClient::recovery_on_decoded`). A miss (probe filler, or an
/// entry aged past the cap) reads `0`: no recovery flags, decoded normally.
pub(crate) fn take_flags(map: &mut VecDeque<(u64, u32)>, pts_us: u64) -> u32 {
    take_by_pts(map, pts_us).unwrap_or(0)
}

/// p50/max of an unsorted µs sample vec, in ms — the HUD's per-stage summary, shared by both
/// presenters. `(0, 0)` when empty.
pub(crate) fn p50_max_ms(mut v: Vec<u64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable();
    (
        v[v.len() / 2] as f64 / 1000.0,
        v[v.len() - 1] as f64 / 1000.0,
    )
}

/// p50, p95 and max of µs samples, in ms, ranked as every client stat is
/// ([`punktfunk_core::hud::rank`]). Zeros when empty.
pub(crate) fn p50_p95_max_ms(mut v: Vec<u64>) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_unstable();
    let ms = |us: u64| us as f64 / 1000.0;
    let rank = punktfunk_core::hud::rank;
    (ms(rank(&v, 50)), ms(rank(&v, 95)), ms(v[v.len() - 1]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_amlogic_hevc_takes_the_wide_poc() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_HEVC};
        assert!(super::widens_poc(
            CODEC_HEVC,
            "c2.amlogic.hevc.decoder",
            None
        ));
        assert!(super::widens_poc(
            CODEC_HEVC,
            "OMX.amlogic.hevc.decoder.awesome2",
            None
        ));
        assert!(!super::widens_poc(CODEC_HEVC, "c2.qti.hevc.decoder", None));
        assert!(!super::widens_poc(
            CODEC_AV1,
            "c2.amlogic.av1.decoder",
            None
        ));
        // The override moves the decoder pick, never the codec.
        assert!(super::widens_poc(
            CODEC_HEVC,
            "c2.qti.hevc.decoder",
            Some(true)
        ));
        assert!(!super::widens_poc(
            CODEC_HEVC,
            "c2.amlogic.hevc.decoder",
            Some(false)
        ));
        assert!(!super::widens_poc(
            CODEC_AV1,
            "c2.amlogic.av1.decoder",
            Some(true)
        ));
    }

    #[test]
    fn realtek_hevc_and_strict_av1_are_concealed() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        assert!(super::conceals(
            CODEC_HEVC,
            "OMX.realtek.video.dec.hevc",
            false,
            None
        ));
        assert!(!super::conceals(
            CODEC_HEVC,
            "c2.qti.hevc.decoder",
            false,
            None
        ));
        assert!(super::conceals(
            CODEC_AV1,
            "c2.google.av1.decoder",
            true,
            None
        ));
        assert!(!super::conceals(
            CODEC_AV1,
            "c2.qti.av1.decoder",
            false,
            None
        ));
        assert!(!super::conceals(
            CODEC_H264,
            "OMX.realtek.video.dec.avc",
            true,
            None
        ));
        // The override moves the decoder pick, never the codec.
        assert!(super::conceals(
            CODEC_AV1,
            "c2.qti.av1.decoder",
            false,
            Some(true)
        ));
        assert!(!super::conceals(
            CODEC_H264,
            "c2.qti.avc.decoder",
            false,
            Some(true)
        ));
        assert!(!super::conceals(
            CODEC_HEVC,
            "OMX.realtek.video.dec.hevc",
            false,
            Some(false)
        ));
    }

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
    fn take_by_pts_evicts_older_pops_the_match_and_holds_newer() {
        let mut q: VecDeque<(u64, i128)> = [(10, 1), (20, 2), (30, 3), (40, 4)].into();
        assert_eq!(take_by_pts(&mut q, 30), Some(3));
        assert_eq!(q, VecDeque::from([(40, 4)]));
        assert_eq!(
            take_by_pts(&mut q, 35),
            None,
            "a miss keeps the newer entry"
        );
        assert_eq!(q, VecDeque::from([(40, 4)]));
        assert_eq!(take_by_pts(&mut q, 50), None);
        assert!(q.is_empty());
    }

    #[test]
    fn take_by_pts_rows() {
        type Queue = [(u64, char)];
        // (name, queue, pts, result, what is left)
        let rows: [(&str, &Queue, u64, Option<char>, &Queue); 7] = [
            (
                "the front is the match",
                &[(10, 'a'), (20, 'b')],
                10,
                Some('a'),
                &[(20, 'b')],
            ),
            (
                "a match pops every older entry",
                &[(10, 'a'), (20, 'b'), (30, 'c')],
                20,
                Some('b'),
                &[(30, 'c')],
            ),
            (
                "a miss between entries evicts the older",
                &[(10, 'a'), (30, 'c')],
                20,
                None,
                &[(30, 'c')],
            ),
            (
                "a pts below the queue keeps it whole",
                &[(30, 'c')],
                10,
                None,
                &[(30, 'c')],
            ),
            (
                "a pts above the queue empties it",
                &[(10, 'a'), (20, 'b')],
                99,
                None,
                &[],
            ),
            ("an empty queue misses", &[], 10, None, &[]),
            (
                "equal pts pair one at a time",
                &[(10, 'a'), (10, 'b')],
                10,
                Some('a'),
                &[(10, 'b')],
            ),
        ];
        for (name, queue, pts, want, left) in rows {
            let mut q: VecDeque<_> = queue.iter().copied().collect();
            assert_eq!(take_by_pts(&mut q, pts), want, "{name}");
            assert_eq!(q, VecDeque::from(left.to_vec()), "{name}: what is left");
        }
    }

    #[test]
    fn take_flags_reads_zero_on_a_miss() {
        let mut q: VecDeque<(u64, u32)> = [(10, 0b101), (20, 0), (30, 0b10)].into();
        assert_eq!(take_flags(&mut q, 10), 0b101, "a parked AU's flags");
        assert_eq!(take_flags(&mut q, 25), 0, "a miss evicts 20 and reads 0");
        assert_eq!(take_flags(&mut q, 30), 0b10, "the next AU is still there");
        assert_eq!(take_flags(&mut q, 30), 0, "a taken entry is gone");
    }

    #[test]
    fn p50_max_ms_summarises_microsecond_samples() {
        // (name, samples in µs, (p50, max) in ms)
        let rows: [(&str, &[u64], (f64, f64)); 6] = [
            ("no samples", &[], (0.0, 0.0)),
            ("one sample", &[1500], (1.5, 1.5)),
            (
                "an odd count takes the middle",
                &[3000, 1000, 2000],
                (2.0, 3.0),
            ),
            (
                "an even count takes the upper middle",
                &[4000, 1000, 3000, 2000],
                (3.0, 4.0),
            ),
            (
                "the max is not the last sample",
                &[9000, 1000, 2000, 3000, 4000],
                (3.0, 9.0),
            ),
            ("repeats", &[2000, 2000, 2000], (2.0, 2.0)),
        ];
        for (name, samples, want) in rows {
            assert_eq!(p50_max_ms(samples.to_vec()), want, "{name}");
        }
    }

    #[test]
    fn p50_p95_max_ms_ranks_like_every_client_stat() {
        let twenty: Vec<u64> = (1..=20).map(|k| k * 1000).collect();
        // (name, samples in µs, (p50, p95, max) in ms)
        let rows = [
            ("no samples", vec![], (0.0, 0.0, 0.0)),
            ("one sample", vec![1500], (1.5, 1.5, 1.5)),
            (
                "p95 of twenty is the last",
                twenty.clone(),
                (11.0, 20.0, 20.0),
            ),
            (
                "one spike in forty-one, unsorted",
                [twenty.clone(), twenty, vec![9_000_000]].concat(),
                (11.0, 20.0, 9000.0),
            ),
        ];
        for (name, samples, want) in rows {
            assert_eq!(p50_p95_max_ms(samples), want, "{name}");
        }
    }

    #[test]
    fn a_crop_in_fractions_is_a_pixel_rect_of_at_least_one_pixel() {
        let nan = f32::NAN;
        // (name, crop, width, height, rect)
        let rows = [
            (
                "the whole frame",
                [0.0, 0.0, 1.0, 1.0],
                1920,
                1080,
                [0, 0, 1920, 1080],
            ),
            (
                "the left half",
                [0.0, 0.0, 0.5, 1.0],
                1920,
                1080,
                [0, 0, 960, 1080],
            ),
            (
                "the right half",
                [0.5, 0.0, 1.0, 1.0],
                1920,
                1080,
                [960, 0, 1920, 1080],
            ),
            (
                "the bottom half",
                [0.0, 0.5, 1.0, 1.0],
                1920,
                1080,
                [0, 540, 1920, 1080],
            ),
            (
                "an edge rounds to the nearest pixel",
                [0.3333, 0.0, 0.6667, 1.0],
                1920,
                1080,
                [640, 0, 1280, 1080],
            ),
            (
                "a half pixel rounds up",
                [0.5, 0.0, 1.0, 1.0],
                3,
                3,
                [2, 0, 3, 3],
            ),
            (
                "an empty crop keeps one pixel",
                [0.5, 0.5, 0.5, 0.5],
                100,
                100,
                [50, 50, 51, 51],
            ),
            (
                "an inverted crop keeps one pixel at its left edge",
                [0.8, 0.0, 0.2, 1.0],
                100,
                100,
                [80, 0, 81, 100],
            ),
            (
                "a crop at the far edge keeps the last pixel",
                [1.0, 1.0, 1.0, 1.0],
                100,
                100,
                [99, 99, 100, 100],
            ),
            (
                "fractions below 0 clamp to the edge",
                [-0.5, -0.5, 1.0, 1.0],
                100,
                100,
                [0, 0, 100, 100],
            ),
            (
                "fractions above 1 clamp to the edge",
                [0.0, 0.0, 2.0, 2.0],
                100,
                100,
                [0, 0, 100, 100],
            ),
            (
                "NaN edges are the first pixel",
                [nan, nan, nan, nan],
                100,
                100,
                [0, 0, 1, 1],
            ),
            (
                "a one-pixel buffer",
                [0.5, 0.5, 1.0, 1.0],
                1,
                1,
                [0, 0, 1, 1],
            ),
        ];
        for (name, crop, w, h, want) in rows {
            assert_eq!(crop_px(crop, w, h), want, "{name}");
        }
    }

    #[test]
    fn the_mime_follows_the_codec_and_anything_else_is_hevc() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
        // (name, wire codec, mime)
        let rows = [
            ("H.264", CODEC_H264, "video/avc"),
            ("HEVC", CODEC_HEVC, "video/hevc"),
            ("AV1", CODEC_AV1, "video/av01"),
            (
                "PyroWave never asks MediaCodec",
                CODEC_PYROWAVE,
                "video/hevc",
            ),
            ("no codec is HEVC", 0, "video/hevc"),
            ("an unknown id is HEVC", 0xFF, "video/hevc"),
        ];
        for (name, codec, want) in rows {
            assert_eq!(codec_mime(codec), want, "{name}");
        }
    }

    #[test]
    fn a_forced_profile_wins_and_half_rate_tv_is_read_only_for_a_mediatek_decoder() {
        // (name, forced, decoder, is a half-rate TV, profile, whether the TV check ran)
        let rows = [
            (
                "default",
                None,
                "c2.qti.hevc.decoder",
                true,
                "standard",
                false,
            ),
            (
                "forced standard on a TV",
                Some("standard"),
                "c2.mtk.hevc.decoder",
                true,
                "standard",
                false,
            ),
            (
                "forced off",
                Some("off"),
                "c2.qti.hevc.decoder",
                false,
                "off",
                false,
            ),
            (
                "forced mtk-tv anywhere",
                Some("mtk-tv"),
                "c2.qti.hevc.decoder",
                false,
                "mtk-tv",
                false,
            ),
            (
                "forced mtk-boost",
                Some("mtk-boost"),
                "c2.mtk.hevc.decoder",
                false,
                "mtk-boost",
                false,
            ),
            (
                "forced qti-fence",
                Some("qti-fence"),
                "c2.mtk.hevc.decoder",
                true,
                "qti-fence",
                false,
            ),
            (
                "an unknown name falls to detection",
                Some("turbo"),
                "c2.mtk.hevc.decoder",
                true,
                "mtk-tv",
                true,
            ),
            (
                "an unknown name on another decoder",
                Some("turbo"),
                "c2.qti.hevc.decoder",
                true,
                "standard",
                false,
            ),
            (
                "a MediaTek decoder on a half-rate TV",
                None,
                "c2.mtk.hevc.decoder",
                true,
                "mtk-tv",
                true,
            ),
            (
                "the decoder name is case-folded",
                None,
                "C2.MTK.HEVC.DECODER",
                true,
                "mtk-tv",
                true,
            ),
            (
                "a MediaTek decoder elsewhere",
                None,
                "c2.mtk.hevc.decoder",
                false,
                "standard",
                true,
            ),
            (
                "OMX MediaTek is not Codec2",
                None,
                "omx.mtk.video.decoder.hevc",
                true,
                "standard",
                false,
            ),
        ];
        for (name, forced, decoder, tv, want, asked) in rows {
            let ran = std::cell::Cell::new(false);
            let got = low_latency_profile(forced, decoder, || {
                ran.set(true);
                tv
            });
            assert_eq!((got, ran.get()), (want, asked), "{name}");
        }
    }

    const PICTURE_ORDER: (&str, i32) = ("vendor.qti-ext-dec-picture-order.enable", 1);
    const QTI_LOW_LATENCY: (&str, i32) = ("vendor.qti-ext-dec-low-latency.enable", 1);
    const MAX_RATE: (&str, i32) = ("operating-rate", 32767);
    const REALTIME: (&str, i32) = ("priority", 0);
    const STANDARD: (&str, i32) = ("low-latency", 1);
    const MEDIATEK: (&str, i32) = ("vdec-lowlatency", 1);
    type Keys<'a> = &'a [(&'a str, i32)];

    #[test]
    fn the_keys_follow_the_profile_and_the_decoders_vendor() {
        // (name, profile, decoder, aggressive, keys in set order)
        let rows: [(&str, &str, &str, bool, Keys); 14] = [
            (
                "Qualcomm gets its vendor pair and the max clock",
                "standard",
                "c2.qti.avc.decoder",
                true,
                &[STANDARD, MEDIATEK, PICTURE_ORDER, QTI_LOW_LATENCY, MAX_RATE],
            ),
            (
                "the OMX Qualcomm name matches too, in any case",
                "standard",
                "OMX.qcom.video.decoder.hevc",
                true,
                &[STANDARD, MEDIATEK, PICTURE_ORDER, QTI_LOW_LATENCY, MAX_RATE],
            ),
            (
                "Exynos, which is Tensor, gets realtime priority",
                "standard",
                "c2.exynos.hevc.decoder",
                true,
                &[
                    STANDARD,
                    MEDIATEK,
                    ("vendor.rtc-ext-dec-low-latency.enable", 1),
                    REALTIME,
                ],
            ),
            (
                "Amlogic",
                "standard",
                "c2.amlogic.hevc.decoder",
                true,
                &[
                    STANDARD,
                    MEDIATEK,
                    ("vendor.low-latency.enable", 1),
                    REALTIME,
                ],
            ),
            (
                "HiSilicon sets a request and a ready value of -1",
                "standard",
                "omx.hisi.video.decoder.hevc",
                true,
                &[
                    STANDARD,
                    MEDIATEK,
                    (
                        "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
                        1,
                    ),
                    (
                        "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy",
                        -1,
                    ),
                    REALTIME,
                ],
            ),
            (
                "MediaTek has only the unconditional key",
                "standard",
                "c2.mtk.hevc.decoder",
                true,
                &[STANDARD, MEDIATEK, REALTIME],
            ),
            (
                "an unlisted vendor gets the same",
                "standard",
                "c2.android.hevc.decoder",
                true,
                &[STANDARD, MEDIATEK, REALTIME],
            ),
            (
                "the master toggle off sets the blind pair and both clock hints",
                "standard",
                "c2.android.hevc.decoder",
                false,
                &[STANDARD, QTI_LOW_LATENCY, REALTIME, MAX_RATE],
            ),
            (
                "the toggle off ignores the decoder name",
                "standard",
                "c2.exynos.hevc.decoder",
                false,
                &[STANDARD, QTI_LOW_LATENCY, REALTIME, MAX_RATE],
            ),
            (
                "off leaves out the standard key",
                "off",
                "c2.qti.avc.decoder",
                true,
                &[MEDIATEK, PICTURE_ORDER, QTI_LOW_LATENCY, MAX_RATE],
            ),
            (
                "mtk-tv leaves out the standard key and adds its game keys",
                "mtk-tv",
                "c2.mtk.hevc.decoder",
                true,
                &[
                    MEDIATEK,
                    REALTIME,
                    ("vendor.mtk-codec2.game-mode", 1),
                    ("vendor.mtk-codec2.low-latency-mode", 1),
                    ("operating-rate", 60),
                ],
            ),
            (
                "mtk-boost keeps the standard key",
                "mtk-boost",
                "c2.mtk.hevc.decoder",
                true,
                &[
                    STANDARD,
                    MEDIATEK,
                    REALTIME,
                    ("vendor.mtk.vdec.cpu.boost.mode.value", 1),
                ],
            ),
            (
                "qti-fence on Qualcomm on 13 sets the picture order key twice",
                "qti-fence",
                "c2.qti.avc.decoder",
                true,
                &[
                    STANDARD,
                    MEDIATEK,
                    PICTURE_ORDER,
                    QTI_LOW_LATENCY,
                    MAX_RATE,
                    PICTURE_ORDER,
                    ("vendor.qti-ext-output-sw-fence-enable.value", 1),
                    ("vendor.qti-ext-output-fence.enable", 1),
                    ("vendor.qti-ext-output-fence.fence_type", 1),
                ],
            ),
            (
                "qti-fence on another vendor adds nothing",
                "qti-fence",
                "c2.mtk.hevc.decoder",
                true,
                &[STANDARD, MEDIATEK, REALTIME],
            ),
        ];
        for (name, profile, decoder, aggressive, want) in rows {
            assert_eq!(
                low_latency_keys(profile, decoder, aggressive, 60, 34),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn qti_fence_adds_its_keys_only_from_android_13() {
        let on_12 = low_latency_keys("qti-fence", "c2.qti.avc.decoder", true, 60, 32);
        let plain = low_latency_keys("standard", "c2.qti.avc.decoder", true, 60, 32);
        assert_eq!(on_12, plain, "a refused fence leaves the standard keys");
        let on_13 = low_latency_keys("qti-fence", "c2.qti.avc.decoder", true, 60, 33);
        assert_eq!(on_13.len(), plain.len() + 4);
    }

    #[test]
    fn mtk_tv_runs_the_decoder_at_the_stream_rate() {
        let rate = |hz| {
            let keys = low_latency_keys("mtk-tv", "c2.mtk.hevc.decoder", true, hz, 34);
            keys.last().copied()
        };
        assert_eq!(rate(120), Some(("operating-rate", 120)));
        assert_eq!(rate(24), Some(("operating-rate", 24)));
        // A mode without a rate writes 0, though `frame-rate` is left out for it.
        assert_eq!(
            rate(0),
            Some(("vendor.mtk-codec2.low-latency-mode", 1)),
            "no rate key for a mode that states none"
        );
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
