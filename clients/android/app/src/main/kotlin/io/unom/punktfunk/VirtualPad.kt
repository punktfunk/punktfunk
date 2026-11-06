package io.unom.punktfunk

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.absoluteOffset
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.PointerId
import androidx.compose.ui.input.pointer.changedToDownIgnoreConsumed
import androidx.compose.ui.input.pointer.changedToUpIgnoreConsumed
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChangedIgnoreConsumed
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.compose.ui.window.DialogWindowProvider
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.R
import kotlin.math.atan2
import kotlin.math.hypot
import kotlin.math.roundToInt

/*
 * The virtual on-screen controller (design/touch-client-overlay.md §4): a layer of hit regions
 * above the stream's gesture layer and below the ring, driving one wire pad through the same
 * per-pad events a real controller sends. Every control is its own pointer-input node, so a
 * finger that lands between controls never reaches this layer and falls through to the touch
 * mode beneath — tap-to-click keeps working beside the pad. Buttons are hit tests; the D-pad is
 * an eight-way angle; a stick is a per-finger state machine; a trigger reads the finger's
 * position along its travel, so a slow press is a slow press.
 *
 * The layer knows nothing of the wire: [PadSink] is a router `ExternalPad` in-stream, which
 * gives the pad the lowest free wire index, its Arrival before any input, the ring's mask, and
 * its Remove on close — exactly a real controller's lifetime.
 */

/** Where the pad's events go: a router `ExternalPad` in-stream, nothing in a preview. [ring]
 *  opens the quick-action ring. */
class PadSink(
    val button: (bit: Int, down: Boolean) -> Unit,
    val axis: (axis: Int, value: Int) -> Unit,
    val ring: () -> Unit = {},
)

const val PAD_SCALE_MIN = 0.6f
const val PAD_SCALE_MAX = 1.6f
const val PAD_OPACITY_MIN = 0.15f
// A per-control tweak's scale bounds; what a blob claims is clamped here.
const val PAD_TWEAK_SCALE_MIN = 0.5f
const val PAD_TWEAK_SCALE_MAX = 2f

/** A control's place in the layer, in dp before [PadConfig.scale]; the origin is the top-left. */
internal data class PadRect(val x: Float, val y: Float, val w: Float, val h: Float) {
    fun overlaps(o: PadRect): Boolean = x < o.x + o.w && o.x < x + w && y < o.y + o.h && o.y < y + h
}

/** A control's name for the editor and for accessibility. */
internal sealed interface PadName {
    /** One of the pad's named parts: a message. */
    class Message(val id: Int) : PadName

    /** The name printed on a button (Select, Guide, Start): data, the same in every language. */
    class Printed(val text: String) : PadName
}

@Composable
internal fun PadName.text(): String = when (this) {
    is PadName.Message -> stringResource(id)
    is PadName.Printed -> text
}

/** One disc in a [PadControl.Buttons] group: its centre and radius relative to the group's rect. */
internal data class PadDisc(val glyph: String, val bit: Int, val cx: Float, val cy: Float, val r: Float)

/**
 * [id] is the tweak key in the blob ([PadTweak]); [sc] and [hidden] are the applied tweak — the
 * geometry in [rect] and any discs is already sized by [sc], and the touch and draw code
 * multiplies only its named constants (stick radius, dead zones, label sizes) by it.
 */
internal sealed class PadControl(val id: String, val label: PadName, val rect: PadRect, val sc: Float = 1f, val hidden: Boolean = false) {
    /** Discs that press while a finger is on them; a finger rolling from one to the next presses the next. */
    class Buttons(id: String, label: PadName, rect: PadRect, val discs: List<PadDisc>, sc: Float = 1f, hidden: Boolean = false) :
        PadControl(id, label, rect, sc, hidden)

    /** Eight directions by angle from the centre, as `BTN_DPAD_*` bits. */
    class Dpad(rect: PadRect, sc: Float = 1f, hidden: Boolean = false) :
        PadControl("dpad", PadName.Message(R.string.pad_control_dpad), rect, sc, hidden)

    /** The first finger owns it; its travel from where it landed is the deflection. */
    class Stick(id: String, label: PadName, rect: PadRect, val axisX: Int, val axisY: Int, sc: Float = 1f, hidden: Boolean = false) :
        PadControl(id, label, rect, sc, hidden)

    /** The finger's position down the pill is the pull: the top is 0, the bottom is full. */
    class Trigger(id: String, label: PadName, rect: PadRect, val axis: Int, sc: Float = 1f, hidden: Boolean = false) :
        PadControl(id, label, rect, sc, hidden)

