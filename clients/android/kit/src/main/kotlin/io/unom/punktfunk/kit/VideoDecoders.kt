package io.unom.punktfunk.kit

import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecInfo.CodecCapabilities
import android.media.MediaCodecInfo.VideoCapabilities.PerformancePoint
import android.media.MediaCodecList
import android.os.Build

/**
 * The decoder Kotlin ranked for a MIME, handed to [NativeBridge.nativeStartVideo]. [pixelRate]
 * is the most pixels a second it declares, 0 when it declares none.
 */
data class DecoderChoice(
    val name: String,
    val lowLatencyFeature: Boolean,
    val pixelRate: Long = 0,
)

/** What a session asks of its decoder: the requested size and rate, and 10-bit (HDR or SDR). */
data class DecodeNeed(val width: Int, val height: Int, val fps: Int, val tenBit: Boolean)

/** [VideoDecoders.probe]'s answer: the `quic::CODEC_*` bits, and whether 10-bit stays asked. */
data class DecodeCaps(val codecBits: Int, val tenBit: Boolean)

/**
 * One decoder for one MIME, as the probe reads it. [hardware] is `isHardwareAccelerated`, which
 * only API 29+ has; [fits] says it takes the asked size and rate.
 */
internal data class DecoderFacts(
    val name: String,
    val mime: String,
    val hardware: Boolean,
    val profiles: List<Int> = emptyList(),
    val fits: Boolean = true,
    val lowLatency: Boolean = false,
    val secureRequired: Boolean = false,
    /** `FEATURE_PartialFrame`; null when the codec couldn't say. */
    val partialFrame: Boolean? = false,
)

/**
 * Rank the platform's `MediaCodecList` decoders for a video MIME and pick the best one for
 * low-latency streaming, the way Moonlight-Android does. There is no NDK `MediaCodecList`, so this
 * enumeration must live on the Kotlin (framework) side; Rust then creates the chosen decoder by
 * name (`AMediaCodec_createCodecByName`) and derives the per-SoC vendor low-latency keys from it.
 * The rules themselves are the pure [bestDecoder] and [codecCaps].
 */
object VideoDecoders {
    /**
     * The codecs this device decodes in hardware and whether 10-bit stays asked ([codecCaps]),
     * plus PyroWave (8) when the GPU answers [NativeBridge.nativePyrowaveCapable]. [need] null
     * checks no size, rate or profile: the codec pickers' question. PyroWave is Vulkan compute,
     * so the driver answers for it, not `MediaCodecList`.
     */
    fun probe(need: DecodeNeed? = null): DecodeCaps = snapshot(need).decode

    /** [probe]'s codec bits with no mode in hand. */
    fun decodableCodecBits(): Int = probe().codecBits

