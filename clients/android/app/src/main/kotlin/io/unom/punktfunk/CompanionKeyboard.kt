package io.unom.punktfunk

import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp

/*
 * The companion panel's keyboard page (design/android-dual-screen.md §6, D6): a key grid of
 * ours on the lower screen. An IME cannot show on the second display's unfocusable window, and a
 * focusable window would take the handheld's buttons off the stream, so each key goes to the
 * host as a virtual key — the host's own layout decides what it types, as for a physical
 * keyboard. A modifier latches: held on the host until the next key lifts or a second tap.
 */

/** One edge on the wire; down and up apart, so a held key repeats on the host. */
internal fun interface KeySink {
    fun key(vk: Int, down: Boolean)
}

/** One key: its legend, the virtual key it sends, whether it latches, and its share of the row. */
internal data class Key(val legend: String, val vk: Int, val modifier: Boolean = false, val weight: Float = 1f)

private fun k(legend: String, vk: Int, weight: Float = 1f) = Key(legend, vk, weight = weight)
private fun k(legend: String, weight: Float = 1f) = Key(legend, requireNotNull(keyVk(legend)) { legend }, weight = weight)
private fun mod(legend: String, weight: Float = 1f) =
    Key(legend, requireNotNull(keyVk(legend)) { legend }, modifier = true, weight = weight)

/** The function row, shown where the page is wide enough for it. */
internal val F_ROW: List<Key> = (1..12).map { k("F$it") }

/** The rows, in US order; punctuation carries its `VK_OEM_*` code, the rest comes from the chord table. */
internal val KEY_ROWS: List<List<Key>> = listOf(
    listOf(k("Esc", 1.5f), k("`", 0xC0)) + "1234567890".map { k("$it") } +
        // The one symbol here: the word does not fit a Thor's row, and every phone keyboard uses it.
        listOf(k("-", 0xBD), k("=", 0xBB), k("⌫", 0x08, 2f)),
    listOf(k("Tab", 1.5f)) + "qwertyuiop".map { k("$it") } + listOf(k("[", 0xDB), k("]", 0xDD), k("\\", 0xDC, 1.5f)),
    listOf(k("Caps", 0x14, 1.75f)) + "asdfghjkl".map { k("$it") } + listOf(k(";", 0xBA), k("'", 0xDE), k("Enter", 2.25f)),
    listOf(mod("Shift", 2.25f)) + "zxcvbnm".map { k("$it") } +
        listOf(k(",", 0xBC), k(".", 0xBE), k("/", 0xBF), mod("Shift", 1.75f), k("↑", 0x26)),
    listOf(
        mod("Ctrl", 1.5f), mod("Win", 1.25f), mod("Alt", 1.25f), k("Space", 0x20, 6f), mod("Alt", 1.25f),
        mod("Ctrl", 1.5f), k("←", 0x25), k("↓", 0x28), k("→", 0x27),
    ),
)

/** The page needs this much width for the function row; below it the five rows stand alone. */
private val F_ROW_WIDTH = 560.dp

/**
 * The key grid. [sink] takes every edge. Latched modifiers go down at the tap and up after the
 * next key's lift, on a second tap, or when the page leaves — the host never keeps one.
 */
@Composable
internal fun CompanionKeyboard(sink: KeySink, haptics: ConsoleHaptics, modifier: Modifier = Modifier) {
    val out by rememberUpdatedState(sink)
    var latched by remember { mutableStateOf(setOf<Int>()) }
    fun releaseLatched() {
        latched.forEach { out.key(it, false) }
        latched = emptySet()
    }
    DisposableEffect(Unit) { onDispose { releaseLatched() } }
    BoxWithConstraints(modifier.fillMaxSize()) {
        val rows = if (maxWidth >= F_ROW_WIDTH) listOf(F_ROW) + KEY_ROWS else KEY_ROWS
        Column(Modifier.fillMaxSize().padding(start = 4.dp, end = 12.dp, bottom = 12.dp)) {
            for (row in rows) {
                Row(Modifier.fillMaxWidth().weight(1f)) {
                    for (key in row) {
                        KeyCap(
                            key,
                            armed = key.modifier && key.vk in latched,
                            modifier = Modifier.weight(key.weight).fillMaxHeight().padding(2.dp),
                            onDown = {
                                haptics.tick()
                                if (key.modifier) {
                                    if (key.vk in latched) {
                                        out.key(key.vk, false)
                                        latched = latched - key.vk
                                    } else {
                                        out.key(key.vk, true)
                                        latched = latched + key.vk
                                    }
                                } else {
                                    out.key(key.vk, true)
                                }
                            },
                            onUp = {
                                if (!key.modifier) {
                                    out.key(key.vk, false)
                                    releaseLatched()
                                }
                            },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun KeyCap(key: Key, armed: Boolean, modifier: Modifier, onDown: () -> Unit, onUp: () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    val down by rememberUpdatedState(onDown)
    val up by rememberUpdatedState(onUp)
    Surface(
        shape = MaterialTheme.shapes.small,
        color = if (armed) scheme.primaryContainer else scheme.surfaceVariant,
        contentColor = if (armed) scheme.onPrimaryContainer else scheme.onSurfaceVariant,
        modifier = modifier
            .semantics {
                role = Role.Button
                stateDescription = if (armed) "held" else ""
            }
            .pointerInput(key) {
                detectTapGestures(
                    onPress = {
                        down()
                        tryAwaitRelease()
                        up()
                    },
                )
            },
    ) {
        Box(contentAlignment = Alignment.Center) {
            Text(key.legend, style = MaterialTheme.typography.labelLarge, maxLines = 1)
        }
    }
}
