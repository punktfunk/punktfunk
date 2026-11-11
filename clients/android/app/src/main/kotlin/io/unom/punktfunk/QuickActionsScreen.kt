package io.unom.punktfunk

import android.content.Context
import androidx.activity.compose.BackHandler
import androidx.annotation.StringRes
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
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.SlotId
import io.unom.punktfunk.kit.chordChip
import io.unom.punktfunk.kit.keyLegend
import io.unom.punktfunk.kit.keyVk
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
    val ctx = textContext()
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
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.common_back))
            }
            Text(stringResource(R.string.ring_editor_title), style = MaterialTheme.typography.headlineMedium)
        }
        // The ring in a card like every other field, but without the group's 16 dp inset: the
        // inset left the stage narrower than the ring. Its caption sits under the card the way a
        // group's footer does.
        Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
            OutlinedCard(modifier = Modifier.fillMaxWidth()) {
                RingStage(cfg, RingEditing(pick = { picking = it }, swap = ::swap))
            }
            Text(
                stringResource(R.string.ring_editor_hint_tap) +
                    if (overridden) " " + stringResource(R.string.ring_editor_preset_owns) else "",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
        // The virtual controller's preset and look (§4.3), written to the blob's `pad` through the
        // same path the ring uses.
        SettingsGroup(
            stringResource(R.string.pad_virtual_title),
            footer = stringResource(R.string.pad_virtual_hint),
        ) {
            SettingDropdown(
                label = stringResource(R.string.pad_virtual_layout),
                options = listOf(
                    "full" to stringResource(R.string.pad_virtual_layout_full),
                    "sticks" to stringResource(R.string.pad_virtual_layout_sticks),
                    "dpad" to stringResource(R.string.pad_virtual_layout_dpad),
                ),
                selected = cfg.pad.layout,
            ) { onChange(cfg.copy(pad = cfg.pad.copy(layout = it)).toJson()) }
            PadSlider(stringResource(R.string.pad_virtual_opacity), cfg.pad.opacity, PAD_OPACITY_MIN..1f) {
                onChange(cfg.copy(pad = cfg.pad.copy(opacity = it)).toJson())
            }
            PadSlider(stringResource(R.string.pad_virtual_scale), cfg.pad.scale, PAD_SCALE_MIN..PAD_SCALE_MAX) {
                onChange(cfg.copy(pad = cfg.pad.copy(scale = it)).toJson())
            }
            TextButton(onClick = { editingLayout = true }) { Text(stringResource(R.string.pad_virtual_edit_layout)) }
        }
        SettingsGroup(
            stringResource(R.string.ring_editor_shortcuts),
            footer = stringResource(R.string.ring_editor_shortcuts_hint),
        ) {
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
            }) { Text(stringResource(R.string.ring_shortcut_add)) }
        }
        SettingsGroup {
            TextButton(onClick = onReset) {
                Text(stringResource(R.string.ring_reset), color = MaterialTheme.colorScheme.error)
            }
        }
    }

    picking?.let { k ->
        SlotPicker(
            groups = slotGroups(cfg, ctx),
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
    val ctx = textContext()
    val previewActions = remember(ctx) { previewActionsIn(ctx) }
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
        Text(
            "$label · " + stringResource(R.string.common_percent, (v * 100).roundToInt()),
            style = MaterialTheme.typography.bodyLarge,
        )
        Slider(value = v, onValueChange = { v = it }, valueRange = range, onValueChangeFinished = { onCommit(v) })
    }
}

