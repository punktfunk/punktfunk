package io.unom.punktfunk.kit

import android.media.MediaCodec
import android.media.MediaCodecInfo.CodecCapabilities
import android.media.MediaCodecList
import android.os.Build

/** The decoder Kotlin ranked for a MIME, handed to [NativeBridge.nativeStartVideo]. */
data class DecoderChoice(val name: String, val lowLatencyFeature: Boolean)

/**
 * Rank the platform's `MediaCodecList` decoders for a video MIME and pick the best one for
 * low-latency streaming, the way Moonlight-Android does. There is no NDK `MediaCodecList`, so this
 * enumeration must live on the Kotlin (framework) side; Rust then creates the chosen decoder by
 * name (`AMediaCodec_createCodecByName`) and derives the per-SoC vendor low-latency keys from it.
 *
 * Ranking (best first): hardware over software; a real SoC-vendor decoder (Qualcomm/Amlogic/…) over
 * the generic AOSP software fallback; a decoder advertising `FEATURE_LowLatency` over one that
 * doesn't. Known-bad software decoders (`omx.google.*`, `c2.android.*`, Qualcomm/Samsung SW HEVC)
 * are dropped outright — matching Moonlight's blacklist.
 */
object VideoDecoders {
    /** Decoder-name prefixes/names we never want, mirroring Moonlight's blacklist. */
    private val BLOCKED_PREFIXES = listOf("omx.google.", "c2.android.", "avcdecoder", "omx.ffmpeg.")
    private val BLOCKED_EXACT = listOf("omx.qcom.video.decoder.hevcswvdec", "omx.sec.hevc.sw.dec")

    /**
     * Real SoC-vendor decoder prefixes we prefer over the generic AOSP fallback, covering the common
     * targets: Qualcomm Snapdragon and MediaTek (most phones + many TV boxes), Samsung Exynos (+
     * Google Tensor, whose decoder is `c2.exynos.*`), NVIDIA Tegra (Shield TV), Amlogic / Rockchip /
     * Realtek (TV boxes & smart TVs), and HiSilicon Kirin (older Huawei).
     */
    private val VENDOR_PREFIXES = listOf(
        "omx.qcom", "c2.qti",
        "omx.mtk", "c2.mtk",
        "omx.exynos", "c2.exynos",
        "omx.nvidia", "c2.nvidia",
        "omx.amlogic", "c2.amlogic",
        "omx.rk", "c2.rk",
        "omx.realtek", "c2.realtek",
        "omx.hisi", "c2.hisi",
    )

    /**
     * Pick the best decoder for [mime] (`"video/hevc"` / `"video/avc"` / `"video/av01"`), or `null`
     * to let the platform resolve its default. Enumerates once — call at stream start.
     */
    /**
     * The `quic::CODEC_*` bitfield of codecs this device can decode, advertised in the Hello so the
     * host never emits a codec the decode loop can't open: H.264 (1) always, HEVC (2) and AV1 (4)
     * only when [pickDecoder] finds a real (non-blocked) decoder for them, plus PyroWave (8) when
     * the GPU answers [NativeBridge.nativePyrowaveCapable]. ChromeOS (ARC) ships no HEVC decoder,
     * and the host's automatic pick prefers HEVC.
     *
     * PyroWave is the one bit here that names no `MediaCodec`: it is Vulkan compute, so the probe
     * asks the driver rather than `MediaCodecList`. Enumerates `MediaCodecList` (and, once per
     * process, a Vulkan instance) — call at connect time, not per frame.
     */
    fun decodableCodecBits(): Int =
        1 or
            (if (pickDecoder("video/hevc") != null) 2 else 0) or
            (if (pickDecoder("video/av01") != null) 4 else 0) or
            (if (pyrowaveCapable()) 8 else 0)

    /** The `MediaCodec` MIMEs [decodableCodecBits] advertises. */
    private fun advertisedMimes(): List<String> {
        val bits = decodableCodecBits()
        return buildList {
            add("video/avc")
            if (bits and 2 != 0) add("video/hevc")
            if (bits and 4 != 0) add("video/av01")
        }
    }

