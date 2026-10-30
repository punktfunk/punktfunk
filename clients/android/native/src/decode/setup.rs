//! Codec creation, low-latency config, thread/frame-rate tuning, HDR static-info encode.

use crate::sys::{set_thread_nice, sysprop};
use ndk::media::media_codec::MediaCodec;
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use std::ffi::c_void;
use std::time::Duration;

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

/// The codec the host resolved, as every overlay names it: empty for an id this build does not
/// know, which the stream notification leaves out.
pub(crate) use punktfunk_core::hud::codec_label;

/// Create the decoder: prefer the specific codec Kotlin ranked from `MediaCodecList`
/// (`from_codec_name`), falling back to the platform's default decoder for the MIME
/// (`from_decoder_type`) if that name can't be created (codec busy / renamed across an OS update).
pub(super) fn create_codec(mime: &str, preferred: Option<&str>) -> Option<MediaCodec> {
    if let Some(name) = preferred.filter(|n| !n.is_empty()) {
        if let Some(c) = MediaCodec::from_codec_name(name) {
            return Some(c);
        }
        log::warn!(
            "decode: from_codec_name({name}) failed — falling back to default {mime} decoder"
        );
    }
    MediaCodec::from_decoder_type(mime)
}

/// Apply the low-latency MediaFormat keys for `codec_name`.
///
/// `aggressive` = the "Low-latency mode" master toggle. **Off** ⇒ the pre-overhaul key set,
/// byte-for-byte — the standard `low-latency` key, the blind Qualcomm vendor twin, `priority = 0` AND
/// `operating-rate = MAX` set together — kept as the per-device escape hatch (the profile every device
/// streamed with before the overhaul). **On** (default) ⇒ the Moonlight-parity
/// profile: MediaTek's `vdec-lowlatency` (unconditionally — ignored off MediaTek), the per-SoC
/// vendor extension keys (gated on the decoder-name prefix the way Moonlight-Android does, since a
/// key one vendor honours is meaningless on another), and one *mutually exclusive* clock hint.
///
/// Vendor keys mirror Moonlight's `MediaCodecHelper` (verified against current source): Qualcomm
/// picture-order + low-latency, Exynos (also Google Tensor), Amlogic, HiSilicon, MediaTek. NVIDIA
/// Tegra / Rockchip / Realtek expose no such key (nor does Moonlight) — they're covered by the
/// standard key + clock hint + being ranked first in `VideoDecoders`.
///
/// `standard_key` = false leaves out the standard `low-latency` key (see [`low_latency_format`]).
pub(super) fn configure_low_latency(
    format: &mut MediaFormat,
    codec_name: &str,
    aggressive: bool,
    standard_key: bool,
) {
    // Standard key: request the no-reorder low-latency path where the platform decoder supports it.
    if standard_key {
        format.set_i32("low-latency", 1);
    }
    if !aggressive {
        // The original profile: the Qualcomm vendor twin set blind (unknown keys are ignored by
        // other vendors' codecs), realtime priority, and the AOSP "unbounded" operating-rate
        // sentinel — decode each frame at max clocks rather than pacing to the frame rate.
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
        format.set_i32("priority", 0); // 0 = realtime
        format.set_i32("operating-rate", i16::MAX as i32); // 32767 = "as fast as possible"
        return;
    }
    // MediaTek's low-latency key — very common (mid/budget phones + many Google TV / Fire TV boxes).
    // Set unconditionally like the standard key: MediaTek decoders honour it, others ignore it, so it
    // covers MediaTek whatever the exact decoder name (omx.mtk / c2.mtk / an OEM rename). Moonlight
    // does the same, and also relies on it for Amazon's Amlogic fork.
    format.set_i32("vdec-lowlatency", 1);
    let name = codec_name.to_ascii_lowercase();
    let is = |prefix: &str| name.starts_with(prefix);
    // Qualcomm Snapdragon (the most common phone SoC): picture-order forces decode-order output
    // (kills the reorder buffer on decoders that predate the standard key); low-latency is the older
    // vendor twin.
    if is("omx.qcom") || is("c2.qti") {
        format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
        format.set_i32("vendor.qti-ext-dec-low-latency.enable", 1);
    }
    // Samsung Exynos — also covers Google Tensor (Pixel 6+), whose hardware decoder is `c2.exynos.*`.
    if is("omx.exynos") || is("c2.exynos") {
        format.set_i32("vendor.rtc-ext-dec-low-latency.enable", 1);
    }
    // Amlogic — the Android TV boxes (onn 4K, Chromecast w/ Google TV, Homatics).
    if is("omx.amlogic") || is("c2.amlogic") {
        format.set_i32("vendor.low-latency.enable", 1);
    }
    // HiSilicon / Kirin (older Huawei; paired req/rdy keys).
    if is("omx.hisi") || is("c2.hisi") {
        format.set_i32(
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-req",
            1,
        );
        format.set_i32(
            "vendor.hisi-ext-low-latency-video-dec.video-scene-for-low-latency-rdy",
            -1,
        );
    }
    // NVIDIA Tegra (Shield TV) and Rockchip/Realtek (budget TV boxes / smart TVs) expose no
    // low-latency vendor key (Moonlight has none either) — their decoders are already low-latency
    // oriented, so the standard `low-latency` key + the clock hint below + being ranked first
    // (see `VideoDecoders`) is their treatment.
    //
    // Clock hint, mutually exclusive (matching Moonlight): the AOSP "unbounded" operating-rate
    // sentinel (Short.MAX) tells the decoder to run each frame at max clocks and finish ASAP rather
    // than pace to the frame rate — shaving per-frame decode latency at a power/heat cost. Only
    // Qualcomm is known to handle the sentinel; every other vendor mis-paces on it, so they get the
    // plain realtime `priority` hint instead.
    if decoder_supports_max_operating_rate(&name) {
        format.set_i32("operating-rate", i16::MAX as i32); // 32767 = "as fast as possible"
    } else {
        format.set_i32("priority", 0); // 0 = realtime
    }
}

