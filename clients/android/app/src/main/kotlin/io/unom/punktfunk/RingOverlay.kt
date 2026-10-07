package io.unom.punktfunk

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.snap
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.VolumeOff
import androidx.compose.material.icons.automirrored.filled.VolumeUp
import androidx.compose.material.icons.filled.Bedtime
import androidx.compose.material.icons.filled.Cancel
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.DoNotTouch
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Insights
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material.icons.filled.Logout
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material.icons.filled.MoreHoriz
import androidx.compose.material.icons.filled.Mouse
import androidx.compose.material.icons.filled.PanTool
import androidx.compose.material.icons.filled.PowerSettingsNew
import androidx.compose.material.icons.filled.RestartAlt
import androidx.compose.material.icons.filled.SpaceDashboard
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.SwapVert
import androidx.compose.material.icons.filled.TextFields
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material.icons.filled.VideogameAsset
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.RingNav
import io.unom.punktfunk.kit.library.RunningGame
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlin.math.atan2
import kotlin.math.cos
import kotlin.math.hypot
import kotlin.math.roundToInt
import kotlin.math.sin

/*
 * The quick-action ring (design/touch-client-overlay.md §2): six round buttons on a circle under
 * the fingers plus a centre "More" that opens the sheet — the complete catalogue with values.
 * The two-finger twist drives the opening frame by frame ([RingState.turn]); the back gesture
 * opens it at the screen centre. Closed, it leaves the composition entirely (tenet 1).
 */

private val RING_RADIUS = 120.dp
private val SLOT_D = 56.dp
private val CENTRE_D = 64.dp
private const val IDLE_CLOSE_MS = 8_000L
private const val ARM_MS = 2_000L
private const val HINT_MS = 2_000L
/** Button k lags the previous one by this much of the twist, so the ring visibly unwinds. */
private const val SLOT_LAG = 0.06f

/** The ring's open/closed state and what it is showing. One per stream; the overlay reads it. */
class RingState {
    /** 0 closed … 1 open — driven by the twist until [committed]. */
    var progress by mutableStateOf(0f)
    var committed by mutableStateOf(false)
    var clockwise by mutableStateOf(true)
    /** Container px; the ring is centred here, clamped so it stays on screen. */
    var centre by mutableStateOf(Offset.Zero)
    var sheet by mutableStateOf(false)
    /** A destructive slot awaiting its second press (the slot id). */
    var armed by mutableStateOf<String?>(null)
    /** The label under the ring: a slot's name, why it is unavailable, or "press again". */
    var hint by mutableStateOf<String?>(null)
    var lastTouch by mutableLongStateOf(0L)
    private var twistArmed = false
    /** The pad's highlight: a slot 0…5, or 6 for the centre (the initial one — `Select+A`
     *  then A opens the sheet in two presses). `null` until a pad moves it. */
    var highlight by mutableStateOf<Int?>(null)
    /** The sheet row the pad is on. */
    var sheetCursor by mutableIntStateOf(0)
    /** The mode at first open — the Resolution row's "Native". */
    var nativeMode: IntArray? = null
    /** A pad press awaiting the overlay ([RingOverlay] consumes it); [navSeq] makes each one an event. */
    var pendingNav by mutableStateOf<RingNav?>(null)
    var navSeq by mutableIntStateOf(0)
    /** Open/close edges, for the shell: the pad router masks itself while the ring is up. */
    var onOpenChange: ((Boolean) -> Unit)? = null
    /** The wire pad whose `Select+A` opened the ring; null for a touch or key open. */
    var opener: Int? = null

    fun nav(n: RingNav) {
        pendingNav = n
        navSeq++
    }

    val visible: Boolean get() = committed || progress > 0f

    /** Returns true on the first turn of a twist — the moment the dial arms, worth one tick. */
    fun turn(p: Float, cw: Boolean, x: Float, y: Float): Boolean {
        if (committed) return false
        progress = p
        clockwise = cw
        centre = Offset(x, y)
        val first = !twistArmed
        twistArmed = true
        return first
    }

    fun commit() {
        val was = committed
        committed = true
        progress = 1f
        touch()
        if (!was) onOpenChange?.invoke(true)
    }

    /** The twist lifted short of commit, or wound back after one: the ring winds back in. */
    fun cancel() = close()

    fun openAt(c: Offset) {
        centre = c
        commit()
    }

    fun close() {
        val was = committed
        committed = false
        progress = 0f
        sheet = false
        armed = null
        hint = null
        highlight = null
        twistArmed = false
        opener = null
        if (was) onOpenChange?.invoke(false)
    }

    fun touch() {
        lastTouch = System.currentTimeMillis()
    }
}

