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
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp

/** What to press, then what it does. */
data class ShortcutItem(val keys: String, val text: String)

data class ShortcutGroup(val title: String, val items: List<ShortcutItem>)

/**
 * The in-stream keys, gestures and controller chords, as this client binds them: the key chords
 * in [MainActivity.dispatchKeyEvent], the gestures in `StreamScreen`, the pad chords in the kit's
 * `GamepadRouter`. The console draws the same facts from `pf-client-core`'s `shortcuts`.
 */
val shortcutGroups: List<ShortcutGroup> = listOf(
    ShortcutGroup(
        "Keyboard",
        listOf(
            ShortcutItem("Ctrl+Alt+Shift+Q", "Release the pointer, or capture it again"),
            ShortcutItem("Ctrl+Alt+Shift+O", "Open the quick actions dial"),
        ),
    ),
    ShortcutGroup(
        "Touchscreen (Trackpad and Direct pointer modes)",
        listOf(
            ShortcutItem("Back", "Open the quick actions dial"),
            ShortcutItem("Three-finger tap", "Cycle the statistics overlay"),
            ShortcutItem("Three-finger swipe up", "Show the keyboard"),
            ShortcutItem("Three-finger swipe down", "Hide the keyboard"),
            ShortcutItem("Two-finger twist", "Open the quick actions dial"),
        ),
    ),
    ShortcutGroup(
        "Controller (Select is Back or View)",
        listOf(
            ShortcutItem("Select + A", "Open the quick actions dial"),
            ShortcutItem("Select + X", "Cycle the statistics overlay"),
            ShortcutItem("Select + Y", "Mute or unmute the microphone"),
            ShortcutItem("Hold Select", "Press the host's guide button, where Hold Select for guide is on"),
            ShortcutItem("L1 + R1 + Start + Select", "Hold to disconnect"),
        ),
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
                Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
            }
            Text("Stream controls", style = MaterialTheme.typography.headlineSmall)
        }
        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(start = 20.dp, end = 20.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            Text(
                "Press these while a stream is running.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            for (group in shortcutGroups) {
                SettingsGroup(header = group.title) {
                    for (item in group.items) {
                        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                            Text(
                                item.keys,
                                style = MaterialTheme.typography.bodyMedium,
                                fontWeight = FontWeight.SemiBold,
                                modifier = Modifier.width(132.dp),
                            )
                            Text(item.text, style = MaterialTheme.typography.bodyMedium)
                        }
                    }
                }
            }
        }
    }
}
