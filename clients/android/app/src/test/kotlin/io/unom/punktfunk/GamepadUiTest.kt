package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * [gamepadUiActive] is pure — table-tested over its inputs, and the mirror of the Apple client's
 * `GamepadUIEnvironmentTests`. The two clients share the stored `gamepad_ui_mode` values, so a
 * disagreement here is a device that behaves differently from the same setting.
 */
class GamepadUiTest {

    /** The default mode waits for a pad. */
    @Test
    fun whenConnectedWaitsForAPad() {
        assertTrue(gamepadUiActive(true, GAMEPAD_UI_WHEN_CONNECTED, true, tv = false, forced = false))
        assertFalse(gamepadUiActive(true, GAMEPAD_UI_WHEN_CONNECTED, false, tv = false, forced = false))
        assertFalse(gamepadUiActive(false, GAMEPAD_UI_WHEN_CONNECTED, true, tv = false, forced = false))
        assertFalse(gamepadUiActive(false, GAMEPAD_UI_WHEN_CONNECTED, false, tv = false, forced = false))
        // A TV is in console mode whatever the mode says — its remote is the only input.
        assertTrue(gamepadUiActive(true, GAMEPAD_UI_WHEN_CONNECTED, false, tv = true, forced = false))
    }

    /** Always drops the controller from the decision — but never the switch, which is the one
     * way back to the touch UI. */
    @Test
    fun alwaysIgnoresThePadButNotTheSwitch() {
        assertTrue(gamepadUiActive(true, GAMEPAD_UI_ALWAYS, false, tv = false, forced = false))
        assertTrue(gamepadUiActive(true, GAMEPAD_UI_ALWAYS, true, tv = false, forced = false))
        assertFalse(gamepadUiActive(false, GAMEPAD_UI_ALWAYS, false, tv = false, forced = false))
        assertFalse(gamepadUiActive(false, GAMEPAD_UI_ALWAYS, true, tv = false, forced = false))
    }

    /** A value a newer client wrote waits for a pad rather than stranding this build in a
     * layout it has no way back out of. */
    @Test
    fun anUnknownModeWaitsForAPad() {
        assertFalse(gamepadUiActive(true, "whenever-i-say-so", false, tv = false, forced = false))
        assertTrue(gamepadUiActive(true, "whenever-i-say-so", true, tv = false, forced = false))
        assertFalse(gamepadUiActive(true, "", false, tv = false, forced = false))
    }

    /** The shipped default: the console UI still waits for a controller. */
    @Test
    fun theDefaultIsUnchangedBehaviour() {
        val s = Settings()
        assertTrue(s.gamepadUiEnabled)
        assertEquals(GAMEPAD_UI_WHEN_CONNECTED, s.gamepadUiMode)
        assertFalse(gamepadUiActive(s.gamepadUiEnabled, s.gamepadUiMode, false, tv = false, forced = false))
    }
}
