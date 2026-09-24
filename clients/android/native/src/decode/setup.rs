//! Codec creation, low-latency config, thread/frame-rate tuning, HDR static-info encode.

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

/// A short human label for the codec the host resolved, for the stats HUD's video-feed line
/// (`"H.264"` / `"HEVC"` / `"AV1"` / `"PyroWave"`). Mirrors [`codec_mime`]'s fallback: anything
/// not H.264/AV1/PyroWave is reported as HEVC (every pre-negotiation host emitted HEVC). Kept
/// beside [`codec_mime`] because the MIME collapses PyroWave onto `video/hevc` and so can't name it.
pub(crate) fn codec_label(codec: u8) -> &'static str {
    match codec {
        punktfunk_core::quic::CODEC_H264 => "H.264",
        punktfunk_core::quic::CODEC_AV1 => "AV1",
        punktfunk_core::quic::CODEC_PYROWAVE => "PyroWave",
        _ => "HEVC",
    }
}

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
/// reassembly) and the audio decode thread — toward the display band, matching this decode thread's
/// own boost. `setpriority(PRIO_PROCESS, tid)` targets any task in the process, so we do it from
/// here once their tids are known (the same set ADPF hints), without a per-platform priority hook
/// in the shared core. Slightly below the decode thread's -10 so the display path still wins.
/// Best-effort; skips this thread (already boosted) and is non-fatal if the platform refuses.
pub(super) fn boost_hot_threads(tids: &[i32]) {
    // SAFETY: `gettid` is an always-safe syscall on the calling thread.
    let self_tid = unsafe { libc::gettid() };
    for &tid in tids {
        if tid == self_tid {
            continue;
        }
        // SAFETY: `setpriority` with PRIO_PROCESS + a live tid in our own process is an always-safe
        // syscall; a refusal is reported via the return value, not UB.
        unsafe {
            if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8) != 0 {
                log::debug!("decode: setpriority(-8) on hot tid {tid} failed (non-fatal)");
            }
        }
    }
}

