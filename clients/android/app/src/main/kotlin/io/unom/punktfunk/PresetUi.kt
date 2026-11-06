package io.unom.punktfunk

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.ArrowDropDown
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.AssistChip
import androidx.compose.material3.AssistChipDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.pluralStringResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R

/**
 * The scope switcher: the one new settings concept. Selecting a preset puts the WHOLE settings
 * surface into that preset's scope — there is one settings UI, never a second parallel editor
 * that drifts from it. "Default settings" is the base layer every preset inherits from.
 *
 * A chips row rather than a menu, because on touch the scopes are worth seeing at a glance and
 * there are rarely more than a handful. Managing a preset lives ON its chip: the selected one
 * grows a chevron, and tapping it again opens Edit / Duplicate / Delete anchored under it. That
 * replaced a lone overflow button parked after the LAST chip — which meant scrolling past every
 * preset to reach an action that applied to one of them, with nothing on screen saying which.
 *
 * With no presets at all the row is just "Default settings" and a "New preset" chip, which is
 * all the clutter a user who never wants this feature ever sees.
 */
@Composable
internal fun PresetScopeChips(
    presets: List<StreamPreset>,
    selectedId: String?,
    onSelect: (String?) -> Unit,
    onNew: () -> Unit,
    onEdit: (StreamPreset) -> Unit,
    onDuplicate: (StreamPreset) -> Unit,
    onDelete: (StreamPreset) -> Unit,
    modifier: Modifier = Modifier,
) {
    // Which chip's menu is open — one at a time, and it closes itself when the scope changes.
    var menuFor by remember { mutableStateOf<String?>(null) }
    Row(
        modifier = modifier.horizontalScroll(rememberScrollState()).padding(horizontal = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        FilterChip(
            selected = selectedId == null,
            onClick = { onSelect(null) },
            label = { Text(stringResource(R.string.prefs_scope_defaults)) },
        )
        presets.forEach { p ->
            val isSelected = selectedId == p.id
            Box {
                FilterChip(
                    selected = isSelected,
                    // Tap to select; tap the selected one — the one wearing the chevron — to manage
                    // it. The action is on the object it acts on, which is the whole point.
                    onClick = { if (isSelected) menuFor = p.id else onSelect(p.id) },
                    // The dot and the chevron ride INSIDE the label, not in the `leadingIcon` /
                    // `trailingIcon` slots: those reserve an 18dp icon and shrink the chip's padding
                    // to suit, so a chip with an accent (or with the chevron) would sit differently
                    // from "Default settings" beside it. In the label every chip keeps the same
                    // padding and the spacing is ours to set.
                    label = {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            accentColor(p.accent)?.let { dot ->
                                AccentDot(dot, size = 8)
                                Spacer(Modifier.width(8.dp))
                            }
                            Text(p.name)
                            if (isSelected) {
                                Spacer(Modifier.width(2.dp))
                                Icon(
                                    Icons.Filled.ArrowDropDown,
                                    contentDescription = stringResource(R.string.android_preset_manage_a11y, p.name),
                                    modifier = Modifier.size(18.dp),
                                )
                            }
                        }
                    },
                )
                DropdownMenu(expanded = menuFor == p.id, onDismissRequest = { menuFor = null }) {
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.android_preset_menu_edit)) },
                        onClick = { menuFor = null; onEdit(p) },
                    )
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.common_duplicate)) },
                        onClick = { menuFor = null; onDuplicate(p) },
                    )
                    DropdownMenuItem(
                        text = { Text(stringResource(R.string.preset_menu_delete)) },
                        onClick = { menuFor = null; onDelete(p) },
                    )
                }
            }
        }
        AssistChip(
            onClick = onNew,
            label = { Text(stringResource(R.string.preset_new)) },
            leadingIcon = {
                Icon(Icons.Filled.Add, contentDescription = null, Modifier.size(AssistChipDefaults.IconSize))
            },
        )
    }
}

/**
 * Create or edit a preset: its name and its colour, decided together. They were two flows —
 * a name dialog at creation, "Change colour…" afterwards — which meant every preset started
 * colourless-looking until the user went hunting for a menu item, and the accent is exactly the
 * signal that has to be there from the first moment (it is all a bound host card's chip and a
 * pinned card's tint have to go on).
 *
 * Names must be unique case-insensitively: two "Work" chips in a menu are ambiguous, and a
 * `punktfunk://…?preset=Work` link would have to refuse rather than guess. [taken] is the live
 * duplicate check, which lets an edit keep its own name (and change only its case).
 */
