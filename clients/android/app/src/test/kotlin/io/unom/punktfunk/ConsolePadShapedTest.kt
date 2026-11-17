package io.unom.punktfunk

import android.view.KeyEvent
import io.unom.punktfunk.console.padShaped
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [padShaped] decides which keys the console's pad route claims. It exists because the source
 * class on a key event is the platform's per-device guess: a keyboard on a composite receiver
 * reports `SOURCE_GAMEPAD` on every key it sends, and claiming those by source would leave its
 * Enter and its typing dead in the menus while the same keys still reach a running stream.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class ConsolePadShapedTest {

    /** A controller's own keys — the pad route keeps these whatever the device also claims. */
    @Test
    fun padButtonsAndBackAreTheRoute() {
        assertTrue(padShaped(KeyEvent.KEYCODE_BUTTON_A))
        assertTrue(padShaped(KeyEvent.KEYCODE_BUTTON_START))
        assertTrue(padShaped(KeyEvent.KEYCODE_BUTTON_MODE))
        // A pad with no Select scancode delivers Select as BACK; the console's Back is the same
        // event from a keyboard, so leaving it on the pad route keeps one owner for it.
        assertTrue(padShaped(KeyEvent.KEYCODE_BACK))
    }

    /** Typing and its punctuation reach the key route even from a pad-tagged keyboard. */
    @Test
    fun typingIsNeverPadShaped() {
        assertFalse(padShaped(KeyEvent.KEYCODE_ENTER))
        assertFalse(padShaped(KeyEvent.KEYCODE_A))
        assertFalse(padShaped(KeyEvent.KEYCODE_Y))
        assertFalse(padShaped(KeyEvent.KEYCODE_X))
        assertFalse(padShaped(KeyEvent.KEYCODE_SPACE))
        assertFalse(padShaped(KeyEvent.KEYCODE_DEL))
        assertFalse(padShaped(KeyEvent.KEYCODE_ESCAPE))
        assertFalse(padShaped(KeyEvent.KEYCODE_TAB))
    }
}
