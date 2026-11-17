package io.unom.punktfunk

import android.os.Build
import android.os.SystemClock
import android.view.InputDevice
import android.view.MotionEvent
import android.view.View
import androidx.annotation.RequiresApi
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.isExternalDevice
import kotlin.math.roundToInt

/** How long after a mouse Back edge a system Back is that button's own echo. */
private const val BACK_ECHO_MS = 300L

/** How long after the capture-engaging DOWN its own BUTTON_PRESS may still arrive. */
private const val ENGAGE_PRESS_MS = 100L

/** True when any connected input device is a pointer (USB/BT mouse, or a touchpad driving one). */
fun hasPhysicalMouse(): Boolean = InputDevice.getDeviceIds().any { id ->
    InputDevice.getDevice(id)?.supportsSource(InputDevice.SOURCE_MOUSE) == true
}

/** True when a full keyboard is attached — not the box's own buttons. */
fun hasPhysicalKeyboard(): Boolean = InputDevice.getDeviceIds().any { id ->
    InputDevice.getDevice(id)?.let {
        it.keyboardType == InputDevice.KEYBOARD_TYPE_ALPHABETIC && it.isExternalDevice()
    } == true
}

/**
 * Whether a BACK/FORWARD key is a mouse side button, sent to the host as X1/X2, rather than the
 * quick-action ring's Back. Plain facts in, so the rule is tested ([MouseSideKeyTest]).
 *
 * Android gives a mouse's consumer-page Back the same device shape as a remote's, so off a TV
 * nothing is guessed: the Back gesture, the twist and Ctrl+Alt+Shift+O open the ring, and every
 * external non-pad Back belongs to the host. On a TV the remote's Back is the way in, so only a
 * mouse with no D-pad claims it — an air-mouse remote keeps its Back.
 *
 * A side button the system maps to Back (One UI 8 does, with no setting) never reaches us as a
 * button: the Back key arrives from a device that is not external, like the nav bar's. From
 * Android 16 the Back gesture takes the predictive dispatch path and is no key at all, so with a
 * mouse attached a Back from no external device is the mouse's. Before that ([gestureIsKey]) the
 * gesture and the nav bar send that same key, and it stays the ring's.
 */
fun isMouseSideKey(
    tv: Boolean,
    external: Boolean,
    pad: Boolean,
    fallback: Boolean,
    mouse: Boolean,
    dpad: Boolean,
    mousePresent: Boolean = false,
    gestureIsKey: Boolean = false,
): Boolean = when {
    fallback || pad -> false
    !external -> mousePresent && !tv && !gestureIsKey
    !tv -> true
    else -> mouse && !dpad
}

/**
 * Wire scroll source for a MotionEvent's source bitmask (the `isFromSource` shape): a touchpad
 * measures distance → [ScrollWire.SOURCE_FINGER]; a wheel counts detents →
 * [ScrollWire.SOURCE_WHEEL], with SOURCE_MOUSE_RELATIVE alongside since a captured mouse reports
 * it. The touchpad check goes first; anything else is Unknown, priced as a wheel.
 */
internal fun wireScrollSource(motionSource: Int): Int = when {
    motionSource and InputDevice.SOURCE_TOUCHPAD == InputDevice.SOURCE_TOUCHPAD ->
        ScrollWire.SOURCE_FINGER
    motionSource and InputDevice.SOURCE_MOUSE == InputDevice.SOURCE_MOUSE ||
        motionSource and InputDevice.SOURCE_MOUSE_RELATIVE == InputDevice.SOURCE_MOUSE_RELATIVE ->
        ScrollWire.SOURCE_WHEEL
    else -> ScrollWire.SOURCE_UNKNOWN
}

/**
 * Whether a touch-stream DOWN opens a touchpad scroll, not a click. ChromeOS reports a touchpad's
 * two-finger scroll as a one-pointer mouse drag that holds no button; a click-drag always holds one.
 */