@Composable
internal fun PresetEditorDialog(
    title: String,
    confirmLabel: String,
    initialName: String,
    initialAccent: String?,
    creating: Boolean,
    taken: (String) -> Boolean,
    onConfirm: (name: String, accent: String?) -> Unit,
    onDismiss: () -> Unit,
) {
    var name by remember { mutableStateOf(initialName) }
    var accent by remember { mutableStateOf(initialAccent) }
    val trimmed = name.trim()
    val duplicate = trimmed.isNotEmpty() && taken(trimmed)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            PresetEditorFields(
                name = name,
                accent = accent,
                duplicate = duplicate,
                creating = creating,
                onNameChange = { name = it },
                onAccentChange = { accent = it },
            )
        },
        confirmButton = {
            TextButton(
                enabled = trimmed.isNotEmpty() && !duplicate,
                onClick = { onConfirm(trimmed, accent) },
            ) { Text(confirmLabel) }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) } },
    )
}

/**
 * The editor's body. Extracted from [PresetEditorDialog] so the screenshot harness can render
 * exactly these — a focused text field inside a Dialog window never reaches idle under Robolectric,
 * so the dialog itself is uncapturable, and an eyeballed-only layout is how this shipped once with
 * the field and its caption touching.
 */
@OptIn(ExperimentalLayoutApi::class)
@Composable
internal fun PresetEditorFields(
    name: String,
    accent: String?,
    duplicate: Boolean,
    creating: Boolean,
    onNameChange: (String) -> Unit,
    onAccentChange: (String?) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        OutlinedTextField(
            value = name,
            onValueChange = onNameChange,
            label = { Text(stringResource(R.string.preset_name)) },
            placeholder = { Text(stringResource(R.string.preset_name_placeholder)) },
            singleLine = true,
            isError = duplicate,
        )
        Text(
            when {
                duplicate -> stringResource(R.string.preset_name_taken, name.trim())
                creating -> stringResource(R.string.preset_new_hint)
                else -> stringResource(R.string.android_preset_edit_hint)
            },
            style = MaterialTheme.typography.bodySmall,
            color = if (duplicate) {
                MaterialTheme.colorScheme.error
            } else {
                MaterialTheme.colorScheme.onSurfaceVariant
            },
        )
        Text(
            stringResource(R.string.preset_color),
            style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.padding(top = 4.dp),
        )
        // A fixed 4×2 grid rather than a flow: eight colours wrapping to whatever fits the dialog
        // landed 6-then-2, which reads as a mistake. Two even rows read as a palette. The order is
        // the hue sweep from PRESET_ACCENTS, so it looks like a spectrum rather than a bag.
        //
        // Each row FILLS the width, its swatches sharing it equally, so the palette's edges line up
        // with the name field above it and every row is the same length. A fixed swatch size left
        // the rows short of the dialog's edge and wrapped unevenly, which read as the grid having
        // run out rather than as a deliberate block.
        Column(
            verticalArrangement = Arrangement.spacedBy(SWATCH_GAP),
            modifier = Modifier.fillMaxWidth(),
        ) {
            PRESET_ACCENTS.chunked(SWATCHES_PER_ROW).forEach { row ->
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(SWATCH_GAP),
                ) {
                    row.forEach { hex ->
                        Swatch(
                            colour = accentColor(hex),
                            selected = accent?.equals(hex, ignoreCase = true) == true,
                            onClick = { onAccentChange(hex) },
                            modifier = Modifier.weight(1f),
                        )
                    }
                }
            }
        }
        // "No colour" is a real choice, not only an initial state — the chip then falls back to the
        // theme's own accent, which is what a preset made before colours existed shows. It sits
        // apart from the grid and says so in words, rather than hiding as a ninth, colourless
        // circle that breaks the palette's rhythm.
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(10.dp))
                .clickable { onAccentChange(null) }
                .padding(vertical = 4.dp),
        ) {
            Swatch(colour = null, selected = accent == null, onClick = { onAccentChange(null) })
            Text(
                stringResource(R.string.preset_color_none),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 12.dp),
            )
        }
    }
}

/**
 * One colour choice. The selected one keeps its size and grows a ring OUTSIDE the disc with a gap
 * between the two, plus a check — a border drawn on the disc's own edge reads as a heavier circle
 * rather than as a selection, and colour-plus-check survives a reader who can't tell two of these
 * hues apart. The ring's space is always reserved, so picking never nudges the grid.
 *
 * `null` is "no colour": the surface's own variant, outlined so it reads as an empty slot rather
 * than a dark swatch.
 */