    /** A tap opens the quick-action ring; nothing goes on the wire. */
    class Ring(rect: PadRect, sc: Float = 1f, hidden: Boolean = false) :
        PadControl("ring", PadName.Message(R.string.pad_control_ring), rect, sc, hidden)

    /** The same control at [rect], sized by [sc], possibly [hidden] — how a tweak is applied. */
    fun tweaked(rect: PadRect, sc: Float, hidden: Boolean): PadControl = when (this) {
        is Buttons -> Buttons(id, label, rect, discs.map { it.copy(cx = it.cx * sc, cy = it.cy * sc, r = it.r * sc) }, sc, hidden)
        is Dpad -> Dpad(rect, sc, hidden)
        is Stick -> Stick(id, label, rect, axisX, axisY, sc, hidden)
        is Trigger -> Trigger(id, label, rect, axis, sc, hidden)
        is Ring -> Ring(rect, sc, hidden)
    }
}

// Sizes in dp before the scale.
private const val MARGIN = 16f
private const val STICK_R = 60f
private const val STICK_KNOB_R = 26f
/** The node is wider than the base: a thumb landing just outside the ring still takes the stick. */
private const val STICK_HIT = 2 * (STICK_R + 20f)
private const val FACE_R = 24f
private const val FACE_HIT = 152f
private const val DPAD_HIT = 120f
private const val SMALL_R = 18f
private const val BUMPER_R = 22f
private const val TRIGGER_W = 56f
private const val TRIGGER_H = 84f
/** A disc takes a finger this far past its edge, so a thumb need not be exact. */
private const val HIT_SLOP = 1.3f
// ponytail: fixed dead zones; a setting when a device disagrees with these.
private const val STICK_DEAD = 6f
private const val DPAD_DEAD = 14f
/** A layer narrower than this (a phone upright) stacks the clusters instead of spreading them. */
private const val NARROW = 720f

private fun disc(id: String, label: PadName, glyph: String, bit: Int, cx: Float, cy: Float, r: Float) =
    PadControl.Buttons(id, label, PadRect(cx - r, cy - r, 2 * r, 2 * r), listOf(PadDisc(glyph, bit, r, r, r)))

private fun named(id: Int): PadName = PadName.Message(id)

/**
 * The preset's controls for a layer [w] × [h] dp (the container divided by the scale). Positions
 * are fixed per preset (§4.3): sticks in the bottom corners, the D-pad beside the left stick, the
 * face buttons in the bottom-right corner with the right stick beside them, the shoulders in the
 * top corners with the stick clicks beside them, Select, Guide and Start along the bottom edge, and
 * the quick-action ring's button inboard of the left bumper. A narrow layer lifts the D-pad and the
 * right stick above their neighbours; the middle three keep the bottom edge only while the clusters
 * leave it free, and take the top edge when they do not. An unknown preset is `full`.
 */