internal fun isTouchpadScrollDown(ev: MotionEvent): Boolean =
    ev.actionMasked == MotionEvent.ACTION_DOWN &&
        ev.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE &&
        ev.buttonState == 0

/**
 * Physical mouse → wire, in two modes (the iPadOS/desktop model):
 *  * **uncaptured** (default): hover/drag positions forward as absolute cursor moves
 *    (`MouseMoveAbs`, host-normalized against the window size) — desktop-style pointing. The
 *    local pointer wears the host's shape when the host forwards its cursor ([HostCursor]);
 *    otherwise it is hidden and the host's cursor in the video is the one you see.
 *  * **captured**: the OS pointer is grabbed ([android.view.View.requestPointerCapture]) and raw
 *    relative deltas forward as `MouseMove` — FPS mouse-look. Engaged at stream start / by
 *    clicking into the stream when the mouse mode is [MouseMode.CAPTURE], and toggled any time
 *    by Ctrl+Alt+Shift+Q (the default Release input chord). Focus loss releases it (the OS
 *    guarantees that); a click re-engages.
 *
 * Buttons ride [MotionEvent.ACTION_BUTTON_PRESS]/RELEASE edges (left/middle/right/back/forward →
 * wire 1/2/3/4/5), the wheel rides [MotionEvent.ACTION_SCROLL] through [ScrollNormalizer] — a
 * touchpad's measured distance goes out as Finger DIP, a detent-counted wheel as v120, and the
 * unsent fraction rides the normalizer so high-resolution wheels don't lose sub-notch travel.
 * Held buttons are tracked and flushed on capture loss / stream exit so nothing sticks on the
 * host. Events reach this class from MainActivity's dispatch overrides (uncaptured) and the
 * capture view's captured-pointer listener.
 */
