package io.unom.punktfunk.kit

import android.view.KeyEvent
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The truth table behind "is a controller attached" — the question the console UI's
 * "With a controller" mode is answered by. A false positive here is not cosmetic: it pins the
 * console UI on with no pad in the room, and no setting short of turning the whole thing off can
 * dismiss it, because the phantom pad never disconnects.
 */
class PadPresenceTest {

    /** A real pad: the source class plus a stick or a HAT (a d-pad-only pad or arcade stick). */
    @Test
    fun realPadsCount() {
        assertTrue(Gamepad.looksLikeController(padSource = true, virtual = false, hasStick = true))
    }

    /** The gaming-phone shoulder triggers and OEM game-mode overlays: a virtual device wearing the
     * gamepad source class. */
    @Test
    fun virtualDevicesAreNotControllers() {
        assertFalse(Gamepad.looksLikeController(padSource = true, virtual = true, hasStick = true))
    }

    /** A TV remote claims the gamepad source and, through a generic key layout, the A/B keys. With
     * no stick or HAT it is not a controller. */
    @Test
    fun aRemoteWithGamepadKeysIsNotAController() {
        assertFalse(Gamepad.looksLikeController(padSource = true, virtual = false, hasStick = false))
    }

    /** And a keyboard/mouse with sticks it never reports on the joystick source stays out. */
    @Test
    fun nonPadSourcesNeverCount() {
        assertFalse(Gamepad.looksLikeController(padSource = false, virtual = false, hasStick = true))
    }

    /**
     * A composite receiver carries a keyboard collection beside its pad, so it satisfies
     * [Gamepad.isPad] on every key it sends. Its arrows and its BACK must stay on the keyboard
     * path — arrows are the VK route, and BACK is how a couch user leaves the stream.
     */
    @Test
    fun aCompositePadsArrowsAndBackAreNotPadKeys() {
        for (fallback in listOf(false, true)) {
            for (code in DPAD + KeyEvent.KEYCODE_BACK) {
                assertFalse(
                    "keycode $code, sc2 fallback $fallback",
                    Gamepad.eventFromPad(
                        eventFromGamepad = false, deviceIsPad = true, deviceIsSc2 = false,
                        padButton = false, keyCode = code, fallback = false,
                        includeSc2Fallback = fallback,
                    ),
                )
            }
        }
    }

    /** Its face buttons still are, whatever the event's own source class claims. */
    @Test
    fun aCompositePadsFaceButtonsAreStillPadKeys() {
        assertTrue(
            Gamepad.eventFromPad(
                eventFromGamepad = false, deviceIsPad = true, deviceIsSc2 = false,
                padButton = true, keyCode = KeyEvent.KEYCODE_BUTTON_A, fallback = false,
                includeSc2Fallback = false,
            ),
        )
        // An event the platform stamped itself never needs the device at all.
        assertTrue(
            Gamepad.eventFromPad(
                eventFromGamepad = true, deviceIsPad = false, deviceIsSc2 = false,
                padButton = false, keyCode = KeyEvent.KEYCODE_DPAD_UP, fallback = false,
                includeSc2Fallback = false,
            ),
        )
    }

    /**
     * A Steam Controller 2 in lizard mode presents as a keyboard and a mouse by design, so no
     * capability probe can find it and its identity is the only signal. The menus widen for it;
     * a stream does not, because an uncaptured SC2's keys have to keep typing at the host.
     */
    @Test
    fun anSc2TakesTheWidenedRouteOnlyInTheMenus() {
        for (code in DPAD + KeyEvent.KEYCODE_BACK) {
            assertTrue(
                "keycode $code",
                Gamepad.eventFromPad(
                    eventFromGamepad = false, deviceIsPad = false, deviceIsSc2 = true,
                    padButton = false, keyCode = code, fallback = false,
                ),
            )
            assertFalse(
                "keycode $code",
                Gamepad.eventFromPad(
                    eventFromGamepad = false, deviceIsPad = false, deviceIsSc2 = true,
                    padButton = false, keyCode = code, fallback = false,
                    includeSc2Fallback = false,
                ),
            )
        }
        // A FLAG_FALLBACK BACK duplicates a button press the pad path already saw.
        assertFalse(
            Gamepad.eventFromPad(
                eventFromGamepad = false, deviceIsPad = false, deviceIsSc2 = true,
                padButton = false, keyCode = KeyEvent.KEYCODE_BACK, fallback = true,
            ),
        )
        // Everything else an SC2 types is still typing.
        assertFalse(
            Gamepad.eventFromPad(
                eventFromGamepad = false, deviceIsPad = false, deviceIsSc2 = true,
                padButton = false, keyCode = KeyEvent.KEYCODE_A, fallback = false,
            ),
        )
    }

    /** A plain keyboard reaches no pad route at all. */
    @Test
    fun aKeyboardIsNeverAPad() {
        for (code in DPAD + KeyEvent.KEYCODE_BACK + KeyEvent.KEYCODE_A) {
            assertFalse(
                "keycode $code",
                Gamepad.eventFromPad(
                    eventFromGamepad = false, deviceIsPad = false, deviceIsSc2 = false,
                    padButton = false, keyCode = code, fallback = false,
                ),
            )
        }
    }

    /** The identities the widened route is keyed on — wired, BLE, and the two Puck dongles. */
    @Test
    fun sc2IdentitiesAreValvesFourProductIds() {
        for (pid in listOf(0x1302, 0x1303, 0x1304, 0x1305)) {
            assertTrue("pid $pid", Gamepad.isSc2VidPid(0x28DE, pid))
        }
        assertFalse(Gamepad.isSc2VidPid(0x28DE, 0x1205)) // Steam Deck
        assertFalse(Gamepad.isSc2VidPid(0x28DE, 0x1102)) // classic Steam Controller
        assertFalse(Gamepad.isSc2VidPid(0x054C, 0x0CE6)) // DualSense
    }

    /** A remote that claims gamepad buttons gets no wire pad from its D-pad, OK or Back; a pad's
     * first button, or any key from a device with a stick, does. */
    @Test
    fun onlyPadInputOpensAWirePad() {
        assertTrue(GamepadRouter.opensSlotOnKey(gamepadButton = true, hasStick = false))
        assertTrue(GamepadRouter.opensSlotOnKey(gamepadButton = false, hasStick = true))
        assertFalse(GamepadRouter.opensSlotOnKey(gamepadButton = false, hasStick = false))
    }

    private companion object {
        val DPAD = listOf(
            KeyEvent.KEYCODE_DPAD_UP,
            KeyEvent.KEYCODE_DPAD_DOWN,
            KeyEvent.KEYCODE_DPAD_LEFT,
            KeyEvent.KEYCODE_DPAD_RIGHT,
        )
    }
}
