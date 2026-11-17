package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The claim/read hand-off that lets [DsCapture] read a pad's motion calibration off the claiming
 * thread. Two things are pinned here, and both are about the gap before the read comes back.
 *
 * What the gap DOES: the pad streams, scaled by the nominal calibration. What it must NOT do:
 * inherit the previous pad's factory numbers (calibration is per unit), or accept a read that
 * outlived its claim, which an unplug, a stop, or a re-claim can all cause.
 */
class MotionCalHandoffTest {
    /**
     * A calibration whose gyro reads [rawLsbPerDegS] raw LSB per °/s and whose accel sits at
     * [accelZero] raw counts at 0 g, so two of them are told apart by what they DO — identity
     * alone would let a regression that returns the wrong instance still look right.
     */
    private fun cal(rawLsbPerDegS: Int, accelZero: Int = 0): DsDevice.MotionCal {
        val speed = 500 // speed_plus = speed_minus, so speed_2x = 1000
        val span = rawLsbPerDegS * 1000 // |plus − bias| + |minus − bias| = span
        val blob = ByteArray(41)
        fun put(o: Int, v: Int) {
            blob[o] = (v and 0xFF).toByte()
            blob[o + 1] = ((v shr 8) and 0xFF).toByte()
        }
        blob[0] = 0x05
        for (i in 0 until 3) {
            put(7 + 4 * i, span / 2) // gyro plus
            put(9 + 4 * i, -span / 2) // gyro minus
            put(23 + 4 * i, accelZero + 8192) // accel plus / minus: 8192 raw LSB per g
            put(25 + 4 * i, accelZero - 8192)
        }
        put(19, speed)
        put(21, speed)
        return DsDevice.MotionCal.parse(blob, 0x05)
    }

    /** One DS5 input report: cross held, sticks centred, gyro pitch 1600 raw, accel z 8000 raw. */
    private fun report(): ByteArray = ByteArray(64).also {
        it[0] = 0x01
        it[1] = 0x80.toByte(); it[2] = 0x80.toByte(); it[3] = 0x80.toByte(); it[4] = 0x80.toByte()
        it[8] = (0x08 or 0x20).toByte() // hat neutral | cross
        it[16] = 0x40; it[17] = 0x06 // gyro pitch = 1600
        it[26] = 0x40; it[27] = 0x1F // accel z = 8000
        it[33] = 0x80.toByte(); it[37] = 0x80.toByte() // no touch contacts
    }

    @Test
    fun `a claim scales nominally until its read lands`() {
        val h = MotionCalHandoff()
        assertSame(DsDevice.MotionCal.NOMINAL, h.effective)
        val claim = h.begin()
        assertSame("the read is in flight — scale nominally, do not wait", DsDevice.MotionCal.NOMINAL, h.effective)
        val read = cal(16)
        assertTrue(h.publish(claim, read))
        assertSame(read, h.effective)
    }

    /**
     * The whole point of scaling nominally instead of holding reports back: a pad answers its
     * buttons from the first report, and only its motion changes when the calibration arrives.
     */
    @Test
    fun `a report in the gap is forwarded, nominally scaled, and rescales once the read lands`() {
        val h = MotionCalHandoff()
        val claim = h.begin()
        val r = report()

        val gap = DsDevice.State()
        assertTrue(
            "a report must still be parsed while the read is in flight",
            DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, gap, h.effective),
        )
        assertEquals("buttons reach the wire immediately", Gamepad.BTN_A, gap.buttons)
        assertEquals("and so do sticks", 128, gap.lsX)
        assertEquals("nominal gyro is the raw count", 1600, gap.gyro[0])
        assertEquals("nominal accel is ×10000/8192", 8000L * 10000 / 8192, gap.accel[2].toLong())

        assertTrue(h.publish(claim, cal(16, accelZero = 100)))
        val live = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, live, h.effective))
        assertEquals("buttons do not depend on the calibration", gap.buttons, live.buttons)
        assertEquals("1600 raw at 16 LSB/°·s = 100 °/s = 2000 wire", 2000, live.gyro[0])
        assertNotEquals("the same raw report must convert differently now", gap.gyro[0], live.gyro[0])
        assertNotEquals(gap.accel[2], live.accel[2])
    }

    @Test
    fun `a read that outlived its claim publishes nothing`() {
        val h = MotionCalHandoff()
        val claim = h.begin()
        h.end() // unplug, or DsCapture.stop, while the read was in flight
        assertFalse("a straggler may not publish into a dead claim", h.publish(claim, cal(16)))
        assertSame(DsDevice.MotionCal.NOMINAL, h.effective)
    }

    @Test
    fun `a new claim scales nominally rather than inheriting the previous pad's calibration`() {
        val h = MotionCalHandoff()
        val first = h.begin()
        val hot = cal(4, accelZero = 400) // a pad reading 4 raw LSB per °/s, well off nominal
        assertTrue(h.publish(first, hot))
        assertSame(hot, h.effective)

        // Re-claimed without an end() in between — the pad was swapped while a read was in flight.
        val second = h.begin()
        assertNotEquals(first, second)
        assertSame(
            "the next pad starts on the nominal scaling, NOT the last pad's factory numbers",
            DsDevice.MotionCal.NOMINAL,
            h.effective,
        )
        assertFalse("the first pad's read may not scale the second pad", h.publish(first, hot))
        assertSame(DsDevice.MotionCal.NOMINAL, h.effective)

        // And that fallback is a real difference, not two names for the same numbers: the inherited
        // calibration would have turned this pad's motion into something else entirely.
        val r = report()
        val nominal = DsDevice.State()
        val inherited = DsDevice.State()
        DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, nominal, h.effective)
        DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, inherited, hot)
        assertNotEquals(inherited.gyro[0], nominal.gyro[0])
        assertNotEquals(inherited.accel[2], nominal.accel[2])

        val slow = cal(32)
        assertTrue(h.publish(second, slow))
        assertSame(slow, h.effective)
    }

    @Test
    fun `ending a claim twice still refuses every outstanding token`() {
        val h = MotionCalHandoff()
        val claim = h.begin()
        h.end() // DsCapture.stop
        h.end() // …and the unplug that followed it
        assertFalse(h.publish(claim, cal(16)))
        assertSame(DsDevice.MotionCal.NOMINAL, h.effective)
        val next = h.begin()
        assertNotEquals(claim, next)
        val read = cal(16)
        assertTrue(h.publish(next, read))
        assertSame(read, h.effective)
    }
}