class MouseForwarder internal constructor(
    private val handle: Long,
    private val captureWanted: Boolean,
    /** `ViewConfiguration.scaledVerticalScrollFactor` — axis units → pixels for a touchpad. */
    private val scrollFactorV: Float,
    /** `ViewConfiguration.scaledHorizontalScrollFactor` — same, horizontal. */
    private val scrollFactorH: Float,
    /** Display density (px per DIP), pricing a touchpad's distance in device-independent units. */
    private val density: Float,
    /**
     * Window point → `[x, y, frameWidth, frameHeight, onPicture]` on the letterboxed picture, with
     * `onPicture` 1 or 0: a point on a bar clamps to the picture's edge. `null` before layout.
     */
    private val frameAt: (Float, Float) -> IntArray?,
    /** The host's touchpad as last read ([StreamUi.touchpad]): while it presents, a captured
     *  touchpad sends it every contact. Read as each gesture starts. */
    private val touchpad: () -> HostTouchpad = { HostTouchpad.NONE },
) {
    /** Capture plumbing, owned by StreamScreen (the focusable capture view). */
    var onRequestCapture: (() -> Unit)? = null
    var onReleaseCapture: (() -> Unit)? = null

    /** Where an absolute move put the host pointer, in frame pixels, and whether the mouse is on
     *  the picture ([HostCursor.mouseAt]). */
    var onAbs: ((Int, Int, Boolean) -> Unit)? = null

    /**
     * Whether this session's access includes the POINTER grant ([io.unom.punktfunk.kit.SessionAccess.POINTER])
     * — seeded from the Welcome, kept live by StreamScreen's access poll. Without it the mouse
     * path goes inert: nothing forwards, and — the part that matters — the pointer is never
     * GRABBED, because a captured mouse that moves nothing is the "my mouse does nothing and
     * nobody says why" failure the grants UX exists to prevent (the Access chip says why
     * instead). Revocation mid-session releases an existing grab (StreamScreen calls [release]).
     * Volatile: set on the main thread, read wherever the dispatch path runs.
     */
    @Volatile
    var pointerGranted: Boolean = true

    /** Live capture state, updated from [android.app.Activity.onPointerCaptureChanged]. */
    @Volatile
    var captured = false
        private set

    /** Chord-released: no auto re-engage (start / click) until the user opts back in. */
    private var userReleased = false

    /** The quick-action ring is up: the mouse is the ring's, uncaptured, and forwards nothing. */
    private var suspended = false
    private var regrabAfterSuspend = false

    private val heldButtons = mutableSetOf<Int>()
    private val scrollNorm = ScrollNormalizer()
    private var moveAccX = 0f
    private var moveAccY = 0f

    /** A ChromeOS touchpad scroll is open ([isTouchpadScrollDown]); its drag was last here. */
    private var dragScroll = false
    private var dragX = 0f
    private var dragY = 0f

    /**
     * Uncaptured mouse events on the TOUCH stream: position while a button is down, and a
     * touchpad's two-finger scroll where the platform reports it as a drag.
     */
    fun onTouchEvent(ev: MotionEvent): Boolean {
        if (suspended) return false // the ring's clickable slots take it
        if (!pointerGranted) return true // inert: consumed over the stream, nothing forwards
        // An Android 14+ touchpad reports its gestures as a fake finger on this stream: two-finger
        // scroll carries its distance on the gesture axes, a pinch is not the host's. Neither is
        // pointer motion, and neither engages capture.
        if (Build.VERSION.SDK_INT >= 34) {
            when (ev.classification) {
                MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE -> {
                    gestureScroll(ev)
                    return true
                }
                MotionEvent.CLASSIFICATION_PINCH -> return true
            }
        }
        // Each DOWN decides whether its whole stream is a scroll; the scroll never engages capture.
        if (ev.actionMasked == MotionEvent.ACTION_DOWN) dragScroll = isTouchpadScrollDown(ev)
        if (dragScroll) {
            when (ev.actionMasked) {
                MotionEvent.ACTION_MOVE -> scrollNorm.travel(ev.x - dragX, ev.y - dragY, density).forEach(::sendScroll)
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> dragScroll = false
            }
            dragX = ev.x
            dragY = ev.y
            return true
        }
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                if (captureWanted && !captured && !userReleased) {
                    // The engaging click: grab the pointer and swallow the click (desktop
                    // parity — the click that captures never reaches the host). Its
                    // BUTTON_PRESS follows as its own event and is dropped in [press]; the
                    // paired BUTTON_RELEASE falls to the held-set guard.
                    engageClickAt = SystemClock.uptimeMillis()
                    onRequestCapture?.invoke()
                    return true
                }
                sendAbs(ev)
            }
            MotionEvent.ACTION_MOVE -> sendAbs(ev)
            // Button edges are documented on the generic stream, but be robust to either.
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
        }
        return true
    }

    /** Uncaptured mouse events on the GENERIC stream (hover motion, wheel, button edges). */
    fun onGenericMotion(ev: MotionEvent): Boolean {
        if (suspended) return false
        if (!pointerGranted) return true // inert: consumed over the stream, nothing forwards
        when (ev.actionMasked) {
            MotionEvent.ACTION_HOVER_MOVE -> sendAbs(ev)
            MotionEvent.ACTION_SCROLL -> wheel(ev)
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_EXIT -> {}
            else -> return false
        }
        return true
    }

    /**
     * Captured-pointer events (the view holds [android.view.View.requestPointerCapture]). A mouse
     * reports [InputDevice.SOURCE_MOUSE_RELATIVE], its x/y ARE the deltas, and DOWN/UP carry that
     * report's motion too. A touchpad reports [InputDevice.SOURCE_TOUCHPAD] with absolute finger
     * positions: toward a host with a touchpad of its own every contact goes there ([padFrame]),
     * else one finger moves by its [MotionEvent.AXIS_RELATIVE_X]/Y and its clicks are buttons.
     * Batched samples are included either way.
     */
    fun onCapturedPointer(ev: MotionEvent): Boolean {
        // A revocation or the ring is racing the release of the grab.
        if (!pointerGranted || suspended) return true
        if (ev.isFromSource(InputDevice.SOURCE_TOUCHPAD)) {
            if (padFrame(ev)) return true
            when (ev.actionMasked) {
                MotionEvent.ACTION_SCROLL -> wheel(ev)
                MotionEvent.ACTION_MOVE -> if (ev.pointerCount == 1) {
                    moveBy(ev, MotionEvent.AXIS_RELATIVE_X, MotionEvent.AXIS_RELATIVE_Y)
                }
                MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
                MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
            }
            return true
        }
        if (!ev.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)) return false
        when (ev.actionMasked) {
            MotionEvent.ACTION_MOVE, MotionEvent.ACTION_DOWN, MotionEvent.ACTION_UP ->
                moveBy(ev, MotionEvent.AXIS_X, MotionEvent.AXIS_Y, counts = true)
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
            MotionEvent.ACTION_SCROLL -> wheel(ev)
        }
        return true
    }

    /** This gesture's pad axes when its contacts go to the host's touchpad; set on its first
     *  finger, cleared on the last lift. */
    private var pad: PadAxes? = null
    private var padW = 0
    private var padH = 0
    private var padHeld = false
    /** The decision core refused an update under; its gestures take the path without frames. */
    private var padRefused: HostTouchpad? = null
    private val padIds = LongArray(PAD_MAX_TOUCHES)
    private val padXs = FloatArray(PAD_MAX_TOUCHES)
    private val padYs = FloatArray(PAD_MAX_TOUCHES)

    /** A captured touchpad's contacts go to the host's touchpad now. */
    private fun padOn(): Boolean = touchpad().let { it.presents && it !== padRefused }

    /**
     * A captured touchpad's event as the host's touchpad (design/touchpad-passthrough.md §4):
     * every finger, the pad's press as the frame's button, batched samples at their own times.
     * `false` for a gesture that began without frames or the device's ranges, and from a refused
     * update on: the path above takes the event.
     */
    private fun padFrame(ev: MotionEvent): Boolean {
        if (ev.actionMasked == MotionEvent.ACTION_DOWN) pad = if (padOn()) padAxes(ev.device) else null
        val axes = pad ?: return false
        padW = axes.widthDmm
        padH = axes.heightDmm
        val button = ev.buttonState and MotionEvent.BUTTON_PRIMARY != 0
        // pos: a batched sample, or historySize for the event itself; skip: a pointer lifting.
        fun send(pos: Int, skip: Int = -1, keep: Boolean = false): Boolean {
            val now = pos == ev.historySize
            var n = 0
            for (i in 0 until ev.pointerCount) {
                if (i == skip || n == PAD_MAX_TOUCHES) continue
                padIds[n] = ev.getPointerId(i).toLong()
                padXs[n] = ((if (now) ev.getX(i) else ev.getHistoricalX(i, pos)) - axes.x0) / axes.w
                padYs[n] = ((if (now) ev.getY(i) else ev.getHistoricalY(i, pos)) - axes.y0) / axes.h
                n++
            }
            return padSurface(n, button, keep, (if (now) ev.eventTime else ev.getHistoricalEventTime(pos)) * 1000)
        }
        val sent = when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN, MotionEvent.ACTION_MOVE,
            MotionEvent.ACTION_BUTTON_PRESS, MotionEvent.ACTION_BUTTON_RELEASE,
            -> (0..ev.historySize).all { send(it) }
            MotionEvent.ACTION_POINTER_UP -> send(ev.historySize, skip = ev.actionIndex)
            MotionEvent.ACTION_UP -> padSurface(0, button, false, ev.eventTime * 1000).also { pad = null }
            // Android cancels a contact it took for a palm: taken back, so the host never taps on it.
            MotionEvent.ACTION_CANCEL -> (send(ev.historySize, keep = true) && padSurface(0, false, false, ev.eventTime * 1000))
                .also { pad = null }
            else -> true
        }
        if (!sent) regrab() // the grab drops back to the mode without frames
        return sent
    }

    /** One update to the host's touchpad. `false`: core refused it, and frames stop until the
     *  host's touchpad is read anew. */
    private fun padSurface(count: Int, button: Boolean, keep: Boolean, timeUs: Long): Boolean {
        padHeld = count > 0 || button
        val answer = NativeBridge.nativeTouchpadSurface(
            handle, padIds, padXs, padYs, count, PAD_MAX_CONTACTS, button, keep,
            retractTaps = false, widthDmm = padW, heightDmm = padH, timeUs = timeUs,
        )
        if (answer != NativeBridge.SURFACE_REFUSED) return true
        padRefused = touchpad()
        pad = null
        padHeld = false
        return false
    }

    /** The host's touchpad was read anew ([StreamUi.touchpad]). Contacts already sent are core's
     *  to lift; a grab takes the mode that matches, and announces the touchpad when it is on. */
    fun touchpadChanged() {
        pad = null
        padHeld = false
        if (!captured) return
        regrab()
        announcePad()
    }

    /** From API 37 the grab's mode decides what a touchpad reports, so a held grab asks again;
     *  a grab the ring holds released asks when the ring closes. */
    private fun regrab() {
        if (captured && !suspended && Build.VERSION.SDK_INT >= 37) onRequestCapture?.invoke()
    }

    /** With a touchpad attached, the host's touchpad is ready before the first finger lands. */
    private fun announcePad() {
        if (padOn() && hasTouchpad()) padSurface(0, false, false, SystemClock.uptimeMillis() * 1000)
    }

    /** Relative motion off `ev`'s [axisX]/[axisY], history included, whole pixels only. With
     *  [counts] they are a mouse's own, sent with the time of the event's last sample. */
    private fun moveBy(ev: MotionEvent, axisX: Int, axisY: Int, counts: Boolean = false) {
        var dx = ev.getAxisValue(axisX)
        var dy = ev.getAxisValue(axisY)
        for (i in 0 until ev.historySize) {
            dx += ev.getHistoricalAxisValue(axisX, i)
            dy += ev.getHistoricalAxisValue(axisY, i)
        }
        moveAccX += dx
        moveAccY += dy
        val ox = moveAccX.toInt() // truncate toward zero — sub-pixel remainder kept w/ sign
        val oy = moveAccY.toInt()
        if (ox != 0 || oy != 0) {
            if (counts) {
                val us = if (Build.VERSION.SDK_INT >= 34) ev.eventTimeNanos / 1000 else ev.eventTime * 1000
                NativeBridge.nativeSendMouseCounts(handle, ox, oy, us)
            } else {
                NativeBridge.nativeSendPointerMove(handle, ox, oy)
            }
            moveAccX -= ox
            moveAccY -= oy
        }
    }

    /** Ctrl+Alt+Shift+Q: release the grab, or (re-)engage it — works even when auto-capture is off. */
    fun toggleCapture() {
        if (captured) {
            userReleased = true
            onReleaseCapture?.invoke()
        } else if (pointerGranted) { // never grab a pointer whose input can't land
            userReleased = false
            onRequestCapture?.invoke()
        }
    }

    /** Auto-engage at stream start (mouse mode Capture + a mouse actually present). */
    fun engageFromStart() {
        if (pointerGranted && captureWanted && !captured && !userReleased && hasPhysicalMouse()) {
            onRequestCapture?.invoke()
        }
    }

    /** From [android.app.Activity.onPointerCaptureChanged] — the OS is the source of truth. */
    fun onCaptureChanged(has: Boolean) {
        captured = has
        // Losing the grab (focus loss, chord) must not leave buttons held on the host.
        if (!has) flushButtons()
        if (has) announcePad()
    }

    /** Grabs the pointer on [v]. From API 37 a plain grab reports a touchpad as a relative mouse,
     *  so while its contacts go to the host's touchpad the grab asks for them absolute. */
    fun grab(v: View) {
        if (padOn() && Build.VERSION.SDK_INT >= 37) {
            v.requestPointerCapture(View.POINTER_CAPTURE_MODE_ABSOLUTE)
        } else {
            v.requestPointerCapture()
        }
    }

    /** The ring opens (`true`) or closes: lift what is held, hand the pointer to it, take it back. */
    fun setSuspended(on: Boolean) {
        if (on == suspended) return
        suspended = on
        if (on) {
            flushButtons()
            regrabAfterSuspend = captured
            if (captured) onReleaseCapture?.invoke()
        } else if (regrabAfterSuspend) {
            regrabAfterSuspend = false
            if (pointerGranted) onRequestCapture?.invoke()
        }
    }

    /** Stream teardown: lift anything held and let the grab go. */
    fun release() {
        flushButtons()
        if (captured) onReleaseCapture?.invoke()
    }

    private fun sendAbs(ev: MotionEvent) {
        // Clamped onto the picture: a pointer out on a bar has no host position of its own, and the
        // edge is the honest answer for it.
        val (x, y, w, h, onPicture) = frameAt(ev.x, ev.y) ?: return
        NativeBridge.nativeSendPointerAbs(handle, x, y, w, h)
        onAbs?.invoke(x, y, onPicture == 1)
    }

    /**
     * One two-finger swipe sample, batched history included. The gesture axes hold the negated
     * finger travel in pixels (AOSP `GestureConverter`), so both flip back into travel.
     */
    @RequiresApi(34)
    private fun gestureScroll(ev: MotionEvent) {
        if (ev.actionMasked != MotionEvent.ACTION_MOVE) return
        val axes = MotionEvent.AXIS_GESTURE_SCROLL_X_DISTANCE to MotionEvent.AXIS_GESTURE_SCROLL_Y_DISTANCE
        var dx = ev.getAxisValue(axes.first)
        var dy = ev.getAxisValue(axes.second)
        for (i in 0 until ev.historySize) {
            dx += ev.getHistoricalAxisValue(axes.first, i)
            dy += ev.getHistoricalAxisValue(axes.second, i)
        }
        scrollNorm.travel(-dx, -dy, density).forEach(::sendScroll)
    }

    private fun sendScroll(s: NormalizedScroll) =
        NativeBridge.nativeSendNormalizedScroll(handle, s.axis, s.delta, s.source, s.phase)

    private fun wheel(ev: MotionEvent) {
        // ACTION_SCROLL carries no gesture boundary, so these always send PHASE_NONE —
        // inversion is the core's outbound seam, not a sign flip here.
        scrollNorm.wheel(
            ev.getAxisValue(MotionEvent.AXIS_VSCROLL).toDouble(),
            ev.getAxisValue(MotionEvent.AXIS_HSCROLL).toDouble(),
            wireScrollSource(ev.source),
            scrollFactorV.toDouble(),
            scrollFactorH.toDouble(),
            density.toDouble(),
        ).forEach(::sendScroll)
    }

    /**
     * A mouse side button that arrived as a KEY event rather than a BUTTON_* motion edge.
     *
     * Not every mouse reports its side buttons the same way. One that puts them on the HID button
     * page (BTN_SIDE/BTN_EXTRA) gets `BUTTON_BACK`/`BUTTON_FORWARD` in the motion button state and
     * lands in [button]. One that puts them on the consumer page (AC Back / AC Forward — common on
     * Bluetooth mice, and the shape Android TV boxes tend to see) produces ONLY synthesized
     * `KEYCODE_BACK`/`KEYCODE_FORWARD` key events, so [button] never fires and the side buttons are
     * dead on the wire. This is the key-shaped entry point for those.
     *
     * Devices that report BOTH send the key first and the motion edge second (that is the order the
     * input reader synthesizes them in), so both paths funnel into the same held-set and the
     * add/remove guard collapses the pair into a single wire press.
     */
    fun sideButtonKey(back: Boolean, down: Boolean) {
        if (pointerGranted) press(if (back) 4 else 5, down)
    }

    private fun button(actionButton: Int, down: Boolean) {
        val b = when (actionButton) {
            MotionEvent.BUTTON_PRIMARY -> 1
            MotionEvent.BUTTON_TERTIARY -> 2
            MotionEvent.BUTTON_SECONDARY -> 3
            MotionEvent.BUTTON_BACK -> 4
            MotionEvent.BUTTON_FORWARD -> 5
            else -> return
        }
        press(b, down)
    }

    /** When the last mouse Back edge went to the host ([SystemClock.uptimeMillis]). */
    @Volatile
    private var lastBackAt = 0L

    /**
     * Android 16+ also turns a mouse's Back button into a system Back, which reaches the ring's
     * BackHandler right after the edge already went to the host as X1: that Back is the mouse's.
     */
    fun backIsMouseEcho(): Boolean = SystemClock.uptimeMillis() - lastBackAt < BACK_ECHO_MS

    /** When a primary DOWN engaged capture; its own BUTTON_PRESS follows within [ENGAGE_PRESS_MS]. */
    private var engageClickAt = 0L

    private fun press(b: Int, down: Boolean) {
        if (b == 1 && down && SystemClock.uptimeMillis() - engageClickAt < ENGAGE_PRESS_MS) {
            engageClickAt = 0L
            return
        }
        if (b == 4) lastBackAt = SystemClock.uptimeMillis()
        if (down) {
            // add() is false when the button is already held — the second delivery of a button
            // this device reports on two paths at once. Sending the down again would double-press
            // it on the host.
            if (heldButtons.add(b)) NativeBridge.nativeSendPointerButton(handle, b, true)
        } else if (heldButtons.remove(b)) {
            // Only release what we pressed — drops the release of a swallowed engaging click
            // and anything that raced a capture transition.
            NativeBridge.nativeSendPointerButton(handle, b, false)
        }
    }

    /** Lifts every held button, and every contact on the host's touchpad. */
    private fun flushButtons() {
        heldButtons.forEach { NativeBridge.nativeSendPointerButton(handle, it, false) }
        heldButtons.clear()
        pad = null
        if (padHeld) padSurface(0, false, false, SystemClock.uptimeMillis() * 1000)
    }
}