/** The ring's commands with nothing behind them: the editor shows, it never fires (§3.3). */
internal fun previewActionsIn(ctx: Context) = RingActions(
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
            HostActions.Action(
                "power.sleep", ctx.getString(R.string.host_power_sleep),
                danger = false, available = true, unavailableReason = "",
            ),
            HostActions.Action(
                "power.reboot", ctx.getString(R.string.host_power_reboot),
                danger = true, available = true, unavailableReason = "",
            ),
            HostActions.Action(
                "power.shutdown", ctx.getString(R.string.host_power_shutdown),
                danger = true, available = true, unavailableReason = "",
            ),
        )
    },
    invokeHost = {}, sendShortcut = {},
    padAvailable = { true }, padShown = { false }, togglePad = {}, tapPadButton = {},
    pointerGranted = { true }, padMouseTarget = { 1 }, padMouseMode = { 0 }, cyclePadMouse = {},
    audioMute = { 0 }, audioMuteLabel = { null }, toggleStreamMute = {},
    currentMode = { intArrayOf(1920, 1080, 60) }, requestMode = { _, _, _ -> },
    screenLayouts = { ScreenLayout.entries },
    pointerCapture = { true },
)

/** One catalogue entry. [dim]: never on this device, though a preset made elsewhere may pick it. */
internal data class SlotOption(val id: String, val label: String, val note: String? = null, val dim: Boolean = false)
internal data class SlotGroup(val title: String, val options: List<SlotOption>)

/** The catalogue by group (§3.3) with each entry's availability note; the preset's own key
 *  combinations are appended per config. The dial ends on the empty slot; a shortcut
 *  ([forShortcut]) starts on Quick actions itself instead. */
internal fun slotGroups(cfg: OverlayConfig, ctx: Context, forShortcut: Boolean = false): List<SlotGroup> {
    fun t(@StringRes id: Int) = ctx.getString(id)
    val g = mutableListOf(
        SlotGroup(t(R.string.ring_group_session), listOfNotNull(
            SlotOption("ring", t(R.string.ring_open_ring)).takeIf { forShortcut },
            SlotOption("end_stream", t(R.string.ring_end_stream)),
            SlotOption("end_game", t(R.string.ring_end_game), t(R.string.ring_end_game_note)),
            SlotOption("disconnect_linger", t(R.string.ring_disconnect_linger)),
        )),
        SlotGroup(t(R.string.ring_group_input), listOf(
            SlotOption("touch_mode", t(R.string.ring_touch_mode)),
            SlotOption("keyboard", t(R.string.ring_keyboard)),
            SlotOption("pad", t(R.string.ring_pad), t(R.string.ring_pad_note)),
            SlotOption("send_text", t(R.string.ring_send_text)),
            SlotOption("guide", t(R.string.ring_guide), t(R.string.ring_guide_note)),
            SlotOption("qam", t(R.string.ring_qam), t(R.string.ring_qam_note)),
            SlotOption("pad_mouse", t(R.string.ring_pad_mouse), t(R.string.ring_pad_mouse_note)),
            SlotOption("pad_type", t(R.string.ring_pad_type), t(R.string.ring_pad_type_note)),
            SlotOption("release_input", t(R.string.ring_release_input)),
            SlotOption("mouse_mode", t(R.string.ring_mouse_mode)),
            SlotOption("clipboard", t(R.string.ring_clipboard), t(R.string.ring_clipboard_unavailable), dim = true),
        )),
        SlotGroup(t(R.string.ring_group_view), listOf(
            SlotOption("stats", t(R.string.ring_stats)),
            SlotOption("swap_screens", t(R.string.android_ring_screens), t(R.string.android_ring_screens_note)),
            SlotOption("fullscreen", t(R.string.ring_fullscreen), t(R.string.ring_no_window), dim = true),
            SlotOption("minimize", t(R.string.ring_minimize), t(R.string.ring_no_window), dim = true),
        )),
        SlotGroup(t(R.string.ring_group_audio), listOf(
            SlotOption("mic", t(R.string.ring_mic)),
            SlotOption("stream_mute", t(R.string.ring_stream_mute), t(R.string.ring_stream_mute_note)),
        )),
        SlotGroup(t(R.string.ring_group_host), listOf(
            SlotOption("host:power.sleep", t(R.string.host_power_sleep), t(R.string.ring_host_note)),
            SlotOption("host:power.reboot", t(R.string.host_power_reboot), t(R.string.ring_host_note)),
            SlotOption("host:power.shutdown", t(R.string.host_power_shutdown), t(R.string.ring_host_note)),
        )),
    )
    if (cfg.shortcuts.isNotEmpty()) {
        g += SlotGroup(t(R.string.ring_group_shortcuts), cfg.shortcuts.map {
            SlotOption("shortcut:${it.id}", it.label.ifEmpty { chordChip(it.keys) }, if (it.label.isEmpty()) null else chordChip(it.keys))
        })
    }
    if (!forShortcut) g += SlotGroup(t(R.string.ring_group_empty), listOf(SlotOption("", t(R.string.ring_empty_slot))))
    return g
}

