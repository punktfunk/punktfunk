package io.unom.punktfunk

import android.os.SystemClock
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerId
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.input.pointer.changedToDownIgnoreConsumed
import androidx.compose.ui.input.pointer.changedToUpIgnoreConsumed
import androidx.compose.ui.input.pointer.positionChanged
import androidx.compose.ui.unit.IntSize
import io.unom.punktfunk.kit.VideoFit
import io.unom.punktfunk.kit.VideoPlacement
import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.hypot
import kotlin.math.roundToInt

// Touch-gesture tuning (px / ms). TAP_SLOP: movement under this still counts as a tap, not a drag.
// TAP_DRAG_MS: a new touch within this long after a tap starts a left-button drag. LONG_PRESS_MS:
// one finger held still this long presses the left button and drags until it lifts. Two-finger pan
// scrolls in DIP (Gesture.scrollPerPx), so the content travels with the fingers at any density.
private const val TAP_SLOP = 12f
private const val TAP_DRAG_MS = 250L
private const val LONG_PRESS_MS = 500L

// The dial (design/touch-client-overlay.md §2.1): a two-finger TWIST opens the quick-action ring.
// DIAL_ARM_DEG: below this rotation the gesture is still a scroll candidate — natural scrolls
// rotate a few degrees, and this is what absorbs them. DIAL_COMMIT_DEG: the ring commits and stays
// open after the fingers lift. DIAL_SLOP: until the centroid travels this far the twist can still
// arm; past it the gesture is a scroll. The scroll never waits for it.
private const val DIAL_ARM_DEG = 10f
private const val DIAL_COMMIT_DEG = 30f
private const val DIAL_SLOP = 2 * TAP_SLOP

/** The twist's progress, for the ring: [Turn] on every move once armed, then [Commit] at the
 *  commit angle, or [Cancel] when the fingers lift short of it (or wind it back). */
sealed class DialEvent {
    /** [progress] 0…1 drives the ring's unwind; [clockwise] is the hand's direction; [x]/[y]
     *  (container px) the centroid the ring is centred on. */
    data class Turn(val progress: Float, val clockwise: Boolean, val x: Float, val y: Float) : DialEvent()
    object Commit : DialEvent()
    object Cancel : DialEvent()
}

// Three-finger vertical swipe: the fraction of the view height the centroid must travel to
// summon (up) / dismiss (down) the local soft keyboard.
private const val KB_SWIPE_FRACTION = 0.10f

// Trackpad-mode pointer ballistics (relative one-finger motion). POINTER_SENS: base finger-px →
// host-px gain (~1:1, never twitchy). The rest is mild acceleration so a flick crosses the screen
// while a slow drag stays precise: above ACCEL_SPEED_FLOOR px/ms the gain ramps by ACCEL_GAIN per
// px/ms, capped at ACCEL_MAX (so a fast swipe can't fling the cursor uncontrollably).
private const val POINTER_SENS = 1.3f
private const val ACCEL_GAIN = 0.6f
private const val ACCEL_SPEED_FLOOR = 0.3f
private const val ACCEL_MAX = 3.0f

/**
 * Touch → mouse, run inside the stream overlay's `pointerInput`. Two models, chosen by the
 * Trackpad-mode setting:
 *  * trackpad (default): the cursor STAYS where it is on touch-down and moves by the finger's
 *    relative delta (MouseMove) with mild pointer acceleration — swipe to nudge, lift and
 *    re-swipe to walk it across, tap to click where it is. This is what makes the cursor
 *    reachable on a small screen.
 *  * direct (opt-out): the cursor jumps to the finger and follows it (MouseMoveAbs,
 *    host-normalized against the overlay size), the old "direct pointing" behaviour.
 *
 * Both share the same gesture vocabulary: tap = left click; two-finger tap = right click;
 * two-finger drag = scroll; tap-then-press-and-drag OR press-and-hold-then-drag = left-drag
 * (text selection / moving windows); three-finger tap = [onCycleStats] (cycle the stats-HUD tier);
 * three-finger swipe up/down = [onKeyboard] (summon/dismiss the local soft keyboard, for
 * typing on the host).
 */
