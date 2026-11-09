package io.unom.punktfunk

import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.Shortcuts
import io.unom.punktfunk.kit.SlotId
import io.unom.punktfunk.kit.say
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The Shortcuts screen with an empty blob: the chords every build before bound, as rows. */
class ShortcutsScreenTest {
    private val defaults = Settings().shortcutBindings()
    private val presses = defaults.rows(touchscreen = true).flatMap { it.rows }.map { it.press }

    @Test
    fun listsEveryControllerChord() {
        val chords = listOf(
            Say.Text("Select + A"), Say.Text("Select + X"), Say.Text("Select + Y"),
            say(R.string.controls_hold, "L1 + R1 + Start + Select"),
        )
        for (p in chords) assertTrue("missing $p", p in presses)
        // Hold Select for guide is off on Android unless the retired setting was on.
        assertFalse(say(R.string.controls_hold, "Select") in presses)
        val legacy = Settings(guideGesture = "on").shortcutBindings().rows(touchscreen = true).flatMap { it.rows }
        assertTrue(legacy.any { it.press == say(R.string.controls_hold, "Select") })
    }

    @Test
    fun listsTheKeyboardAndTouchGestures() {
        val chords = listOf(
            Say.Text("Ctrl+Alt+Shift+Q"), Say.Text("Ctrl+Alt+Shift+O"),
            say(R.string.controls_gesture_two_finger_twist),
            say(R.string.controls_gesture_three_finger_tap),
        )
        for (p in chords) assertTrue("missing $p", p in presses)
    }

    @Test
    fun noKeysAreListedTwiceInAGroup() {
        for (g in defaults.rows(touchscreen = true)) assertEquals("${g.kind}", g.rows.size, g.rows.map { it.press }.toSet().size)
    }

    /** What Android cannot do stays listed, dimmed; the engine never sees it. */
    @Test
    fun aWindowActionOrAGoneKeyCombinationDoesNotActHere() {
        val cfg = OverlayConfig.parse("""{"v":2,"shortcuts":[{"id":"s1","keys":["win"]}]}""")
        fun acts(id: String) = Shortcuts.Binding(Shortcuts.Trigger.Pad(listOf(1, 2), false), Shortcuts.Action.parse(id)).actsHere(cfg)
        assertTrue(acts("ring") && acts("stats") && acts("shortcut:s1") && acts("release_input"))
        assertFalse(acts("fullscreen") || acts("minimize") || acts("clipboard"))
        assertFalse("a dangling key combination", acts("shortcut:s9"))
        assertFalse("an action from a newer client", acts("teleport"))
        assertEquals(Shortcuts.Action.Slot(SlotId.Stats), Shortcuts.Action.parse("stats"))
    }
}
