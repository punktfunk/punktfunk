package io.unom.punktfunk.kit

import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * The ring's keyboard/remote vocabulary ([ringNavForKey]), the twin of the pad's
 * `ringNavFor`. A TV remote is the device this exists for: it is not a gamepad and not a finger,
 * so without this mapping it could open the ring and then drive nothing while its D-pad walked
 * the game underneath. That failure is invisible — the menu is on screen and the host is still
 * moving — so the mapping is pinned rather than left to a couch to discover.
 */
class RingKeyNavTest {

    /** Every key a D-pad remote can send at the ring, and what the ring does with it. */
    @Test
    fun `a remote drives the ring`() {
        assertEquals(RingNav.Up, ringNavForKey(KeyEvent.KEYCODE_DPAD_UP))
        assertEquals(RingNav.Down, ringNavForKey(KeyEvent.KEYCODE_DPAD_DOWN))
        assertEquals(RingNav.Left, ringNavForKey(KeyEvent.KEYCODE_DPAD_LEFT))
        assertEquals(RingNav.Right, ringNavForKey(KeyEvent.KEYCODE_DPAD_RIGHT))
        assertEquals(RingNav.Confirm, ringNavForKey(KeyEvent.KEYCODE_DPAD_CENTER))
        assertEquals(RingNav.Back, ringNavForKey(KeyEvent.KEYCODE_BACK))
    }

    /** A keyboard reaches the same ring, on the keys a keyboard actually has. */
    @Test
    fun `a keyboard drives the ring`() {
        assertEquals(RingNav.Confirm, ringNavForKey(KeyEvent.KEYCODE_ENTER))
        assertEquals(RingNav.Confirm, ringNavForKey(KeyEvent.KEYCODE_NUMPAD_ENTER))
        assertEquals(RingNav.Confirm, ringNavForKey(KeyEvent.KEYCODE_SPACE))
        assertEquals(RingNav.Back, ringNavForKey(KeyEvent.KEYCODE_ESCAPE))
    }

    /**
     * Null is "the ring has no meaning for this", not "send it on": the caller swallows the key
     * either way, because a W aimed at a menu must not reach the game under it.
     */
    @Test
    fun `a game key means nothing to the ring`() {
        assertNull(ringNavForKey(KeyEvent.KEYCODE_W))
        assertNull(ringNavForKey(KeyEvent.KEYCODE_VOLUME_UP))
    }
}
