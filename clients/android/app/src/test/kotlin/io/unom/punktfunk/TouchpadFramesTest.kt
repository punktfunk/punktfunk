package io.unom.punktfunk

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performTouchInput
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Frame mode (design/touchpad-passthrough.md §4): the Trackpad gestures with every finger sent
 * to the host's touchpad. Read back off a recording wire and a recording surface: what still
 * reaches the wire, what the surface hears, and when a local gesture keeps the fingers.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class TouchpadFramesTest {
    @get:Rule
    val compose = createComposeRule()

    private class Wire : TouchSink {
        val log = mutableListOf<String>()
        override fun pointerMove(dx: Int, dy: Int) { log += "move $dx $dy" }
        override fun pointerAbs(x: Int, y: Int, w: Int, h: Int) { log += "abs $x $y" }
        override fun button(button: Int, down: Boolean) { log += "btn $button ${if (down) "down" else "up"}" }
        override fun scroll(axis: Int, delta: Int, source: Int, phase: Int) { log += "scroll $axis $delta" }
        override fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int) { log += "touch $id $kind" }
    }

    private data class Update(val ids: List<Long>, val xs: List<Float>, val keep: Boolean, val timeUs: Long, val w: Int, val h: Int)

    /** A recording surface; core refuses every update from the [refuseFrom]th on. */
    private class Pad(override val hostTaps: Boolean, private val refuseFrom: Int = Int.MAX_VALUE) : TouchpadFrames {
        val updates = mutableListOf<Update>()
        var calls = 0
        override fun surface(count: Int, ids: LongArray, xs: FloatArray, ys: FloatArray, keep: Boolean, timeUs: Long, w: Int, h: Int): Boolean {
            assertTrue((0 until count).all { xs[it] in 0f..1f && ys[it] in 0f..1f })
            if (calls++ >= refuseFrom) return false
            updates += Update(ids.take(count), xs.take(count), keep, timeUs, w, h)
            return true
        }
        fun counts() = updates.map { it.ids.size }
    }

    private val wire = Wire()
    private var stats = 0
    private val dial = mutableListOf<DialEvent>()

    /** The decision the gesture layer reads as each gesture starts. */
    private var current: Pad? = null

    private fun compose(pad: Pad?) {
        current = pad
        compose.mainClock.autoAdvance = false
        compose.setContent {
            Box(
                Modifier.fillMaxSize().testTag("surface").pointerInput(Unit) {
                    streamTouchInput(
                        sink = wire, stylus = null,
                        video = { VideoFrame(io.unom.punktfunk.kit.VideoFit.FIT, 0, 0) },
                        trackpad = true,
                        frames = { current },
                        onCycleStats = { stats++ }, onKeyboard = {}, onDial = { dial += it },
                    )
                },
            )
        }
    }

    private fun drag() = touch {
        advanceEventTime(400) // past the tap-drag window
        down(center)
        repeat(5) { moveBy(Offset(20f, 0f), delayMillis = 10) }
        up()
    }

    private fun touch(block: androidx.compose.ui.test.TouchInjectionScope.() -> Unit) {
        compose.onNodeWithTag("surface").performTouchInput(block)
        compose.mainClock.advanceTimeBy(100)
    }

    @Test
    fun aDragSendsContactsAtTheirOwnTimesAndNoMotion() {
        val pad = Pad(hostTaps = true)
        compose(pad)
        drag()
        assertEquals(emptyList<String>(), wire.log)
        assertEquals(listOf(1, 1, 1, 1, 1, 1, 0), pad.counts())
        val held = pad.updates.dropLast(1)
        assertEquals("one finger, one id", 1, held.map { it.ids }.toSet().size)
        assertTrue("x follows the finger", held.zipWithNext().all { (a, b) -> b.xs[0] > a.xs[0] })
        assertEquals(0.5f, held[0].xs[0], 0.01f)
        assertEquals("each move at its own time", listOf(10_000L), held.zipWithNext { a, b -> b.timeUs - a.timeUs }.distinct())
        // 360 × 800 dp at xxhdpi: the view, not the picture, is the surface.
        assertTrue(pad.updates.all { it.w == 1080 && it.h == 2400 && !it.keep })
    }

    @Test
    fun towardAHostThatTapsTheClientSendsNoClicks() {
        val pad = Pad(hostTaps = true)
        compose(pad)
        touch { down(center); advanceEventTime(50); up() }
        touch { down(0, center); down(1, center + Offset(80f, 0f)); advanceEventTime(50); up(0); up(1) }
        touch { down(center) }
        compose.mainClock.advanceTimeBy(700)
        touch { advanceEventTime(700); up() }
        assertEquals(emptyList<String>(), wire.log)
        assertEquals(0, pad.counts().last())
    }

    @Test
    fun towardAHostThatDoesNotTapTheClientStillClicks() {
        val pad = Pad(hostTaps = false)
        compose(pad)
        touch { down(center); advanceEventTime(50); up() }
        assertEquals(listOf("btn 1 down", "btn 1 up"), wire.log)
        wire.log.clear()
        touch { advanceEventTime(400); down(0, center); down(1, center + Offset(80f, 0f)); advanceEventTime(50); up(0); up(1) }
        assertEquals(listOf("btn 3 down", "btn 3 up"), wire.log)
        assertTrue(2 in pad.counts())
    }

    @Test
    fun aTwoFingerPanScrollsOnlyOnTheHost() {
        val pad = Pad(hostTaps = true)
        compose(pad)
        touch {
            down(0, center); down(1, center + Offset(80f, 0f))
            repeat(6) {
                advanceEventTime(16)
                updatePointerBy(0, Offset(0f, -30f)); updatePointerBy(1, Offset(0f, -30f)); move()
            }
            up(0); up(1)
        }
        assertEquals(emptyList<String>(), wire.log)
        assertTrue(pad.counts().count { it == 2 } >= 6)
        assertTrue(pad.updates.none { it.keep })
    }

    @Test
    fun anArmedTwistKeepsThePairForTheRing() {
        val pad = Pad(hostTaps = true)
        compose(pad)
        touch {
            val r = 60f
            fun at(deg: Double) = Offset(
                center.x + r * Math.cos(Math.toRadians(deg)).toFloat(),
                center.y + r * Math.sin(Math.toRadians(deg)).toFloat(),
            )
            down(0, at(0.0)); down(1, at(180.0))
            for (step in 1..8) {
                advanceEventTime(16)
                val a = step * 5.0
                updatePointerTo(0, at(a)); updatePointerTo(1, at(180.0 + a)); move()
            }
            up(0); up(1)
        }
        assertTrue(dial.contains(DialEvent.Commit))
        val keeps = pad.updates.filter { it.ids.size == 2 }.map { it.keep }
        assertEquals("forwarded until it arms, kept from then", keeps.sortedBy { it }, keeps)
        assertTrue(keeps.first() == false && keeps.last())
        assertEquals(emptyList<String>(), wire.log)
    }

    @Test
    fun threeFingersStayTheClientsWithoutAKeep() {
        val pad = Pad(hostTaps = false)
        compose(pad)
        touch {
            down(0, center); down(1, center + Offset(80f, 0f)); down(2, center + Offset(160f, 0f))
            advanceEventTime(50); up(0); up(1); up(2)
        }
        assertEquals(1, stats)
        assertEquals(emptyList<String>(), wire.log)
        assertTrue(3 in pad.counts())
        assertTrue(pad.updates.none { it.keep })
    }

    @Test
    fun theFilterPassesOnlyClicksAndOnlyTowardAHostThatDoesNotTap() {
        for (hostTaps in listOf(true, false)) {
            val out = Wire()
            val sink = FrameModeSink(out, hostTaps)
            sink.pointerMove(3, 4)
            sink.pointerAbs(1, 2, 10, 10)
            sink.scroll(0, 256, ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_BEGIN)
            sink.touch(0, 0, 1, 1, 10, 10)
            sink.button(1, true)
            sink.button(1, false)
            val want = if (hostTaps) emptyList() else listOf("btn 1 down", "btn 1 up")
            assertEquals("hostTaps $hostTaps", want, out.log)
        }
    }

    @Test
    fun withFramingOffEverythingGoesButAReleaseWithoutItsPress() {
        val out = Wire()
        val sink = FrameModeSink(out, hostTaps = true)
        sink.button(1, true) // a long press, kept home while framing
        sink.framing = false
        sink.pointerMove(3, 4)
        sink.scroll(0, 256, ScrollWire.SOURCE_TOUCH, ScrollWire.PHASE_UPDATE)
        sink.button(1, false)
        sink.button(3, true)
        sink.button(3, false)
        assertEquals(listOf("move 3 4", "scroll 0 256", "btn 3 down", "btn 3 up"), out.log)
    }

    @Test
    fun aRefusedUpdateHandsTheGestureBackUntilTheNextDecision() {
        val pad = Pad(hostTaps = true, refuseFrom = 2)
        compose(pad)
        drag()
        assertEquals("the down and the first move", 2, pad.updates.size)
        assertEquals("nothing after the refusal", 3, pad.calls)
        assertTrue(wire.log.isNotEmpty() && wire.log.all { it.startsWith("move ") && it.split(" ")[1].toInt() > 0 })

        wire.log.clear()
        touch { advanceEventTime(400); down(center); advanceEventTime(50); up() }
        assertEquals("the refused decision is not tried again", 3, pad.calls)
        assertEquals(listOf("btn 1 down", "btn 1 up"), wire.log)

        val next = Pad(hostTaps = true)
        current = next
        wire.log.clear()
        drag()
        assertEquals(emptyList<String>(), wire.log)
        assertEquals(listOf(1, 1, 1, 1, 1, 1, 0), next.counts())
    }

    @Test
    fun aDecisionTurnedOffTakesTheOldPathFromTheNextGesture() {
        val pad = Pad(hostTaps = true)
        compose(pad)
        drag()
        assertEquals(emptyList<String>(), wire.log)
        val calls = pad.calls
        current = null
        drag()
        assertEquals(calls, pad.calls)
        assertTrue(wire.log.isNotEmpty() && wire.log.all { it.startsWith("move ") })
    }

    @Test
    fun aDecisionTurnedOffMidGestureHandsTheRestBackWithoutAsking() {
        val pad = Pad(hostTaps = true)
        compose(pad)
        touch { advanceEventTime(400); down(center); repeat(2) { moveBy(Offset(20f, 0f), delayMillis = 10) } }
        current = null
        touch { repeat(3) { moveBy(Offset(20f, 0f), delayMillis = 10) }; up() }
        assertEquals("the down and two moves, no refused call", 3, pad.calls)
        assertEquals(3, wire.log.size)
        assertTrue(wire.log.all { it.startsWith("move ") })
    }

    /** Core answers no touchpad while its invert flag is on; the fake here does the same. */
    private fun ui(initialInvert: Boolean, order: MutableList<String>, apply: Boolean = true): StreamUi {
        var coreInvert: Boolean? = null
        return StreamUi(
            0, intArrayOf(io.unom.punktfunk.kit.SessionAccess.ALL, 0), StatsVerbosity.entries.first(), initialInvert,
            applyInvertScroll = { order += "apply $it"; if (apply) coreInvert = it; apply },
            readTouchpad = { order += "read"; (coreInvert == false).let { HostTouchpad(it, it) } },
        )
    }

    @Test
    fun theDecisionIsReadAfterCoreHasTheInvertFlagAndAgainOnEveryChange() {
        val order = mutableListOf<String>()
        val ui = ui(initialInvert = true, order)
        assertTrue("nothing read before the seed", order.isEmpty() && !ui.touchpad.presents)
        ui.seed()
        assertEquals(listOf("apply true", "read"), order)
        assertTrue("invert on: no frames", !ui.touchpad.presents)
        ui.setScrollInverted(false)
        assertTrue("invert off: frames", ui.touchpad.presents && ui.touchpad.taps)
        val on = ui.touchpad
        ui.setScrollInverted(true)
        assertTrue(!ui.touchpad.presents && !ui.touchpad.taps)
        ui.setScrollInverted(false)
        assertTrue("each read is a new decision", ui.touchpad.presents && ui.touchpad !== on)
        assertEquals(listOf("apply true", "read", "apply false", "read", "apply true", "read", "apply false", "read"), order)
    }

    @Test
    fun anInvertChangeCoreRefusedKeepsTheDecision() {
        val order = mutableListOf<String>()
        val ui = ui(initialInvert = false, order, apply = false)
        ui.seed()
        val seeded = ui.touchpad
        ui.setScrollInverted(true)
        assertTrue(ui.touchpad === seeded)
        assertEquals(listOf("apply false", "read", "apply true"), order)
    }

    @Test
    fun aViewSideInTenthsOfAMillimetreFromThePhysicalDensity() {
        // (px, physical dpi, density bucket, tenths of a millimetre)
        val cases = listOf(
            listOf(1080f, 400f, 480f, 686f), // 2.7 in = 68.58 mm
            listOf(2400f, 400f, 480f, 1524f),
            listOf(1080f, 160f, 480f, 572f), // Android's 160 placeholder: the bucket stands in
            listOf(1080f, 0f, 480f, 572f),
            listOf(1080f, 2000f, 480f, 572f),
            listOf(0f, 400f, 480f, 0f),
            listOf(1080f, 0f, 0f, 0f),
            listOf(100_000f, 100f, 100f, 65535f),
        )
        for ((px, dpi, bucket, want) in cases) {
            assertEquals("$px px at $dpi dpi", want.toInt(), pxToDmm(px.toInt(), dpi, bucket.toInt()))
        }
    }

    @Test
    fun aPadSideInTenthsOfAMillimetreFromItsResolution() {
        assertEquals("3000 units at 30/mm", 1000, padDmm(3000f, 30f))
        assertEquals("rounds", 1067, padDmm(3200f, 30f))
        assertEquals("no resolution is unknown", 0, padDmm(3000f, 0f))
        assertEquals("no span is unknown", 0, padDmm(0f, 30f))
        assertEquals("caps at 16 bits", 65535, padDmm(1e9f, 1f))
    }
}
