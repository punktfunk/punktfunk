package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGesturesAfterLongPress
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Add
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.boundsInParent
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.zIndex
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.keyLegend
import kotlin.math.roundToInt

/**
 * The key row's editor: the row as the stream draws it, wrapped onto lines so every button
 * shows. A tap on a button changes or removes it, a button held and dragged onto another takes
 * its place, and the plus adds one. [cfg] is the quick actions being edited and [onChange] takes
 * them with the new row. Reset drops the row, which then follows [DEFAULT_ROW] again.
 */
@Composable
internal fun KeyRowEditor(cfg: OverlayConfig, onChange: (OverlayConfig) -> Unit, onBack: () -> Unit) {
    BackHandler(onBack = onBack)
    val ctx = textContext()
    val preview = remember(ctx) { previewActionsIn(ctx) }
    val buttons = cfg.rowButtons
    // The button being picked: its place in the row, or the row's size for a new one.
    var picking by remember { mutableStateOf<Int?>(null) }
    fun write(row: List<RowSlot>) = onChange(cfg.copy(row = row))

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
            Text(stringResource(R.string.ring_row_title), style = MaterialTheme.typography.headlineMedium)
        }
        SettingsGroup(footer = stringResource(R.string.ring_row_hint)) {
            RowCaps(
                buttons, cfg, preview,
                onTap = { picking = it },
                onMove = { from, to -> write(buttons.toMutableList().apply { add(to, removeAt(from)) }) },
                onAdd = { picking = buttons.size },
            )
        }
        SettingsGroup {
            TextButton(onClick = { onChange(cfg.copy(row = null)) }, enabled = cfg.row != null) {
                Text(stringResource(R.string.common_reset))
            }
        }
    }

    picking?.let { k ->
        RowButtonPicker(
            groups = slotGroups(cfg, ctx).dropLast(1), // a row has no empty slot
            current = buttons.getOrNull(k)?.id,
            onPick = { id ->
                RowSlot.parse(id)?.let { b -> write(buttons.toMutableList().apply { if (k < size) set(k, b) else add(b) }) }
                picking = null
            },
            onRemove = {
                write(buttons.filterIndexed { i, _ -> i != k })
                picking = null
            },
            onDismiss = { picking = null },
        )
    }
}

/**
 * The row's buttons on as many lines as they need, then the plus. A held button follows the
 * finger; let go over another button, it moves to that button's place.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun RowCaps(
    buttons: List<RowSlot>,
    cfg: OverlayConfig,
    actions: RingActions,
    onTap: (Int) -> Unit,
    onMove: (Int, Int) -> Unit,
    onAdd: () -> Unit,
) {
    val haptics = rememberConsoleHaptics()
    // Where each button lies in the flow, and the one under a finger with how far it was carried.
    val bounds = remember { HashMap<Int, Rect>() }
    var drag by remember { mutableStateOf<Pair<Int, Offset>?>(null) }
    val move by rememberUpdatedState(onMove)
    val tap by rememberUpdatedState(onTap)
    FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        buttons.forEachIndexed { i, slot ->
            val carried = drag?.takeIf { it.first == i }?.second
            RowCap(
                slot, cfg, actions, latch = null,
                modifier = Modifier
                    .zIndex(if (carried != null) 1f else 0f)
                    .onGloballyPositioned { bounds[i] = it.boundsInParent() }
                    .offset { carried?.let { IntOffset(it.x.roundToInt(), it.y.roundToInt()) } ?: IntOffset.Zero }
                    .height(40.dp)
                    .widthIn(min = 52.dp, max = 160.dp)
                    .pointerInput(i) { detectTapGestures(onTap = { tap(i) }) }
                    // After the tap detector, so it sees the lift first and a drag's end is no tap.
                    .pointerInput(i, buttons.size) {
                        detectDragGesturesAfterLongPress(
                            onDragStart = {
                                haptics.tick()
                                drag = i to Offset.Zero
                            },
                            onDragEnd = {
                                val at = drag?.second?.let { d -> bounds[i]?.center?.plus(d) }
                                drag = null
                                val target = at?.let { p -> bounds.entries.firstOrNull { it.value.contains(p) }?.key }
                                if (target != null && target != i && target < buttons.size) move(i, target)
                            },
                            onDragCancel = { drag = null },
                        ) { change, delta ->
                            change.consume()
                            drag = i to ((drag?.second ?: Offset.Zero) + delta)
                        }
                    },
            )
        }
        Cap(armed = false, Modifier.height(40.dp).width(52.dp).clickable(onClick = onAdd)) {
            Icon(Icons.Filled.Add, contentDescription = stringResource(R.string.common_add))
        }
    }
}

/**
 * What one row button does: a single key, picked on a second page, or any quick action of the
 * ring's catalogue ([groups]). [current] is the id of the button being changed, which can also be
 * removed; `null` picks for a new button.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun RowButtonPicker(
    groups: List<SlotGroup>,
    current: String?,
    onPick: (String) -> Unit,
    onRemove: () -> Unit,
    onDismiss: () -> Unit,
) {
    var keys by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(if (keys) R.string.ring_row_key else R.string.ring_picker_title)) },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                if (keys) {
                    // The modifiers first and untitled, then the groups a keyboard has.
                    (listOf(null to MODIFIER_KEYS) + KEY_GROUPS).forEach { (title, names) ->
                        if (title != null) {
                            Text(
                                stringResource(title),
                                style = MaterialTheme.typography.labelMedium,
                                color = MaterialTheme.colorScheme.primary,
                                modifier = Modifier.padding(top = 12.dp, bottom = 4.dp),
                            )
                        }
                        FlowRow(
                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                            verticalArrangement = Arrangement.spacedBy(6.dp),
                        ) {
                            names.forEach { name ->
                                Cap(
                                    armed = current == "key:$name",
                                    Modifier.height(40.dp).widthIn(min = 48.dp).clickable { onPick("key:$name") },
                                ) {
                                    Text(
                                        keyLegend(name),
                                        style = MaterialTheme.typography.labelLarge,
                                        maxLines = 1,
                                        modifier = Modifier.padding(horizontal = 8.dp),
                                    )
                                }
                            }
                        }
                    }
                } else {
                    Row(
                        Modifier.fillMaxWidth().clickable { keys = true }.padding(vertical = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(stringResource(R.string.ring_row_key), style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
                        if (current?.startsWith("key:") == true) Text("✓", color = MaterialTheme.colorScheme.primary)
                        Icon(
                            Icons.AutoMirrored.Filled.KeyboardArrowRight,
                            contentDescription = null,
                            tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    SlotOptions(groups, current.orEmpty(), onPick)
                }
            }
        },
        confirmButton = {
            if (current != null) {
                TextButton(onClick = onRemove) {
                    Text(stringResource(R.string.common_remove), color = MaterialTheme.colorScheme.error)
                }
            }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) } },
    )
}
