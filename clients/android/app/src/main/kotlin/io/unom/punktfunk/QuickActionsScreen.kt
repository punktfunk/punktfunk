package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay
import kotlin.math.roundToInt

/**
 * The quick-action ring's editor (design/touch-client-overlay.md §3.3): the editor IS the ring —
 * the in-stream [RingOverlay], the same composable, full size over a backdrop that runs the real
 * twist ([streamTouchInput] with no session behind it). Tap a slot to pick its action from the
 * catalogue, drag a disc onto another to swap, tap the centre to see depth two; the shortcuts
 * list and the reset sit under it. A shortcut is edited on its own screen: a name, the modifiers
 * as chips, the key on a keyboard you tap, and the disc as it will look. A deep sub-screen of
 * Settings like [ControllersScreen]; [blob] is the `overlay_actions` of the layer being edited
 * and [onChange] writes it back through the same `update` every row uses, so a preset that
 * touches it owns the whole ring (D10).
 */
@Composable
internal fun QuickActionsScreen(
    blob: String,
    onChange: (String) -> Unit,
    onReset: () -> Unit,
    onBack: () -> Unit,
    overridden: Boolean = false,
) {
    val cfg = remember(blob) { OverlayConfig.parse(blob) }
    var picking by remember { mutableStateOf<Int?>(null) }
    var editingShortcut by remember { mutableStateOf<ShortcutDraft?>(null) }
    var editingLayout by remember { mutableStateOf(false) }

    if (editingLayout) {
        PadLayoutEditor(
            pad = cfg.pad,
            onChange = { onChange(cfg.copy(pad = it).toJson()) },
            onBack = { editingLayout = false },
        )
        return
    }

    fun set(k: Int, id: String) {
        onChange(cfg.copy(ring = cfg.ring.toMutableList().also { it[k] = SlotId.parse(id) }).toJson())
    }
    fun swap(a: Int, b: Int) {
        val ring = cfg.ring.toMutableList()
        val t = ring[a]
        ring[a] = ring[b]
        ring[b] = t
        onChange(cfg.copy(ring = ring).toJson())
    }
    fun save(d: ShortcutDraft) {
        val sc = Shortcut(d.id, d.label, d.keys)
        val i = cfg.shortcuts.indexOfFirst { it.id == d.id }
        val next = if (i >= 0) {
            cfg.copy(shortcuts = cfg.shortcuts.toMutableList().also { it[i] = sc })
        } else {
            val ring = cfg.ring.toMutableList()
            ring.indexOf(null).takeIf { it >= 0 }?.let { ring[it] = SlotId.Shortcut(sc.id) }
            cfg.copy(ring = ring, shortcuts = cfg.shortcuts + sc)
        }
        onChange(next.toJson())
    }
    fun remove(id: String) {
        onChange(
            cfg.copy(
                // `parse` would empty a dangling slot on the next read; write it empty now so
                // the ring shows it at once.
                ring = cfg.ring.map { if (it is SlotId.Shortcut && it.shortcutId == id) null else it },
                shortcuts = cfg.shortcuts.filter { it.id != id },
            ).toJson(),
        )
    }

    editingShortcut?.let { draft ->
        ShortcutEditor(
            draft = draft,
            onSave = { save(it); editingShortcut = null },
            onDelete = { remove(draft.id); editingShortcut = null },
            onBack = { editingShortcut = null },
        )
        return
    }

    BackHandler(onBack = onBack)
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text("Quick actions", style = MaterialTheme.typography.headlineMedium)
        }
        // The ring in a card like every other field, but without the group's 16 dp inset: the
        // inset left the stage narrower than the ring. Its caption sits under the card the way a
        // group's footer does.
        Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
            OutlinedCard(modifier = Modifier.fillMaxWidth()) {
                RingStage(cfg, RingEditing(pick = { picking = it }, swap = ::swap))
            }
            Text(
                "Tap a button to change it, drag one onto another to swap." +
                    if (overridden) " This preset has its own quick actions; the default dial no longer reaches it." else "",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
        // The virtual controller's preset and look (§4.3), written to the blob's `pad` through the
        // same path the ring uses.
        SettingsGroup(
            "Virtual controller",
            footer = "Shown from the dial's Virtual controller button. " +
                "Wide and upright screens keep separate layouts.",
        ) {
            SettingDropdown(
                label = "Layout",
                options = listOf("full" to "Full", "sticks" to "Sticks and shoulders", "dpad" to "D-pad and face buttons"),
                selected = cfg.pad.layout,
            ) { onChange(cfg.copy(pad = cfg.pad.copy(layout = it)).toJson()) }
            PadSlider("Opacity", cfg.pad.opacity, PAD_OPACITY_MIN..1f) {
                onChange(cfg.copy(pad = cfg.pad.copy(opacity = it)).toJson())
            }
            PadSlider("Scale", cfg.pad.scale, PAD_SCALE_MIN..PAD_SCALE_MAX) {
                onChange(cfg.copy(pad = cfg.pad.copy(scale = it)).toJson())
            }
            TextButton(onClick = { editingLayout = true }) { Text("Edit layout") }
        }
        SettingsGroup("Shortcuts", footer = "A new shortcut takes the first empty dial slot.") {
            cfg.shortcuts.forEach { sc ->
                Row(
                    modifier = Modifier.fillMaxWidth().clickable {
                        editingShortcut = ShortcutDraft(sc.id, sc.label, sc.keys, isNew = false)
                    },
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    KeycapDisc(sc.keys, 40.dp)
                    Column(Modifier.weight(1f)) {
                        Text(sc.label.ifEmpty { chordChip(sc.keys) }, style = MaterialTheme.typography.bodyLarge)
                        if (sc.label.isNotEmpty()) {
                            Text(
                                chordChip(sc.keys),
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                    Icon(
                        Icons.AutoMirrored.Filled.KeyboardArrowRight,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
            TextButton(onClick = {
                val next = (cfg.shortcuts.mapNotNull { it.id.drop(1).toIntOrNull() }.maxOrNull() ?: 0) + 1
                editingShortcut = ShortcutDraft("s$next", "", emptyList(), isNew = true)
            }) { Text("Add shortcut") }
        }
        SettingsGroup {
            TextButton(onClick = onReset) {
                Text("Reset to default", color = MaterialTheme.colorScheme.error)
            }
        }
    }

    picking?.let { k ->
        SlotPicker(
            groups = slotGroups(cfg),
            current = cfg.ring[k]?.id ?: "",
            onPick = { set(k, it); picking = null },
            onDismiss = { picking = null },
        )
    }
}

/**
 * The live ring over a backdrop that runs the stream's own twist: [streamTouchInput] with a `0`
 * handle sends nothing to a host and still arms, commits and cancels the dial. Whatever closes
 * the ring — a twist wound back, a preview row that ends the stream — it springs back open, so
 * there is never a dead editor with nothing to tap.
 */
@Composable
private fun RingStage(cfg: OverlayConfig, editing: RingEditing) {
    val ring = remember { RingState() }
    val haptics = rememberConsoleHaptics()
    var size by remember { mutableStateOf(IntSize.Zero) }
    // Read the size INSIDE the effects: a centre computed at composition was one step behind
    // the size state on the first open and put the ring at (0, 0), clamped to the margin.
    fun centre() = Offset(size.width / 2f, size.height / 2f)
    LaunchedEffect(size) {
        if (size != IntSize.Zero && !ring.committed) ring.openAt(centre())
    }
    LaunchedEffect(ring.committed, ring.progress) {
        if (!ring.committed && ring.progress == 0f && size != IntSize.Zero) {
            delay(250)
            if (!ring.committed && ring.progress == 0f) ring.openAt(centre())
        }
    }
    // No fill of its own: the card it sits in is the field. The twist surface is a SIBLING
    // under the ring, as the stream's gesture layer is — on a parent it took the moves before
    // the discs' drag detectors saw them, so a drag never started.
    Box(
        Modifier
            .fillMaxWidth()
            .height(380.dp)
            .onSizeChanged { size = it },
    ) {
        Box(
            Modifier
                .matchParentSize()
                .pointerInput(Unit) {
                    streamTouchInput(
                        sink = NativeTouchSink(0L), stylus = null,
                        video = { VideoFrame(io.unom.punktfunk.kit.VideoFit.FIT, 0, 0) },
                        trackpad = true,
                        // Only the twist is this stage's: a lone finger keeps scrolling the
                        // settings page under the card (dialOnly's whole point).
                        dialOnly = true,
                        onCycleStats = {}, onKeyboard = {},
                    ) { ev ->
                        when (ev) {
                            is DialEvent.Turn -> if (ring.turn(ev.progress, ev.clockwise, ev.x, ev.y)) haptics.tick()
                            DialEvent.Commit -> { ring.commit(); haptics.confirm() }
                            DialEvent.Cancel -> ring.cancel()
                        }
                    }
                },
        )
        RingOverlay(
            state = ring,
            cfg = cfg,
            actions = previewActions,
            containerSize = size,
            haptics = haptics,
            editing = editing,
        )
    }
}

/** A slider that names its value as a percentage and writes it when the finger lifts, not per frame. */
@Composable
private fun PadSlider(
    label: String,
    value: Float,
    range: ClosedFloatingPointRange<Float>,
    onCommit: (Float) -> Unit,
) {
    var v by remember(value) { mutableFloatStateOf(value) }
    Column {
        Text("$label · ${(v * 100).roundToInt()}%", style = MaterialTheme.typography.bodyLarge)
        Slider(value = v, onValueChange = { v = it }, valueRange = range, onValueChangeFinished = { onCommit(v) })
    }
}

/** The ring's commands with nothing behind them: the editor shows, it never fires (§3.3). */
private val previewActions = RingActions(
    endStream = {}, disconnectLinger = {},
    touchMode = { TouchMode.TRACKPAD }, cycleTouchMode = {},
    keyboardGranted = { true }, keyboard = {},
    textSupported = true, sendText = {},
    stats = { StatsVerbosity.COMPACT }, cycleStats = {},
    micAvailable = { true }, micMuted = { false }, toggleMic = {},
    // The three power actions as a host that offers all three would show them; a dimmed
    // "does not offer it" would lie about the slot.
    hostActions = {
        listOf(
            HostActions.Action("power.sleep", "Sleep host", danger = false, available = true, unavailableReason = ""),
            HostActions.Action("power.reboot", "Restart host", danger = true, available = true, unavailableReason = ""),
            HostActions.Action("power.shutdown", "Shut down host", danger = true, available = true, unavailableReason = ""),
        )
    },
    invokeHost = {}, sendShortcut = {},
    padAvailable = { true }, padShown = { false }, togglePad = {}, tapPadButton = {},
    pointerGranted = { true }, padMouseTarget = { 1 }, padMouseMode = { 0 }, cyclePadMouse = {},
    audioMute = { 0 }, audioMuteLabel = { null }, toggleStreamMute = {},
    currentMode = { intArrayOf(1920, 1080, 60) }, requestMode = { _, _, _ -> },
    screenLayouts = { ScreenLayout.entries },
)

private data class SlotOption(val id: String, val label: String, val note: String? = null)
private data class SlotGroup(val title: String, val options: List<SlotOption>)

/** The catalogue by group (§3.3) with each entry's availability note; the preset's own
 *  shortcuts and the empty slot are appended per config. */
private fun slotGroups(cfg: OverlayConfig): List<SlotGroup> {
    val g = mutableListOf(
        SlotGroup("Session", listOf(
            SlotOption("end_stream", "End stream"),
            SlotOption("end_game", "End game", "Only a game this device launched"),
            SlotOption("disconnect_linger", "Disconnect, keep the game running"),
        )),
        SlotGroup("Input", listOf(
            SlotOption("touch_mode", "Touch mode"),
            SlotOption("keyboard", "Keyboard"),
            SlotOption("pad", "Virtual controller", "Shows or hides the on-screen controller"),
            SlotOption("send_text", "Send text"),
            SlotOption("guide", "Guide button", "The host's Xbox / PS / Steam button"),
            SlotOption("qam", "Quick access menu", "Only where the host's pad is Steam-shaped"),
            SlotOption("pad_mouse", "Controller mouse", "Your controller moves the host's pointer"),
        )),
        SlotGroup("View", listOf(
            SlotOption("stats", "Statistics"),
            SlotOption("swap_screens", "Screens", "Dual-screen handhelds: the panel below, the picture below, or across both"),
        )),
        SlotGroup("Audio", listOf(
            SlotOption("mic", "Microphone"),
            SlotOption("stream_mute", "Mute this stream", "This device only — the host keeps playing"),
        )),
        SlotGroup("Host", listOf(
            SlotOption("host:power.sleep", "Sleep host", "Only where the host offers it"),
            SlotOption("host:power.reboot", "Restart host", "Only where the host offers it"),
            SlotOption("host:power.shutdown", "Shut down host", "Only where the host offers it"),
        )),
    )
    if (cfg.shortcuts.isNotEmpty()) {
        g += SlotGroup("Shortcuts", cfg.shortcuts.map {
            SlotOption("shortcut:${it.id}", it.label.ifEmpty { chordChip(it.keys) }, if (it.label.isEmpty()) null else chordChip(it.keys))
        })
    }
    g += SlotGroup("Empty", listOf(SlotOption("", "Empty slot")))
    return g
}

/** The catalogue by group; the current pick is marked. */
@Composable
private fun SlotPicker(groups: List<SlotGroup>, current: String, onPick: (String) -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Slot action") },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                groups.forEach { g ->
                    Text(
                        g.title.uppercase(),
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.padding(top = 12.dp, bottom = 4.dp),
                    )
                    g.options.forEach { o ->
                        Row(
                            Modifier.fillMaxWidth().clickable { onPick(o.id) }.padding(vertical = 8.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Column(Modifier.weight(1f)) {
                                Text(o.label, style = MaterialTheme.typography.bodyLarge)
                                o.note?.let {
                                    Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                                }
                            }
                            if (o.id == current) Text("✓", color = MaterialTheme.colorScheme.primary)
                        }
                    }
                }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}

/** A shortcut on the editing screen, new or existing. */
private data class ShortcutDraft(val id: String, val label: String, val keys: List<String>, val isNew: Boolean)

private val MODIFIER_KEYS = listOf("ctrl", "alt", "shift", "win")

/** The keys a chord can end on, grouped the way a keyboard groups them — every name [keyVk] knows. */
private val KEY_GROUPS: List<Pair<String, List<String>>> = listOf(
    "Function" to (listOf("escape") + (1..12).map { "f$it" }),
    "Letters" to "qwertyuiopasdfghjklzxcvbnm".map { it.toString() },
    "Numbers" to ((1..9).map { it.toString() } + "0"),
    "Editing" to listOf("tab", "space", "enter", "backspace", "delete", "insert"),
    "Navigation" to listOf("home", "end", "pageup", "pagedown", "up", "down", "left", "right"),
    "Other" to listOf("printscreen", "pause", "capslock"),
)

/**
 * One shortcut: a name, the modifiers held as chips, the key it ends on picked from a keyboard,
 * and the disc as the ring will draw it.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ShortcutEditor(
    draft: ShortcutDraft,
    onSave: (ShortcutDraft) -> Unit,
    onDelete: () -> Unit,
    onBack: () -> Unit,
) {
    BackHandler(onBack = onBack)
    var label by remember { mutableStateOf(draft.label) }
    var mods by remember { mutableStateOf(draft.keys.filter { it in MODIFIER_KEYS }) }
    var key by remember { mutableStateOf(draft.keys.firstOrNull { it !in MODIFIER_KEYS }) }
    // Modifiers first in keyboard order, then the key — the order the chord is sent.
    val keys = MODIFIER_KEYS.filter { it in mods } + listOfNotNull(key)

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text(if (draft.isNew) "New shortcut" else "Shortcut", style = MaterialTheme.typography.headlineMedium)
        }
        SettingsGroup {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(14.dp)) {
                KeycapDisc(keys)
                Column {
                    Text(
                        label.ifEmpty { if (key == null) "Pick a key" else chordChip(keys) },
                        style = MaterialTheme.typography.bodyLarge,
                    )
                    Text(
                        if (key == null) "The disc as the dial will draw it" else chordChip(keys),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        fontFamily = if (key == null) null else FontFamily.Monospace,
                    )
                }
            }
            OutlinedTextField(
                value = label,
                onValueChange = { label = it },
                label = { Text("Name (optional)") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
        }
        SettingsGroup("Hold") {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                MODIFIER_KEYS.forEach { m ->
                    FilterChip(
                        selected = m in mods,
                        onClick = { mods = if (m in mods) mods - m else mods + m },
                        label = { Text(keyLegend(m)) },
                    )
                }
            }
        }
        KEY_GROUPS.forEach { (title, group) ->
            SettingsGroup(title) {
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    group.forEach { k ->
                        FilterChip(
                            selected = key == k,
                            onClick = { key = k },
                            // A fixed-width chip lays its label out from the start; centre it.
                            label = {
                                Box(Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
                                    Text(keyLegend(k), maxLines = 1, softWrap = false)
                                }
                            },
                            modifier = Modifier.width(if (keyLegend(k).length > 2) 88.dp else 48.dp),
                        )
                    }
                }
            }
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            Button(onClick = { onSave(draft.copy(label = label, keys = keys)) }, enabled = key != null) {
                Text(if (draft.isNew) "Add" else "Save")
            }
            TextButton(onClick = onBack) { Text("Cancel") }
            Spacer(Modifier.weight(1f))
            if (!draft.isNew) {
                TextButton(onClick = onDelete) { Text("Remove", color = MaterialTheme.colorScheme.error) }
            }
        }
    }
}