/** What the ring can do this session — the shell's live state and commands behind each slot. */
class RingActions(
    val endStream: () -> Unit,
    val disconnectLinger: () -> Unit,
    val touchMode: () -> TouchMode,
    val cycleTouchMode: () -> Unit,
    val keyboardGranted: () -> Boolean,
    val keyboard: () -> Unit,
    val textSupported: Boolean,
    val sendText: (String) -> Unit,
    val stats: () -> StatsVerbosity,
    val cycleStats: () -> Unit,
    val micAvailable: () -> Boolean,
    val micMuted: () -> Boolean,
    val toggleMic: () -> Unit,
    val hostActions: () -> List<HostActions.Action>,
    val invokeHost: (HostActions.Action) -> Unit,
    val sendShortcut: (List<String>) -> Unit,
    /** The virtual controller (§4): whether its input can reach the host, whether it is up, and the toggle. */
    val padAvailable: () -> Boolean,
    val padShown: () -> Boolean,
    val togglePad: () -> Unit,
    /** One synthetic system-button tap on the host's pad (a `Gamepad.BTN_*` bit). */
    val tapPadButton: (Int) -> Unit,
    /** Controller mouse: the pointer grant it needs, the wire pads it acts on (a bit per pad,
     *  `0` = none), the mode they share (`NativeBridge.nativePadMouseMode`), and the step to the
     *  next one. */
    val pointerGranted: () -> Boolean,
    val padMouseTarget: () -> Int,
    val padMouseMode: () -> Int,
    val cyclePadMouse: () -> Unit,
    /** This device's speakers: the live mute mask (`StreamUi.AUDIO_MUTE_*`), the sentence for it,
     *  and the local-only toggle. */
    val audioMute: () -> Int,
    val audioMuteLabel: () -> String?,
    val toggleStreamMute: () -> Unit,
    /** `[w, h, hz]` as last requested (Android has no live read-back of the negotiated mode). */
    val currentMode: () -> IntArray,
    val requestMode: (Int, Int, Int) -> Unit,
    val scrollInverted: () -> Boolean = { false },
    val toggleScrollInversion: () -> Unit = {},
    /** The game this device launched that this stream plays ([RunningGame.streamedHere]); null
     *  offers no End game. */
    val streamedGame: () -> RunningGame? = { null },
    /** End that game on the host, then the stream. */
    val endGame: () -> Unit = {},
    /** A second screen's layouts this stream can build (one or none = a single screen), the
     *  live one, and the step to the next. */
    val screenLayouts: () -> List<ScreenLayout> = { emptyList() },
    val screenLayout: () -> ScreenLayout = { ScreenLayout.PANEL },
    val cycleScreens: () -> Unit = {},
    /** Controller type: the GamepadPref every controller declares, whether one is forwarded, and
     *  the step to the next type ([nextPadType]) for this stream. */
    val padType: () -> Int = { Gamepad.PREF_AUTO },
    val padTypeAvailable: () -> Boolean = { false },
    val cyclePadType: () -> Unit = {},
)

/**
 * The editor's hooks (design §3.3): a tap on a slot picks its action instead of firing it, and a
 * disc dragged onto another slot swaps the two. Null in-stream.
 */
class RingEditing(val pick: (Int) -> Unit, val swap: (Int, Int) -> Unit)

/** One button as the ring draws it: glyph or keycap chip, its state, and why it is dimmed. */
internal data class SlotSpec(
    val id: String,
    val label: String,
    val icon: ImageVector? = null,
    val chip: String? = null,
    val enabled: Boolean = true,
    val reason: String = "",
    /** Destructive: two presses. */
    val armed: Boolean = false,
    /** A toggle leaves the ring open so the new state is visible (D6). */
    val toggle: Boolean = false,
    val state: String = "",
)

/** The controller-mouse slot's state for a `nativePadMouseMode` value; the Rust and Apple rings
 *  say the same. */
internal fun padMouseState(mode: Int): String = when (mode) {
    1 -> "Touchpad"
    2 -> "Full"
    else -> "Off"
}

private val padMarks = HashMap<Int, ImageVector?>()

/** The console's outline for a GamepadPref wire byte, stroked at its 1.5 weight; null for
 *  Automatic, or off-device where the native library is absent. Built once per type. */
internal fun padMark(pref: Int): ImageVector? = padMarks.getOrPut(pref) {
    val d = runCatching { NativeBridge.nativePadMark(pref) }.getOrDefault("")
    if (d.isEmpty()) return@getOrPut null
    ImageVector.Builder(
        defaultWidth = 24.dp, defaultHeight = 24.dp, viewportWidth = 24f, viewportHeight = 24f,
    ).addPath(
        pathData = PathParser().parsePathString(d).toNodes(),
        stroke = SolidColor(Color.White),
        strokeLineWidth = 1.5f,
        strokeLineCap = StrokeCap.Round,
        strokeLineJoin = StrokeJoin.Round,
    ).build()
}

