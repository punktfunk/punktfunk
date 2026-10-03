package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** Pure JVM test of the stream's display-mode pick ([pickStreamMode]). */
class StreamModeTest {
    /** A Pixel 8 Pro's table: two sizes, each at 60 and 120 Hz. */
    private val low60 = PanelMode(1, 2244, 1008, 60f)
    private val low120 = PanelMode(2, 2244, 1008, 120f)
    private val full60 = PanelMode(3, 2992, 1344, 60f)
    private val full120 = PanelMode(4, 2992, 1344, 120f)
    private val table = listOf(low60, low120, full60, full120)

    private fun pick(current: PanelMode, hz: Int, size: Pair<Int, Int>?) =
        pickStreamMode(table, current, hz, size)?.id

    @Test
    fun aStreamOfThePanelSizeTakesThePanelToIt() {
        assertEquals(3, pick(low60, 60, 2992 to 1344))
        assertEquals(4, pick(low60, 120, 2992 to 1344))
        // The refresh rule holds at the new size: a 30 stream gets its smallest multiple.
        assertEquals(3, pick(low120, 30, 2992 to 1344))
    }

    @Test
    fun anyOtherStreamKeepsTheUsersResolution() {
        assertEquals(1, pick(low60, 60, 2244 to 1008))
        assertEquals(2, pick(low60, 120, 1920 to 1080))
        assertEquals(2, pick(low120, 120, null))
        // A phone already at full size is never taken down to a smaller stream's size.
        assertEquals(3, pick(full120, 60, 2244 to 1008))
        assertNull(pick(low60, 0, 2992 to 1344))
    }
}
