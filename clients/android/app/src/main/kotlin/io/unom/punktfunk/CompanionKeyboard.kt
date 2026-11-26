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
import io.unom.punktfunk.kit.keyVk

/*
 * The companion panel's keyboard page (design/android-dual-screen.md §6, D6): a key grid of
 * ours on the lower screen. An IME cannot show on the second display's unfocusable window, and a
 * focusable window would take the handheld's buttons off the stream, so each key goes to the
 * host as a virtual key — the host's own layout decides what it types, as for a physical
 * keyboard. The stream's key row ([KeyRow]) shares the latch and the cap.
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

/** The stream's key row ([KeyRow]): the keys a phone's keyboard lacks. */
internal val KEY_ROW: List<Key> = listOf(
    k("Esc"), k("Tab"), mod("Ctrl"), mod("Alt"), mod("Shift"), mod("Win"),
    k("←", 0x25), k("↓", 0x28), k("↑", 0x26), k("→", 0x27),
    k("Del"), k("Home"), k("End"), k("PgUp", 0x21), k("PgDn", 0x22),
) + F_ROW

/** The key that types [c] unshifted in the US order of [KEY_ROWS]; `null` for one it lacks. */
internal fun typedVk(c: Char): Int? = when (c) {
    ' ' -> 0x20
    '\n' -> 0x0D
    else -> KEY_ROWS.flatten().firstOrNull { it.legend.singleOrNull() == c.lowercaseChar() }?.vk
}

/**
 * Latched modifiers over [sink]. A modifier goes down at its press and up on a second press,
 * after the next other key lifts, or at [release]. Whoever owns a latch releases it when its keys
 * leave, so the host never keeps one. [latched] is Compose state, which lights a held cap.
 */
internal class KeyLatch(private val sink: KeySink) {
    var latched by mutableStateOf(emptySet<Int>())
        private set

    fun press(key: Key) {
        when {
            !key.modifier -> sink.key(key.vk, true)
            key.vk in latched -> {
                sink.key(key.vk, false)
                latched = latched - key.vk
            }
            else -> {
                sink.key(key.vk, true)
                latched = latched + key.vk
            }
        }
    }

    fun lift(key: Key) {
        if (key.modifier) return
        sink.key(key.vk, false)
        release()
    }

    fun release() {
        latched.forEach { sink.key(it, false) }
        latched = emptySet()
    }

    /**
     * [text] typed on another keyboard while a modifier is latched: one character with a key
     * ([typedVk]) goes down and up under the modifiers, anything else to [asText] once they are up.
     */
    fun type(text: String, asText: (String) -> Unit) {
        val vk = text.singleOrNull()?.let(::typedVk)
        if (vk != null) {
            sink.key(vk, true)
            sink.key(vk, false)
        }
        release()
        if (vk == null) asText(text)
    }
}

/** The page needs this much width for the function row; below it the five rows stand alone. */
private val F_ROW_WIDTH = 560.dp

/**
 * The key grid. [sink] takes every edge; a key goes down at the touch and up at the lift, and the
 * page's [KeyLatch] lets every modifier go when the page leaves.
 */
@Composable
internal fun CompanionKeyboard(sink: KeySink, haptics: ConsoleHaptics, modifier: Modifier = Modifier) {
    val out by rememberUpdatedState(sink)
    val latch = remember { KeyLatch { vk, down -> out.key(vk, down) } }
    DisposableEffect(latch) { onDispose { latch.release() } }
    BoxWithConstraints(modifier.fillMaxSize()) {
        val rows = if (maxWidth >= F_ROW_WIDTH) listOf(F_ROW) + KEY_ROWS else KEY_ROWS
        Column(Modifier.fillMaxSize().padding(start = 4.dp, end = 12.dp, bottom = 12.dp)) {
            for (row in rows) {
                Row(Modifier.fillMaxWidth().weight(1f)) {
                    for (key in row) {
                        KeyCap(
                            key,
                            armed = key.vk in latch.latched,
                            modifier = Modifier.weight(key.weight).fillMaxHeight().padding(2.dp)
                                .pointerInput(key) {
                                    detectTapGestures(
                                        onPress = {
                                            haptics.tick()
                                            latch.press(key)
                                            tryAwaitRelease()
                                            latch.lift(key)
                                        },
                                    )
                                },
                        )
                    }
                }
            }
        }
    }
}

/** One key's cap, lit while [armed]. The gesture is the caller's, in [modifier]. */
@Composable
internal fun KeyCap(key: Key, armed: Boolean, modifier: Modifier) {
    val scheme = MaterialTheme.colorScheme
    Surface(
        shape = MaterialTheme.shapes.small,
        color = if (armed) scheme.primaryContainer else scheme.surfaceVariant,
        contentColor = if (armed) scheme.onPrimaryContainer else scheme.onSurfaceVariant,
        modifier = modifier.semantics {
            role = Role.Button
            stateDescription = if (armed) "held" else ""
        },
    ) {
        Box(contentAlignment = Alignment.Center) {
            Text(key.legend, style = MaterialTheme.typography.labelLarge, maxLines = 1)
        }
    }
}