    /**
     * One walk of the codec list at [need] ([DecoderSnapshot]); null checks no size or rate. A
     * decoder is asked only for the MIMEs it lists, so a missing one costs no exception.
     */
    fun snapshot(need: DecodeNeed? = null): DecoderSnapshot {
        val api = Build.VERSION.SDK_INT
        val infos = codecInfos()
        val trust = need != null && infos != null && pointsTrusted(infos)
        val read = MIMES.map { mime -> mime to infos?.let { decoders(it, mime) }.orEmpty() }
        val facts = read.flatMap { (mime, rows) ->
            rows.map { (info, caps) -> facts(info, caps, mime, need, trust) }
        }
        val census = when {
            infos == null -> "codec list unavailable"
            need == null -> ""
            else -> read.joinToString("  ") { (mime, rows) ->
                val line = rows.joinToString("; ") { (info, caps) -> censusRow(info, caps, need, trust) }
                "${mime.removePrefix("video/")}: ${line.ifEmpty { "none" }}"
            }
        }
        return DecoderSnapshot(
            facts, need?.tenBit == true, pyrowaveCapable(), api, vendorApiLevel(), infos != null, census,
        )
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

    /** `ro.vendor.api_level`, 0 when unreadable. Android 15+ vendors report YYYYMM (202404). */
    private fun vendorApiLevel(): Int = runCatching {
        Class.forName("android.os.SystemProperties")
            .getMethod("getInt", String::class.java, Int::class.javaPrimitiveType)
            .invoke(null, "ro.vendor.api_level", 0) as Int
    }.getOrDefault(0)

    /** [DecoderSnapshot.partialFrameCapable] with no mode in hand. */
    fun partialFrameCapable(): Boolean = snapshot().partialFrameCapable

    /**
     * One decoder as the census line reads it: hardware or not, whether it takes [need], its
     * declared top bitrate and its latency features.
     */
    private fun censusRow(
        info: MediaCodecInfo,
        caps: CodecCapabilities,
        need: DecodeNeed,
        trust: Boolean,
    ): String {
        val hw = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && info.isHardwareAccelerated
        val has = { feature: String ->
            runCatching { caps.isFeatureSupported(feature) }.getOrDefault(false)
        }
        val features = listOfNotNull(
            "lowLatency".takeIf { has(CodecCapabilities.FEATURE_LowLatency) },
            "partialFrame".takeIf { has(CodecCapabilities.FEATURE_PartialFrame) },
            "tunneled".takeIf { has(CodecCapabilities.FEATURE_TunneledPlayback) },
        ).joinToString("+")
        val mbps = caps.videoCapabilities?.bitrateRange?.upper?.div(1_000_000)
        return "${info.name} ${if (hw) "hw" else "sw"} fits=${fits(caps, need, trust)}" +
            " maxMbps=${mbps ?: "?"} [$features]"
    }

    /** Substrings of the vendor parameter names worth a line in the connect log. */
    private val VENDOR_KEY_HINTS = listOf(
        "fence", "slice", "early", "picture-order", "low-latency", "lowlatency", "boost",
        "display-delay", "dpb", "lowdelay", "low-delay", "reorder",
    )
    private val vendorKeyCache = HashMap<String, String>()

    /**
     * Opens each advertised codec's pick once, for the vendor keys [DecoderSnapshot.capsReport]
     * logs. Run it off the main thread before any session: a box with one hardware decoder can't
     * open a second while a stream holds it.
     */
    fun readVendorKeys() = snapshot().picks.forEach { (_, pick) -> vendorKeys(pick?.name) }

    /**
     * The vendor parameters of [name] that match [VENDOR_KEY_HINTS], comma-joined; `?` below
     * API 31 or when the codec can't be opened. Opens the codec once per name and releases it.
     */
    internal fun vendorKeys(name: String?): String {
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

    /**
     * The decoder to stream [mime] with ([bestDecoder]), or `null` to let the platform resolve
     * its default. Enumerates `MediaCodecList` — call at stream start, not per frame.
     */
    fun pickDecoder(mime: String): DecoderChoice? {
        if (mime.isEmpty()) return null
        val infos = codecInfos() ?: return null
        val rows = decoders(infos, mime)
        val facts = rows.map { (info, caps) -> facts(info, caps, mime, null, false) }
        val best = bestDecoder(facts, Build.VERSION.SDK_INT) ?: return null
        val video = rows.firstOrNull { (info, _) -> info.name == best.name }?.second?.videoCapabilities
        return DecoderChoice(best.name, best.lowLatency, video?.let(::pixelRate) ?: 0)
    }

    private val MIMES = listOf("video/avc", "video/hevc", "video/av01")

    /** The most pixels a second [video] declares: its largest size at the rate it takes there. */
    private fun pixelRate(video: MediaCodecInfo.VideoCapabilities): Long = runCatching {
        val w = video.supportedWidths.upper
        val h = video.getSupportedHeightsFor(w).upper
        (w.toLong() * h * video.getSupportedFrameRatesFor(w, h).upper).toLong()
    }.getOrDefault(0L)

    private fun codecInfos(): Array<MediaCodecInfo>? =
        runCatching { MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos }.getOrNull()

    /**
     * The decoders of [infos] that list [mime], in list order, each with its capabilities for it.
     * `supportedTypes` is the set `getCapabilitiesForType` answers from, which throws on a miss.
     */
    private fun decoders(
        infos: Array<MediaCodecInfo>,
        mime: String,
    ): List<Pair<MediaCodecInfo, CodecCapabilities>> =
        infos.filter { !it.isEncoder && mime in it.supportedTypes }.mapNotNull { info ->
            runCatching { info.getCapabilitiesForType(mime) }.getOrNull()?.let { info to it }
        }

    /** [info]'s facts for [mime]. [need] null leaves size and rate unchecked. */
    private fun facts(
        info: MediaCodecInfo,
        caps: CodecCapabilities,
        mime: String,
        need: DecodeNeed?,
        trustPoints: Boolean,
    ) = DecoderFacts(
        name = info.name,
        mime = mime,
        hardware = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q && info.isHardwareAccelerated,
        profiles = caps.profileLevels.orEmpty().map { it.profile },
        fits = need == null || fits(caps, need, trustPoints),
        lowLatency = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
            runCatching { caps.isFeatureSupported(CodecCapabilities.FEATURE_LowLatency) }
                .getOrDefault(false),
        secureRequired = runCatching {
            caps.isFeatureRequired(CodecCapabilities.FEATURE_SecurePlayback)
        }.getOrDefault(false),
        partialFrame = runCatching {
            caps.isFeatureSupported(CodecCapabilities.FEATURE_PartialFrame)
        }.getOrNull(),
    )

    /**
     * Whether [caps] decodes [need]'s size at its rate: the performance points where listed and
     * trusted, which the vendor guarantees; else the size and rate limits, either way round.
     */
    private fun fits(caps: CodecCapabilities, need: DecodeNeed, trustPoints: Boolean): Boolean {
        val video = caps.videoCapabilities ?: return false
        val (w, h, fps) = need
        if (fps <= 0) return video.isSizeSupported(w, h) || video.isSizeSupported(h, w)
        if (trustPoints && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            val points = video.supportedPerformancePoints
            if (!points.isNullOrEmpty()) return points.any { it.covers(PerformancePoint(w, h, fps)) }
        }
        return video.areSizeAndRateSupported(w, h, fps.toDouble()) ||
            video.areSizeAndRateSupported(h, w, fps.toDouble())
    }

    /**
     * Below API 35 some vendors' performance points leave real modes out. They count only when an
     * H.264 decoder's list covers 720p60, which the CDD requires wherever the display is 720 lines
     * or taller (ExoPlayer checks the same).
     */
    private fun pointsTrusted(infos: Array<MediaCodecInfo>): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.VANILLA_ICE_CREAM) return true
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) return false
        val avc = infos.filter { !it.isEncoder && "video/avc" in it.supportedTypes }
        val listed = avc.firstNotNullOfOrNull { info ->
            runCatching {
                info.getCapabilitiesForType("video/avc").videoCapabilities?.supportedPerformancePoints
            }.getOrNull()?.takeIf { it.isNotEmpty() }
        } ?: return false
        return listed.any { it.covers(PerformancePoint(1280, 720, 60)) }
    }
}

