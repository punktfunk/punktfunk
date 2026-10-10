package io.unom.punktfunk

import android.view.InputDevice
import android.view.MotionEvent
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File

/**
 * The Kotlin twin of the core's `ScrollAccumulator` — same Q24.8 wire, same residue and
 * validation rules — plus the MotionEvent-source → wire-source mapping the mouse path uses.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class ScrollNormalizerTest {
    private val norm = ScrollNormalizer()

    @Test
    fun wheelCountsDetentsInV120() {
        assertEquals(
            listOf(NormalizedScroll(0, 30 * 256, ScrollWire.SOURCE_WHEEL, ScrollWire.PHASE_NONE)),
            norm.wheel(0.25, 0.0, ScrollWire.SOURCE_WHEEL, 0.0, 0.0, 1.0),
        )
        assertEquals(
            listOf(NormalizedScroll(0, 120 * 256, ScrollWire.SOURCE_WHEEL, ScrollWire.PHASE_NONE)),
            norm.wheel(1.0, 0.0, ScrollWire.SOURCE_WHEEL, 0.0, 0.0, 1.0),
        )
    }

    @Test
    fun touchpadDistanceIsDipNotDetents() {
        // 10 px per axis unit at density 2 → 1.0 axis = 5 DIP = 1280 wire, never repriced to 120.
        assertEquals(
            listOf(NormalizedScroll(0, 5 * 256, ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_NONE)),
            norm.wheel(1.0, 0.0, ScrollWire.SOURCE_FINGER, 10.0, 10.0, 2.0),
        )
    }

    @Test
    fun sameDipDistanceAtAnyDensity() {
        // 20 px at density 1 and 40 px at density 2 are the same 20-DIP travel.
        val a = ScrollNormalizer().wheel(2.0, 0.0, ScrollWire.SOURCE_FINGER, 10.0, 10.0, 1.0)
        val b = ScrollNormalizer().wheel(4.0, 0.0, ScrollWire.SOURCE_FINGER, 10.0, 10.0, 2.0)
        assertEquals(a, b)
    }

    @Test
    fun unknownSourceIsPricedAsAWheel() {
        assertEquals(
            listOf(NormalizedScroll(0, 120 * 256, ScrollWire.SOURCE_UNKNOWN, ScrollWire.PHASE_NONE)),
            norm.wheel(1.0, 0.0, ScrollWire.SOURCE_UNKNOWN, 10.0, 10.0, 2.0),
        )
    }

    @Test
    fun nonFiniteAndBadPairsDropWithoutTouchingResidue() {
        norm.event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_BEGIN, 0, 0.9) // holds 0.4 residue
        for (bad in listOf(Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY)) {
            assertNull(norm.event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_UPDATE, 0, bad))
        }
        assertNull(norm.event(ScrollWire.SOURCE_WHEEL, ScrollWire.PHASE_BEGIN, 0, 1.0)) // phased wheel
        assertNull(norm.event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_END, 0, 1.0)) // stop w/ delta
        assertNull(norm.event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_UPDATE, 2, 1.0)) // bad axis
        // Every rejection left the held residue alone: 0.3*256 + 0.4 = 77.2 → 77.
        assertEquals(77, norm.event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_UPDATE, 0, 0.3)!!.delta)
    }

    @Test
    fun boundariesEmitAtZeroAndRestartResidue() {
        norm.event(ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_BEGIN, 0, 0.4)
        assertEquals(
            NormalizedScroll(0, 0, ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_END),
            norm.event(ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_END, 0, 0.0),
        )
        // A new gesture starts clean — no stale 0.4 carry from the ended one.
        assertEquals(102, norm.event(ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_BEGIN, 0, 0.4)!!.delta)
        // A zero-quantized Update emits nothing; a zero Cancel still closes.
        assertNull(norm.event(ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_UPDATE, 0, 0.001))
        assertEquals(
            NormalizedScroll(0, 0, ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_CANCEL),
            norm.event(ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_CANCEL, 0, 0.0),
        )
    }

    @Test
    fun sourceSwitchClearsTheResidue() {
        norm.event(ScrollWire.SOURCE_FINGER, ScrollWire.PHASE_UPDATE, 0, 0.9) // 0.4 DIP residue
        // Wheel residue never reprices the finger's leftover: 0.25 v120 = 64, not 64 + DIP junk.
        assertEquals(64, norm.event(ScrollWire.SOURCE_WHEEL, ScrollWire.PHASE_NONE, 0, 0.25)!!.delta)
    }

    @Test
    fun motionEventSourcesMapToWireSources() {
        fun eventWithSource(source: Int): MotionEvent {
            val ev = MotionEvent.obtain(0L, 0L, MotionEvent.ACTION_SCROLL, 0f, 0f, 0)
            ev.source = source
            return ev
        }
        assertEquals(ScrollWire.SOURCE_FINGER, wireScrollSource(eventWithSource(InputDevice.SOURCE_TOUCHPAD).source))
        assertEquals(ScrollWire.SOURCE_WHEEL, wireScrollSource(eventWithSource(InputDevice.SOURCE_MOUSE).source))
        assertEquals(
            ScrollWire.SOURCE_WHEEL,
            wireScrollSource(eventWithSource(InputDevice.SOURCE_MOUSE_RELATIVE).source),
        )
        assertEquals(
            ScrollWire.SOURCE_UNKNOWN,
            wireScrollSource(eventWithSource(InputDevice.SOURCE_JOYSTICK).source),
        )
    }

    /** Every sequence Rust's `ScrollAccumulator` wrote; `../../../` from the module is the root. */
    @Test
    fun matchesTheRustVectors() {
        val file = File("../../../crates/core/punktfunk-core/testdata/scroll-vectors.json")
        assertTrue("the vector file must be reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("cases")
        assertTrue("the vector file has cases", cases.length() > 0)
        val wrong = mutableListOf<String>()
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val steps = case.getJSONArray("steps")
            val n = ScrollNormalizer()
            for (j in 0 until steps.length()) {
                val s = steps.getJSONObject(j)
                val got = n.event(s.getInt("source"), s.getInt("phase"), s.getInt("axis"), s.getDouble("delta"))
                val want = if (s.isNull("wire")) {
                    null
                } else {
                    val w = s.getJSONObject("wire")
                    NormalizedScroll(w.getInt("axis"), w.getInt("delta"), w.getInt("source"), w.getInt("phase"))
                }
                if (got != want) wrong += "${case.getString("name")} step $j: Rust $want, Kotlin $got"
            }
        }
        assertTrue(wrong.joinToString("\n"), wrong.isEmpty())
    }
}
