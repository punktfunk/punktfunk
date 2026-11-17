package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Pure JVM test of the panel-rate reading ([measuredMilliHz]). */
class PanelRateTest {
    /** [count] frame times [periodNs] apart, each up to [jitterNs] late, minus the [missed]. */
    private fun frames(
        periodNs: Long,
        count: Int = 180,
        jitterNs: Long = 0,
        missed: Set<Int> = emptySet(),
        step: Int = 1,
    ): LongArray {
        var seed = 0x9E3779B97F4A7C15uL
        return (0 until count * step step step)
            .filter { it / step !in missed }
            .map { k ->
                seed = seed xor (seed shl 13)
                seed = seed xor (seed shr 7)
                seed = seed xor (seed shl 17)
                val late = if (jitterNs > 0) (seed % jitterNs.toULong()).toLong() else 0L
                1_000_000_000L + k * periodNs + late
            }
            .toLongArray()
    }

    @Test
    fun aPanelBesideItsStatedRateReadsItsOwn() {
        // 16.70 ms under a stated 60.000 Hz: 59.88.
        assertEquals(59_880, measuredMilliHz(frames(16_700_000), 60_000))
        assertEquals(59_940, measuredMilliHz(frames(16_683_350), 60_000))
        assertEquals(60_000, measuredMilliHz(frames(16_666_667), 60_000))
        assertEquals(119_880, measuredMilliHz(frames(8_341_675), 120_000))
    }

    @Test
    fun jitterAndMissedCallbacksDoNotMoveTheReading() {
        val jittery = frames(16_700_000, jitterNs = 150_000)
        assertEquals(59_880.0, measuredMilliHz(jittery, 60_000)!!.toDouble(), 1.0)
        val gappy = frames(16_700_000, missed = setOf(17, 18, 90, 140))
        assertEquals(59_880, measuredMilliHz(gappy, 60_000))
        // Two callbacks in one period are one sample.
        val doubled = frames(16_700_000).flatMap { listOf(it, it) }.toLongArray()
        assertEquals(59_880, measuredMilliHz(doubled, 60_000))
    }

    @Test
    fun anAppServedEverySecondRefreshStillReadsThePanel() {
        assertEquals(119_760, measuredMilliHz(frames(8_350_000, count = 120, step = 2), 120_000))
    }

    @Test
    fun aReadingWithinClockToleranceIsTheStatedRate() {
        assertTrue(besideStated(59_880, 60_000))
        assertTrue(besideStated(59_987, 60_000))
        assertFalse(besideStated(59_988, 60_000))
        assertFalse(besideStated(60_002, 60_000))
        assertFalse(besideStated(120_000, 120_000))
    }

    @Test
    fun aReadingThatIsNotThisModeIsNone() {
        // A rate 2 % off is another mode.
        assertNull(measuredMilliHz(frames(17_000_000), 60_000))
        // A rate that moves inside the window is no reading.
        val drifting = frames(16_700_000, count = 90) + frames(16_600_000, count = 90).map { it + 2_000_000_000L }
        assertNull(measuredMilliHz(drifting, 60_000))
        assertNull(measuredMilliHz(frames(16_700_000, count = 30), 60_000))
        // A pause is too long a gap to count in periods.
        assertNull(measuredMilliHz(frames(16_700_000, missed = (40..70).toSet()), 60_000))
        assertNull(measuredMilliHz(LongArray(180) { 5L }, 60_000))
    }
}
