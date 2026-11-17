package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Test

/**
 * The Kotlin half of the rumble JNI boundary. The Rust half is pinned by `pack_rumble_tests` in
 * `clients/android/native/src/feedback.rs`; the two suites describe the same layout from opposite
 * sides, which is the only thing that catches one of them drifting.
 */
class RumbleWireTest {

    /** `pack_rumble` from the native side, transcribed — the packer these tests unpack. */
    private fun pack(pad: Int, low: Int, high: Int, backstopMs: Int): Long =
        ((pad and 0xF).toLong() shl 49) or
            ((backstopMs.coerceAtMost(0xFFFF)).toLong() shl 32) or
            (low.toLong() shl 16) or
            high.toLong()

    @Test
    fun `every field round-trips at its extremes`() {
        val cases = listOf(
            listOf(0, 0, 0, 0),
            listOf(15, 0xFFFF, 0xFFFF, 0xFFFF),
            listOf(1, 0x1234, 0x5678, 500),
            listOf(7, 0, 0xFFFF, 2000),
        )
        for ((pad, low, high, backstop) in cases) {
            val cmd = unpackRumbleEvent(pack(pad, low, high, backstop))!!
            assertEquals("pad", pad, cmd.pad)
            assertEquals("low", low, cmd.low)
            assertEquals("high", high, cmd.high)
            assertEquals("backstop", backstop.toLong(), cmd.backstopMs)
        }
    }

    /** MAX_PADS is 16, so all 16 indices must survive the 4-bit field without aliasing. */
    @Test
    fun `all sixteen pad indices are distinct`() {
        val seen = (0 until 16).map { unpackRumbleEvent(pack(it, 1, 2, 3))!!.pad }
        assertEquals((0 until 16).toList(), seen)
    }

    @Test
    fun `the negative sentinel is not a command`() {
        assertNull(unpackRumbleEvent(-1L))
        assertNull(unpackRumbleEvent(Long.MIN_VALUE))
    }

    @Test
    fun `a stop is distinguishable from a hold`() {
        val stop = unpackRumbleEvent(pack(2, 0, 0, 0))!!
        val hold = unpackRumbleEvent(pack(2, 0x8000, 0x8000, 500))!!
        assertEquals(0, stop.low)
        assertEquals(0, stop.high)
        assertNotEquals(stop, hold)
    }

    // --- wireAmplitudeToByte ---

    @Test
    fun `amplitude takes the high byte`() {
        assertEquals(0xFF, wireAmplitudeToByte(0xFFFF))
        assertEquals(0x80, wireAmplitudeToByte(0x8000))
        assertEquals(0x12, wireAmplitudeToByte(0x1234))
    }

    @Test
    fun `zero stays silent but a weak nonzero never does`() {
        assertEquals("only a real zero may render as silence", 0, wireAmplitudeToByte(0))
        // Everything below 0x0100 has a zero high byte — without the floor these all vanish.
        for (v in listOf(1, 0x0042, 0x00FF)) {
            assertEquals("wire $v collapsed to silence", 1, wireAmplitudeToByte(v))
        }
        assertEquals(1, wireAmplitudeToByte(0x0100)) // first value that reaches 1 on its own
    }
}
