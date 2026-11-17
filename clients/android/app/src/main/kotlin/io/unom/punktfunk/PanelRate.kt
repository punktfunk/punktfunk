package io.unom.punktfunk

import android.content.Context
import android.view.Choreographer
import android.view.Display
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.abs
import kotlin.math.roundToInt
import kotlin.math.roundToLong

/**
 * A display mode's refresh rate as the panel keeps it, read off the choreographer's frame times.
 * A mode table states a rate and a panel can run a fraction beside it: 59.88 Hz under a stated
 * 60.000. A stream at the stated rate then gains a frame on the panel every few seconds.
 *
 * A reading is kept across launches, so a connect right after one has it.
 */
internal object PanelRate {
    /** One measurement window: two halves of three seconds read a steady panel alike. */
    private const val WINDOW_NS = 6_000_000_000L

    /** Windows one measurement tries. The first after a launch holds the unsettled start. */
    private const val WINDOWS = 4
    private const val PREFS = "panel_rate"

    private var measuring = false

    /** The modes read since this process started. */
    private val read = HashSet<String>()

    private fun stated(display: Display): Int = (display.mode.refreshRate * 1000f).roundToInt()

    private fun key(display: Display): String =
        "${display.displayId}:${display.mode.modeId}:${stated(display)}"

    /**
     * The rate measured in [display]'s current mode, in millihertz, where it sits beside the
     * stated one ([besideStated]). Null where the stated rate holds or nothing was read.
     */
    fun of(context: Context, display: Display): Int? =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getInt(key(display), 0)
            .takeIf { it > 0 && besideStated(it, stated(display)) }

    /**
     * Measure [display]'s current mode, once a process. Main thread: the frame times come from
     * this thread's choreographer, and nothing is drawn for them.
     */
    fun measure(context: Context, display: Display) {
        val key = key(display)
        if (measuring || key in read) return
        measuring = true
        val prefs = context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val stated = stated(display)
        val times = ArrayList<Long>()
        var windows = WINDOWS
        val choreographer = Choreographer.getInstance()
        choreographer.postFrameCallback(object : Choreographer.FrameCallback {
            override fun doFrame(frameTimeNanos: Long) {
                times.add(frameTimeNanos)
                if (frameTimeNanos - times[0] < WINDOW_NS) {
                    choreographer.postFrameCallback(this)
                    return
                }
                // A mode switch inside the window measured two modes: that ends the measurement.
                val sameMode = key(display) == key
                val rate = if (sameMode) measuredMilliHz(times.toLongArray(), stated) else null
                runCatching { NativeBridge.nativeLogDisplay("panel rate ${rate ?: "unread"} mHz, mode states $stated") }
                if (rate == null && sameMode && --windows > 0) {
                    times.clear()
                    choreographer.postFrameCallback(this)
                    return
                }
                measuring = false
                if (rate != null) {
                    read.add(key)
                    prefs.edit().putInt(key, rate).apply()
                }
            }
        })
    }
}

/**
 * Whether a mode that states [statedMilliHz] and measured [measuredMilliHz] runs beside its
 * stated rate. Two clocks differ by up to 200 ppm, so a reading that close is the stated rate:
 * a healthy panel never asks a host for 60.002 Hz.
 */
internal fun besideStated(measuredMilliHz: Int, statedMilliHz: Int): Boolean =
    abs(measuredMilliHz - statedMilliHz) * 5000L > statedMilliHz

/**
 * The refresh rate in millihertz that [frameTimesNs] ran at, or null when they do not settle on
 * one near [statedMilliHz]. A gap between two callbacks counts as the whole periods it spans, so
 * a missed callback, or an app the system serves every second refresh, still reads the panel.
 * The period is the least-squares fit over every sample: frame times jitter by tens of
 * microseconds, which two end points would carry into the rate. The two halves of the window
 * must agree to 100 ppm, and the result must sit within 1 % of the stated rate: past that it is
 * another mode, not the same one read exactly.
 */
internal fun measuredMilliHz(frameTimesNs: LongArray, statedMilliHz: Int): Int? {
    if (frameTimesNs.isEmpty() || statedMilliHz <= 0) return null
    val nominalNs = 1e12 / statedMilliHz
    // The frame times that are a period or more apart, and how many periods lie before each.
    // Two callbacks inside one period are one sample.
    val times = ArrayList<Long>(frameTimesNs.size)
    val periods = ArrayList<Long>(frameTimesNs.size)
    times.add(frameTimesNs[0])
    periods.add(0L)
    for (i in 1 until frameTimesNs.size) {
        val apart = ((frameTimesNs[i] - times.last()) / nominalNs).roundToLong()
        if (apart < 1) continue
        // A rate known to 1 % counts a gap to a quarter period only this far.
        if (apart > 25) return null
        times.add(frameTimesNs[i])
        periods.add(periods.last() + apart)
    }
    if (times.size < 60) return null
    // Millihertz of the stretch [from, to].
    fun rate(from: Int, to: Int): Double {
        val n = to - from + 1
        var sumX = 0.0
        var sumY = 0.0
        for (i in from..to) {
            sumX += periods[i]
            sumY += times[i] - times[from]
        }
        var xy = 0.0
        var xx = 0.0
        for (i in from..to) {
            val dx = periods[i] - sumX / n
            xy += dx * (times[i] - times[from] - sumY / n)
            xx += dx * dx
        }
        return 1e12 * xx / xy
    }
    val last = times.size - 1
    val whole = rate(0, last)
    val near = abs(whole - statedMilliHz) <= statedMilliHz / 100.0
    val steady = abs(rate(0, last / 2) - rate(last / 2, last)) <= whole / 10_000.0
    return if (near && steady) whole.roundToInt() else null
}