internal fun padControls(layout: String, w: Float, h: Float): List<PadControl> {
    val narrow = w < NARROW
    val sticks = layout != "dpad"
    val face = layout != "sticks"
    val bottom = h - MARGIN
    val out = mutableListOf<PadControl>()
    if (sticks) {
        out += disc("lb", named(R.string.pad_control_left_bumper), "LB", Gamepad.BTN_LB, MARGIN + BUMPER_R, MARGIN + BUMPER_R, BUMPER_R)
        out += PadControl.Trigger(
            "lt", named(R.string.pad_control_left_trigger),
            PadRect(MARGIN, MARGIN + 2 * BUMPER_R + 8, TRIGGER_W, TRIGGER_H), Gamepad.AXIS_LT,
        )
        out += disc("rb", named(R.string.pad_control_right_bumper), "RB", Gamepad.BTN_RB, w - MARGIN - BUMPER_R, MARGIN + BUMPER_R, BUMPER_R)
        out += PadControl.Trigger(
            "rt", named(R.string.pad_control_right_trigger),
            PadRect(w - MARGIN - TRIGGER_W, MARGIN + 2 * BUMPER_R + 8, TRIGGER_W, TRIGGER_H), Gamepad.AXIS_RT,
        )
        // The stick clicks sit inboard of the triggers, level with them: the index finger already
        // rests there, and the thumb keeps the stick — which is what L3-to-sprint asks for.
        val clickY = MARGIN + 2 * BUMPER_R + 8 + TRIGGER_H / 2
        val clickX = MARGIN + TRIGGER_W + 8 + SMALL_R
        out += disc("l3", named(R.string.pad_control_left_stick_click), "L3", Gamepad.BTN_LS_CLICK, clickX, clickY, SMALL_R)
        out += disc("r3", named(R.string.pad_control_right_stick_click), "R3", Gamepad.BTN_RS_CLICK, w - clickX, clickY, SMALL_R)
        out += PadControl.Stick(
            "ls", named(R.string.pad_control_left_stick),
            PadRect(MARGIN, bottom - STICK_HIT, STICK_HIT, STICK_HIT), Gamepad.AXIS_LS_X, Gamepad.AXIS_LS_Y,
        )
    }
    if (face) {
        val dpad = when {
            !sticks -> PadRect(MARGIN + 20, bottom - 20 - DPAD_HIT, DPAD_HIT, DPAD_HIT)
            narrow -> PadRect(MARGIN + 20, bottom - STICK_HIT - 8 - DPAD_HIT, DPAD_HIT, DPAD_HIT)
            else -> PadRect(MARGIN + STICK_HIT + 10, bottom - 110 - DPAD_HIT, DPAD_HIT, DPAD_HIT)
        }
        out += PadControl.Dpad(dpad)
        val c = FACE_HIT / 2
        val gap = 40f
        out += PadControl.Buttons(
            "face",
            named(R.string.pad_control_face),
            PadRect(w - MARGIN - FACE_HIT, bottom - FACE_HIT, FACE_HIT, FACE_HIT),
            listOf(
                PadDisc("Y", Gamepad.BTN_Y, c, c - gap, FACE_R),
                PadDisc("X", Gamepad.BTN_X, c - gap, c, FACE_R),
                PadDisc("B", Gamepad.BTN_B, c + gap, c, FACE_R),
                PadDisc("A", Gamepad.BTN_A, c, c + gap, FACE_R),
            ),
        )
    }
    if (sticks) {
        val right = when {
            !face -> PadRect(w - MARGIN - STICK_HIT, bottom - STICK_HIT, STICK_HIT, STICK_HIT)
            narrow -> PadRect(w - MARGIN - STICK_HIT, bottom - FACE_HIT - 8 - STICK_HIT, STICK_HIT, STICK_HIT)
            else -> PadRect(w - MARGIN - FACE_HIT - 16 - STICK_HIT, bottom - STICK_HIT, STICK_HIT, STICK_HIT)
        }
        out += PadControl.Stick("rs", named(R.string.pad_control_right_stick), right, Gamepad.AXIS_RS_X, Gamepad.AXIS_RS_Y)
    }
    // Measured against the clusters, not guessed from the width: between [NARROW] and about
    // 900 dp the right stick still spreads into the bottom corner, near enough to sit on Start.
    val middle = { y: Float ->
        listOf(
            disc("select", PadName.Printed("Select"), "⧉", Gamepad.BTN_BACK, w / 2 - 64, y, SMALL_R),
            disc("guide", PadName.Printed("Guide"), "◎", Gamepad.BTN_GUIDE, w / 2, y, SMALL_R),
            disc("start", PadName.Printed("Start"), "☰", Gamepad.BTN_START, w / 2 + 64, y, SMALL_R),
        )
    }
    val low = middle(bottom - SMALL_R)
    out += if (low.none { m -> out.any { it.rect.overlaps(m.rect) } }) low else middle(MARGIN + SMALL_R)
    out += PadControl.Ring(PadRect(MARGIN + 2 * BUMPER_R + 8, MARGIN + BUMPER_R - SMALL_R, 2 * SMALL_R, 2 * SMALL_R))
    return out
}

/**
 * The preset with the user's overrides laid over it: each [PadTweak] moves a control's centre
 * to fractions of the layer, scales it about that centre, or marks it hidden. Scales clamp to
 * [PAD_TWEAK_SCALE_MIN]..[PAD_TWEAK_SCALE_MAX] and centres clamp so the control stays on the
 * layer; a tweak with no control in this preset is ignored.
 */
internal fun applyPadTweaks(controls: List<PadControl>, tweaks: Map<String, PadTweak>, w: Float, h: Float): List<PadControl> =
    controls.map { c ->
        val t = tweaks[c.id] ?: return@map c
        val sc = (t.scale ?: 1f).coerceIn(PAD_TWEAK_SCALE_MIN, PAD_TWEAK_SCALE_MAX)
        val cw = c.rect.w * sc
        val ch = c.rect.h * sc
        val cx = ((t.x ?: ((c.rect.x + c.rect.w / 2) / w)) * w).coerceIn(minOf(cw / 2, w / 2), maxOf(w - cw / 2, w / 2))
        val cy = ((t.y ?: ((c.rect.y + c.rect.h / 2) / h)) * h).coerceIn(minOf(ch / 2, h / 2), maxOf(h - ch / 2, h / 2))
        c.tweaked(PadRect(cx - cw / 2, cy - ch / 2, cw, ch), sc, t.hidden)
    }

