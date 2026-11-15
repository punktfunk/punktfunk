package io.unom.punktfunk

import android.content.res.Resources
import android.os.SystemClock
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.roundToInt

/**
 * The wire the touch gestures drive: [NativeTouchSink] in-stream, [DroppedTouchSink] under
 * [TouchMode.OFF], and a recorder so the gesture machine in `TouchInput.kt` runs on the JVM.
 */
internal interface TouchSink {
    /** Relative mouse motion (screen +y down). */
    fun pointerMove(dx: Int, dy: Int)

    /** Absolute cursor position in a [w]×[h] pixel space. */
    fun pointerAbs(x: Int, y: Int, w: Int, h: Int)

    /** One button transition: 1 = left, 2 = middle, 3 = right. */
    fun button(button: Int, down: Boolean)

    /** One normalized scroll step: [axis] 0 = vertical, 1 = horizontal; [delta] signed Q24.8 in
     *  the [source]'s unit (v120 wheel, DIP distance); [source]/[phase] are ScrollWire bytes. */
    fun scroll(axis: Int, delta: Int, source: Int, phase: Int)

    /** One touchscreen contact transition: kind 0 = down, 1 = move, 2 = up. */
    fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int)
}

/** [TouchMode.OFF]: the gesture machine still runs for the ring and the HUD; the host gets nothing. */
internal object DroppedTouchSink : TouchSink {
    override fun pointerMove(dx: Int, dy: Int) = Unit
    override fun pointerAbs(x: Int, y: Int, w: Int, h: Int) = Unit
    override fun button(button: Int, down: Boolean) = Unit
    override fun scroll(axis: Int, delta: Int, source: Int, phase: Int) = Unit
    override fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int) = Unit
}

/**
 * Frame mode's wire filter (design/touchpad-passthrough.md §4): the host's touchpad derives
 * pointer motion and scroll from the contacts, so only the gesture machine's clicks go, and only
 * toward a host that does not click on taps itself. With [framing] off everything goes, as
 * without frames; a release goes only for a press that went.
 */
internal class FrameModeSink(private val wire: TouchSink, private val hostTaps: Boolean) : TouchSink {
    var framing = true
    private val held = mutableSetOf<Int>()

    override fun pointerMove(dx: Int, dy: Int) {
        if (!framing) wire.pointerMove(dx, dy)
    }
    override fun pointerAbs(x: Int, y: Int, w: Int, h: Int) {
        if (!framing) wire.pointerAbs(x, y, w, h)
    }
    override fun button(button: Int, down: Boolean) {
        if (framing && hostTaps) return
        if (down) held += button else if (!held.remove(button)) return
        wire.button(button, down)
    }
    override fun scroll(axis: Int, delta: Int, source: Int, phase: Int) {
        if (!framing) wire.scroll(axis, delta, source, phase)
    }
    override fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int) {
        if (!framing) wire.touch(id, kind, x, y, w, h)
    }
}

/**
 * The host's touchpad as core answers it at one read. Core answers no while the player's
 * invert-scroll is on, since the host's touchpad scrolls by the host's own setting; each read is
 * a new decision, so a source that saw a refusal tries again only after the next read.
 */
internal class HostTouchpad(val presents: Boolean, val taps: Boolean) {
    companion object {
        val NONE = HostTouchpad(presents = false, taps = false)

        fun read(handle: Long): HostTouchpad {
            val presents = NativeBridge.nativeHostPresentsTouchpad(handle)
            return HostTouchpad(presents, presents && NativeBridge.nativeHostTapsClick(handle))
        }
    }
}

/** The stream view as the host's touchpad: every finger on it, on every change. */
internal interface TouchpadFrames {
    /** The host OS clicks on taps, so the gesture machine's clicks stay home. */
    val hostTaps: Boolean

    /**
     * Every finger down now: the first [count] [ids] with [xs]/[ys] in 0…1 of a [w]×[h] px view,
     * origin top-left. [keep] while a local gesture owns them; [timeUs] is their capture time on
     * the uptime clock. `false`: core refused it, and the view takes its path without frames.
     */
    fun surface(count: Int, ids: LongArray, xs: FloatArray, ys: FloatArray, keep: Boolean, timeUs: Long, w: Int, h: Int): Boolean

    /** No finger down: announces the surface when frame mode starts and lifts every contact when it ends. */
    fun idle(w: Int, h: Int) = surface(0, NO_IDS, NO_XY, NO_XY, false, SystemClock.uptimeMillis() * 1000, w, h)
}

private val NO_IDS = LongArray(0)
private val NO_XY = FloatArray(0)

/** Contacts a touchscreen forwards; a third finger stays the client's (stats, keyboard). */
private const val SCREEN_MAX_CONTACTS = 2

/**
 * [px] at [dpi] in tenths of a millimetre, `0` for no size. A [dpi] more than 2× off the
 * density bucket [bucketDpi] is a placeholder, so the bucket stands in for it.
 */
internal fun pxToDmm(px: Int, dpi: Float, bucketDpi: Int): Int {
    val real = dpi.takeIf { it >= bucketDpi / 2f && it <= bucketDpi * 2f } ?: bucketDpi.toFloat()
    if (px <= 0 || real <= 0f) return 0
    return (px * 254f / real).roundToInt().coerceAtMost(65535)
}

/**
 * Frame mode toward the live session. The view's size comes from `DisplayMetrics.xdpi`/`ydpi`,
 * the panel's physical density; `densityDpi` is a rounded bucket. [onPointer] hears every
 * update with a finger down, as [NativeTouchSink]'s does.
 */
internal class NativeTouchpadFrames(
    private val handle: Long,
    override val hostTaps: Boolean,
    private val resources: Resources,
    private val onPointer: () -> Unit = {},
) : TouchpadFrames {
    override fun surface(count: Int, ids: LongArray, xs: FloatArray, ys: FloatArray, keep: Boolean, timeUs: Long, w: Int, h: Int): Boolean {
        if (count > 0) onPointer()
        val m = resources.displayMetrics
        return NativeBridge.nativeTouchpadSurface(
            handle, ids, xs, ys, count, SCREEN_MAX_CONTACTS,
            button = false, keep = keep, retractTaps = !hostTaps,
            widthDmm = pxToDmm(w, m.xdpi, m.densityDpi), heightDmm = pxToDmm(h, m.ydpi, m.densityDpi),
            timeUs = timeUs,
        ) != NativeBridge.SURFACE_REFUSED
    }
}

/** [onPointer] hears every pointer move, so a client-drawn host cursor hands back at once. */
internal class NativeTouchSink(private val handle: Long, private val onPointer: () -> Unit = {}) : TouchSink {
    override fun pointerMove(dx: Int, dy: Int) {
        onPointer()
        NativeBridge.nativeSendPointerMove(handle, dx, dy)
    }
    override fun pointerAbs(x: Int, y: Int, w: Int, h: Int) {
        onPointer()
        NativeBridge.nativeSendPointerAbs(handle, x, y, w, h)
    }
    override fun button(button: Int, down: Boolean) = NativeBridge.nativeSendPointerButton(handle, button, down)
    override fun scroll(axis: Int, delta: Int, source: Int, phase: Int) =
        NativeBridge.nativeSendNormalizedScroll(handle, axis, delta, source, phase)
    override fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int) =
        NativeBridge.nativeSendTouch(handle, id, kind, x, y, w, h)
}