/// Whether a decoder tolerates `operating-rate = Short.MAX` rather than regressing on it. Follows
/// Moonlight's allowlist: Qualcomm decoders honour the sentinel (the Adreno 620 generation is the
/// known exception Moonlight excludes by GPU model — undetectable from native code here, so it
/// rides the master toggle as its escape hatch). Other vendors fall back to the plain `priority`
/// hint above.
fn decoder_supports_max_operating_rate(name_lower: &str) -> bool {
    name_lower.starts_with("omx.qcom") || name_lower.starts_with("c2.qti")
}

/// Raise the pipeline's OTHER hot threads — the core's data-plane pump (UDP receive + FEC
/// reassembly) and any other registered thread — toward the display band, matching this decode
/// thread's own boost. `setpriority(PRIO_PROCESS, tid)` targets any task in the process, so we do
/// it from here once their tids are known (the same set ADPF hints), without a per-platform
/// priority hook in the shared core. Slightly below the decode thread's -10 so the display path
/// still wins. Only ever raises: the audio and mic threads already sit at
/// [`crate::audio::AUDIO_NICE`]. Best-effort; skips this thread and is non-fatal if refused.
pub(super) fn boost_hot_threads(tids: &[i32]) {
    let self_tid = crate::sys::gettid();
    for &tid in tids.iter().filter(|&&tid| tid != self_tid) {
        // SAFETY: `getpriority` takes no pointers. A failed read returns -1, above -8, so the set
        // is tried and fails the same way.
        if unsafe { libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t) } <= -8 {
            continue;
        }
        if set_thread_nice(Some(tid), -8).is_err() {
            log::debug!("decode: setpriority(-8) on hot tid {tid} failed (non-fatal)");
        }
    }
}