internal fun spec(slot: SlotId, cfg: OverlayConfig, a: RingActions): SlotSpec = when (slot) {
    SlotId.EndStream -> SlotSpec("end_stream", "End stream", Icons.Filled.Close, armed = true)
    SlotId.EndGame -> SlotSpec(
        "end_game", "End game", Icons.Filled.Cancel,
        enabled = a.streamedGame() != null,
        reason = "No game this device launched is running here",
        armed = true,
    )
    SlotId.DisconnectLinger ->
        SlotSpec("disconnect_linger", "Disconnect, keep the game running", Icons.Filled.Logout)
    SlotId.TouchMode -> {
        val m = a.touchMode()
        SlotSpec(
            "touch_mode", "Touch mode",
            when (m) {
                TouchMode.TRACKPAD -> Icons.Filled.TouchApp
                TouchMode.POINTER -> Icons.Filled.Mouse
                TouchMode.TOUCH -> Icons.Filled.PanTool
                TouchMode.OFF -> Icons.Filled.DoNotTouch
            },
            toggle = true, state = m.name.lowercase().replaceFirstChar { it.uppercase() },
        )
    }
    SlotId.Keyboard -> SlotSpec(
        "keyboard", "Keyboard", Icons.Filled.Keyboard,
        enabled = a.keyboardGranted(), reason = "Keyboard input is not granted for this session",
    )
    SlotId.Stats -> SlotSpec(
        "stats", "Statistics", Icons.Filled.Insights, toggle = true, state = a.stats().label,
    )
    SlotId.Mic -> SlotSpec(
        "mic", "Microphone", if (a.micMuted()) Icons.Filled.MicOff else Icons.Filled.Mic,
        enabled = a.micAvailable(), reason = "No microphone is running this session",
        toggle = true, state = if (a.micMuted()) "Muted" else "On",
    )
    SlotId.Pad -> SlotSpec(
        "pad", "Virtual controller", Icons.Filled.SportsEsports,
        enabled = a.padAvailable(), reason = "Controller input is not forwarded this session",
        toggle = true, state = if (a.padShown()) "On" else "Off",
    )
    SlotId.SendText -> SlotSpec(
        "send_text", "Send text", Icons.Filled.TextFields,
        enabled = a.textSupported && a.keyboardGranted(),
        reason = "This host does not take typed text",
    )
    SlotId.Guide -> SlotSpec(
        "guide", "Guide button", Icons.Filled.Home,
        enabled = a.padAvailable(), reason = "Controller input is not forwarded this session",
    )
    SlotId.Qam -> SlotSpec(
        "qam", "Quick access menu", Icons.Filled.SpaceDashboard,
        enabled = a.padAvailable(), reason = "Controller input is not forwarded this session",
    )
    SlotId.PadMouse -> SlotSpec(
        "pad_mouse", "Controller mouse", Icons.Filled.Mouse,
        enabled = a.pointerGranted() && a.padMouseTarget() != 0,
        reason = if (a.pointerGranted()) "No controller is connected" else "This host only allows controller input",
        toggle = true, state = padMouseState(a.padMouseMode()),
    )
    // Automatic keeps a plain gamepad; a picked type shows its outline.
    SlotId.PadType -> SlotSpec(
        "pad_type", "Controller type", padMark(a.padType()) ?: Icons.Filled.VideogameAsset,
        enabled = a.padTypeAvailable(), reason = "No controller is connected",
        toggle = true, state = padTypeLabel(a.padType()),
    )
    SlotId.StreamMute -> SlotSpec(
        "stream_mute", "Mute this stream",
        if (a.audioMute() != 0) Icons.AutoMirrored.Filled.VolumeOff else Icons.AutoMirrored.Filled.VolumeUp,
        // The state describes the mute: "Off" while audible, else whose mute it is.
        toggle = true, state = a.audioMuteLabel() ?: "Off",
    )
    // The wire id predates the three layouts; every client's ring stores it.
    SlotId.SwapScreens -> SlotSpec(
        "swap_screens", "Screens", Icons.Filled.SwapVert,
        enabled = a.screenLayouts().size > 1, reason = "This device has one screen",
        toggle = true, state = a.screenLayout().label,
    )
    is SlotId.Host -> {
        val act = a.hostActions().firstOrNull { it.id == slot.actionId }
        // Three power actions, three glyphs — the same icon on all three made them one button.
        val icon = when (slot.actionId) {
            "power.sleep" -> Icons.Filled.Bedtime
            "power.reboot" -> Icons.Filled.RestartAlt
            else -> Icons.Filled.PowerSettingsNew
        }
        SlotSpec(
            "host:${slot.actionId}", act?.label ?: slot.actionId, icon,
            enabled = act?.available == true,
            reason = act?.unavailableReason?.ifEmpty { null } ?: "This host doesn't offer it",
            armed = act?.danger ?: true,
        )
    }
    is SlotId.Shortcut -> {
        val s = cfg.shortcut(slot.shortcutId)
        SlotSpec(
            "shortcut:${slot.shortcutId}", s?.label?.ifEmpty { null } ?: chordChip(s?.keys.orEmpty()),
            chip = chordChip(s?.keys.orEmpty()),
            enabled = a.keyboardGranted() && s?.keys?.all { keyVk(it) != null } == true,
            reason = if (a.keyboardGranted()) "A key in this chord is unknown" else "Keyboard input is not granted for this session",
        )
    }
}

/** The stick's sector read as a list direction for the sheet: 12 o'clock is up, 6 o'clock down,
 *  the right half of the ring right and the left half left. Neutral moves no cursor. */
private fun sheetDir(slot: Int?): RingNav? = when (slot) {
    null -> null
    0 -> RingNav.Up
    3 -> RingNav.Down
    1, 2 -> RingNav.Right
    else -> RingNav.Left
}

/**
 * The ring and its sheet. Sits above the gesture layer, so its buttons take the finger first;
 * a tap on the scrim outside closes it. Composed only while [RingState.visible].
 */