/**
 * The controls [pad] puts on a layer [w] × [h] dp: the preset, then the overrides for the
 * layer's class — wide or narrow, the split [padControls] itself already makes — with the
 * hidden ones still in the list (the stream filters them; the editor ghosts them).
 */
internal fun padControls(pad: PadConfig, w: Float, h: Float): List<PadControl> =
    applyPadTweaks(padControls(pad.layout, w, h), if (w < NARROW) pad.controlsNarrow else pad.controls, w, h)

/** The D-pad bits for a finger [dx], [dy] px from the centre (screen +y down): eight ways, none inside [dead]. */
internal fun dpadBits(dx: Float, dy: Float, dead: Float): Int {
    if (hypot(dx, dy) < dead) return 0
    val deg = Math.toDegrees(atan2(-dy, dx).toDouble())
    return when (((deg + 22.5 + 360.0) % 360.0 / 45.0).toInt()) {
        0 -> Gamepad.BTN_DPAD_RIGHT
        1 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_RIGHT
        2 -> Gamepad.BTN_DPAD_UP
        3 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_LEFT
        4 -> Gamepad.BTN_DPAD_LEFT
        5 -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_LEFT
        6 -> Gamepad.BTN_DPAD_DOWN
        else -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_RIGHT
    }
}

/**
 * A stick's wire pair for a travel of [dx], [dy] px from where the finger landed: i16 with +y up,
 * nothing inside [dead], the travel itself past it, full deflection at [radius] and beyond.
 * [dead] only gates: subtracting it would stack on the game's own dead zone.
 */
internal fun stickWire(dx: Float, dy: Float, radius: Float, dead: Float): Pair<Int, Int> {
    val mag = hypot(dx, dy)
    if (mag <= dead) return 0 to 0
    val v = (mag / radius).coerceAtMost(1f) * 32767f / mag
    return (dx * v).roundToInt() to (-dy * v).roundToInt()
}

/** A trigger's wire value for a finger [y] px down a pill [h] px tall: 0 at the top, 255 at the bottom. */
internal fun triggerWire(y: Float, h: Float): Int = ((y / h).coerceIn(0f, 1f) * 255f).roundToInt()

/**
 * The controller over the stream. [containerSize] is the whole container in px; the controls
 * anchor to its edges, not to the picture. Composed only while the pad is shown (tenet 1).
 */
@Composable
fun VirtualPadLayer(
    pad: PadConfig,
    containerSize: IntSize,
    sink: PadSink,
    haptics: ConsoleHaptics,
    modifier: Modifier = Modifier,
) {
    if (containerSize == IntSize.Zero) return
    val density = LocalDensity.current.density
    val scale = pad.scale.coerceIn(PAD_SCALE_MIN, PAD_SCALE_MAX)
    val opacity = pad.opacity.coerceIn(PAD_OPACITY_MIN, 1f)
    val px = density * scale
    val controls = remember(pad, containerSize, px) {
        padControls(pad, containerSize.width / px, containerSize.height / px).filter { !it.hidden }
    }
    // No pointer input on this box: a finger between the controls passes through to the
    // gesture layer beneath, which is the whole of the fall-through rule.
    Box(modifier.fillMaxSize()) {
        for (c in controls) {
            key(c.id) { PadControlView(c, scale, px, opacity, sink, haptics) }
        }
    }
}

/** The editor's hooks into one control: selection, and the dragged centre in container px.
 *  One stable instance per control — the drag coroutine holds it across recompositions, so
 *  everything live is read through a lambda, never captured as a value. */
internal class PadEdit(
    val selected: () -> Boolean,
    val select: () -> Unit,
    val moved: (centrePx: Offset) -> Unit,
)

