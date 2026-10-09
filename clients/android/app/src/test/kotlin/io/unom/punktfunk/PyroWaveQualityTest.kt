package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The PyroWave quality row asks core for the mode a connect would ask, and the dial carries the
 * quality in hundredths. Core's own pricing and wording are tested in `pyrowave_quality.rs`: a
 * JVM test cannot load the native library, so the price here is a stand-in that records its ask.
 */
class PyroWaveQualityTest {
    private val wire = 1 to 2500

    @Test
    fun theRowPricesTheModeAConnectWouldAsk() {
        var asked: List<Any>? = null
        val half = Settings(renderScale = 0.5, hdrEnabled = false, pyrowaveBpp = 1.2)
        val lines = PyroWaveQuality.lines(half, Triple(1920, 1080, 60), wire) { w, h, hz, chroma, depth, x100, _, _ ->
            asked = listOf(w, h, hz, chroma, depth, x100)
            "50 Mbit/s\n"
        }
        assertEquals("960×540 at 60 Hz: 50 Mbit/s" to null, lines)
        assertEquals(listOf(960, 540, 60, false, 8, 120), asked)

        val warning = "Needs 1.6 Gbit/s — more than a 1 GbE link carries."
        val hdr = PyroWaveQuality.lines(Settings(), Triple(3840, 2160, 120), wire) { _, _, _, _, depth, _, _, _ ->
            asked = listOf(depth)
            "1.6 Gbit/s\n$warning"
        }
        assertEquals("3840×2160 at 120 Hz: 1.6 Gbit/s" to warning, hdr)
        assertEquals("HDR asks 10 bits", listOf(10), asked)
    }

    /** A phone barred from `/sys` reads its link as other; Android's Wi-Fi transport stands in,
     *  so core words the Wi-Fi line. A kind core did read stays. */
    @Test
    fun androidsTransportFillsAKindCoreCouldNotRead() {
        val barred = PyroWaveQuality.filledIn(3 to 0, wifi = true, ethernet = false)
        assertEquals(2 to 0, barred)
        var kind = -1
        PyroWaveQuality.lines(Settings(), Triple(1920, 1080, 60), barred) { _, _, _, _, _, _, k, _ ->
            kind = k
            ""
        }
        assertEquals(2, kind)
        assertEquals(1 to 0, PyroWaveQuality.filledIn(0 to 0, wifi = false, ethernet = true))
        assertEquals(wire, PyroWaveQuality.filledIn(wire, wifi = true, ethernet = false))
        assertEquals(3 to 0, PyroWaveQuality.filledIn(3 to 0, wifi = false, ethernet = false))
    }

    @Test
    fun theDialCarriesHundredthsInsideTheBounds() {
        assertEquals(160, Settings().pyrowaveBppX100())
        // The prefs store keeps a float.
        assertEquals(160, Settings(pyrowaveBpp = 1.6f.toDouble()).pyrowaveBppX100())
        assertEquals(200, Settings(pyrowaveBpp = 9.0).pyrowaveBppX100())
        assertEquals(1.6, PyroWaveQuality.snapped(1.6f.toDouble()), 0.0)
        assertEquals(0.5, PyroWaveQuality.snapped(0.1), 0.0)
    }
}