@Composable
fun RingOverlay(
    state: RingState,
    cfg: OverlayConfig,
    actions: RingActions,
    containerSize: IntSize,
    haptics: ConsoleHaptics,
    modifier: Modifier = Modifier,
    /** Set by the settings editor; null in-stream. */
    editing: RingEditing? = null,
) {
    if (!state.visible) return
    val density = LocalDensity.current
    val radiusPx = with(density) { RING_RADIUS.toPx() }
    val slotPx = with(density) { SLOT_D.toPx() }
    val marginPx = radiusPx + slotPx / 2 + with(density) { 16.dp.toPx() }
    // Clamped into the container so the whole ring is always on screen; a container narrower
    // than two margins (the editor's stage) centres it instead of pinning it to one side.
    val cx = if (containerSize.width < 2 * marginPx) containerSize.width / 2f
        else state.centre.x.coerceIn(marginPx, containerSize.width - marginPx)
    val cy = if (containerSize.height < 2 * marginPx) containerSize.height / 2f
        else state.centre.y.coerceIn(marginPx, containerSize.height - marginPx)

    // The twist drives the opening frame by frame; the commit settles with a spring; a close
    // shrinks it into the centre.
    val shown = remember { Animatable(0f) }
    LaunchedEffect(state.progress, state.committed) {
        when {
            state.committed -> shown.animateTo(1f, spring(dampingRatio = 0.55f, stiffness = Spring.StiffnessMediumLow))
            state.progress <= 0f -> shown.animateTo(0f, tween(200))
            else -> shown.snapTo(state.progress)
        }
    }
    // Idle: the exit disc's 8 s rule, for the same latency reason — unless the sheet is up.
    LaunchedEffect(state.committed, state.lastTouch, state.sheet) {
        if (state.committed && !state.sheet && editing == null) {
            delay(IDLE_CLOSE_MS)
            state.close()
        }
    }
    ExpireRingHint(state)
    var textDialog by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) { if (state.nativeMode == null) state.nativeMode = actions.currentMode() }
    val rows = if (state.sheet) sheetRows(state, cfg, actions, haptics) { textDialog = true } else emptyList()

    val fire = { s: SlotSpec, slot: SlotId -> fireSlot(s, slot, state, cfg, actions, haptics) { textDialog = true } }

    // The pad (design §2.6): the left stick AIMS — its sector is the highlight, so the ring
    // follows the thumb — while the D-pad steps, Right clockwise, Left anticlockwise, Up to 12
    // o'clock, Down to 6; Y returns the highlight to the centre, A fires it (the centre opens
    // the sheet), B closes. In the sheet, Up/Down walk the rows and Left/Right adjust one.
    LaunchedEffect(state.navSeq) {
        val n = state.pendingNav ?: return@LaunchedEffect
        state.pendingNav = null
        state.touch()
        navigate(n, state, rows, cfg, actions, haptics, fire)
    }

    Box(
        modifier
            .fillMaxSize()
            // The scrim: a tap outside the ring closes it, and nothing reaches the stream while
            // it is open. No backdrop blur — a blur over a video surface is a full-screen pass.
            // The editor has no stream under it and draws no scrim.
            .background(Color.Black.copy(alpha = if (editing == null) 0.18f * shown.value else 0f))
            // In the editor the backdrop under the ring owns the twist; the scrim takes nothing.
            .then(
                if (editing == null) {
                    Modifier.clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) { if (state.sheet) state.sheet = false else state.close() }
                } else {
                    Modifier
                },
            ),
    ) {
        val slotHalf = slotPx / 2
        val edit = remember { SlotDrag() }
        val scope = rememberCoroutineScope()
        cfg.ring.forEachIndexed { k, slot ->
            val q = ((shown.value - k * SLOT_LAG) / (1f - 5 * SLOT_LAG)).coerceIn(0f, 1f)
            if (q <= 0f) return@forEachIndexed
            // Slot k sits at 12, 2, 4… o'clock; it travels out along a short spiral that turns
            // the way the hand turns. `order` redirects a disc to another slot while a swap plays.
            val turn = if (state.clockwise) -40f else 40f
            val deg = -90f + 60f * edit.order[k] + (1f - q) * turn
            val rad = Math.toRadians(deg.toDouble())
            val carried = edit.carried(k)
            val targetX = cx + radiusPx * q * cos(rad).toFloat() - slotHalf + carried.x
            val targetY = cy + radiusPx * q * sin(rad).toFloat() - slotHalf + carried.y
            // Only a swap animates the position; the twist and a drag follow the finger.
            val swapSpring = if (edit.swapping) spring<Float>(dampingRatio = 0.82f, stiffness = Spring.StiffnessMedium) else snap()
            val x by animateFloatAsState(targetX, swapSpring, label = "slotX")
            val y by animateFloatAsState(targetY, swapSpring, label = "slotY")
            val s = slot?.let { spec(it, cfg, actions) }
            val dragModifier = if (editing != null) {
                Modifier.slotDrag(k, editing, edit, radiusPx, scope)
            } else {
                Modifier
            }
            RingButton(
                spec = s,
                size = SLOT_D,
                scale = 0.6f + 0.4f * q,
                alpha = q,
                armed = s != null && state.armed == s.id,
                highlighted = state.highlight == k,
                modifier = Modifier.offset { IntOffset(x.roundToInt(), y.roundToInt()) }.then(dragModifier),
                editable = editing != null,
                onTap = {
                    if (editing != null) editing.pick(k) else if (slot != null && s != null) fire(s, slot)
                },
            )
        }
        // The centre arrives last and opens the sheet.
        val cq = ((shown.value - 6 * SLOT_LAG) / (1f - 6 * SLOT_LAG)).coerceIn(0f, 1f)
        if (cq > 0f) {
            val centreHalf = with(density) { CENTRE_D.toPx() } / 2
            // In the editor the centre is not editable, so it sits dimmed and inert.
            RingButton(
                spec = SlotSpec("more", "More", Icons.Filled.MoreHoriz),
                size = CENTRE_D,
                scale = 0.6f + 0.4f * cq,
                alpha = cq * (if (editing == null) 1f else 0.45f),
                armed = false,
                highlighted = state.highlight == 6,
                modifier = Modifier.offset { IntOffset((cx - centreHalf).roundToInt(), (cy - centreHalf).roundToInt()) },
                inert = editing != null,
                onTap = { state.touch(); haptics.tick(); state.sheet = true },
            )
        }
        // The label under the ring: a hint, else the highlighted slot's name.
        val label = state.hint ?: state.highlight?.let { h ->
            if (h == 6) "More" else cfg.ring[h]?.let { spec(it, cfg, actions).label }
        }
        label?.let { hint ->
            val labelY = cy + radiusPx + slotPx
            Text(
                hint,
                modifier = Modifier
                    .align(Alignment.TopCenter)
                    .offset { IntOffset(0, labelY.roundToInt()) }
                    .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
                    .padding(horizontal = 14.dp, vertical = 8.dp),
                color = Color.White,
                fontSize = 15.sp,
            )
        }
        if (state.sheet) {
            RingSheet(state, rows, haptics, Modifier.align(Alignment.BottomCenter))
        }
    }

    if (textDialog) {
        SendTextDialog(onDismiss = { textDialog = false }) { text ->
            textDialog = false
            state.close()
            actions.sendText(text)
        }
    }
}