/// Best-effort: raise the decode thread toward Android's URGENT_DISPLAY band so background work
/// can't preempt it under load (which shows up as late/dropped frames). Non-fatal if the platform
/// refuses (foreground apps may set their own threads; the exact floor is policy-dependent).
pub(crate) fn boost_thread_priority() {
    if let Err(e) = set_thread_nice(None, -10) {
        log::warn!("decode: setpriority(-10) failed (non-fatal): {e}");
    }
}

/// Set the surface's frame-rate hint to the stream's refresh so SurfaceFlinger picks a matching
/// display mode and aligns vsync (no 60-in-120 judder). Both NDK entry points sit above our API-28
/// floor, so both are dlsym-resolved at runtime (a hard import of a >floor symbol makes
/// `dlopen`/`System.load` fail on every API-28/29 device, even where this path is never hit —
/// mirrors [`crate::adpf`]):
///   - `always` ([`super::DecodeOptions::force_rate_switch`]):
///     `ANativeWindow_setFrameRateWithChangeStrategy` (**API 31**) with strategy `ALWAYS`, which
///     may switch the panel non-seamlessly (a brief blank once at stream start). Falls through to
///     the 2-arg hint on API 30.
///   - Otherwise: `ANativeWindow_setFrameRate` (**API 30**), the seamless-only hint.
///
/// Both pass `compatibility = FIXED_SOURCE` (1): the stream is fixed-rate video the client cannot
/// re-pace. `DEFAULT` (0) invites an OEM governor to simply not switch.
///
/// Returns `true` when the platform accepted a hint; `false` on API < 30 (symbols absent) or a
/// decline.
pub(super) fn try_set_frame_rate(window: &NativeWindow, frame_rate: f32, always: bool) -> bool {
    // int32_t ANativeWindow_setFrameRate(ANativeWindow*, float frameRate, int8_t compatibility)
    type SetFrameRateFn = unsafe extern "C" fn(*mut c_void, f32, i8) -> i32;
    // int32_t ANativeWindow_setFrameRateWithChangeStrategy(
    //     ANativeWindow*, float frameRate, int8_t compatibility, int8_t changeFrameRateStrategy)
    type SetFrameRateStrategyFn = unsafe extern "C" fn(*mut c_void, f32, i8, i8) -> i32;
    // SAFETY: `dlopen` of the always-mapped `libandroid.so` (only bumps its refcount; never closed —
    // process-lifetime handle; null is checked). Each `sym` type is the NDK header's signature;
    // absent = below its API level. `window.ptr()` is live for the call (`&NativeWindow`).
    unsafe {
        let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return false;
        }
        // ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_FIXED_SOURCE — fixed-rate video content.
        const FIXED_SOURCE: i8 = 1;
        // Strategy 1 = ALWAYS. Absent on API 30 ⇒ fall through to the 2-arg hint below.
        if always {
            if let Some(set) = crate::sym::<SetFrameRateStrategyFn>(
                lib,
                c"ANativeWindow_setFrameRateWithChangeStrategy",
            ) {
                return set(window.ptr().as_ptr().cast(), frame_rate, FIXED_SOURCE, 1) == 0;
            }
        }
        let Some(set) = crate::sym::<SetFrameRateFn>(lib, c"ANativeWindow_setFrameRate") else {
            return false; // device API < 30 — no per-surface frame-rate hint
        };
        set(window.ptr().as_ptr().cast(), frame_rate, FIXED_SOURCE) == 0
    }
}