/**
 * One walk of the codec list ([VideoDecoders.snapshot]): a connect's codec bits at its need, and
 * the flags and log line that read the pick of each codec advertised with no mode in hand.
 * [facts] holds each MIME's decoders in list order, [DecoderFacts.fits] read at the need; it is
 * empty, and [listed] false, when the list can't be read. [census] is empty without a need.
 */
class DecoderSnapshot internal constructor(
    facts: List<DecoderFacts>,
    tenBit: Boolean,
    pyrowave: Boolean,
    api: Int,
    vendorApi: Int,
    private val listed: Boolean,
    /** Every decoder for H.264, HEVC and AV1 at the need, one line for the log bundle. */
    val census: String,
) {
    /** The codec bits and 10-bit at the need ([codecCaps]), plus PyroWave (8). */
    val decode: DecodeCaps = codecCaps(facts, tenBit, api).let {
        if (pyrowave) it.copy(codecBits = it.codecBits or 8) else it
    }

    /**
     * Each MIME advertised with no mode in hand, H.264 always, with its pick ([bestDecoder]). A
     * null pick leaves the platform to resolve its default, which can't be inspected.
     */
    internal val picks: List<Pair<String, DecoderFacts?>> = facts.map { it.copy(fits = true) }.let { all ->
        val bits = codecCaps(all, tenBit = false, api).codecBits
        val hevc = "video/hevc".takeIf { bits and 2 != 0 }
        listOfNotNull("video/avc", hevc, "video/av01".takeIf { bits and 4 != 0 })
            .map { mime -> mime to bestDecoder(all.filter { it.mime == mime }, api) }
    }

    /**
     * Whether every pick tolerates multi-slice access units: the `VIDEO_CAP_MULTI_SLICE`
     * advertisement. Amlogic HEVC decoders wedge the whole device on multi-slice frames
     * (Chromecast with Google TV, onn 4K), so an Amlogic pick, or one that can't be inspected,
     * keeps the host on one slice.
     */
    val multiSliceTolerant: Boolean = picks.all { (_, pick) ->
        val name = pick?.name?.lowercase() ?: return@all false
        !name.startsWith("omx.amlogic") && !name.startsWith("c2.amlogic")
    }

    /**
     * Whether this device's display shows full-range BT.709 video as studio range: the
     * `FEATURE_STUDIO_RANGE` ask, so the host sends studio SDR. See [limitedOnlyBt709Hal].
     */
    val showsFullRangeAsStudio: Boolean = picks.any { (_, pick) ->
        pick != null && limitedOnlyBt709Hal(pick.name, vendorApi)
    }

    /**
     * Whether every pick supports partial-frame input (`FEATURE_PartialFrame`): the client may then
     * opt into slice-progressive delivery. The codec is chosen after connect, so all must qualify.
     */
    val partialFrameCapable: Boolean = picks.all { (_, pick) -> pick?.partialFrame == true }

    /**
     * The connect log's readout (`adb logcat -s pf.caps`): each advertised MIME's pick, whether it
     * declares `FEATURE_PartialFrame`, and its latency-named vendor parameters
     * ([VideoDecoders.vendorKeys]), which show whether a `low_latency_key` profile's keys exist.
     */
    fun capsReport(): String {
        if (!listed) return "codec list unavailable"
        return picks.joinToString("  ") { (mime, pick) ->
            "${mime.removePrefix("video/")}=${pick?.name ?: "platform-default"}" +
                " partialFrame=${pick?.partialFrame ?: "?"} vendor=[${VideoDecoders.vendorKeys(pick?.name)}]"
        }
    }
}

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