/** An armed slot and a hint both time out. */
@Composable
internal fun ExpireRingHint(state: RingState) {
    LaunchedEffect(state.armed, state.hint, state.lastTouch) {
        if (state.armed != null || state.hint != null) {
            delay(ARM_MS.coerceAtLeast(HINT_MS))
            state.armed = null
            state.hint = null
        }
    }
}

/**
 * The haptic vocabulary: a tap per press, a firm "no" on a dimmed button, a warning when a
 * destructive slot arms, and the confirm on the commit (StreamScreen fires that one).
 */
internal fun fireSlot(
    s: SlotSpec,
    slot: SlotId,
    state: RingState,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    onSendText: () -> Unit,
) {
    state.touch()
    if (!s.enabled) {
        haptics.boundary()
        state.armed = null
        state.hint = s.reason
        return
    }
    if (s.armed && state.armed != s.id) {
        haptics.boundary()
        state.armed = s.id
        state.hint = "${s.label}? Tap again"
        return
    }
    haptics.tick()
    state.armed = null
    state.hint = null
    when (slot) {
        SlotId.EndStream -> { state.close(); actions.endStream() }
        SlotId.EndGame -> { state.close(); actions.endGame() }
        SlotId.DisconnectLinger -> { state.close(); actions.disconnectLinger() }
        SlotId.TouchMode -> actions.cycleTouchMode()
        SlotId.Keyboard -> { state.close(); actions.keyboard() }
        SlotId.Stats -> actions.cycleStats()
        SlotId.Mic -> actions.toggleMic()
        SlotId.Pad -> actions.togglePad()
        SlotId.SendText -> onSendText()
        // The host's own overlay is taking the screen: close first, like End stream.
        SlotId.Guide -> { state.close(); actions.tapPadButton(Gamepad.BTN_GUIDE) }
        SlotId.Qam -> { state.close(); actions.tapPadButton(Gamepad.BTN_MISC1) }
        SlotId.PadMouse -> actions.cyclePadMouse()
        SlotId.PadType -> actions.cyclePadType()
        SlotId.StreamMute -> actions.toggleStreamMute()
        // The ring lives on the picture, which is about to change screens.
        SlotId.SwapScreens -> { state.close(); actions.cycleScreens() }
        is SlotId.Host -> {
            actions.hostActions().firstOrNull { it.id == slot.actionId }?.let { state.close(); actions.invokeHost(it) }
        }
        is SlotId.Shortcut -> {
            cfg.shortcut(slot.shortcutId)?.let { state.close(); actions.sendShortcut(it.keys) }
        }
    }
    if (s.toggle) state.hint = spec(slot, cfg, actions).let { "${it.label}: ${it.state}" }
}

/**
 * The pad (design §2.6): the left stick AIMS — its sector is the highlight, so the ring follows
 * the thumb — while the D-pad steps, Right clockwise, Left anticlockwise, Up to 12 o'clock, Down
 * to 6; Y returns the highlight to the centre, A fires it (the centre opens the sheet), B closes.
 * In the sheet, Up/Down walk the rows and Left/Right adjust one.
 */