/// Serialize [`HdrMeta`](punktfunk_core::quic::HdrMeta) into Android's `KEY_HDR_STATIC_INFO`
/// (`hdr-static-info`) layout: a 25-byte CTA-861.3 / `HDRStaticInfo.Type1` blob — descriptor id 0,
/// then primaries in **R, G, B** order, white point, max/min display luminance, MaxCLL, MaxFALL, all
/// **little-endian** `u16`. Two conversions vs our wire form: HdrMeta stores primaries in ST.2086
/// **G, B, R** order (reorder to R, G, B), and `max_display_mastering_luminance` is in 0.0001-cd/m²
/// units while Android wants **whole nits** (min stays 0.0001-nit). Chromaticities (1/50000) and
/// MaxCLL/MaxFALL (nits) match 1:1.
pub(super) fn android_hdr_static_info(m: &punktfunk_core::quic::HdrMeta) -> [u8; 25] {
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

/// HDR static metadata (ST.2086 mastering + content light level) for `KEY_HDR_STATIC_INFO`, so the
/// display tone-maps from the source's real grade. MediaCodec wants it BEFORE configure(), and the
/// host sends a 0xCE right after the handshake, so it's typically already queued; wait briefly
/// otherwise. The Surface DataSpace (applied on the format change) carries transfer/primaries
/// regardless — this adds the luminance the tone-mapper needs. `None` on an SDR session.
/// The newest entry, not the first: the host follows its generic baseline with the source's grade.
pub(super) fn hdr_static(client: &NativeClient) -> Option<punktfunk_core::quic::HdrMeta> {
    if !client.video_format().color.is_hdr() {
        return None;
    }
    match client.latest_hdr_meta(Duration::from_millis(250)) {
        Some(meta) => {
            log::info!("decode: HDR static metadata applied (KEY_HDR_STATIC_INFO)");
            Some(meta)
        }
        None => {
            log::info!("decode: HDR session but no mastering metadata yet — DataSpace only");
            None
        }
    }
}

/// Overrides the low-latency profile: `standard` (the default), `off` (no standard `low-latency`
/// key), `mtk-tv` (no standard key; the TV decoder's game keys and `operating-rate` = fps instead),
/// `mtk-boost` (standard plus MediaTek's `vendor.mtk.vdec.cpu.boost.mode.value`: the codec asks
/// PowerHAL for a CPU minimum-clock lock for its life, which the MediaTek library reads at init).
/// `mtk-boost` stays opt-in because it spends power; a decoder without the key ignores it.
/// `qti-fence` (standard plus Qualcomm's output-fence keys, [`qti_fence_eligible`]): the codec
/// returns each output when the input is consumed, with a fence that signals when the pixels are
/// written. Opt-in until the glass gain is measured; the `pf.present` line's `fenceTailMs` is
/// the codec time the HUD `decode` then no longer sees.
/// `adb shell setprop debug.punktfunk.low_latency_key mtk-boost`.
pub(super) const LOW_LATENCY_KEY_PROP: &std::ffi::CStr = c"debug.punktfunk.low_latency_key";

/// The Qualcomm output-fence keys fit a Codec2 Qualcomm decoder on Android 13+. Only
/// `CCodecBufferChannel` since 13 forwards the codec's fence to the consumer; earlier releases
/// queue the buffer unfenced, and SurfaceFlinger would show it unfinished.
fn qti_fence_eligible(codec_name: &str, sdk: u32) -> bool {
    codec_name.to_ascii_lowercase().starts_with("c2.qti") && sdk >= 33
}

/// TV platforms that show half the stream rate, as (`ro.product.manufacturer`, `ro.product.device`):
/// Philips' Pentonic 1000 sets (OLED809/810/888, PUS8508) and TCL's Pentonic 700 `G08` (C6K, C755,
/// C805). Under the standard `low-latency` key their display stack registers the stream at the
/// 120 Hz panel maximum, runs VRR only for HDMI inputs, and drops every other frame to fit the
/// 60 Hz app output (HWC log `PqLink … => 1/2`). They get the `mtk-tv` profile.
const HALF_RATE_TVS: &[(&str, &str)] = &[("TPV", "PH1M_WW_9972"), ("TCL", "G08")];

fn half_rate_tv() -> bool {
    let (Some(maker), Some(device)) = (
        sysprop(c"ro.product.manufacturer"),
        sysprop(c"ro.product.device"),
    ) else {
        return false;
    };
    HALF_RATE_TVS
        .iter()
        .any(|(m, d)| maker.eq_ignore_ascii_case(m) && device == *d)
}

/// The decoder's configure format: the mode, the stream rate (`frame-rate`, the standard key a
/// decoder's clock table may read), an input buffer generous enough that a large keyframe AU is
/// never truncated, the HDR static info, and the low-latency keys for `codec_name`.
/// `keys` is `Some(aggressive)` for [`configure_low_latency`]'s profile, `None` for no
/// low-latency key at all. A `c2.mtk` decoder on a [`HALF_RATE_TVS`] platform swaps the `Some`
/// profile for `mtk-tv`; [`LOW_LATENCY_KEY_PROP`] overrides either way.
/// ACodec ignores a refused `max-input-size`, so that key stays.
pub(super) fn low_latency_format(
    mime: &str,
    mode: &Mode,
    codec_name: &str,
    keys: Option<bool>,
    hdr_static: Option<&punktfunk_core::quic::HdrMeta>,
) -> MediaFormat {
    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", mode.width as i32);
    format.set_i32("height", mode.height as i32);
    if mode.refresh_hz > 0 {
        format.set_i32("frame-rate", mode.refresh_hz as i32);
    }
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    if let Some(aggressive) = keys {
        let forced = sysprop(LOW_LATENCY_KEY_PROP);
        let profile = match forced.as_deref() {
            Some(p @ ("standard" | "off" | "mtk-tv" | "mtk-boost" | "qti-fence")) => p,
            _ if codec_name.to_ascii_lowercase().starts_with("c2.mtk") && half_rate_tv() => {
                "mtk-tv"
            }
            _ => "standard",
        };
        let standard_key = matches!(profile, "standard" | "mtk-boost" | "qti-fence");
        configure_low_latency(&mut format, codec_name, aggressive, standard_key);
        if profile != "standard" {
            log::info!("decode: low-latency profile {profile}");
        }
        match profile {
            "mtk-tv" => {
                format.set_i32("vendor.mtk-codec2.game-mode", 1);
                format.set_i32("vendor.mtk-codec2.low-latency-mode", 1);
                format.set_i32("operating-rate", mode.refresh_hz as i32);
            }
            "mtk-boost" => format.set_i32("vendor.mtk.vdec.cpu.boost.mode.value", 1),
            "qti-fence" if qti_fence_eligible(codec_name, crate::sys::sdk_int()) => {
                // Decode-order output is the fence's prerequisite. The sw-fence key is the
                // 8 Gen 2 spelling, the output-fence pair the 8 Gen 3 / 8 Elite one (type 1:
                // the driver signals). A codec without a key ignores it.
                format.set_i32("vendor.qti-ext-dec-picture-order.enable", 1);
                format.set_i32("vendor.qti-ext-output-sw-fence-enable.value", 1);
                format.set_i32("vendor.qti-ext-output-fence.enable", 1);
                format.set_i32("vendor.qti-ext-output-fence.fence_type", 1);
            }
            "qti-fence" => log::warn!("decode: qti-fence needs a c2.qti decoder on Android 13+"),
            _ => {}
        }
    }
    if let Some(meta) = hdr_static {
        format.set_buffer("hdr-static-info", &android_hdr_static_info(meta));
    }
    format
}

#[cfg(test)]
mod tests {
    use super::qti_fence_eligible;

    #[test]
    fn qti_fence_needs_a_codec2_qualcomm_decoder_on_13() {
        assert!(qti_fence_eligible("c2.qti.hevc.decoder.low_latency", 33));
        assert!(qti_fence_eligible("C2.QTI.avc.decoder", 35));
        assert!(!qti_fence_eligible("c2.qti.hevc.decoder", 32));
        assert!(!qti_fence_eligible("omx.qcom.video.decoder.hevc", 34));
        assert!(!qti_fence_eligible("c2.mtk.hevc.decoder", 34));
    }
}
