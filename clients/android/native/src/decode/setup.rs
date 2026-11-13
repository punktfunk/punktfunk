//! Codec creation, the configure format, thread and frame-rate tuning.

use crate::codec_format::{low_latency_keys, low_latency_profile, qti_fence_eligible};
use crate::colour::android_hdr_static_info;
use crate::sys::{set_thread_nice, sysprop};
use ndk::media::media_codec::MediaCodec;
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use std::ffi::c_void;
use std::time::Duration;

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
/// `keys` is `Some(aggressive)` for [`low_latency_keys`]' profile, `None` for no low-latency key
/// at all. A `c2.mtk` decoder on a [`HALF_RATE_TVS`] platform swaps the `Some` profile for
/// `mtk-tv`; [`LOW_LATENCY_KEY_PROP`] overrides either way.
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
        let profile = low_latency_profile(forced.as_deref(), codec_name, half_rate_tv);
        if profile != "standard" {
            log::info!("decode: low-latency profile {profile}");
        }
        let sdk = crate::sys::sdk_int();
        for (key, value) in low_latency_keys(profile, codec_name, aggressive, mode.refresh_hz, sdk)
        {
            format.set_i32(key, value);
        }
        if profile == "qti-fence" && !qti_fence_eligible(codec_name, sdk) {
            log::warn!("decode: qti-fence needs a c2.qti decoder on Android 13+");
        }
    }
    if let Some(meta) = hdr_static {
        format.set_buffer("hdr-static-info", &android_hdr_static_info(meta));
    }
    format
}
