package io.unom.punktfunk

import android.os.Build
import android.os.SystemClock
import android.view.InputDevice
import android.view.MotionEvent
import androidx.annotation.RequiresApi
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.isExternalDevice

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
 * button: Android injects a Back key from a virtual device of its own, stamped like the nav
 * bar's. From Android 16 the Back gesture takes the predictive dispatch path and is no key at
 * all, so with a mouse attached a Back from no external device is the mouse's. Before that
 * ([gestureIsKey]) the gesture and the nav bar send that same key, and it stays the ring's.
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
 *    clicking into the stream when the "Capture pointer for games" setting is on, and toggled
 *    any time by Ctrl+Alt+Shift+Q (the cross-client chord). Focus loss releases it (the OS
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
class MouseForwarder(
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
     * positions; one finger moves by its [MotionEvent.AXIS_RELATIVE_X]/Y, and its clicks are
     * buttons. Batched samples are included either way.
     */
    fun onCapturedPointer(ev: MotionEvent): Boolean {
        // A revocation or the ring is racing the release of the grab.
        if (!pointerGranted || suspended) return true
        if (ev.isFromSource(InputDevice.SOURCE_TOUCHPAD)) {
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
                moveBy(ev, MotionEvent.AXIS_X, MotionEvent.AXIS_Y)
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
            MotionEvent.ACTION_SCROLL -> wheel(ev)
        }
        return true
    }

    /** Relative motion off `ev`'s [axisX]/[axisY], history included, whole pixels only. */
    private fun moveBy(ev: MotionEvent, axisX: Int, axisY: Int) {
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
            NativeBridge.nativeSendPointerMove(handle, ox, oy)
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

    /** Auto-engage at stream start (setting on + a mouse actually present). */
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

    private fun flushButtons() {
        heldButtons.forEach { NativeBridge.nativeSendPointerButton(handle, it, false) }
        heldButtons.clear()
    }
}