/**
 * Real multi-touch passthrough ([TouchMode.TOUCH]): every finger forwards as a host touchscreen
 * contact (down/move/up with a stable per-finger id), with NO gesture interpretation — taps,
 * drags and multi-finger input mean whatever the remote app decides. Coordinates are overlay
 * pixels with the overlay size as the surface, exactly like the absolute-mouse path (the host
 * normalizes and maps into the output). On teardown (stream leaves composition) every still-held
 * contact is lifted so nothing stays stuck on the host.
 */
/**
 * The stream frame the gesture layer maps into: how it fills the container ([fit]) and its size
 * — or, when the picture spans two screens, the [span] rows from [originY] down that this
 * container shows of the [height]-row frame. The video SurfaceView is laid out with the same
 * placement, so every absolute mapping — direct pointer, passthrough, the pen lane, the mouse —
 * lands where the picture is. A frame of unknown size (an older native lib) maps the container
 * onto itself.
 */
internal class VideoFrame(
    val fit: VideoFit,
    val width: Int,
    val height: Int,
    val originY: Int = 0,
    val span: Int = height,
) {
    fun at(size: IntSize): FrameMap =
        if (width > 0 && span > 0) {
            FrameMap(VideoFit.place(fit, size.width, size.height, width, span), width, height, originY, span)
        } else {
            FrameMap(VideoFit.place(fit, size.width, size.height, size.width, size.height), size.width, size.height)
        }
}

/**
 * One placement of the [span] rows from [originY] of a [width]×[height] frame in a container.
 * Container points clamp onto the visible rows: a contact on a bar or on a cropped-away edge has
 * no host position of its own. The host always sees the whole frame's size.
 */
internal data class FrameMap(
    val placement: VideoPlacement,
    val width: Int,
    val height: Int,
    val originY: Int = 0,
    val span: Int = height,
) {
    val isEmpty: Boolean get() = placement.isEmpty || width <= 0 || span <= 0

    /** Container x → frame pixel. */
    fun x(viewX: Float): Int = placement.frameX(viewX.toDouble()).roundToInt().coerceIn(0, width - 1)

    /** Container y → frame pixel. */
    fun y(viewY: Float): Int = originY + placement.frameY(viewY.toDouble()).roundToInt().coerceIn(0, span - 1)

    /** Container x → 0…1 across the frame, the pen plane's unit. */
    fun nx(viewX: Float): Float =
        (placement.frameX(viewX.toDouble()) / (width - 1).coerceAtLeast(1)).toFloat().coerceIn(0f, 1f)

    /** Container y → 0…1 down the frame. */
    fun ny(viewY: Float): Float =
        ((originY + placement.frameY(viewY.toDouble())) / (height - 1).coerceAtLeast(1)).toFloat().coerceIn(0f, 1f)

    /** The part of the whole frame this placement shows, as fractions: left, top, right, bottom. */
    fun sourceCrop(): FloatArray {
        if (isEmpty) return floatArrayOf(0f, 0f, 1f, 1f)
        val p = placement
        return floatArrayOf(
            (p.srcX / width).toFloat(),
            ((originY + p.srcY) / height).toFloat(),
            ((p.srcX + p.srcW) / width).toFloat(),
            ((originY + p.srcY + p.srcH) / height).toFloat(),
        )
    }
}

/** Whether this change belongs to the stylus lane (only when a pen-capable host is live). */
private fun isStylus(c: PointerInputChange, stylus: StylusStream?): Boolean =
    stylus != null && (c.type == PointerType.Stylus || c.type == PointerType.Eraser)

/** [awaitFirstDown] with the stylus lane split out: pen events feed [stylus] and never start a
 *  mouse/touch gesture. Toward a pen-less host ([stylus] == null) a stylus stays a finger. */
