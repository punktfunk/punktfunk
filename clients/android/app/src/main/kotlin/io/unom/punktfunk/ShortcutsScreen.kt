package io.unom.punktfunk

import android.content.Context
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
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
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.Keymap
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Shortcuts
import io.unom.punktfunk.kit.Shortcuts.Action
import io.unom.punktfunk.kit.Shortcuts.Binding
import io.unom.punktfunk.kit.Shortcuts.Conflict
import io.unom.punktfunk.kit.Shortcuts.Trigger
import io.unom.punktfunk.kit.SlotId
import io.unom.punktfunk.kit.keyLegend
import io.unom.punktfunk.kit.keyVk

/**
 * Whether a stream on this device acts on [this] binding. Android has no window and no live
 * clipboard switch, a key combination must still be in the ring's list ([cfg]), and an action
 * from a newer client does nothing. The rest is gated live, like the dial's slot.
 */
internal fun Binding.actsHere(cfg: OverlayConfig): Boolean = when (val a = action) {
    is Action.Unknown -> false
    Action.Ring -> true
    is Action.Slot -> when (val s = a.slot) {
        SlotId.Fullscreen, SlotId.Minimize, SlotId.Clipboard -> false
        is SlotId.Shortcut -> cfg.shortcut(s.shortcutId) != null
        else -> true
    }
}

/** An action as the dial's picker names it, a key combination by the name the player gave it, a
 *  host action by this app's words for it; anything else by its id. */
internal fun actionLabel(action: Action, cfg: OverlayConfig, ctx: Context): String =
    slotGroups(cfg, ctx, forShortcut = true).flatMap { it.options }.firstOrNull { it.id == action.id }?.label
        ?: ((action as? Action.Slot)?.slot as? SlotId.Host)?.let {
            HostActions.Action(it.actionId, action.id, danger = false, available = false, unavailableReason = "")
                .text(ctx.resources)
        }
        ?: action.id

/** Every key a shortcut can end on, for the recorder's reverse lookup of a VK. */
private val KEY_NAMES: List<String> = KEY_GROUPS.flatMap { it.second } + (13..24).map { "f$it" }

/**
 * Settings → Input → Shortcuts (design §5): what the controller, keyboard, touchscreen and remote
 * do in a stream, each binding row openable. [settings] is the layer being edited; [onChange]
 * writes the whole blob back through the row's `update`, so a preset that changes one owns them
 * all, and [onReset] drops that, or writes the defaults' empty blob.
 */