/** Main10, Main10 HDR10 and Main10 HDR10+: the same values in HEVC and AV1. */
private val TEN_BIT_PROFILES = setOf(0x2, 0x1000, 0x2000)

/**
 * A software decoder by name, for below API 29. The platform and FFmpeg ones carry their
 * prefix, vendors mark theirs `.sw.` or `swvdec`, and a name outside `omx.` / `c2.` is an app's
 * own codec. ChromeOS's `arc.` decoders hand off to the hardware.
 */
internal fun softwareByName(name: String): Boolean {
    val n = name.lowercase()
    if (n.startsWith("arc.")) return false
    return n.startsWith("omx.google.") || n.startsWith("omx.ffmpeg.") || n.startsWith("c2.android.") ||
        ".sw." in n || n.endsWith("swvdec") || !(n.startsWith("omx.") || n.startsWith("c2."))
}

/** `isHardwareAccelerated` from API 29, the name below. */
private fun DecoderFacts.isHardware(api: Int): Boolean =
    if (api >= Build.VERSION_CODES.Q) hardware else !softwareByName(name)

/**
 * Never streamed with: a blocked name, or a DRM twin. A `.secure` decoder needs a secure surface
 * and fails or renders black on a clear stream; its plain twin is in the list too.
 */
private fun DecoderFacts.usable(): Boolean {
    val n = name.lowercase()
    return BLOCKED_PREFIXES.none { n.startsWith(it) } && n !in BLOCKED_EXACT &&
        !n.endsWith(".secure") && !secureRequired
}

/**
 * The decoder to stream with among [decoders]: hardware over software, a SoC vendor over the
 * AOSP fallback, `FEATURE_LowLatency` over none; the list's order breaks ties. `null` when none
 * is usable.
 */
internal fun bestDecoder(decoders: List<DecoderFacts>, api: Int): DecoderFacts? =
    decoders.filter { it.usable() }.maxByOrNull { f ->
        val vendor = VENDOR_PREFIXES.any { f.name.lowercase().startsWith(it) }
        (if (f.isHardware(api)) 100 else 0) + (if (vendor) 40 else 0) + (if (f.lowLatency) 20 else 0)
    }

/**
 * The codec bits to advertise from [decoders]' facts. H.264 (1) is always on; HEVC (2) and AV1
 * (4) need a usable hardware decoder that [DecoderFacts.fits], unless no hardware decoder of any
 * codec fits: there H.264 alone would fare worst. With [tenBit] asked, a codec also needs a
 * Main10 profile, since the host picks the codec before the depth. When no codec has one,
 * 10-bit is dropped and the codecs stand without it.
 */
internal fun codecCaps(decoders: List<DecoderFacts>, tenBit: Boolean, api: Int): DecodeCaps {
    val sized = decoders.any { it.usable() && it.isHardware(api) && it.fits }
    fun bits(deep: Boolean) = listOf("video/hevc" to 2, "video/av01" to 4).sumOf { (mime, bit) ->
        val ok = decoders.any { f ->
            f.mime == mime && f.usable() && f.isHardware(api) && (f.fits || !sized) &&
                (!deep || f.profiles.any { it in TEN_BIT_PROFILES })
        }
        if (ok) bit else 0
    }
    val deep = if (tenBit) bits(true) else 0
    return if (deep != 0) DecodeCaps(1 or deep, true) else DecodeCaps(1 or bits(false), false)
}

/**
 * A Qualcomm display HAL older than kalama's (SM8550) converts every BT.709 YUV layer with the
 * limited matrix whatever its range tag (`SelectCscType`): full-range video shows crushed blacks
 * and clipped highlights. Vendor API 33 approximates the cut; some API-33 HALs (DIVAR) still
 * ignore the range. A Qualcomm decoder marks the HAL's vendor; an unreadable vendor level (0)
 * counts as old.
 */
internal fun limitedOnlyBt709Hal(decoder: String, vendorApi: Int): Boolean {
    val name = decoder.lowercase()
    val qualcomm = name.startsWith("c2.qti") || name.startsWith("omx.qcom")
    return qualcomm && vendorApi < Build.VERSION_CODES.TIRAMISU
}
