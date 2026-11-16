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
        pickStreamMode(table, current, hz.toFloat(), size)?.id

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

    /** A TV's table: 59.94 beside 60, 119.88 beside 120. The stream's own rate wins. */
    @Test
    fun aFractionalStreamTakesTheModeAtItsRate() {
        val ntsc60 = PanelMode(1, 3840, 2160, 59.94f)
        val whole60 = PanelMode(2, 3840, 2160, 60f)
        val ntsc120 = PanelMode(3, 3840, 2160, 119.88f)
        val whole120 = PanelMode(4, 3840, 2160, 120f)
        val tv = listOf(ntsc60, whole60, ntsc120, whole120)
        fun at(hz: Float) = pickStreamMode(tv, whole60, hz, null)?.id
        assertEquals(1, at(59.94f))
        assertEquals(2, at(60f))
        assertEquals(3, at(119.88f))
        assertEquals(4, at(120f))
        // No mode at the stream's rate: the nearest double, 119.88 under a 59.94 stream.
        val doubles = listOf(ntsc120, whole120)
        assertEquals(3, pickStreamMode(doubles, whole120, 59.94f, null)?.id)
        assertEquals(4, pickStreamMode(doubles, whole120, 60f, null)?.id)
        // A panel that lists only 59.94 serves a 60 stream at it.
        assertEquals(1, pickStreamMode(listOf(ntsc60), ntsc60, 60f, null)?.id)
    }
}
