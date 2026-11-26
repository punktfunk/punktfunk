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
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Shortcuts
import io.unom.punktfunk.kit.SlotId
import io.unom.punktfunk.kit.keyLegend
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

/** Ctrl, Alt, Shift and Win: the keys that latch. */
private val MODIFIER_VKS = setOf(0x11, 0x12, 0x10, 0x5B)

/** A key row button's key ([RowSlot.Key]); `null` for a name [keyVk] does not know. */
internal fun rowKey(name: String): Key? = keyVk(name)?.let { Key(keyLegend(name), it, modifier = it in MODIFIER_VKS) }

/**
 * The key that types [c] unshifted in the US order of [KEY_ROWS], a capital as its letter, Tab
 * and Return as theirs; `null` for one it lacks. ASCII only: an arrow's legend is no typed arrow.
 */
internal fun typedVk(c: Char): Int? = when {
    c == ' ' -> 0x20
    c == '\n' || c == '\r' -> 0x0D
    c == '\t' -> 0x09
    c.code >= 0x80 -> null
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

/**
 * One button of the stream's key row under a finger. The row scrolls, so nothing goes down at the
 * touch: a tap fires, and a long press holds a key or a key combination down until [end], so the
 * host repeats it. A modifier latches either way.
 *
 * A quick action goes to [fire], the stream's shortcut path, by the phases a held shortcut uses.
 * A key combination is the next key to a latched modifier, which lifts after it; any other
 * action leaves the latch alone. `clients/shared/key-row-vectors.json` holds the rules.
 */
internal class RowButton(
    private val latch: KeyLatch,
    slot: RowSlot,
    private val fire: (Shortcuts.Action, Shortcuts.Phase) -> Unit,
) {
    private val key = (slot as? RowSlot.Key)?.let { rowKey(it.name) }
    private val action = (slot as? RowSlot.Action)?.let { Shortcuts.Action.Slot(it.slot) }
    private val keyLike = action?.kind == Shortcuts.Kind.KEY_LIKE
    private val combination = action?.slot is SlotId.Shortcut
    private var held = false

    fun tap() {
        key?.let {
            latch.press(it)
            latch.lift(it)
        }
        action?.let { fire(it, if (keyLike) Shortcuts.Phase.TAP else Shortcuts.Phase.ONCE) }
        if (combination) latch.release()
    }

    fun longPress() {
        when {
            key != null && !key.modifier -> latch.press(key)
            action != null && keyLike -> fire(action, Shortcuts.Phase.DOWN)
            else -> return tap()
        }
        held = true
    }

    /** The finger is gone, lifted or cancelled: what a long press holds comes up. */
    fun end() {
        if (!held) return
        held = false
        key?.let(latch::lift)
        action?.let { fire(it, Shortcuts.Phase.UP) }
        if (combination) latch.release()
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
    Cap(armed, modifier) { Text(key.legend, style = MaterialTheme.typography.labelLarge, maxLines = 1) }
}

/** A cap around [content], lit while [armed]: a key's, or a key row button's face. [state] is what a toggle reads out. */
@Composable
internal fun Cap(armed: Boolean, modifier: Modifier, state: String = "", content: @Composable () -> Unit) {
    val scheme = MaterialTheme.colorScheme
    val held = stringResource(R.string.android_key_held_a11y)
    Surface(
        shape = MaterialTheme.shapes.small,
        color = if (armed) scheme.primaryContainer else scheme.surfaceVariant,
        contentColor = if (armed) scheme.onPrimaryContainer else scheme.onSurfaceVariant,
        modifier = modifier.semantics {
            role = Role.Button
            stateDescription = if (armed) held else state
        },
    ) {
        Box(contentAlignment = Alignment.Center) { content() }
    }
}