@Composable
internal fun Swatch(
    colour: Color?,
    selected: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier.size(SWATCH_TOTAL),
) {
    val fill = colour ?: MaterialTheme.colorScheme.surfaceVariant
    val description = stringResource(
        if (colour == null) R.string.preset_color_none else R.string.android_preset_swatch_a11y,
    )
    Box(
        modifier = modifier
            .aspectRatio(1f)
            .clip(CircleShape)
            .then(
                if (selected) {
                    Modifier.border(2.dp, MaterialTheme.colorScheme.onSurface, CircleShape)
                } else {
                    Modifier
                },
            )
            .clickable(onClick = onClick)
            .semantics { contentDescription = description },
        contentAlignment = Alignment.Center,
    ) {
        Box(
            Modifier
                // Padding, not a fixed size: the disc has to scale with a swatch that shares its
                // row's width, while the gap that makes the selection ring read stays constant.
                .fillMaxSize()
                .padding(RING_GAP)
                .clip(CircleShape)
                .background(fill)
                .then(
                    if (colour == null) {
                        Modifier.border(1.dp, MaterialTheme.colorScheme.outline, CircleShape)
                    } else {
                        Modifier
                    },
                ),
            contentAlignment = Alignment.Center,
        ) {
            if (selected) {
                Icon(
                    Icons.Filled.Check,
                    contentDescription = null,
                    modifier = Modifier.size(18.dp),
                    // These hues are all light enough that a near-black check is the readable one;
                    // the empty slot is dark, so it takes the surface's foreground instead.
                    tint = if (colour == null) {
                        MaterialTheme.colorScheme.onSurfaceVariant
                    } else {
                        Color(0xFF1B1633)
                    },
                )
            }
        }
    }
}

/**
 * The palette's geometry. [SWATCHES_PER_ROW] divides [PRESET_ACCENTS] exactly — that is the whole
 * reason the palette has ten colours — so both rows are full. [SWATCH_TOTAL] is only the fallback
 * footprint for a swatch outside the grid (the "no colour" one); in the grid a swatch takes an
 * equal share of the row instead.
 */
private const val SWATCHES_PER_ROW = 5
private val SWATCH_TOTAL = 44.dp
private val RING_GAP = 5.dp
private val SWATCH_GAP = 10.dp

/**
 * Deleting a preset is not destructive to anything but the preset — a host bound to it falls
 * back to the default settings and a card pinned to it disappears, neither of which is an error.
 * The warning counts both so the consequence is stated rather than discovered.
 */
@Composable
internal fun DeletePresetDialog(
    preset: StreamPreset,
    boundHosts: Int,
    pinnedCards: Int,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.preset_delete_named_title, preset.name)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                val consequences = buildList {
                    if (boundHosts > 0) {
                        add(pluralStringResource(R.plurals.preset_delete_hosts, boundHosts, boundHosts))
                    }
                    if (pinnedCards > 0) {
                        add(pluralStringResource(R.plurals.preset_delete_cards, pinnedCards, pinnedCards))
                    }
                }
                Text(
                    if (consequences.isEmpty()) {
                        stringResource(R.string.preset_delete_unused)
                    } else {
                        consequences.joinToString(" ")
                    },
                )
                Text(
                    stringResource(R.string.android_preset_delete_note),
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton(onClick = onConfirm) { Text(stringResource(R.string.common_delete)) } },
        dismissButton = { TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) } },
    )
}

/**
 * The per-host half of presets, inside the host's Edit sheet: which preset a plain tap uses
 * (the binding — the one thing that IS sticky; "Connect with ▸" on a card never rebinds), and which
 * presets get their own card in the host list.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun HostPresetBinding(
    presets: List<StreamPreset>,
    boundId: String?,
    onBind: (String?) -> Unit,
    pins: List<String>,
    onTogglePin: (String) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val bound = presets.firstOrNull { it.id == boundId }
    val defaults = stringResource(R.string.prefs_scope_defaults)
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { expanded = it }) {
            OutlinedTextField(
                value = bound?.name ?: defaults,
                onValueChange = {},
                readOnly = true,
                label = { Text(stringResource(R.string.android_preset_binding)) },
                trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
                modifier = Modifier
                    .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable)
                    .fillMaxWidth(),
            )
            ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                DropdownMenuItem(
                    text = { Text(defaults) },
                    onClick = { onBind(null); expanded = false },
                )
                presets.forEach { p ->
                    DropdownMenuItem(
                        text = { Text(p.name) },
                        onClick = { onBind(p.id); expanded = false },
                    )
                }
            }
        }
        Text(
            stringResource(R.string.preset_bind_host_hint),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            stringResource(R.string.android_preset_pinned_cards),
            style = MaterialTheme.typography.labelLarge,
            modifier = Modifier.padding(top = 8.dp),
        )
        Text(
            stringResource(R.string.preset_pin_hint),
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        presets.forEach { p ->
            Row(
                modifier = Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(p.name, modifier = Modifier.weight(1f))
                Checkbox(checked = p.id in pins, onCheckedChange = { onTogglePin(p.id) })
            }
        }
    }
}

/** The accent marker a preset's chip and its pinned cards wear. */
@Composable
internal fun AccentDot(color: Color, size: Int = 10) {
    Box(Modifier.size(size.dp).clip(CircleShape).background(color))
}

/** `#RRGGBB` → a Compose colour, or null when the stored string isn't one (never a crash). */
internal fun accentColor(hex: String?): Color? {
    val h = hex?.removePrefix("#") ?: return null
    if (h.length != 6 || !h.all { it.isDigit() || it.lowercaseChar() in 'a'..'f' }) return null
    return runCatching { Color(h.toLong(16) or 0xFF000000L) }.getOrNull()
}
