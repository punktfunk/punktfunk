package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** [shortcutGroups]: the reference lists the chords the pad router and the key handler bind. */
class ShortcutsScreenTest {
    private val keys = shortcutGroups.flatMap { it.items }.map { it.keys }

    @Test
    fun listsEveryControllerChord() {
        for (k in listOf("Select + A", "Select + X", "Select + Y", "Hold Select", "L1 + R1 + Start + Select")) {
            assertTrue("missing $k", k in keys)
        }
    }

    @Test
    fun listsTheKeyboardAndTouchGestures() {
        for (k in listOf("Ctrl+Alt+Shift+Q", "Ctrl+Alt+Shift+O", "Two-finger twist", "Three-finger tap")) {
            assertTrue("missing $k", k in keys)
        }
    }

    @Test
    fun noKeysAreListedTwiceInAGroup() {
        for (g in shortcutGroups) assertEquals(g.title, g.items.size, g.items.map { it.keys }.toSet().size)
    }
}