@Composable
private fun PadControlView(
    ctl: PadControl,
    scale: Float,
    px: Float,
    opacity: Float,
    sink: PadSink,
    haptics: ConsoleHaptics,
    edit: PadEdit? = null,
) {
    val st = remember(ctl, px, sink) {
        when (ctl) {
            is PadControl.Buttons -> ButtonsTouch(ctl, px, sink, haptics)
            is PadControl.Dpad -> DpadTouch(ctl, px, sink, haptics)
            is PadControl.Stick -> StickTouch(ctl, px, sink)
            is PadControl.Trigger -> TriggerTouch(ctl, px, sink, haptics)
            is PadControl.Ring -> RingTouch(ctl, px, sink, haptics)
        }
    }
    val r = ctl.rect
    val name = ctl.label.text()
    // The drag's live offset, px; zero outside the editor. Keyed on the control: the commit
    // rebuilds the control at its new rect, and the offset must not apply twice.
    var drag by remember(ctl) { mutableStateOf(Offset.Zero) }
    Box(
        Modifier
            .absoluteOffset((r.x * scale).dp, (r.y * scale).dp)
            .offset { IntOffset(drag.x.roundToInt(), drag.y.roundToInt()) }
            .size((r.w * scale).dp, (r.h * scale).dp)
            .alpha(
                when {
                    edit != null && ctl.hidden -> opacity * 0.35f
                    edit != null -> opacity
                    st.active -> (opacity + 0.35f).coerceAtMost(1f)
                    else -> opacity
                },
            )
            .then(if (edit?.selected?.invoke() == true) Modifier.border(2.dp, Color.White, RoundedCornerShape(12.dp)) else Modifier)
            .semantics { contentDescription = name }
            .pointerInput(st, edit) {
                if (edit == null) {
                    awaitPointerEventScope {
                        try {
                            while (true) {
                                val ev = awaitPointerEvent()
                                for (ch in ev.changes) {
                                    when {
                                        ch.changedToDownIgnoreConsumed() -> st.down(ch.id, ch.position)
                                        ch.changedToUpIgnoreConsumed() -> st.up(ch.id)
                                        ch.pressed && ch.positionChangedIgnoreConsumed() -> st.move(ch.id, ch.position)
                                    }
                                    ch.consume()
                                }
                            }
                        } finally {
                            // The layer left (hidden, rotated, or the session ended) mid-touch.
                            st.reset()
                        }
                    }
                } else {
                    detectDragGestures(
                        onDragStart = { edit.select() },
                        onDrag = { ch, amount -> ch.consume(); drag += amount },
                        onDragEnd = {
                            edit.moved(Offset((r.x + r.w / 2) * px, (r.y + r.h / 2) * px) + drag)
                            drag = Offset.Zero
                        },
                        onDragCancel = { drag = Offset.Zero },
                    )
                }
            }
            .pointerInput(edit) {
                if (edit != null) detectTapGestures(onTap = { edit.select() })
            },
    ) {
        when (st) {
            is ButtonsTouch -> Discs(st, scale)
            is DpadTouch -> Cross(st)
            is StickTouch -> StickFace(st, scale)
            is TriggerTouch -> Pill(st, scale)
            is RingTouch -> RingFace(st, scale)
        }
    }
}

private val FILL = Color.White.copy(alpha = 0.16f)
private val FILL_ON = Color.White.copy(alpha = 0.55f)
private val EDGE = Color.White.copy(alpha = 0.75f)

// ---- per-control finger state: what each does with the fingers it owns ----

private sealed class PadTouch {
    /** Pressed or deflected: the control draws brighter. */
    var active by mutableStateOf(false)
    abstract fun down(id: PointerId, p: Offset)
    abstract fun move(id: PointerId, p: Offset)
    abstract fun up(id: PointerId)
    abstract fun reset()
}

/** Controls that resolve to a set of button bits: the union over every finger, sent on change. */
private sealed class BitsTouch(private val sink: PadSink, private val haptics: ConsoleHaptics) : PadTouch() {
    private val fingers = HashMap<PointerId, Int>()
    var held by mutableIntStateOf(0)
        private set

    abstract fun hit(p: Offset): Int

    override fun down(id: PointerId, p: Offset) { fingers[id] = hit(p); sync() }
    override fun move(id: PointerId, p: Offset) { if (fingers.containsKey(id)) { fingers[id] = hit(p); sync() } }
    override fun up(id: PointerId) { fingers.remove(id); sync() }
    override fun reset() { fingers.clear(); sync() }

    private fun sync() {
        var next = 0
        for (b in fingers.values) next = next or b
        if (next == held) return
        var changed = next xor held
        while (changed != 0) {
            val bit = changed and -changed
            val down = next and bit != 0
            sink.button(bit, down)
            if (down) haptics.tick()
            changed = changed and bit.inv()
        }
        held = next
        active = next != 0
    }
}

private class ButtonsTouch(val c: PadControl.Buttons, private val px: Float, sink: PadSink, haptics: ConsoleHaptics) :
    BitsTouch(sink, haptics) {
    /** The nearest disc the finger is within slop of; 0 between discs. */
    override fun hit(p: Offset): Int {
        var best = 0
        var bestD = Float.MAX_VALUE
        for (d in c.discs) {
            val dist = hypot(p.x - d.cx * px, p.y - d.cy * px)
            if (dist <= d.r * px * HIT_SLOP && dist < bestD) { best = d.bit; bestD = dist }
        }
        return best
    }
}

private class DpadTouch(c: PadControl.Dpad, px: Float, sink: PadSink, haptics: ConsoleHaptics) : BitsTouch(sink, haptics) {
    private val centre = c.rect.w * px / 2
    private val dead = DPAD_DEAD * px * c.sc
    override fun hit(p: Offset): Int = dpadBits(p.x - centre, p.y - centre, dead)
}