    /**
     * Whether this device's GPU decodes PyroWave ([NativeBridge.nativePyrowaveCapable]) — the
     * codec picker's gate as well as the advertised bit, so both answer from one place.
     *
     * Falls back to `false` when the native library is not loaded at all. That is not defensive
     * padding: the JVM screenshot tests render the REAL settings screen under Robolectric with no
     * `.so` behind it, and `System.loadLibrary` throws an `Error` (not an `Exception`) there.
     * "This device has no PyroWave" is the truthful answer for a process that has no core.
     */
    fun pyrowaveCapable(): Boolean =
        runCatching { NativeBridge.nativePyrowaveCapable() }.getOrDefault(false)

    /**
     * Whether EVERY decoder this device would use tolerates multi-slice access units — the
     * `VIDEO_CAP_MULTI_SLICE` advertisement (decoder truth, per the 0.17.0 field regression:
     * Amlogic HEVC decoders wedge the whole DEVICE on multi-slice frames — Chromecast with
     * Google TV, onn 4K — which is why Moonlight requests single-slice for every hardware
     * decoder). We advertise per-family instead: tolerant unless the pick for ANY advertised
     * codec is an Amlogic decoder or can't be inspected (a null pick = the platform default —
     * uninspectable, so conservatively single-slice). Probed once at connect time; the host
     * defaults to >1 slice only toward clients that set the bit.
     */
    fun multiSliceTolerant(): Boolean {
        val mimes = advertisedMimes()
        return mimes.all { mime ->
            val name = pickDecoder(mime)?.name?.lowercase() ?: return@all false
            !name.startsWith("omx.amlogic") && !name.startsWith("c2.amlogic")
        }
    }

    /**
     * Whether this device's display shows full-range BT.709 video as studio range — the
     * `FEATURE_STUDIO_RANGE` ask, so the host sends studio SDR. See [limitedOnlyBt709Hal].
     */
    fun showsFullRangeAsStudio(): Boolean {
        val vendorApi = vendorApiLevel()
        return advertisedMimes().any { mime ->
            val name = pickDecoder(mime)?.name ?: return@any false
            limitedOnlyBt709Hal(name, vendorApi)
        }
    }

    /** `ro.vendor.api_level`, 0 when unreadable. Android 15+ vendors report YYYYMM (202404). */
    private fun vendorApiLevel(): Int = runCatching {
        Class.forName("android.os.SystemProperties")
            .getMethod("getInt", String::class.java, Int::class.javaPrimitiveType)
            .invoke(null, "ro.vendor.api_level", 0) as Int
    }.getOrDefault(0)

