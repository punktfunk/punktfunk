package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The latch the key row and the companion keyboard share: a modifier held until the next key
 * lifts, a second press or a release, and a character typed under it sent as that key.
 */
class KeyLatchTest {
    private val sent = mutableListOf<Pair<Int, Boolean>>()
    private val typed = mutableListOf<String>()
    private val latch = KeyLatch { vk, down -> sent += vk to down }

    private fun tap(legend: String) {
        val key = KEY_ROW.first { it.legend == legend }
        latch.press(key)
        latch.lift(key)
    }

    @Test
    fun aModifierHoldsUntilTheNextKeyLifts() {
        tap("Ctrl")
        assertEquals(listOf(0x11 to true), sent)
        assertEquals(setOf(0x11), latch.latched)
        tap("Tab")
        assertEquals(listOf(0x11 to true, 0x09 to true, 0x09 to false, 0x11 to false), sent)
        assertTrue(latch.latched.isEmpty())
    }

    @Test
    fun aSecondPressLetsTheModifierGo() {
        tap("Alt")
        tap("Alt")
        assertEquals(listOf(0x12 to true, 0x12 to false), sent)
        assertTrue(latch.latched.isEmpty())
    }

    @Test
    fun releaseLetsEveryModifierGoOnce() {
        tap("Ctrl")
        tap("Shift")
        latch.release()
        latch.release()
        assertEquals(listOf(0x11 to true, 0x10 to true, 0x11 to false, 0x10 to false), sent)
    }

    @Test
    fun aCharacterTypedUnderAModifierGoesAsItsKey() {
        tap("Ctrl")
        latch.type("c") { typed += it }
        assertEquals(listOf(0x11 to true, 0x43 to true, 0x43 to false, 0x11 to false), sent)
        assertTrue(typed.isEmpty())
        assertTrue(latch.latched.isEmpty())
    }

    @Test
    fun textWithNoKeyGoesAsTextOnceTheModifierIsUp() {
        tap("Win")
        latch.type("é") {
            assertEquals(listOf(0x5B to true, 0x5B to false), sent)
            typed += it
        }
        latch.type("hello") { typed += it }
        assertEquals(listOf("é", "hello"), typed)
        assertEquals(listOf(0x5B to true, 0x5B to false), sent)
    }

    @Test
    fun aTypedCharacterFindsItsKeyInTheUsOrder() {
        assertEquals(0x41, typedVk('a'))
        assertEquals(0x41, typedVk('A'))
        assertEquals(0x35, typedVk('5'))
        assertEquals(0x20, typedVk(' '))
        assertEquals(0x0D, typedVk('\n'))
        assertEquals(0xBF, typedVk('/'))
        assertNull(typedVk('é'))
        assertNull(typedVk('!'))
    }
}