private fun navigate(
    n: RingNav,
    state: RingState,
    rows: List<SheetRowSpec>,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    fire: (SlotSpec, SlotId) -> Unit,
) {
    if (state.sheet) {
        // A sheet is a list, not a dial: the six sectors fold onto its four directions.
        val ev = if (n is RingNav.Sector) sheetDir(n.slot) ?: return else n
        when (ev) {
            RingNav.Up -> { state.sheetCursor = (state.sheetCursor - 1).coerceAtLeast(0); haptics.tick() }
            RingNav.Down -> { state.sheetCursor = (state.sheetCursor + 1).coerceAtMost(rows.lastIndex.coerceAtLeast(0)); haptics.tick() }
            RingNav.Left -> rows.getOrNull(state.sheetCursor)?.onAdjust?.let { it(-1); haptics.tick() } ?: haptics.boundary()
            RingNav.Right -> rows.getOrNull(state.sheetCursor)?.onAdjust?.let { it(1); haptics.tick() } ?: haptics.boundary()
            RingNav.Confirm -> rows.getOrNull(state.sheetCursor)?.let { if (it.enabled) haptics.tick() else haptics.boundary(); it.onTap() }
            RingNav.Back -> { state.sheet = false; haptics.tick() }
            RingNav.Centre, is RingNav.Sector -> {}
        }
        return
    }
    val h = state.highlight ?: 6
    when (n) {
        // The weapon-wheel idiom: the thumb's sector is the slot, neutral is the centre.
        is RingNav.Sector -> (n.slot ?: 6).let {
            if (state.highlight != it) { state.highlight = it; haptics.tick() }
        }
        RingNav.Right -> { state.highlight = if (h >= 6) 0 else (h + 1) % 6; haptics.tick() }
        RingNav.Left -> { state.highlight = if (h >= 6) 5 else (h + 5) % 6; haptics.tick() }
        RingNav.Up -> { state.highlight = 0; haptics.tick() }
        RingNav.Down -> { state.highlight = 3; haptics.tick() }
        RingNav.Centre -> { state.highlight = 6; haptics.tick() }
        RingNav.Confirm -> if (h >= 6) { haptics.tick(); state.sheetCursor = 0; state.sheet = true } else {
            cfg.ring[h]?.let { fire(spec(it, cfg, actions), it) } ?: haptics.boundary()
        }
        RingNav.Back -> state.close()
    }
}

/**
 * Editing: which disc is under a drag and how far it has been carried, and which slot each disc
 * is drawn in — identity except while a swap plays: the two discs travel to each other's slots
 * on a spring, then the blob is written and `order` snaps back to identity with the contents
 * swapped, so nothing visible moves then.
 */
private class SlotDrag {
    var drag by mutableStateOf<Pair<Int, Offset>?>(null)
    var order by mutableStateOf((0 until 6).toList())
    var swapping by mutableStateOf(false)

    fun carried(k: Int): Offset = drag?.takeIf { it.first == k }?.second ?: Offset.Zero
}

/**
 * A disc dragged onto another slot swaps the two (§3.3); released near the centre or over its
 * own slot, nothing changes. The drag consumes past touch slop, which is what keeps the tap from
 * also firing.
 */
private fun Modifier.slotDrag(
    k: Int,
    editing: RingEditing,
    edit: SlotDrag,
    radiusPx: Float,
    scope: CoroutineScope,
): Modifier = pointerInput(k, editing) {
    detectDragGestures(
        onDragStart = { edit.drag = k to Offset.Zero },
        onDragEnd = {
            val d = edit.drag
            edit.drag = null
            if (d == null || d.first != k) return@detectDragGestures
            val home = Math.toRadians(-90.0 + 60.0 * k)
            val px = radiusPx * cos(home).toFloat() + d.second.x
            val py = radiusPx * sin(home).toFloat() + d.second.y
            if (hypot(px, py) <= radiusPx / 2) return@detectDragGestures
            val angle = Math.toDegrees(atan2(py, px).toDouble()) + 90.0
            val target = (((angle / 60.0).roundToInt() % 6) + 6) % 6
            if (target == k || edit.swapping) return@detectDragGestures
            edit.swapping = true
            edit.order = edit.order.toMutableList().also { o ->
                val t = o[k]
                o[k] = o[target]
                o[target] = t
            }
            // Past the spring's settle: a reset while the discs still move jumped them the
            // last few pixels as the contents swapped.
            scope.launch {
                delay(650)
                edit.swapping = false
                edit.order = (0 until 6).toList()
                editing.swap(k, target)
            }
        },
        onDragCancel = { edit.drag = null },
    ) { change, delta ->
        change.consume()
        edit.drag = (edit.drag?.takeIf { it.first == k } ?: (k to Offset.Zero)).let { it.first to it.second + delta }
    }
}

