package io.unom.punktfunk

import io.unom.punktfunk.kit.R
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** [shortcutGroups]: the reference lists the chords the pad router and the key handler bind. */
class ShortcutsScreenTest {
    private val presses = shortcutGroups.flatMap { it.items }.map { it.press }

    @Test
    fun listsEveryControllerChord() {
        val chords = listOf(
            Press.Keys("Select + A"), Press.Keys("Select + X"), Press.Keys("Select + Y"),
            Press.Worded(R.string.controls_hold, "Select"), Press.Keys("L1 + R1 + Start + Select"),
        )
        for (p in chords) assertTrue("missing $p", p in presses)
    }

    @Test
    fun listsTheKeyboardAndTouchGestures() {
        val chords = listOf(
            Press.Keys("Ctrl+Alt+Shift+Q"), Press.Keys("Ctrl+Alt+Shift+O"),
            Press.Worded(R.string.controls_gesture_two_finger_twist),
            Press.Worded(R.string.controls_gesture_three_finger_tap),
        )
        for (p in chords) assertTrue("missing $p", p in presses)
    }

    @Test
    fun noKeysAreListedTwiceInAGroup() {
        for (g in shortcutGroups) assertEquals("group ${g.title}", g.items.size, g.items.map { it.press }.toSet().size)
    }
}
