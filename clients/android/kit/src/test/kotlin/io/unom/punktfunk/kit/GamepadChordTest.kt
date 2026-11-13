package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The router's synthetic taps. Its chords are [Shortcuts.Engine]'s, which `ShortcutVectorsTest`
 * replays against the shared vectors; a [GamepadRouter] itself needs an InputManager, a main
 * Looper and live InputDevices, so driving real KeyEvents through it is not a unit test.
 */
class GamepadChordTest {
    /**
     * Steam reads Quick Access on a pad without the button as Guide held, then A. The tap keeps
     * that order on the wire and lets go of A first. A pad with the button taps it.
     */
    @Test
    fun `a quick access tap holds guide before it presses a`() {
        val tap = Shortcuts.TAP_PRESS_MS
        assertEquals(
            listOf(
                Triple(Gamepad.BTN_GUIDE, true, 0L),
                Triple(Gamepad.BTN_A, true, tap),
                Triple(Gamepad.BTN_A, false, 2 * tap),
                Triple(Gamepad.BTN_GUIDE, false, 2 * tap),
            ),
            GamepadRouter.tapEvents(Gamepad.quickAccessPresses(hasButton = false)),
        )
        assertEquals(
            listOf(
                Triple(Gamepad.BTN_MISC1, true, 0L),
                Triple(Gamepad.BTN_MISC1, false, tap),
            ),
            GamepadRouter.tapEvents(Gamepad.quickAccessPresses(hasButton = true)),
        )
    }
}