    /**
     * True when EVERY decoder this device would stream with supports partial-frame input
     * (MediaCodec `FEATURE_PartialFrame`): the client may then opt into slice-progressive
     * delivery and feed slices with `BUFFER_FLAG_PARTIAL_FRAME` ahead of the AU's tail.
     * Codec selection happens after connect, so all advertised mimes must qualify — the same
     * conservative shape as [multiSliceTolerant] (an unprobeable pick disqualifies).
     */
    fun partialFrameCapable(): Boolean {
        val mimes = advertisedMimes()
        val infos = runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }
            .getOrNull() ?: return false
        return mimes.all { mime ->
            val pick = pickDecoder(mime)?.name ?: return@all false
            val info = infos.firstOrNull { it.name == pick } ?: return@all false
            runCatching {
                info.getCapabilitiesForType(mime)
                    .isFeatureSupported(CodecCapabilities.FEATURE_PartialFrame)
            }.getOrDefault(false)
        }
    }

    /**
     * One-line per-mime probe readout for the connect log (`adb logcat -s pf.caps`): which
     * decoder each advertised mime resolves to, whether it declares `FEATURE_PartialFrame` (the
     * P2 slice-pipeline gate) and which latency-named vendor parameters it declares — whether a
     * `low_latency_key` profile's keys exist on this device, otherwise invisible from a report.
     */
    fun capsReport(): String {
        val mimes = advertisedMimes()
        val infos = runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }
            .getOrNull() ?: return "codec list unavailable"
        return mimes.joinToString("  ") { mime ->
            val pick = pickDecoder(mime)?.name
            val partial = pick?.let { p ->
                infos.firstOrNull { it.name == p }?.let { info ->
                    runCatching {
                        info.getCapabilitiesForType(mime)
                            .isFeatureSupported(CodecCapabilities.FEATURE_PartialFrame)
                    }.getOrNull()
                }
            }
            "${mime.removePrefix("video/")}=${pick ?: "platform-default"}" +
                " partialFrame=${partial ?: "?"} vendor=[${vendorKeys(pick)}]"
        }
    }

    /** Substrings of the vendor parameter names worth a line in the connect log. */
    private val VENDOR_KEY_HINTS = listOf(
        "fence", "slice", "early", "picture-order", "low-latency", "lowlatency", "boost",
        "display-delay", "dpb",
    )
    private val vendorKeyCache = HashMap<String, String>()

    /**
     * The vendor parameters of [name] that match [VENDOR_KEY_HINTS], comma-joined; `?` below
     * API 31 or when the codec can't be opened. Opens the codec once per name and releases it.
     */
    private fun vendorKeys(name: String?): String {
        if (name == null || Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return "?"
        return vendorKeyCache.getOrPut(name) {
            runCatching {
                val codec = MediaCodec.createByCodecName(name)
                try {
                    codec.supportedVendorParameters
                        .filter { key -> VENDOR_KEY_HINTS.any { it in key } }
                        .joinToString(",")
                } finally {
                    codec.release()
                }
            }.getOrDefault("?")
        }
    }

    fun pickDecoder(mime: String): DecoderChoice? {
        if (mime.isEmpty()) return null
        val infos = runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }
            .getOrNull() ?: return null

        var bestName: String? = null
        var bestLowLatency = false
        var bestScore = Int.MIN_VALUE
        for (info in infos) {
            if (info.isEncoder) continue
            val name = info.name
            val lower = name.lowercase()
            if (BLOCKED_PREFIXES.any { lower.startsWith(it) } || lower in BLOCKED_EXACT) continue
            // Never a secure decoder: `.secure` names are the DRM-pipeline twins of the real
            // decoder and require a secure surface — configuring one for a clear stream fails (or
            // renders black). The plain twin is also in the list, so drop rather than rank
            // (a `.secure` twin can otherwise OUT-score its plain sibling when only it advertises
            // FEATURE_LowLatency). Moonlight filters the same way.
            if (lower.endsWith(".secure")) continue
            val caps = runCatching { info.getCapabilitiesForType(mime) }.getOrNull() ?: continue
            val secureRequired = runCatching {
                caps.isFeatureRequired(CodecCapabilities.FEATURE_SecurePlayback)
            }.getOrDefault(false)
            if (secureRequired) continue

            val hardware = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                info.isHardwareAccelerated
            } else {
                // Pre-Q heuristic: the software decoders are the ones we can name (already blocked
                // above), so anything surviving the blacklist is treated as hardware.
                true
            }
            val lowLatency = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
                runCatching { caps.isFeatureSupported(CodecCapabilities.FEATURE_LowLatency) }
                    .getOrDefault(false)
            val vendor = VENDOR_PREFIXES.any { lower.startsWith(it) }

            val score = (if (hardware) 100 else 0) +
                (if (vendor) 40 else 0) +
                (if (lowLatency) 20 else 0)
            if (score > bestScore) {
                bestScore = score
                bestName = name
                bestLowLatency = lowLatency
            }
        }
        return bestName?.let { DecoderChoice(it, bestLowLatency) }
    }
}

/**
 * A Qualcomm display HAL from before the Android 13 vendor release (kalama, SM8550) converts every
 * BT.709 YUV layer with the limited matrix whatever its range tag (`SelectCscType`): full-range
 * video shows crushed blacks and clipped highlights. Later HALs check the range. A Qualcomm
 * decoder marks the HAL's vendor; an unreadable vendor level (0) counts as old.
 */
internal fun limitedOnlyBt709Hal(decoder: String, vendorApi: Int): Boolean {
    val name = decoder.lowercase()
    val qualcomm = name.startsWith("c2.qti") || name.startsWith("omx.qcom")
    return qualcomm && vendorApi < Build.VERSION_CODES.TIRAMISU
}
