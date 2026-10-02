package io.unom.punktfunk

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.log10
import kotlin.math.pow
import kotlin.math.sin

/*
 * The numbers behind the companion panel's graphs (design/android-dual-screen.md §6): one
 * `StatsSample` per formatted window, as `nativeVideoStatsSample` reports it, kept for the last
 * `StatsHistory.capacity` seconds. The lines and the graphs read the same window.
 */

/** One second of the stream. A figure this platform does not have reads as its neighbour or 0. */
internal class StatsSample(private val v: FloatArray) {
    private fun at(i: Int): Float = v.getOrElse(i) { -1f }
    private fun known(i: Int): Float? = at(i).takeIf { it >= 0f }

    val receivedFps: Float get() = at(NativeBridge.STAT_RECEIVED_FPS).coerceAtLeast(0f)

    /** Frames on glass; decoded, then received, stand in where a platform cannot count presents. */
    val presentedFps: Float
        get() = known(NativeBridge.STAT_PRESENTED_FPS) ?: known(NativeBridge.STAT_DECODED_FPS) ?: receivedFps
    val mbps: Float get() = at(NativeBridge.STAT_MBPS).coerceAtLeast(0f)
    val targetMbps: Float get() = at(NativeBridge.STAT_TARGET_MBPS).coerceAtLeast(0f)

    /** Capture → glass, less the OS present pipeline the HUD also leaves out. */
    val e2eMs: Float get() = (at(NativeBridge.STAT_E2E_P50_MS) - at(NativeBridge.STAT_OS_FLOOR_MS).coerceAtLeast(0f)).coerceAtLeast(0f)
    val e2eP95Ms: Float get() = (at(NativeBridge.STAT_E2E_P95_MS) - at(NativeBridge.STAT_OS_FLOOR_MS).coerceAtLeast(0f)).coerceAtLeast(e2eMs)
    val hostMs: Float get() = at(NativeBridge.STAT_HOST_MS).coerceAtLeast(0f)
    val netMs: Float get() = at(NativeBridge.STAT_NET_MS).coerceAtLeast(0f)
    val decodeMs: Float get() = at(NativeBridge.STAT_DECODE_MS).coerceAtLeast(0f)
    val displayMs: Float get() = at(NativeBridge.STAT_DISPLAY_MS).coerceAtLeast(0f)
    val lost: Int get() = at(NativeBridge.STAT_LOST).coerceAtLeast(0f).toInt()
    val skipped: Int get() = at(NativeBridge.STAT_SKIPPED).coerceAtLeast(0f).toInt()
    val rttMs: Float? get() = known(NativeBridge.STAT_RTT_MS)
    val audioBufferMs: Float get() = at(NativeBridge.STAT_AUDIO_BUFFER_MS).coerceAtLeast(0f)
    val judderPermille: Int get() = at(NativeBridge.STAT_JUDDER_PERMILLE).coerceAtLeast(0f).toInt()
    val refreshHz: Int get() = at(NativeBridge.STAT_REFRESH_HZ).coerceAtLeast(0f).toInt()

    /** Lost, as a share of what the window should have had. */
    val lostPct: Float
        get() {
            val had = receivedFps * at(NativeBridge.STAT_WINDOW_MS).coerceAtLeast(1f) / 1000f + lost
            return if (had <= 0f) 0f else lost * 100f / had
        }

    companion object {
        /** A sample from the bridge, or null for one too short to be the shape this build knows. */
        fun of(v: FloatArray?): StatsSample? = v?.takeIf { it.size >= NativeBridge.STAT_COUNT }?.let(::StatsSample)
    }
}

/** The last [capacity] samples, oldest first. [version] steps on every push, so a reader recomposes. */
internal class StatsHistory(val capacity: Int = 90) {
    private val ring = ArrayDeque<StatsSample>()
    var version by mutableIntStateOf(0)
        private set

    val size: Int get() = ring.size
    val latest: StatsSample? get() = ring.lastOrNull()
    val samples: List<StatsSample> get() = ring.toList()

    fun push(sample: StatsSample) {
        ring.addLast(sample)
        while (ring.size > capacity) ring.removeFirst()
        version++
    }

    fun clear() {
        ring.clear()
        version++
    }

    /** One figure per sample, oldest first. */
    fun series(pick: (StatsSample) -> Float): FloatArray = FloatArray(ring.size) { pick(ring[it]) }

    companion object {
        /**
         * A plausible minute for previews and screenshots: a 120 Hz stream with one short dip —
         * deterministic, so a capture never moves.
         */
        fun demo(seconds: Int = 60, refreshHz: Int = 120): StatsHistory {
            val h = StatsHistory()
            for (t in 0 until seconds) {
                val dip = if (t in 38..41) 1f - 0.3f * (1f - (t - 38) / 4f) else 1f
                val v = FloatArray(NativeBridge.STAT_COUNT) { 0f }
                v[NativeBridge.STAT_WINDOW_MS] = 1000f
                v[NativeBridge.STAT_RECEIVED_FPS] = refreshHz - 1f + (if (t % 7 == 0) 1f else 0f)
                v[NativeBridge.STAT_DECODED_FPS] = v[NativeBridge.STAT_RECEIVED_FPS]
                v[NativeBridge.STAT_PRESENTED_FPS] = (refreshHz - 2f) * dip
                v[NativeBridge.STAT_MBPS] = 92f + 4f * sin(t / 5f) - (if (dip < 1f) 30f else 0f)
                v[NativeBridge.STAT_TARGET_MBPS] = 100f
                v[NativeBridge.STAT_E2E_P50_MS] = 18f + 1.5f * sin(t / 3f) + (if (dip < 1f) 9f else 0f)
                v[NativeBridge.STAT_E2E_P95_MS] = v[NativeBridge.STAT_E2E_P50_MS] + 5f + (if (dip < 1f) 8f else 0f)
                v[NativeBridge.STAT_HOST_MS] = 4.2f
                v[NativeBridge.STAT_NET_MS] = 3.1f
                v[NativeBridge.STAT_DECODE_MS] = 2.4f
                v[NativeBridge.STAT_DISPLAY_MS] = 8.3f
                v[NativeBridge.STAT_LOST] = if (t == 39) 2f else 0f
                v[NativeBridge.STAT_SKIPPED] = if (dip < 1f) 1f else 0f
                v[NativeBridge.STAT_RTT_MS] = 1.2f
                v[NativeBridge.STAT_AUDIO_BUFFER_MS] = 28f
                v[NativeBridge.STAT_AV_OFFSET_MS] = 4f
                v[NativeBridge.STAT_REFRESH_HZ] = refreshHz.toFloat()
                v[NativeBridge.STAT_WIDTH] = 1920f
                v[NativeBridge.STAT_HEIGHT] = 1080f
                v[NativeBridge.STAT_E2E_SAMPLES] = v[NativeBridge.STAT_PRESENTED_FPS]
                h.push(StatsSample(v))
            }
            return h
        }
    }
}

/** The axis top for a series peaking at [max]: 1, 2 or 5 × 10ⁿ at or above it, never below 1. */
internal fun niceCeiling(max: Float): Float {
    if (max.isNaN() || max <= 1f) return 1f
    val exp = floor(log10(max))
    val base = 10f.pow(exp)
    val m = max / base
    val step = when {
        m <= 1f -> 1f
        m <= 2f -> 2f
        m <= 5f -> 5f
        else -> 10f
    }
    return step * base
}

/** [max] rounded up to a whole multiple of [step]. */
internal fun ceilTo(max: Float, step: Float): Float = ceil(max / step) * step