private class StickTouch(val c: PadControl.Stick, px: Float, private val sink: PadSink) : PadTouch() {
    private var owner: PointerId? = null
    private var origin = Offset.Zero
    private var lastX = 0
    private var lastY = 0
    private val radius = STICK_R * px * c.sc
    private val dead = STICK_DEAD * px * c.sc
    /** The knob's offset from the base centre, px. */
    var knob by mutableStateOf(Offset.Zero)
        private set

    override fun down(id: PointerId, p: Offset) {
        if (owner != null) return
        owner = id
        origin = p
        emit(Offset.Zero)
    }
    override fun move(id: PointerId, p: Offset) { if (id == owner) emit(p - origin) }
    override fun up(id: PointerId) { if (id == owner) { owner = null; emit(Offset.Zero) } }
    override fun reset() { owner = null; emit(Offset.Zero) }

    private fun emit(d: Offset) {
        val (x, y) = stickWire(d.x, d.y, radius, dead)
        if (x != lastX) { sink.axis(c.axisX, x); lastX = x }
        if (y != lastY) { sink.axis(c.axisY, y); lastY = y }
        knob = Offset(x / 32767f * radius, -y / 32767f * radius)
        active = owner != null
    }
}

private class TriggerTouch(val c: PadControl.Trigger, px: Float, private val sink: PadSink, private val haptics: ConsoleHaptics) :
    PadTouch() {
    private var owner: PointerId? = null
    private var last = 0
    private val h = c.rect.h * px
    /** 0…1, how far down the pill the finger is. */
    var pull by mutableFloatStateOf(0f)
        private set

    override fun down(id: PointerId, p: Offset) {
        if (owner != null) return
        owner = id
        haptics.tick()
        emit(p.y)
    }
    override fun move(id: PointerId, p: Offset) { if (id == owner) emit(p.y) }
    override fun up(id: PointerId) { if (id == owner) { owner = null; emit(0f) } }
    override fun reset() { owner = null; emit(0f) }

    private fun emit(y: Float) {
        val v = triggerWire(y, h)
        if (v != last) { sink.axis(c.axis, v); last = v }
        pull = v / 255f
        active = v > 0
    }
}

/** The first finger owns it; lifting inside the button opens the ring. */
private class RingTouch(val c: PadControl.Ring, px: Float, private val sink: PadSink, private val haptics: ConsoleHaptics) :
    PadTouch() {
    private var owner: PointerId? = null
    private var inside = false
    private val w = c.rect.w * px
    private val h = c.rect.h * px

    override fun down(id: PointerId, p: Offset) {
        if (owner != null) return
        owner = id
        inside = true
        active = true
        haptics.tick()
    }
    override fun move(id: PointerId, p: Offset) { if (id == owner) inside = p.x in 0f..w && p.y in 0f..h }
    override fun up(id: PointerId) {
        if (id != owner) return
        reset()
        if (inside) sink.ring()
    }
    override fun reset() { owner = null; active = false }
}

// ---- how each kind draws ----

@Composable
private fun BoxScope.Discs(st: ButtonsTouch, scale: Float) {
    for (d in st.c.discs) {
        val on = st.held and d.bit != 0
        Box(
            Modifier
                .absoluteOffset(((d.cx - d.r) * scale).dp, ((d.cy - d.r) * scale).dp)
                .size((2 * d.r * scale).dp)
                .clip(CircleShape)
                .background(if (on) FILL_ON else FILL)
                .border(1.5.dp, EDGE, CircleShape),
            contentAlignment = Alignment.Center,
        ) {
            Text(d.glyph, color = Color.White, fontSize = (d.r * 0.7f * scale).sp, fontWeight = FontWeight.Bold)
        }
    }
}

@Composable
private fun BoxScope.Cross(st: DpadTouch) {
    Canvas(Modifier.fillMaxSize()) {
        val s = size.minDimension
        val arm = s * 0.34f
        val c = s / 2
        val corner = CornerRadius(arm / 4)
        val vertical = Offset(c - arm / 2, 0f) to Size(arm, s)
        val horizontal = Offset(0f, c - arm / 2) to Size(s, arm)
        drawRoundRect(FILL, vertical.first, vertical.second, corner)
        drawRoundRect(FILL, horizontal.first, horizontal.second, corner)
        val stroke = Stroke(1.5.dp.toPx())
        drawRoundRect(EDGE, vertical.first, vertical.second, corner, style = stroke)
        drawRoundRect(EDGE, horizontal.first, horizontal.second, corner, style = stroke)
        val reach = c - arm / 2
        if (st.held and Gamepad.BTN_DPAD_UP != 0) drawRoundRect(FILL_ON, Offset(c - arm / 2, 0f), Size(arm, reach), corner)
        if (st.held and Gamepad.BTN_DPAD_DOWN != 0) drawRoundRect(FILL_ON, Offset(c - arm / 2, c + arm / 2), Size(arm, reach), corner)
        if (st.held and Gamepad.BTN_DPAD_LEFT != 0) drawRoundRect(FILL_ON, Offset(0f, c - arm / 2), Size(reach, arm), corner)
        if (st.held and Gamepad.BTN_DPAD_RIGHT != 0) drawRoundRect(FILL_ON, Offset(c + arm / 2, c - arm / 2), Size(reach, arm), corner)
    }
}