/** Contacts a captured touchpad forwards: as many as the host's touchpad takes. */
private const val PAD_MAX_CONTACTS = 5

/** Pointers read per update; the JNI side reads as many. */
private const val PAD_MAX_TOUCHES = 16

/** True when a touchpad is attached. */
private fun hasTouchpad(): Boolean = InputDevice.getDeviceIds().any { id ->
    InputDevice.getDevice(id)?.supportsSource(InputDevice.SOURCE_TOUCHPAD) == true
}

/** A captured touchpad's axes in device units: where x and y start and how far they run, and its
 *  size in tenths of a millimetre. */
private class PadAxes(val x0: Float, val w: Float, val y0: Float, val h: Float, val widthDmm: Int, val heightDmm: Int)

/** [dev]'s captured-touchpad axes, or `null` when it reports none. */
private fun padAxes(dev: InputDevice?): PadAxes? {
    val x = dev?.getMotionRange(MotionEvent.AXIS_X, InputDevice.SOURCE_TOUCHPAD)?.takeIf { it.range > 0f }
    val y = dev?.getMotionRange(MotionEvent.AXIS_Y, InputDevice.SOURCE_TOUCHPAD)?.takeIf { it.range > 0f }
    if (x == null || y == null) return null
    return PadAxes(x.min, x.range, y.min, y.range, padDmm(x.range, x.resolution), padDmm(y.range, y.resolution))
}

/** A pad side [span] device units long at [resolution] units per millimetre, in tenths of a
 *  millimetre; `0` (unknown) when the device gives no resolution. */
internal fun padDmm(span: Float, resolution: Float): Int =
    if (span > 0f && resolution > 0f) (span * 10f / resolution).roundToInt().coerceAtMost(65535) else 0