private suspend fun AwaitPointerEventScope.awaitFirstFingerDown(
    stylus: StylusStream?,
    video: () -> VideoFrame,
): PointerInputChange {
    while (true) {
        val ev = awaitPointerEvent()
        stylus?.intercept(ev, video().at(size))
        val down = ev.changes.firstOrNull {
            it.changedToDownIgnoreConsumed() && !isStylus(it, stylus)
        }
        if (down != null) return down
    }
}

internal suspend fun PointerInputScope.streamTouchPassthrough(
    sink: TouchSink,
    stylus: StylusStream?,
    video: () -> VideoFrame,
) {
    val ids = mutableMapOf<PointerId, Int>()
    fun alloc(p: PointerId): Int {
        var id = 0
        while (ids.containsValue(id)) id++
        ids[p] = id
        return id
    }
    try {
        awaitPointerEventScope {
            while (true) {
                val ev = awaitPointerEvent()
                val r = video().at(size)
                stylus?.intercept(ev, r)
                if (r.isEmpty) continue
                val sw = r.width
                val sh = r.height
                for (c in ev.changes) {
                    if (isStylus(c, stylus)) continue // the pen plane owns it
                    val x = r.x(c.position.x)
                    val y = r.y(c.position.y)
                    when {
                        c.changedToDownIgnoreConsumed() ->
                            sink.touch(alloc(c.id), 0, x, y, sw, sh)
                        c.changedToUpIgnoreConsumed() ->
                            ids.remove(c.id)?.let {
                                sink.touch(it, 2, 0, 0, sw, sh)
                            }
                        c.positionChanged() ->
                            ids[c.id]?.let { id ->
                                // Batched MotionEvents coalesce intermediate points into the
                                // historical list — forward them in order so a fast swipe keeps
                                // its real curvature on the host (usually empty during a stream:
                                // unbuffered dispatch is requested, so this costs nothing).
                                for (hs in c.historical) {
                                    sink.touch(id, 1,
                                        r.x(hs.position.x), r.y(hs.position.y),
                                        sw, sh,
                                    )
                                }
                                sink.touch(id, 1, x, y, sw, sh)
                            }
                    }
                    c.consume()
                }
            }
        }
    } finally {
        // Lift anything still down (composition/session teardown mid-touch).
        ids.values.forEach { sink.touch(it, 2, 0, 0, 1, 1) }
    }
}

internal suspend fun PointerInputScope.streamTouchInput(
    sink: TouchSink,
    stylus: StylusStream?,
    video: () -> VideoFrame,
    trackpad: Boolean,
    /** The dial editor's stage: only multi-finger gestures are owned (the twist, with the real
     *  thresholds); a lone finger passes unconsumed to whatever scrolls beneath, and no click,
     *  cursor move or tap is ever synthesized. */
    dialOnly: Boolean = false,
    onCycleStats: () -> Unit,
    onKeyboard: (show: Boolean) -> Unit,
    onDial: (DialEvent) -> Unit,
) {
    var lastTapUp = 0L
    var lastTapX = 0f
    var lastTapY = 0f
    awaitEachGesture {
        val down = awaitFirstFingerDown(stylus, video)
        // A touch landing just after a quick tap nearby = tap-and-drag: hold the left
        // button for this whole gesture (laptop-trackpad convention).
        val isDrag = down.uptimeMillis - lastTapUp < TAP_DRAG_MS &&
            abs(down.position.x - lastTapX) < TAP_SLOP && abs(down.position.y - lastTapY) < TAP_SLOP
        lastTapUp = 0L // consume the arming either way
        val g = Gesture(
            sink, down, trackpad, density, dialOnly, size.height, onDial, onKeyboard,
        ) { video().at(size) }
        // Direct mode jumps the cursor to the finger; trackpad mode leaves it put (the
        // whole point — you nudge it with swipes instead).
        if (!trackpad) g.moveAbs(down.position.x, down.position.y)
        if (isDrag) g.holdButton()
        var upTime = down.uptimeMillis
        try {
            while (true) {
                // A still finger raises no event, so the long press is a timeout: while one finger
                // is down and nothing has moved, wait at most until the hold time; running out
                // means "held still that long" and picks up the drag.
                val ev = if (g.awaitingLongPress()) {
                    val remaining = LONG_PRESS_MS - (SystemClock.uptimeMillis() - down.uptimeMillis)
                    if (remaining <= 0) null else withTimeoutOrNull(remaining) { awaitPointerEvent() }
                } else {
                    awaitPointerEvent()
                }
                if (ev == null) {
                    g.holdButton()
                    continue
                }
                stylus?.intercept(ev, video().at(size))
                val pressed = ev.changes.filter { it.pressed && !isStylus(it, stylus) }
                    .sortedBy { it.id.value }
                if (pressed.isEmpty()) {
                    g.pairEnded()
                    upTime = ev.changes.firstOrNull()?.uptimeMillis ?: upTime
                    break
                }
                if (g.step(pressed)) ev.changes.forEach { it.consume() }
            }
            if (g.tap()) {
                g.rollBackProvisional() // a tap's jitter must not leave the page nudged
                when {
                    g.maxFingers >= 3 -> onCycleStats() // in-stream HUD verbosity cycle
                    g.maxFingers == 2 -> { // two-finger tap → right click
                        sink.button(3, true)
                        sink.button(3, false)
                    }
                    else -> { // tap → left click (at the cursor's current spot), arm tap-drag
                        sink.button(1, true)
                        sink.button(1, false)
                        lastTapUp = upTime
                        lastTapX = down.position.x
                        lastTapY = down.position.y
                    }
                }
            } else {
                g.endScroll() // an ordinary lift closes any open scroll axes
            }
        } finally {
            g.release() // end a held drag exactly once, teardown mid-drag included
        }
    }
}