@Composable
private fun BoxScope.StickFace(st: StickTouch, scale: Float) {
    Box(
        Modifier
            .align(Alignment.Center)
            .size((2 * STICK_R * scale * st.c.sc).dp)
            .clip(CircleShape)
            .background(Color.White.copy(alpha = 0.12f))
            .border(1.5.dp, EDGE, CircleShape),
    )
    Box(
        Modifier
            .align(Alignment.Center)
            .absoluteOffset { IntOffset(st.knob.x.roundToInt(), st.knob.y.roundToInt()) }
            .size((2 * STICK_KNOB_R * scale * st.c.sc).dp)
            .clip(CircleShape)
            .background(FILL_ON),
    )
}

@Composable
private fun BoxScope.Pill(st: TriggerTouch, scale: Float) {
    val shape = RoundedCornerShape(50)
    Box(
        Modifier
            .fillMaxSize()
            .clip(shape)
            .background(FILL)
            .border(1.5.dp, EDGE, shape),
    ) {
        if (st.pull > 0f) {
            Box(Modifier.align(Alignment.TopCenter).fillMaxWidth().fillMaxHeight(st.pull).background(FILL_ON))
        }
        Text(
            if (st.c.axis == Gamepad.AXIS_LT) "LT" else "RT",
            Modifier.align(Alignment.Center),
            color = Color.White,
            fontSize = (15f * scale * st.c.sc).sp,
            fontWeight = FontWeight.Bold,
        )
    }
}