@Composable
private fun SendTextDialog(onDismiss: () -> Unit, onSend: (String) -> Unit) {
    var text by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Send text") },
        text = { OutlinedTextField(text, { text = it }, singleLine = true, modifier = Modifier.fillMaxWidth()) },
        confirmButton = { TextButton(onClick = { onSend(text) }) { Text("Send") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** One round translucent button — the in-stream pill family's surface, in a circle. */
@Composable
private fun RingButton(
    spec: SlotSpec?,
    size: androidx.compose.ui.unit.Dp,
    scale: Float,
    alpha: Float,
    armed: Boolean,
    modifier: Modifier,
    highlighted: Boolean = false,
    /** The editor: an empty slot is a pick target, not inert. */
    editable: Boolean = false,
    /** The editor's centre: drawn, never pressed. */
    inert: Boolean = false,
    onTap: () -> Unit,
) {
    val tint = when {
        armed -> Color(0xFFFF5A5A)
        spec == null || !spec.enabled -> Color.White.copy(alpha = 0.35f)
        else -> Color.White
    }
    Box(
        modifier
            .size(size)
            .graphicsLayer { scaleX = scale; scaleY = scale; this.alpha = alpha }
            .clip(CircleShape)
            .background(Color.Black.copy(alpha = if (armed) 0.75f else 0.55f))
            .border(
                if (highlighted) 2.dp else 1.dp,
                Color.White.copy(alpha = if (highlighted) 0.8f else if (armed) 0.6f else 0.18f),
                CircleShape,
            )
            .clickable(enabled = !inert && (spec != null || editable), onClick = onTap)
            .semantics {
                contentDescription = spec?.label ?: "Empty slot"
                stateDescription = when {
                    armed -> "armed — press again"
                    spec?.enabled == false -> spec.reason
                    spec?.state?.isNotEmpty() == true -> spec.state
                    else -> ""
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        when {
            spec?.chip != null -> ChordKeycap(spec.chip, tint, size)
            spec?.icon != null -> Icon(spec.icon, contentDescription = null, tint = tint, modifier = Modifier.size(size / 2))
        }
    }
}

/**
 * A chord as a stacked keycap: modifiers small on top, the key large under them — one legend
 * line ran past the disc's edge. [chip] is `chordChip`'s text. The editor previews a chord with
 * this same composable.
 */
@Composable
internal fun ChordKeycap(chip: String, tint: Color, size: androidx.compose.ui.unit.Dp) {
    val parts = chip.split("+")
    Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.width(size - 12.dp)) {
        if (parts.size > 1) {
            Text(
                parts.dropLast(1).joinToString("+"),
                color = tint, fontSize = 8.sp, fontWeight = FontWeight.SemiBold,
                maxLines = 1, softWrap = false,
            )
        }
        Text(
            parts.last(),
            color = tint, fontSize = if (parts.last().length > 3) 10.sp else 13.sp,
            fontWeight = FontWeight.SemiBold, maxLines = 1, softWrap = false,
        )
    }
}

/** A chord on a disc the size the ring draws it, for lists and the editing screen. */
@Composable
internal fun KeycapDisc(keys: List<String>, size: androidx.compose.ui.unit.Dp = SLOT_D) {
    Box(
        Modifier
            .size(size)
            .clip(CircleShape)
            .background(Color(0xFF383838))
            .border(1.dp, Color.White.copy(alpha = 0.18f), CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        if (keys.isEmpty()) {
            Text("?", color = Color.White, fontSize = 16.sp, fontWeight = FontWeight.SemiBold)
        } else {
            ChordKeycap(chordChip(keys), Color.White, size)
        }
    }
}

/** One row of the sheet as data, so a finger and a pad drive the same list. */
private data class SheetRowSpec(
    val header: String? = null,
    val label: String,
    val value: String = "",
    val enabled: Boolean = true,
    /** Left/Right on a pad: cycle a value (the resolution rows); a tap cycles forward. */
    val onAdjust: ((Int) -> Unit)? = null,
    val onTap: () -> Unit,
)

private val RES_PRESETS = listOf("1440p" to (2560 to 1440), "1080p" to (1920 to 1080), "720p" to (1280 to 720))
private val HZ_PRESETS = listOf(120, 60)

/** Depth two: the complete catalogue in a fixed order (D2). */
private fun sheetRows(
    state: RingState,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    requestText: () -> Unit,
): List<SheetRowSpec> {
    val rows = mutableListOf<SheetRowSpec>()
    val mode = actions.currentMode()
    val native = state.nativeMode ?: mode
    val (w, h, hz) = Triple(mode.getOrElse(0) { 0 }, mode.getOrElse(1) { 0 }, mode.getOrElse(2) { 60 })
    val (nw, nh, nhz) = Triple(native.getOrElse(0) { 0 }, native.getOrElse(1) { 0 }, native.getOrElse(2) { 60 })
    val resLabel = if (w == nw && h == nh) "Native ($w×$h)" else RES_PRESETS.firstOrNull { it.second == (w to h) }?.first ?: "$w×$h"
    fun adjustRes(dir: Int) {
        val options = listOf(nw to nh) + RES_PRESETS.map { it.second }
        val i = options.indexOf(w to h).coerceAtLeast(0)
        val n = options.size
        val next = options[((i + dir) % n + n) % n]
        actions.requestMode(next.first, next.second, hz)
    }
    fun adjustHz(dir: Int) {
        val options = (listOf(nhz) + HZ_PRESETS).distinct()
        val i = options.indexOf(hz).coerceAtLeast(0)
        val n = options.size
        actions.requestMode(w, h, options[((i + dir) % n + n) % n])
    }
    rows += SheetRowSpec("Session", "End stream", if (state.armed == "end_stream") "tap again" else "") {
        if (state.armed == "end_stream") { state.close(); actions.endStream() } else { haptics.boundary(); state.armed = "end_stream" }
    }
    if (actions.streamedGame() != null) {
        rows += SheetRowSpec(null, "End game", if (state.armed == "end_game") "tap again" else "") {
            if (state.armed == "end_game") { state.close(); actions.endGame() } else { haptics.boundary(); state.armed = "end_game" }
        }
    }
    rows += SheetRowSpec(null, "Disconnect, keep the game running") { state.close(); actions.disconnectLinger() }
    rows += SheetRowSpec("Resolution", "Resolution", resLabel, onAdjust = ::adjustRes) { adjustRes(1) }
    rows += SheetRowSpec(null, "Refresh", "$hz Hz", onAdjust = ::adjustHz) { adjustHz(1) }
    val tm = spec(SlotId.TouchMode, cfg, actions)
    rows += SheetRowSpec("Input", tm.label, tm.state) { actions.cycleTouchMode() }
    val pointerGranted = actions.pointerGranted()
    rows += SheetRowSpec(null, "Invert scroll direction",
        if (pointerGranted) { if (actions.scrollInverted()) "On" else "Off" } else "Pointer input is not allowed",
        pointerGranted,
    ) { if (actions.pointerGranted()) actions.toggleScrollInversion() }
    val kb = spec(SlotId.Keyboard, cfg, actions)
    rows += SheetRowSpec(null, kb.label, if (kb.enabled) "" else kb.reason, kb.enabled) { if (kb.enabled) { state.close(); actions.keyboard() } }
    val st = spec(SlotId.SendText, cfg, actions)
    rows += SheetRowSpec(null, st.label, if (st.enabled) "" else st.reason, st.enabled) { if (st.enabled) requestText() }
    val pad = spec(SlotId.Pad, cfg, actions)
    rows += SheetRowSpec(null, pad.label, if (pad.enabled) pad.state else pad.reason, pad.enabled) { if (pad.enabled) actions.togglePad() }
    for ((slot, bit) in listOf(SlotId.Guide to Gamepad.BTN_GUIDE, SlotId.Qam to Gamepad.BTN_MISC1)) {
        val sys = spec(slot, cfg, actions)
        rows += SheetRowSpec(null, sys.label, if (sys.enabled) "" else sys.reason, sys.enabled) {
            if (sys.enabled) { state.close(); actions.tapPadButton(bit) }
        }
    }
    val pm = spec(SlotId.PadMouse, cfg, actions)
    rows += SheetRowSpec(null, pm.label, if (pm.enabled) pm.state else pm.reason, pm.enabled) { if (pm.enabled) actions.cyclePadMouse() }
    val pt = spec(SlotId.PadType, cfg, actions)
    rows += SheetRowSpec(null, pt.label, if (pt.enabled) pt.state else pt.reason, pt.enabled) { if (pt.enabled) actions.cyclePadType() }
    rows += SheetRowSpec("View", "Statistics", actions.stats().label) { actions.cycleStats() }
    if (actions.screenLayouts().size > 1) {
        rows += SheetRowSpec(null, "Screens", actions.screenLayout().label) { state.close(); actions.cycleScreens() }
    }
    val mic = spec(SlotId.Mic, cfg, actions)
    rows += SheetRowSpec("Audio", mic.label, if (mic.enabled) mic.state else mic.reason, mic.enabled) { if (mic.enabled) actions.toggleMic() }
    actions.hostActions().forEachIndexed { i, act ->
        val id = "host:${act.id}"
        rows += SheetRowSpec(
            if (i == 0) "Host" else null, act.label,
            when {
                !act.available -> act.unavailableReason
                state.armed == id -> "tap again"
                else -> ""
            },
            act.available,
        ) {
            if (!act.available) return@SheetRowSpec
            if (act.danger && state.armed != id) { haptics.boundary(); state.armed = id } else { state.close(); actions.invokeHost(act) }
        }
    }
    cfg.shortcuts.forEachIndexed { i, s ->
        val ok = actions.keyboardGranted() && s.keys.all { keyVk(it) != null }
        rows += SheetRowSpec(if (i == 0) "Shortcuts" else null, s.label.ifEmpty { chordChip(s.keys) }, chordChip(s.keys), ok) {
            if (ok) { state.close(); actions.sendShortcut(s.keys) }
        }
    }
    return rows
}

/** The sheet as a scrollable bottom panel; the pad's cursor row is tinted. */
@Composable
private fun RingSheet(state: RingState, rows: List<SheetRowSpec>, haptics: ConsoleHaptics, modifier: Modifier) {
    val scroll = rememberScrollState()
    Column(
        modifier
            .fillMaxWidth(0.92f)
            .padding(bottom = 16.dp)
            .background(Color.Black.copy(alpha = 0.78f), RoundedCornerShape(16.dp))
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {}
            .padding(vertical = 8.dp)
            .verticalScroll(scroll),
    ) {
        rows.forEachIndexed { i, r ->
            r.header?.let { text ->
                Text(
                    text, color = Color.White.copy(alpha = 0.6f), fontSize = 12.sp, fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(start = 20.dp, top = 12.dp, bottom = 4.dp),
                )
            }
            Row(
                Modifier
                    .fillMaxWidth()
                    .background(if (state.sheetCursor == i) Color.White.copy(alpha = 0.12f) else Color.Transparent)
                    .clickable {
                        state.touch()
                        state.sheetCursor = i
                        if (r.enabled) haptics.tick() else haptics.boundary()
                        r.onTap()
                    }
                    .padding(horizontal = 20.dp, vertical = 12.dp)
                    .alpha(if (r.enabled) 1f else 0.45f),
            ) {
                Text(r.label, color = Color.White, fontSize = 15.sp, modifier = Modifier.weight(1f))
                if (r.value.isNotEmpty()) Text(r.value, color = Color.White.copy(alpha = 0.7f), fontSize = 15.sp)
            }
        }
        Spacer(Modifier.height(4.dp))
    }
}

/** The chord as key events: modifiers down in order, the key, modifiers up in reverse. */
fun sendChord(handle: Long, keys: List<String>) {
    val vks = keys.mapNotNull { keyVk(it) }
    if (vks.size != keys.size || vks.isEmpty()) return
    vks.forEach { NativeBridge.nativeSendKey(handle, it, true, 0) }
    vks.asReversed().forEach { NativeBridge.nativeSendKey(handle, it, false, 0) }
}
