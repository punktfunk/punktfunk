package io.unom.punktfunk

import io.unom.punktfunk.kit.NativeBridge
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** The graphs' numbers, pure: the ring, the stand-ins, the axis tops. */
class StatsHistoryTest {
    private fun sample(vararg set: Pair<Int, Float>) =
        StatsSample(FloatArray(NativeBridge.STAT_COUNT) { -1f }.also { v -> v[NativeBridge.STAT_WINDOW_MS] = 1000f; set.forEach { (i, x) -> v[i] = x } })

    @Test
    fun theRingKeepsTheLastCapacitySeconds() {
        val h = StatsHistory(capacity = 3)
        for (t in 1..5) h.push(sample(NativeBridge.STAT_RECEIVED_FPS to t.toFloat()))
        assertEquals(3, h.size)
        assertEquals(listOf(3f, 4f, 5f), h.series { it.receivedFps }.toList())
        assertEquals(5f, h.latest?.receivedFps)
        assertEquals(5, h.version)
    }

    @Test
    fun presentedStandsInForWhatThePlatformCannotCount() {
        assertEquals(60f, sample(NativeBridge.STAT_RECEIVED_FPS to 60f).presentedFps)
        assertEquals(58f, sample(NativeBridge.STAT_RECEIVED_FPS to 60f, NativeBridge.STAT_DECODED_FPS to 58f).presentedFps)
        assertEquals(57f, sample(NativeBridge.STAT_DECODED_FPS to 58f, NativeBridge.STAT_PRESENTED_FPS to 57f).presentedFps)
        assertNull(sample().rttMs)
        assertNull(StatsSample.of(FloatArray(3)))
    }

    @Test
    fun theFloorComesOffTheEndToEndFigures() {
        val s = sample(NativeBridge.STAT_E2E_P50_MS to 26f, NativeBridge.STAT_E2E_P95_MS to 30f, NativeBridge.STAT_OS_FLOOR_MS to 8f)
        assertEquals(18f, s.e2eMs)
        assertEquals(22f, s.e2eP95Ms)
        assertEquals(26f, sample(NativeBridge.STAT_E2E_P50_MS to 26f).e2eMs)
    }

    @Test
    fun lossIsAShareOfTheWindow() {
        val s = sample(NativeBridge.STAT_RECEIVED_FPS to 98f, NativeBridge.STAT_LOST to 2f)
        assertEquals(2f, s.lostPct, 1e-3f)
        assertEquals(0f, sample().lostPct, 0f)
    }

    @Test
    fun axisTopsAreOneTwoFive() {
        assertEquals(1f, niceCeiling(0.4f))
        assertEquals(20f, niceCeiling(18.4f))
        assertEquals(50f, niceCeiling(23f))
        assertEquals(100f, niceCeiling(92.1f))
        assertEquals(200f, niceCeiling(119f))
        assertEquals(130f, ceilTo(121f, 10f))
    }

    @Test
    fun theDemoMinuteIsAMinute() {
        val h = StatsHistory.demo()
        assertEquals(60, h.size)
        assertEquals(120, h.latest?.refreshHz)
    }
}