/** The catalogue by group; the current pick is marked. */
@Composable
internal fun SlotPicker(groups: List<SlotGroup>, current: String, onPick: (String) -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.ring_picker_title)) },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                groups.forEach { g ->
                    Text(
                        g.title,
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.padding(top = 12.dp, bottom = 4.dp),
                    )
                    g.options.forEach { o ->
                        Row(
                            Modifier.fillMaxWidth().clickable { onPick(o.id) }.padding(vertical = 8.dp)
                                .alpha(if (o.dim) 0.45f else 1f),
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
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) } },
    )
}

/** A shortcut on the editing screen, new or existing. */
private data class ShortcutDraft(val id: String, val label: String, val keys: List<String>, val isNew: Boolean)

private val MODIFIER_KEYS = listOf("ctrl", "alt", "shift", "win")

/** The keys a chord can end on, grouped the way a keyboard groups them — every name [keyVk] knows. */
internal val KEY_GROUPS: List<Pair<Int, List<String>>> = listOf(
    R.string.ring_key_group_function to (listOf("escape") + (1..12).map { "f$it" }),
    R.string.ring_key_group_letters to "qwertyuiopasdfghjklzxcvbnm".map { it.toString() },
    R.string.ring_key_group_numbers to ((1..9).map { it.toString() } + "0"),
    R.string.ring_key_group_editing to listOf("tab", "space", "enter", "backspace", "delete", "insert"),
    R.string.ring_key_group_navigation to listOf("home", "end", "pageup", "pagedown", "up", "down", "left", "right"),
    R.string.ring_key_group_other to listOf("printscreen", "pause", "capslock"),
)

/**
 * One key combination: a name, the modifiers held as chips, the key it ends on picked from a
 * keyboard (or none: a modifier alone is one), and the disc as the ring will draw it.
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
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.common_back))
            }
            Text(
                stringResource(if (draft.isNew) R.string.ring_shortcut_new_title else R.string.ring_shortcut_title),
                style = MaterialTheme.typography.headlineMedium,
            )
        }
        SettingsGroup {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(14.dp)) {
                KeycapDisc(keys)
                Column {
                    Text(
                        label.ifEmpty { if (keys.isEmpty()) stringResource(R.string.ring_shortcut_no_key) else chordChip(keys) },
                        style = MaterialTheme.typography.bodyLarge,
                    )
                    Text(
                        if (keys.isEmpty()) stringResource(R.string.ring_shortcut_preview) else chordChip(keys),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        fontFamily = if (keys.isEmpty()) null else FontFamily.Monospace,
                    )
                }
            }
            OutlinedTextField(
                value = label,
                onValueChange = { label = it },
                label = { Text(stringResource(R.string.ring_shortcut_name_optional)) },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
        }
        SettingsGroup(stringResource(R.string.ring_shortcut_modifiers)) {
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
            SettingsGroup(stringResource(title)) {
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    group.forEach { k ->
                        FilterChip(
                            selected = key == k,
                            onClick = { key = if (key == k) null else k },
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
            Button(onClick = { onSave(draft.copy(label = label, keys = keys)) }, enabled = keys.isNotEmpty()) {
                Text(stringResource(if (draft.isNew) R.string.common_add else R.string.common_save))
            }
            TextButton(onClick = onBack) { Text(stringResource(R.string.common_cancel)) }
            Spacer(Modifier.weight(1f))
            if (!draft.isNew) {
                TextButton(onClick = onDelete) {
                    Text(stringResource(R.string.ring_shortcut_remove), color = MaterialTheme.colorScheme.error)
                }
            }
        }
    }
}
