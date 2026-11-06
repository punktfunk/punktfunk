package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R

/**
 * What to press. Key and button names are data and read the same in every language; a gesture, or
 * a hold of a key, is worded by a message that takes the key in its `{keys}`.
 */
sealed interface Press {
    data class Keys(val names: String) : Press
    data class Worded(val message: Int, val keys: String = "") : Press
}

/** What to press, then what it does ([text] is a message id). */
data class ShortcutItem(val press: Press, val text: Int)

/** A heading ([title] is a message id; [titleArgs] fill its placeholders in order) and its rows. */
data class ShortcutGroup(val title: Int, val items: List<ShortcutItem>, val titleArgs: List<String> = emptyList())

/**
 * The in-stream keys, gestures and controller chords, as this client binds them: the key chords
 * in [MainActivity.dispatchKeyEvent], the gestures in `StreamScreen`, the pad chords in the kit's
 * `GamepadRouter`. The console draws the same facts from `pf-client-core`'s `shortcuts`.
 */
val shortcutGroups: List<ShortcutGroup> = listOf(
    ShortcutGroup(
        R.string.controls_group_keyboard,
        listOf(
            ShortcutItem(Press.Keys("Ctrl+Alt+Shift+Q"), R.string.controls_release_pointer),
            ShortcutItem(Press.Keys("Ctrl+Alt+Shift+O"), R.string.controls_open_dial),
        ),
    ),
    ShortcutGroup(
        R.string.controls_group_touch_full,
        listOf(
            ShortcutItem(Press.Keys("Back"), R.string.controls_open_dial),
            ShortcutItem(Press.Worded(R.string.controls_gesture_three_finger_tap), R.string.controls_cycle_stats),
            ShortcutItem(Press.Worded(R.string.controls_gesture_three_finger_swipe_up), R.string.controls_show_keyboard),
            ShortcutItem(Press.Worded(R.string.controls_gesture_three_finger_swipe_down), R.string.controls_hide_keyboard),
            ShortcutItem(Press.Worded(R.string.controls_gesture_two_finger_twist), R.string.controls_open_dial),
        ),
    ),
    ShortcutGroup(
        R.string.controls_group_pad_full,
        listOf(
            ShortcutItem(Press.Keys("Select + A"), R.string.controls_open_dial),
            ShortcutItem(Press.Keys("Select + X"), R.string.controls_cycle_stats),
            ShortcutItem(Press.Keys("Select + Y"), R.string.controls_toggle_mic),
            ShortcutItem(Press.Worded(R.string.controls_hold, "Select"), R.string.controls_guide_hold),
            ShortcutItem(Press.Keys("L1 + R1 + Start + Select"), R.string.controls_hold_disconnect),
        ),
        titleArgs = listOf("Select", "Back", "View"),
    ),
)

/** Read-only list of the in-stream controls. Reached from Settings → About; Back returns there. */
@Composable
fun ShortcutsScreen(onBack: () -> Unit) {
    BackHandler(onBack = onBack)
    Column(Modifier.fillMaxSize()) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 4.dp, end = 12.dp, top = 8.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = onBack) {
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = stringResource(R.string.common_back))
            }
            Text(stringResource(R.string.controls_title), style = MaterialTheme.typography.headlineSmall)
        }
        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(start = 20.dp, end = 20.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            Text(
                stringResource(R.string.controls_lead),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            for (group in shortcutGroups) {
                SettingsGroup(header = stringResource(group.title, *group.titleArgs.toTypedArray())) {
                    for (item in group.items) {
                        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                            Text(
                                when (val p = item.press) {
                                    is Press.Keys -> p.names
                                    is Press.Worded -> stringResource(p.message, p.keys)
                                },
                                style = MaterialTheme.typography.bodyMedium,
                                fontWeight = FontWeight.SemiBold,
                                modifier = Modifier.width(132.dp),
                            )
                            Text(stringResource(item.text), style = MaterialTheme.typography.bodyMedium)
                        }
                    }
                }
            }
        }
    }
}
