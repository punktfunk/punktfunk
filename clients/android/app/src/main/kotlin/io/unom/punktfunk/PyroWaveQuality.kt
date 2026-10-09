package io.unom.punktfunk

import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.roundToInt

/** [Settings.pyrowaveBpp] in the hundredths the dial carries, held inside 0.5…2. */
fun Settings.pyrowaveBppX100(): Int = (pyrowaveBpp.coerceIn(0.5, 2.0) * 100).roundToInt()

/** PyroWave quality as its row reads it. Core prices the rate and words the warning. */
object PyroWaveQuality {
    /** Core's `BPP_FLOOR` to `BPP_MAX`; the slider stops at every tenth between them. */
    val RANGE = 0.5f..2.0f
    const val STEPS = 14

    /** `PUNKTFUNK_IFACE_KIND_*`. */
    private const val KIND_UNKNOWN = 0
    private const val KIND_ETHERNET = 1
    private const val KIND_WIFI = 2
    private const val KIND_OTHER = 3

    /** [bpp] on the nearest tenth inside [RANGE]. */
    fun snapped(bpp: Double): Double = (bpp.coerceIn(0.5, 2.0) * 10).roundToInt() / 10.0

    /** This device's link as `kind to mbps`: core's, with Android's transport filling a gap. */
    fun link(context: Context): Pair<Int, Int> {
        val own = NativeBridge.nativeLocalLink()
        val cm = context.getSystemService(ConnectivityManager::class.java)
        val caps = cm?.getNetworkCapabilities(cm.activeNetwork)
        return filledIn(
            (own?.getOrNull(0) ?: KIND_UNKNOWN) to (own?.getOrNull(1) ?: 0),
            wifi = caps?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true,
            ethernet = caps?.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) == true,
        )
    }

    /** [own] unless core named no kind, which an app barred from `/sys` reads as other: then
     *  Android's transport, speed unknown. */
    fun filledIn(own: Pair<Int, Int>, wifi: Boolean, ethernet: Boolean): Pair<Int, Int> = when {
        own.first != KIND_UNKNOWN && own.first != KIND_OTHER -> own
        wifi -> KIND_WIFI to 0
        ethernet -> KIND_ETHERNET to 0
        else -> own
    }

    /**
     * The caption, `3840×2160 at 120 Hz: 1.6 Gbit/s`, and the warning when [link] is short of
     * that rate. [base] is [effectiveMode]'s answer; render scale applies as the connect applies
     * it. [price] is [NativeBridge.nativePyrowaveQuality].
     */
    fun lines(
        s: Settings,
        base: Triple<Int, Int, Int>,
        link: Pair<Int, Int>,
        price: (Int, Int, Int, Boolean, Int, Int, Int, Int) -> String = NativeBridge::nativePyrowaveQuality,
    ): Pair<String, String?> {
        val (w, h) = RenderScale.apply(base.first, base.second, s.renderScale, RenderScale.maxDimension(s.codec))
        val hz = base.third
        // No 4:4:4 on Android; 10 bits follow the switches that ask for them.
        val depth = if (s.hdrEnabled || s.tenBitSdr) 10 else 8
        val out = price(w, h, hz, false, depth, s.pyrowaveBppX100(), link.first, link.second)
        val warning = out.substringAfter('\n', "")
        return "$w×$h at $hz Hz: ${out.substringBefore('\n')}" to warning.ifEmpty { null }
    }
}