/**
 * One gesture of [streamTouchInput], from a finger's down to the last lift: which finger count
 * it has been, whether it became a scroll, a twist, a keyboard swipe or a drag, and the
 * trackpad's relative-motion ledger. [step] takes each event's pressed fingers and says whether
 * the gesture consumed it.
 */
private class Gesture(
    private val sink: TouchSink,
    down: PointerInputChange,
    private val trackpad: Boolean,
    pxPerDip: Float,
    private val dialOnly: Boolean,
    private val viewHeight: Int,
    private val onDial: (DialEvent) -> Unit,
    private val onKeyboard: (show: Boolean) -> Unit,
    private val frame: () -> FrameMap,
) {
    /** Wire Q24.8 delta per scrolled pixel — DIP-priced, so denser screens scroll the same
     *  distance. Inversion is the core's outbound seam, never a sign here. */
    private val scrollPerPx = ScrollWire.SCALE.toFloat() /
        (pxPerDip.takeIf { it.isFinite() && it > 0f } ?: 1f)
    private val startX = down.position.x
    private val startY = down.position.y
    private val downId = down.id
    /** The left button this gesture holds (tap-drag from the start, or a long press later);
     *  released exactly once, in [release], so a teardown mid-drag never strands it. */
    private var dragHeld = false
    private var moved = false
    var maxFingers = 1
        private set
    private var scrolling = false
    private var scrollCount = 0 // pointer count the scroll centroid is anchored at
    /** Wire scroll axes carrying an open gesture (0 = vertical, 1 = horizontal). */
    private val scrollOpen = booleanArrayOf(false, false)
    /** The pair travelled past DIAL_SLOP unarmed: a scroll for the gesture's lifetime. */
    private var scrollLocked = false
    /** Units scrolled while the pair was undecided, sent back if it becomes a twist or a tap. */
    private var provisionalX = 0
    private var provisionalY = 0
    // Sub-unit scroll remainder, so a slow pan isn't lost to Int truncation.
    private var scrollAccX = 0f
    private var scrollAccY = 0f
    // The twist: the finger-to-finger vector when the pair formed, the centroid then, and
    // whether it has armed (owns the gesture) / committed (the ring stays open).
    private var dialIds: Pair<PointerId, PointerId>? = null
    private var dialVx = 0f
    private var dialVy = 0f
    private var dialAnchorX = 0f
    private var dialAnchorY = 0f
    private var dialArmed = false
    private var dialCommitted = false
    // Keyboard-swipe state: the 3+-finger centroid anchor (per finger count, like the
    // scroll anchor) and a once-per-gesture latch.
    private var kbCount = 0
    private var kbAnchorX = 0f
    private var kbAnchorY = 0f
    private var kbFired = false
    private var prevCx = startX
    private var prevCy = startY
    // Trackpad relative-motion state: the tracked finger, its last position/time, and
    // the sub-pixel remainder so a slow drag isn't lost to Int truncation.
    private var trackId = down.id
    private var prevX = startX
    private var prevY = startY
    private var prevT = down.uptimeMillis
    private var accX = 0f
    private var accY = 0f

    fun moveAbs(x: Float, y: Float) {
        val r = frame()
        if (r.isEmpty) return
        sink.pointerAbs(r.x(x), r.y(y), r.width, r.height)
    }

    fun holdButton() {
        dragHeld = true
        sink.button(1, true)
    }

    fun release() {
        if (dragHeld) sink.button(1, false)
        scrollClose(ScrollWire.PHASE_CANCEL) // teardown never strands an open axis on the host
    }

    /** One scroll delta; the first on an axis is its Begin, later ones Update. */
    private fun scrollEmit(axis: Int, delta: Int) {
        if (delta == 0) return
        val phase = if (scrollOpen[axis]) {
            ScrollWire.PHASE_UPDATE
        } else {
            scrollOpen[axis] = true
            ScrollWire.PHASE_BEGIN
        }
        sink.scroll(axis, delta, ScrollWire.SOURCE_TOUCH, phase)
    }

    /** A zero-delta [phase] on every open axis; the axis's sub-unit remainder drops with it. */
    private fun scrollClose(phase: Int) {
        for (axis in 0..1) {
            if (scrollOpen[axis]) {
                scrollOpen[axis] = false
                if (axis == ScrollWire.AXIS_VERTICAL) scrollAccY = 0f else scrollAccX = 0f
                sink.scroll(axis, 0, ScrollWire.SOURCE_TOUCH, phase)
            }
        }
    }

    /** The gesture ended without a rollback: a zero-delta End on each open scroll axis. */
    fun endScroll() = scrollClose(ScrollWire.PHASE_END)

    /** One finger, nothing moved, no drag yet: the long-press timeout is live. */
    fun awaitingLongPress() = !dialOnly && !dragHeld && !moved && maxFingers == 1

    /** Nothing disqualified a tap: the finish classifies it by [maxFingers]. */
    fun tap() = !dialOnly && !dragHeld && !moved

    /** Every finger lifted: a twist short of commit winds the ring back in. */
    fun pairEnded() {
        if (dialIds != null && dialArmed && !dialCommitted) onDial(DialEvent.Cancel)
        dialIds = null
    }

    /** One event's pressed fingers, sorted by id. `true` = consumed. */
    fun step(pressed: List<PointerInputChange>): Boolean {
        // Any change of the pair ends the twist: a lift short of commit winds the ring
        // back in; a committed ring stays open and the UI owns it from here.
        if (pressed.size != 2 && dialIds != null) {
            if (dialArmed && !dialCommitted) onDial(DialEvent.Cancel)
            dialIds = null
            dialArmed = false
            dialCommitted = false
        }
        if (pressed.size > maxFingers) maxFingers = pressed.size
        // Dropping below three fingers forgets the keyboard-swipe anchor, so a 3→2→3
        // bounce re-anchors instead of reading the count change as swipe travel.
        if (pressed.size < 3) kbCount = 0
        if (dialOnly && pressed.size < 2) return false
        return when {
            pressed.size == 2 -> twoFingers(pressed)
            pressed.size >= 3 -> { threeFingers(pressed); true }
            !scrolling -> { oneFinger(pressed); true }
            // Skipped once a gesture turned into a scroll, so dropping back to one finger
            // doesn't jerk the cursor. A committed scroll's axes end with the pair that drove
            // them; a still-undecided (provisional) one stays open — the last lift decides
            // between the tap's rollback and the closing End.
            else -> {
                if (scrollLocked) endScroll()
                true
            }
        }
    }

    /**
     * The dial first (design §2.1): centroid travel past DIAL_SLOP before arming locks a scroll; a
     * twist of the finger-to-finger vector past DIAL_ARM_DEG before that owns the gesture and takes
     * back the provisional scroll. A pinch with no rotation is nothing.
     */
    private fun twoFingers(pressed: List<PointerInputChange>): Boolean {
        val cx = (pressed.sumOf { it.position.x.toDouble() } / pressed.size).toFloat()
        val cy = (pressed.sumOf { it.position.y.toDouble() } / pressed.size).toFloat()
        val (a, b) = pressed
        val ids = a.id to b.id
        if (dialIds != ids && !scrollLocked) {
            dialIds = ids
            dialVx = b.position.x - a.position.x
            dialVy = b.position.y - a.position.y
            dialAnchorX = cx
            dialAnchorY = cy
        }
        if (dialIds == ids && (dialArmed || !scrollLocked)) {
            val vx = b.position.x - a.position.x
            val vy = b.position.y - a.position.y
            val phi = Math.toDegrees(
                atan2(dialVx * vy - dialVy * vx, dialVx * vx + dialVy * vy).toDouble(),
            ).toFloat() // signed; + = clockwise on a y-down screen
            val travel = hypot(cx - dialAnchorX, cy - dialAnchorY)
            if (!dialArmed && travel >= TAP_SLOP) moved = true
            if (!dialArmed && travel >= DIAL_SLOP) {
                scrollLocked = true
                provisionalX = 0
                provisionalY = 0
            } else if (dialArmed || abs(phi) >= DIAL_ARM_DEG) {
                if (!dialArmed) {
                    rollBackProvisional()
                    dialArmed = true
                    moved = true // a twist is never a tap…
                    scrolling = true // …and dropping to one finger must not jerk the cursor
                }
                val p = ((abs(phi) - DIAL_ARM_DEG) / (DIAL_COMMIT_DEG - DIAL_ARM_DEG))
                    .coerceIn(0f, 1f)
                onDial(DialEvent.Turn(p, phi > 0f, cx, cy))
                if (p >= 1f && !dialCommitted) {
                    dialCommitted = true
                    onDial(DialEvent.Commit)
                } else if (p <= 0f && dialCommitted) {
                    dialCommitted = false
                    onDial(DialEvent.Cancel)
                }
                return true
            }
            // Undecided: the editor's stage claims the pair without scrolling, or the page's
            // scroll container steals the fingers before the twist can arm.
            if (dialOnly && !scrollLocked) return true
        }
        // Two fingers → scroll by the centroid delta as a precise distance; never move the
        // cursor. (Re-)anchor whenever the finger COUNT changes, not just on scroll start: the
        // centroid of three fingers sits far from the centroid of two, and real fingers never land
        // (or lift) in the same input frame — so the 2→3 transition would otherwise read as travel.
        if (!scrolling || pressed.size != scrollCount) {
            scrolling = true
            scrollCount = pressed.size
            prevCx = cx
            prevCy = cy
        }
        scrollAccY += (prevCy - cy) * scrollPerPx // finger up → scroll up
        scrollAccX += (cx - prevCx) * scrollPerPx
        prevCx = cx
        prevCy = cy
        val sy = scrollAccY.toInt() // truncates toward zero → remainder kept w/ sign
        val sx = scrollAccX.toInt()
        scrollAccY -= sy
        scrollAccX -= sx
        if (sy == 0 && sx == 0) return true
        if (!scrollLocked && dialIds == ids && !dialArmed) {
            provisionalX += sx
            provisionalY += sy
        } else {
            scrollLocked = true
            moved = true
        }
        scrollEmit(ScrollWire.AXIS_VERTICAL, sy)
        scrollEmit(ScrollWire.AXIS_HORIZONTAL, sx)
        return true
    }

    /** The undecided pair became a twist or a tap: send back what it scrolled, then cancel the
     *  axes — a rolled-back gesture never runs a kinetic tail. */
    fun rollBackProvisional() {
        scrollEmit(ScrollWire.AXIS_VERTICAL, -provisionalY)
        scrollEmit(ScrollWire.AXIS_HORIZONTAL, -provisionalX)
        provisionalX = 0
        provisionalY = 0
        scrollAccX = 0f
        scrollAccY = 0f
        scrollClose(ScrollWire.PHASE_CANCEL)
    }

    /**
     * Three+ fingers → the keyboard swipe, never scroll. Anchor the centroid per finger count
     * (same reasoning as the scroll anchor) and fire once per gesture when the vertical travel
     * crosses the threshold: up = show, down = hide.
     */
    private fun threeFingers(pressed: List<PointerInputChange>) {
        val cx = (pressed.sumOf { it.position.x.toDouble() } / pressed.size).toFloat()
        val cy = (pressed.sumOf { it.position.y.toDouble() } / pressed.size).toFloat()
        if (pressed.size != kbCount) {
            kbCount = pressed.size
            kbAnchorX = cx
            kbAnchorY = cy
        } else {
            val dy = cy - kbAnchorY
            // Real centroid travel disqualifies the tap classification (else a sub-threshold
            // swipe would still fire the three-finger stats tap).
            if (abs(dy) > TAP_SLOP || abs(cx - kbAnchorX) > TAP_SLOP) moved = true
            if (!kbFired && abs(dy) >= viewHeight * KB_SWIPE_FRACTION) {
                kbFired = true
                onKeyboard(dy < 0) // finger up → show, finger down → hide
            }
        }
        // Three or more fingers never scroll: an in-flight pair ends here. Leaving the scroll
        // state stale would also read the 3→2 centroid jump as a wheel notch; clearing it makes
        // a return to two fingers re-anchor fresh. Same for the trackpad's tracked finger: its
        // prev position froze while 3+ fingers were down, so dropping straight back to one
        // finger must re-anchor (zero delta), not replay the phase.
        scrolling = false
        scrollCount = 0
        endScroll()
        trackId = PointerId(Long.MIN_VALUE)
    }

    /** One finger: the cursor, relative (trackpad) or absolute (direct). */
    private fun oneFinger(pressed: List<PointerInputChange>) {
        val p = pressed.firstOrNull { it.id == downId } ?: pressed.first()
        if (abs(p.position.x - startX) > TAP_SLOP || abs(p.position.y - startY) > TAP_SLOP) {
            moved = true
        }
        if (!trackpad) {
            // Direct: cursor follows the finger — historical points first (batched MotionEvent
            // samples), so the host cursor traces the finger's real path.
            for (hs in p.historical) moveAbs(hs.position.x, hs.position.y)
            moveAbs(p.position.x, p.position.y)
            return
        }
        // Relative: move by the finger delta × (sensitivity × acceleration), carrying the
        // sub-pixel remainder. Re-anchor (zero delta this frame) if the tracked finger changed,
        // so lifting one of several fingers never jumps the cursor.
        if (p.id != trackId) {
            trackId = p.id
            prevX = p.position.x
            prevY = p.position.y
            prevT = p.uptimeMillis
        }
        val dx = p.position.x - prevX
        val dy = p.position.y - prevY
        val dt = (p.uptimeMillis - prevT).coerceAtLeast(1L)
        prevX = p.position.x
        prevY = p.position.y
        prevT = p.uptimeMillis
        val speed = hypot(dx, dy) / dt // finger px per ms
        val accel = (1f + ACCEL_GAIN * (speed - ACCEL_SPEED_FLOOR).coerceAtLeast(0f))
            .coerceAtMost(ACCEL_MAX)
        accX += dx * POINTER_SENS * accel
        accY += dy * POINTER_SENS * accel
        val outX = accX.toInt() // truncates toward zero → remainder kept w/ sign
        val outY = accY.toInt()
        if (outX != 0 || outY != 0) {
            sink.pointerMove(outX, outY)
            accX -= outX
            accY -= outY
        }
    }
}