@Composable
private fun BoxScope.RingFace(st: RingTouch, scale: Float) {
    Box(
        Modifier
            .fillMaxSize()
            .clip(CircleShape)
            .background(if (st.active) FILL_ON else FILL)
            .border(1.5.dp, EDGE, CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Text("•••", color = Color.White, fontSize = (st.c.rect.w * 0.3f * scale).sp, fontWeight = FontWeight.Bold)
    }
}

/**
 * The layout editor: the pad's own controls over a stand-in backdrop — the same
 * [PadControlView] the stream mounts, so the editor cannot drift from the layer it edits.
 * Drag moves a control; tap selects it for the size slider and the visibility toggle; hidden
 * controls stay visible here, ghosted. The overrides land in the class the screen is in (wide
 * or narrow, [padControls]' own split), so a phone edited in landscape keeps its upright
 * preset untouched. The chrome sits where the trio of small discs is not: they ride the bottom
 * edge on a wide layer and the top edge on a narrow one.
 */
@Composable
internal fun PadLayoutEditor(pad: PadConfig, onChange: (PadConfig) -> Unit, onBack: () -> Unit) {
    // Its own window: the editor must cover the whole app shell — the bottom tab bar on a
    // phone, the navigation rail on a tablet — which no composable inside a tab can. Back
    // dismisses through the dialog, so no BackHandler of its own.
    Dialog(
        onDismissRequest = onBack,
        properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
    ) {
        val dialogWindow = (LocalView.current.parent as? DialogWindowProvider)?.window
        // The stage is the stream's canvas, not a settings pane: system bars hidden the way the
        // stream hides them, on the editor's own window so the activity's bars come back by
        // themselves. Phones hold landscape as the stream does; tablets keep the upright layout.
        DisposableEffect(Unit) {
            val bars = dialogWindow?.let { WindowCompat.getInsetsController(it, it.decorView) }
            bars?.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            bars?.hide(WindowInsetsCompat.Type.systemBars())
            onDispose {}
        }
        HoldLandscape("pad-editor", true)
        PadLayoutStage(pad, onChange, onBack)
    }
}

@Composable
private fun PadLayoutStage(pad: PadConfig, onChange: (PadConfig) -> Unit, onBack: () -> Unit) {
    var selected by remember { mutableStateOf<String?>(null) }
    var containerSize by remember { mutableStateOf(IntSize.Zero) }
    val density = LocalDensity.current.density
    val scale = pad.scale.coerceIn(PAD_SCALE_MIN, PAD_SCALE_MAX)
    val px = density * scale
    val narrow = containerSize.width / px < NARROW
    fun tweaks() = if (narrow) pad.controlsNarrow else pad.controls
    fun write(map: Map<String, PadTweak>) =
        onChange(if (narrow) pad.copy(controlsNarrow = map) else pad.copy(controls = map))
    fun update(id: String, f: (PadTweak) -> PadTweak) = write(tweaks() + (id to f(tweaks()[id] ?: PadTweak())))
    // The stable funnels the per-control [PadEdit]s call: those instances outlive many
    // recompositions inside the drag coroutine, so nothing there may capture this pass's `pad`.
    val liveSelect = rememberUpdatedState<(String) -> Unit> { selected = it }
    val liveMoved = rememberUpdatedState<(String, Offset) -> Unit> { id, centre ->
        val s = containerSize
        if (s != IntSize.Zero) {
            update(id) { it.copy(x = (centre.x / s.width).coerceIn(0f, 1f), y = (centre.y / s.height).coerceIn(0f, 1f)) }
        }
    }
    val sink = remember { PadSink({ _, _ -> }, { _, _ -> }) }
    val haptics = rememberConsoleHaptics()
    val controls = remember(pad, containerSize, px) {
        if (containerSize == IntSize.Zero) emptyList()
        else padControls(pad, containerSize.width / px, containerSize.height / px)
    }
    Box(
        Modifier
            .fillMaxSize()
            .background(Brush.verticalGradient(listOf(Color(0xFF15191E), Color(0xFF243039))))
            .onSizeChanged { containerSize = it }
            .pointerInput(Unit) { detectTapGestures(onTap = { selected = null }) },
    ) {
        for (c in controls) {
            key(c.id) {
                val edit = remember(c.id) {
                    PadEdit(
                        selected = { selected == c.id },
                        select = { liveSelect.value(c.id) },
                        moved = { centre -> liveMoved.value(c.id, centre) },
                    )
                }
                PadControlView(c, scale, px, pad.opacity.coerceIn(PAD_OPACITY_MIN, 1f), sink, haptics, edit)
            }
        }
        Column(
            Modifier
                .align(if (narrow) Alignment.BottomCenter else Alignment.TopCenter)
                .padding(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Surface(shape = RoundedCornerShape(16.dp), color = Color(0xE617202A), contentColor = Color.White) {
                Row(
                    Modifier.padding(start = 4.dp, end = 8.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.common_back), tint = Color.White)
                    }
                    Column(Modifier.padding(end = 12.dp)) {
                        Text(stringResource(R.string.pad_layout_title), style = MaterialTheme.typography.titleMedium)
                        Text(
                            stringResource(if (narrow) R.string.pad_layout_editing_upright else R.string.pad_layout_editing_wide),
                            style = MaterialTheme.typography.bodySmall,
                            color = Color.White.copy(alpha = 0.7f),
                        )
                    }
                    TextButton(onClick = { write(emptyMap()); selected = null }) { Text(stringResource(R.string.pad_layout_reset)) }
                }
            }
            val ctl = selected?.let { id -> controls.firstOrNull { it.id == id } }
            if (ctl == null) {
                Text(
                    stringResource(R.string.pad_layout_hint),
                    style = MaterialTheme.typography.bodySmall,
                    color = Color.White.copy(alpha = 0.7f),
                )
            } else {
                Surface(shape = RoundedCornerShape(16.dp), color = Color(0xE617202A), contentColor = Color.White) {
                    Column(Modifier.padding(horizontal = 16.dp, vertical = 8.dp).width(300.dp)) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Text(ctl.label.text(), Modifier.weight(1f), style = MaterialTheme.typography.titleSmall)
                            TextButton(onClick = { update(ctl.id) { it.copy(hidden = !ctl.hidden) } }) {
                                Text(stringResource(if (ctl.hidden) R.string.pad_layout_show else R.string.pad_layout_hide))
                            }
                            TextButton(onClick = { write(tweaks() - ctl.id) }) { Text(stringResource(R.string.common_reset)) }
                        }
                        var sz by remember(ctl) { mutableFloatStateOf(ctl.sc) }
                        Text(
                            stringResource(R.string.pad_layout_size) + " · " +
                                stringResource(R.string.common_percent, (sz * 100).roundToInt()),
                            style = MaterialTheme.typography.bodySmall,
                        )
                        Slider(
                            value = sz,
                            onValueChange = { sz = it },
                            valueRange = PAD_TWEAK_SCALE_MIN..PAD_TWEAK_SCALE_MAX,
                            onValueChangeFinished = { update(ctl.id) { it.copy(scale = sz) } },
                        )
                    }
                }
            }
        }
    }
}