@Composable
internal fun ShortcutsScreen(
    settings: Settings,
    onChange: (String) -> Unit,
    onReset: () -> Unit,
    onBack: () -> Unit,
) {
    val ctx = textContext()
    val cfg = remember(settings.overlayActions) { OverlayConfig.parse(settings.overlayActions) }
    val current = remember(settings.shortcuts, settings.guideGesture) { settings.shortcutBindings() }
    val touchscreen = remember { !isTvDevice(ctx) }
    // The binding being edited: its index, or -1 for a new one.
    var editing by remember { mutableStateOf<Int?>(null) }

    editing?.let { i ->
        ShortcutSheet(
            all = current,
            index = i,
            cfg = cfg,
            onSave = { next ->
                onChange(next.toJson())
                editing = null
            },
            onBack = { editing = null },
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
        Header(stringResource(R.string.player_shortcuts), onBack)
        Text(
            stringResource(R.string.shortcuts_lead),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        for (group in current.rows(touchscreen)) {
            if (group.rows.isEmpty()) continue
            SettingsGroup(header = group.heading.words(ctx.resources)) {
                for (row in group.rows) {
                    val b = row.binding?.let { current.bindings[it] }
                    val here = b?.actsHere(cfg) != false
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .then(if (row.binding != null) Modifier.clickable { editing = row.binding } else Modifier)
                            .padding(vertical = 4.dp)
                            .alpha(if (here) 1f else 0.45f),
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(
                                row.text?.words(ctx.resources) ?: b?.let { actionLabel(it.action, cfg, ctx) }.orEmpty(),
                                style = MaterialTheme.typography.bodyLarge,
                            )
                            when {
                                b == null -> stringResource(R.string.shortcuts_fixed)
                                !here -> stringResource(R.string.shortcuts_not_on_device)
                                else -> null
                            }?.let {
                                Text(
                                    it,
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        }
                        Text(
                            row.press.words(ctx.resources),
                            style = MaterialTheme.typography.bodyMedium,
                            fontWeight = FontWeight.SemiBold,
                        )
                        if (b != null) {
                            Icon(
                                Icons.AutoMirrored.Filled.KeyboardArrowRight,
                                contentDescription = null,
                                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }
        SettingsGroup {
            TextButton(onClick = { editing = -1 }) { Text(stringResource(R.string.shortcuts_add)) }
            TextButton(onClick = onReset) {
                Text(stringResource(R.string.shortcuts_reset), color = MaterialTheme.colorScheme.error)
            }
        }
    }
}

@Composable
private fun Header(title: String, onBack: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.common_back))
        }
        Text(title, style = MaterialTheme.typography.headlineMedium)
    }
}

/**
 * One shortcut (design §5): Does (the dial's picker, plus Quick actions itself), Press (Record or
 * Choose), Fires on, Remove. [index] -1 adds one. Save checks the trigger against [all] without
 * this binding: an identical trigger offers Replace, a press inside another chord says which
 * fires first.
 */
@Composable
private fun ShortcutSheet(
    all: Shortcuts,
    index: Int,
    cfg: OverlayConfig,
    onSave: (Shortcuts) -> Unit,
    onBack: () -> Unit,
) {
    BackHandler(onBack = onBack)
    val ctx = textContext()
    val old = all.bindings.getOrNull(index)
    val others = Shortcuts(all.bindings.filterIndexed { i, _ -> i != index })
    var action by remember { mutableStateOf(old?.action) }
    var trigger by remember { mutableStateOf(old?.trigger) }
    var picking by remember { mutableStateOf(false) }
    var recording by remember { mutableStateOf(false) }
    var choosing by remember { mutableStateOf(false) }
    var replacing by remember { mutableStateOf<Int?>(null) }

    val candidate = action?.let { a -> trigger?.let { Binding(it, a) } }
    val conflict = candidate?.let(others::validate)
    /** This binding in place, or added last; [drop] (an index into [others]) is the one it replaces. */
    fun save(drop: Int?) {
        val c = candidate ?: return
        val next = all.bindings.toMutableList()
        if (index >= 0) next[index] = c else next += c
        drop?.let { next.removeAt(if (index in 0..it) it + 1 else it) }
        onSave(Shortcuts(next))
    }

    if (recording) Recorder(onChord = { trigger = it }, onDone = { recording = false })

    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Header(action?.let { actionLabel(it, cfg, ctx) } ?: stringResource(R.string.shortcuts_add), onBack)
        SettingsGroup {
            // Does
            Row(
                Modifier.fillMaxWidth().clickable { picking = true }.padding(vertical = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    stringResource(R.string.shortcuts_does),
                    style = MaterialTheme.typography.bodyLarge,
                    modifier = Modifier.weight(1f),
                )
                Text(action?.let { actionLabel(it, cfg, ctx) } ?: "—", style = MaterialTheme.typography.bodyLarge)
                Icon(
                    Icons.AutoMirrored.Filled.KeyboardArrowRight,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            // Press
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    stringResource(R.string.shortcuts_press),
                    style = MaterialTheme.typography.bodyLarge,
                    modifier = Modifier.weight(1f),
                )
                Text(
                    when {
                        recording -> stringResource(R.string.shortcuts_record_prompt)
                        else -> trigger?.chord() ?: "—"
                    },
                    style = MaterialTheme.typography.bodyLarge,
                    fontWeight = FontWeight.SemiBold,
                )
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(onClick = { choosing = false; recording = true }) {
                    Text(stringResource(R.string.shortcuts_record))
                }
                OutlinedButton(onClick = { recording = false; choosing = !choosing }) {
                    Text(stringResource(R.string.shortcuts_choose))
                }
            }
            // Fires on: keys are presses; one button is a hold.
            val t = trigger
            val oneButton = t is Trigger.Pad && t.buttons.size == 1
            Text(stringResource(R.string.shortcuts_fires_on), style = MaterialTheme.typography.bodyLarge)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                FilterChip(
                    selected = t?.hold == false,
                    onClick = { if (t is Trigger.Pad) trigger = t.copy(hold = false) },
                    label = { Text(stringResource(R.string.shortcuts_fires_press)) },
                    enabled = t is Trigger.Key || (t is Trigger.Pad && !oneButton),
                )
                FilterChip(
                    selected = t?.hold == true,
                    onClick = { if (t is Trigger.Pad) trigger = t.copy(hold = true) },
                    label = { Text(stringResource(R.string.shortcuts_fires_hold)) },
                    enabled = t is Trigger.Pad,
                )
            }
            conflictLine(conflict, candidate, others)?.let {
                Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
            }
            if (oneButton) {
                Text(
                    stringResource(R.string.shortcuts_one_button_hold),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        if (choosing) Chooser(trigger) { trigger = it }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically) {
            Button(
                onClick = { (conflict as? Conflict.Identical)?.let { replacing = it.index } ?: save(drop = null) },
                enabled = candidate != null && (conflict == null || conflict is Conflict.Identical),
            ) { Text(stringResource(if (old == null) R.string.common_add else R.string.common_save)) }
            TextButton(onClick = onBack) { Text(stringResource(R.string.common_cancel)) }
            Spacer(Modifier.weight(1f))
            if (old != null) {
                TextButton(onClick = { onSave(others) }) {
                    Text(stringResource(R.string.shortcuts_remove), color = MaterialTheme.colorScheme.error)
                }
            }
        }
    }

    if (picking) {
        SlotPicker(
            groups = slotGroups(cfg, ctx, forShortcut = true),
            current = action?.id.orEmpty(),
            onPick = { action = Action.parse(it); picking = false },
            onDismiss = { picking = false },
        )
    }
    replacing?.let { i ->
        val them = others.bindings[i]
        AlertDialog(
            onDismissRequest = { replacing = null },
            text = {
                Text(
                    stringResource(
                        R.string.shortcuts_identical_replace,
                        them.trigger.label().words(ctx.resources),
                        actionLabel(them.action, cfg, ctx),
                    ),
                )
            },
            confirmButton = {
                TextButton(onClick = { replacing = null; save(drop = i) }) { Text(stringResource(R.string.shortcuts_replace)) }
            },
            dismissButton = {
                TextButton(onClick = { replacing = null }) { Text(stringResource(R.string.common_cancel)) }
            },
        )
    }
}

/** What stops this trigger, in a line; Identical has its own dialog. */
@Composable
private fun conflictLine(conflict: Conflict?, candidate: Binding?, others: Shortcuts): String? = when (conflict) {
    null, is Conflict.Identical -> null
    is Conflict.PressSubset -> {
        val mine = candidate?.trigger ?: return null
        val theirs = others.bindings[conflict.index].trigger
        // The press inside the other chord fires first.
        val (first, second) = if (!mine.hold && theirs.ids().containsAll(mine.ids())) mine to theirs else theirs to mine
        stringResource(R.string.shortcuts_subset, first.chord(), second.chord())
    }
    // The one-button line already stands under Fires on.
    Conflict.SinglePress -> null
    Conflict.NoModifier -> stringResource(R.string.shortcuts_needs_modifier)
    Conflict.NoKey -> stringResource(R.string.ring_shortcut_pick_key_first)
}

/**
 * Press-to-record: while composed, every pad button and key goes here instead of the menus. A
 * pad's buttons build the chord while down, it is taken when the first lifts, and recording ends
 * once all have lifted, so no release reaches the menus. A key is taken with the modifiers held
 * when it went down. Back from a remote, or a bare Escape, ends it unchanged.
 */
@Composable
private fun Recorder(onChord: (Trigger) -> Unit, onDone: () -> Unit) {
    val activity = LocalContext.current as? MainActivity
    DisposableEffect(activity) {
        val pressed = mutableListOf<Int>()
        val held = mutableSetOf<Int>()
        var taken = false
        var key: Trigger.Key? = null
        var hat = 0
        fun padEdge(bit: Int, down: Boolean) {
            if (down) {
                held += bit
                if (!taken && bit !in pressed && pressed.size < 4) pressed += bit
            } else if (held.remove(bit)) {
                if (!taken && pressed.isNotEmpty()) onChord(Trigger.Pad(pressed.toList(), hold = pressed.size == 1))
                taken = true
                if (held.isEmpty()) onDone()
            }
        }
        val keyProbe: (KeyEvent) -> Boolean = probe@{ e ->
            val down = e.action == KeyEvent.ACTION_DOWN
            if (Gamepad.eventFromPad(e)) {
                val bit = Gamepad.padButtonBit(Gamepad.padKeyCode(e), e.flags)
                if (bit != 0 && e.repeatCount == 0) padEdge(bit, down)
                return@probe true
            }
            val bare = !e.isCtrlPressed && !e.isAltPressed && !e.isShiftPressed
            if (e.keyCode == KeyEvent.KEYCODE_BACK || e.keyCode == KeyEvent.KEYCODE_ESCAPE && bare) {
                if (!down) onDone()
                return@probe true
            }
            if (KeyEvent.isModifierKey(e.keyCode)) return@probe true
            val vk = Keymap.toVk(e.keyCode).takeIf { it != 0 } ?: Keymap.toVk(e)
            val name = KEY_NAMES.firstOrNull { keyVk(it) == vk } ?: return@probe true
            if (down && e.repeatCount == 0) {
                key = Trigger.Key(
                    Shortcuts.MODIFIERS.filter {
                        when (it) {
                            "ctrl" -> e.isCtrlPressed
                            "alt" -> e.isAltPressed
                            else -> e.isShiftPressed
                        }
                    },
                    name,
                )
            } else if (!down) {
                key?.takeIf { it.key == name }?.let {
                    onChord(it)
                    onDone()
                }
            }
            true
        }
        // A D-pad that reports as a hat is buttons too.
        val motionProbe: (MotionEvent) -> Boolean = probe@{ e ->
            if (!e.isFromSource(InputDevice.SOURCE_JOYSTICK)) return@probe false
            val x = e.getAxisValue(MotionEvent.AXIS_HAT_X)
            val y = e.getAxisValue(MotionEvent.AXIS_HAT_Y)
            val now = (if (x < -0.5f) Gamepad.BTN_DPAD_LEFT else 0) or
                (if (x > 0.5f) Gamepad.BTN_DPAD_RIGHT else 0) or
                (if (y < -0.5f) Gamepad.BTN_DPAD_UP else 0) or
                (if (y > 0.5f) Gamepad.BTN_DPAD_DOWN else 0)
            for (bit in listOf(Gamepad.BTN_DPAD_LEFT, Gamepad.BTN_DPAD_RIGHT, Gamepad.BTN_DPAD_UP, Gamepad.BTN_DPAD_DOWN)) {
                if (now and bit != hat and bit) padEdge(bit, now and bit != 0)
            }
            hat = now
            true
        }
        val probes = MainActivity.PadProbes(keyProbe, motionProbe)
        activity?.pushPadProbes(probes)
        onDispose { activity?.removePadProbes(probes) }
    }
}

/**
 * The chooser, for a device not at hand: a pad's buttons (up to four, in the order picked), or
 * Ctrl, Alt, Shift and one key from the key grid.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun Chooser(trigger: Trigger?, onChange: (Trigger) -> Unit) {
    var pad by remember { mutableStateOf(trigger !is Trigger.Key) }
    SettingsGroup {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            FilterChip(pad, onClick = { pad = true }, label = { Text(stringResource(R.string.controls_group_pad)) })
            FilterChip(!pad, onClick = { pad = false }, label = { Text(stringResource(R.string.controls_group_keyboard)) })
        }
        if (pad) {
            val t = trigger as? Trigger.Pad
            val picked = t?.buttons.orEmpty()
            FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                for ((_, bit) in Shortcuts.BUTTONS) {
                    FilterChip(
                        selected = bit in picked,
                        onClick = {
                            val next = if (bit in picked) picked - bit else (picked + bit).take(4)
                            if (next.isNotEmpty()) onChange(Trigger.Pad(next, hold = next.size == 1 || t?.hold == true))
                        },
                        label = { Text(Shortcuts.buttonLabel(bit)) },
                    )
                }
            }
        } else {
            val t = trigger as? Trigger.Key
            val mods = t?.modifiers.orEmpty()
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                for (m in Shortcuts.MODIFIERS) {
                    FilterChip(
                        selected = m in mods,
                        onClick = {
                            val next = if (m in mods) mods - m else mods + m
                            onChange(Trigger.Key(Shortcuts.MODIFIERS.filter { it in next }, t?.key.orEmpty()))
                        },
                        label = { Text(keyLegend(m)) },
                    )
                }
            }
            for ((title, keys) in KEY_GROUPS) {
                Text(stringResource(title), style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.primary)
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    for (k in keys) {
                        FilterChip(
                            selected = t?.key == k,
                            onClick = { onChange(Trigger.Key(mods, k)) },
                            label = { Text(keyLegend(k), maxLines = 1, softWrap = false) },
                            modifier = Modifier.width(if (keyLegend(k).length > 2) 88.dp else 48.dp),
                        )
                    }
                }
            }
        }
    }
}