/// Best-effort: raise the decode thread toward Android's URGENT_DISPLAY band so background work
/// can't preempt it under load (which shows up as late/dropped frames). Non-fatal if the platform
/// refuses (foreground apps may set their own threads; the exact floor is policy-dependent).
pub(crate) fn boost_thread_priority() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls. PRIO_PROCESS
    // with a TID targets that one task on Linux — the same idiom `Process.setThreadPriority` uses.
    unsafe {
        let tid = libc::gettid();
        if libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -10) != 0 {
            log::warn!(
                "decode: setpriority(-10) failed (non-fatal): {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Set the surface's frame-rate hint to the stream's refresh so SurfaceFlinger picks a matching
/// display mode and aligns vsync (no 60-in-120 judder). Both NDK entry points sit above our API-28
/// floor, so both are dlsym-resolved at runtime (a hard import of a >floor symbol makes
/// `dlopen`/`System.load` fail on every API-28/29 device, even where this path is never hit —
/// mirrors [`crate::adpf`]):
///   - On a **TV** (`is_tv`): `ANativeWindow_setFrameRateWithChangeStrategy` (**API 31**) with
///     `changeFrameRateStrategy = ALWAYS`, which actively drives the HDMI output into the matching
///     mode (e.g. 60↔120) instead of leaving the panel at its default and judder-matching. The
///     forced switch may blank the panel briefly — acceptable once at stream start, not wanted on a
///     phone. Falls through to the 2-arg hint on API 30.
///   - Otherwise: `ANativeWindow_setFrameRate` (**API 30**) — the seamless-preferred hint for
///     phones/tablets and the universal fallback.
///
/// Both paths pass `compatibility = FIXED_SOURCE` (1): the stream is fixed-rate video content the
/// client cannot re-pace, which is exactly what that value declares — `DEFAULT` (0) told the
/// platform the app could adapt to whatever rate it picked, an invitation some OEM refresh
/// governors accepted by simply not switching. (The window-level `preferredDisplayModeId` pin in
/// `MainActivity.setStreamDisplayMode` is the phone-side belt to this braces.)
///
/// Returns `true` when the platform accepted a hint; `false` on API < 30 (symbols absent) or a
/// decline.
pub(super) fn try_set_frame_rate(window: &NativeWindow, frame_rate: f32, is_tv: bool) -> bool {
    // int32_t ANativeWindow_setFrameRate(ANativeWindow*, float frameRate, int8_t compatibility)
    type SetFrameRateFn = unsafe extern "C" fn(*mut c_void, f32, i8) -> i32;
    // int32_t ANativeWindow_setFrameRateWithChangeStrategy(
    //     ANativeWindow*, float frameRate, int8_t compatibility, int8_t changeFrameRateStrategy)
    type SetFrameRateStrategyFn = unsafe extern "C" fn(*mut c_void, f32, i8, i8) -> i32;
    // SAFETY: `dlopen` of the always-mapped `libandroid.so` (only bumps its refcount; never closed —
    // process-lifetime handle). Each `dlsym` returns null when the symbol is absent (device below the
    // symbol's API level), checked before transmuting the non-null pointer to its fn-pointer type.
    // `window.ptr()` is the live `ANativeWindow` this `NativeWindow` owns for the call's duration.
    unsafe {
        let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return false;
        }
        // ANATIVEWINDOW_FRAME_RATE_COMPATIBILITY_FIXED_SOURCE — fixed-rate video content.
        const FIXED_SOURCE: i8 = 1;
        // TV: prefer the API-31 change-strategy form to force the mode switch (strategy 1 =
        // ALWAYS). Absent on API 30 ⇒ fall through to the 2-arg hint below.
        if is_tv {
            let sym = libc::dlsym(
                lib,
                c"ANativeWindow_setFrameRateWithChangeStrategy".as_ptr(),
            );
            if !sym.is_null() {
                let set = std::mem::transmute::<*mut c_void, SetFrameRateStrategyFn>(sym);
                return set(window.ptr().as_ptr().cast(), frame_rate, FIXED_SOURCE, 1) == 0;
            }
        }
        let sym = libc::dlsym(lib, c"ANativeWindow_setFrameRate".as_ptr());
        if sym.is_null() {
            return false; // device API < 30 — no per-surface frame-rate hint
        }
        let set_frame_rate = std::mem::transmute::<*mut c_void, SetFrameRateFn>(sym);
        set_frame_rate(window.ptr().as_ptr().cast(), frame_rate, FIXED_SOURCE) == 0
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
pub(super) fn hdr_static(client: &NativeClient) -> Option<punktfunk_core::quic::HdrMeta> {
    if !client.color.is_hdr() {
        return None;
    }
    match client.next_hdr_meta(Duration::from_millis(250)) {
        Ok(meta) => {
            log::info!("decode: HDR static metadata applied (KEY_HDR_STATIC_INFO)");
            Some(meta)
        }
        Err(_) => {
            log::info!("decode: HDR session but no mastering metadata yet — DataSpace only");
            None
        }
    }
}

/// Overrides the low-latency profile: `standard` (the default), `off` (no standard `low-latency`
/// key), `mtk-tv` (no standard key; the TV decoder's game keys and `operating-rate` = fps instead).
/// `adb shell setprop debug.punktfunk.low_latency_key standard`.
pub(super) const LOW_LATENCY_KEY_PROP: &std::ffi::CStr = c"debug.punktfunk.low_latency_key";

/// TV platforms that show half the stream rate, as (`ro.product.manufacturer`, `ro.product.device`):
/// Philips' Pentonic 1000 sets (OLED809/810/888, PUS8508) and TCL's Pentonic 700 `G08` (C6K, C755,
/// C805). Under the standard `low-latency` key their display stack registers the stream at the
/// 120 Hz panel maximum, runs VRR only for HDMI inputs, and drops every other frame to fit the
/// 60 Hz app output (HWC log `PqLink … => 1/2`). They get the `mtk-tv` profile.
const HALF_RATE_TVS: &[(&str, &str)] = &[("TPV", "PH1M_WW_9972"), ("TCL", "G08")];

fn half_rate_tv() -> bool {
    use super::asc_presenter::sysprop;
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

/// The decoder's configure format: the mode, an input buffer generous enough that a large keyframe
/// AU is never truncated, the HDR static info, and the low-latency keys for `codec_name`.
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
    format.set_i32(
        "max-input-size",
        (mode.width * mode.height).max(2_000_000) as i32,
    );
    if let Some(aggressive) = keys {
        let forced = super::asc_presenter::sysprop(LOW_LATENCY_KEY_PROP);
        let profile = match forced.as_deref() {
            Some(p @ ("standard" | "off" | "mtk-tv")) => p,
            _ if codec_name.to_ascii_lowercase().starts_with("c2.mtk") && half_rate_tv() => {
                "mtk-tv"
            }
            _ => "standard",
        };
        configure_low_latency(&mut format, codec_name, aggressive, profile == "standard");
        if profile != "standard" {
            log::info!("decode: low-latency profile {profile} — standard low-latency key left out");
        }
        if profile == "mtk-tv" {
            format.set_i32("vendor.mtk-codec2.game-mode", 1);
            format.set_i32("vendor.mtk-codec2.low-latency-mode", 1);
            format.set_i32("operating-rate", mode.refresh_hz as i32);
        }
    }
    if let Some(meta) = hdr_static {
        format.set_buffer("hdr-static-info", &android_hdr_static_info(meta));
    }
    format
}
