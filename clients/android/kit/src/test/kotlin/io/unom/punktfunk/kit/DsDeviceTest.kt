package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM tests of the Sony USB report codec ([DsDevice]) — the byte-exact inverse of the
 * host's `dualsense_proto.rs` / `dualshock4_proto.rs` serializers (offsets cross-checked against
 * those files' own tests). No Android runtime types ([Gamepad]'s BTN_* are compile-time ints).
 * Run: `./gradlew :kit:testDebugUnitTest`.
 */
class DsDeviceTest {
    private fun ds5Report(mutate: (ByteArray) -> Unit = {}): ByteArray =
        ByteArray(64).also {
            it[0] = 0x01
            // Sticks centred, hat neutral (8).
            it[1] = 0x80.toByte(); it[2] = 0x80.toByte(); it[3] = 0x80.toByte(); it[4] = 0x80.toByte()
            it[8] = 0x08
            // Touch points inactive (bit7 set).
            it[33] = 0x80.toByte(); it[37] = 0x80.toByte()
            mutate(it)
        }

    private fun ds4Report(mutate: (ByteArray) -> Unit = {}): ByteArray =
        ByteArray(64).also {
            it[0] = 0x01
            it[1] = 0x80.toByte(); it[2] = 0x80.toByte(); it[3] = 0x80.toByte(); it[4] = 0x80.toByte()
            it[5] = 0x08
            it[35] = 0x80.toByte(); it[39] = 0x80.toByte()
            mutate(it)
        }

    // ---- input parse ----

    @Test
    fun ds5ButtonsMapPositionally() {
        val s = DsDevice.State()
        // cross+triangle, hat NE, L1+create+L3, PS+touchpad+mute.
        val r = ds5Report {
            it[8] = (0x20 or 0x80 or 0x01).toByte() // cross | triangle | hat=1 (NE)
            it[9] = (0x01 or 0x10 or 0x40).toByte() // L1 | create | L3
            it[10] = (0x01 or 0x02 or 0x04).toByte() // PS | touchpad | mute
        }
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, s))
        val expected = Gamepad.BTN_A or Gamepad.BTN_Y or
            Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_RIGHT or
            Gamepad.BTN_LB or Gamepad.BTN_BACK or Gamepad.BTN_LS_CLICK or
            Gamepad.BTN_GUIDE or Gamepad.BTN_TOUCHPAD or Gamepad.BTN_MISC1
        assertEquals(expected, s.buttons)
    }

    @Test
    fun ds5SticksInvertYAndCoverTheFullRange() {
        val s = DsDevice.State()
        // Device +y down; wire +y up. Left stick fully up-left, right stick fully down-right.
        val r = ds5Report {
            it[1] = 0x00; it[2] = 0x00 // lx min, ly min (up)
            it[3] = 0xFF.toByte(); it[4] = 0xFF.toByte() // rx max, ry max (down)
            it[5] = 0x40; it[6] = 0xFF.toByte()
        }
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, s))
        assertEquals(-32768, s.lsX)
        assertEquals(32767, s.lsY) // device up → wire +32767
        assertEquals(32767, s.rsX)
        assertEquals(-32768, s.rsY) // device down → wire −32768
        assertEquals(0x40, s.lt)
        assertEquals(0xFF, s.rt)
        // Centre stays (near) centre: 0x80 → 128 wire units of bias, the u8 grid's own offset.
        val c = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, ds5Report(), 64, c))
        assertEquals(128, c.lsX)
        assertEquals(-129, c.lsY)
    }

    @Test
    fun ds5MotionAndTouchUnpack() {
        val s = DsDevice.State()
        val r = ds5Report {
            // gyro pitch = 0x0102, accel z = -2 (LE i16s at 16.. / 22..).
            it[16] = 0x02; it[17] = 0x01
            it[26] = 0xFE.toByte(); it[27] = 0xFF.toByte()
            // Touch 0 active, id 5, x=1919 (0x77F), y=1079 (0x437):
            // b0=0x05, b1=0x7F, b2=(x>>8)|((y&0xF)<<4)=0x77, y>>4=0x43.
            it[33] = 0x05
            it[34] = 0x7F
            it[35] = (0x07 or (0x07 shl 4)).toByte()
            it[36] = 0x43
        }
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, s))
        assertEquals(0x0102, s.gyro[0])
        assertEquals(-2, s.accel[2])
        assertTrue(s.touchActive[0])
        assertEquals(1919, s.touchX[0])
        assertEquals(1079, s.touchY[0])
        assertFalse(s.touchActive[1])
    }

    @Test
    fun edgePaddlesParseOnlyOnTheEdge() {
        val r = ds5Report { it[10] = 0xF0.toByte() } // all four FN/BACK bits
        val edge = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE_EDGE, r, 64, edge))
        // Host inverse (`edge_paddle_bits`): PADDLE1/2 = right/left BACK, PADDLE3/4 = right/left Fn.
        assertEquals(
            Gamepad.BTN_PADDLE1 or Gamepad.BTN_PADDLE2 or Gamepad.BTN_PADDLE3 or Gamepad.BTN_PADDLE4,
            edge.buttons,
        )
        val plain = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, r, 64, plain))
        assertEquals(0, plain.buttons) // a non-Edge never reports phantom paddles
    }

    @Test
    fun ds4LayoutDiffersWhereItShould() {
        val s = DsDevice.State()
        val r = ds4Report {
            it[5] = (0x10 or 0x04).toByte() // square | hat=4 (down)
            it[6] = (0x10 or 0x20).toByte() // share | options
            it[7] = 0x03 // PS | touchpad click
            it[8] = 0x11 // L2 analog
            it[9] = 0x99.toByte() // R2 analog
            // gyro yaw at 15.. (second i16 of 13..19).
            it[15] = 0x34; it[16] = 0x12
            // Touch 0 active id 3 at x=100 (0x064), y=941 (0x3AD): b1=0x64, b2=0xD0, b3=0x3A.
            it[35] = 0x03
            it[36] = 0x64
            it[37] = 0xD0.toByte()
            it[38] = 0x3A
        }
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSHOCK4, r, 64, s))
        assertEquals(
            Gamepad.BTN_X or Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_BACK or Gamepad.BTN_START or
                Gamepad.BTN_GUIDE or Gamepad.BTN_TOUCHPAD,
            s.buttons,
        )
        assertEquals(0x11, s.lt)
        assertEquals(0x99, s.rt)
        assertEquals(0x1234, s.gyro[1])
        assertTrue(s.touchActive[0])
        assertEquals(100, s.touchX[0])
        assertEquals(941, s.touchY[0])
    }

    @Test
    fun rejectsForeignAndShortReports() {
        val s = DsDevice.State()
        assertFalse(DsDevice.parseState(DsDevice.Model.DUALSENSE, ds5Report { it[0] = 0x31 }, 64, s))
        assertFalse(DsDevice.parseState(DsDevice.Model.DUALSENSE, ds5Report(), 8, s))
        assertFalse(DsDevice.parseState(DsDevice.Model.DUALSHOCK4, ds4Report(), 8, s))
    }

    // ---- IMU calibration (the pad's own scale factors) ----

    /**
     * A calibration feature report in the pads' USB layout: report id, three gyro bias words, six
     * INTERLEAVED gyro plus/minus words, the two speed words, six accel plus/minus words — all
     * little-endian i16, exactly what [DsDevice.MotionCal.parse] reads and what
     * `crates/pf-inject/tests/motion_contract.rs` writes from the other end.
     */
    private fun calBlob(
        id: Int,
        gyroBias: IntArray,
        gyroPlus: IntArray,
        gyroMinus: IntArray,
        speed: Int,
        accelPlus: IntArray,
        accelMinus: IntArray,
        len: Int = 41,
    ): ByteArray = ByteArray(len).also { b ->
        fun put(o: Int, v: Int) {
            b[o] = (v and 0xFF).toByte()
            b[o + 1] = ((v shr 8) and 0xFF).toByte()
        }
        b[0] = id.toByte()
        for (i in 0 until 3) {
            put(1 + 2 * i, gyroBias[i])
            put(7 + 4 * i, gyroPlus[i])
            put(9 + 4 * i, gyroMinus[i])
            put(23 + 4 * i, accelPlus[i])
            put(25 + 4 * i, accelMinus[i])
        }
        put(19, speed)
        put(21, speed)
    }

    /**
     * A realistic DualSense blob: gyro measured at 512 °/s each way over ±8192 counts about a
     * small factory bias — 16384/1024 = 16 raw LSB per °/s, the ≈±2000 °/s full scale a real pad
     * has — and accel spanning about ±8192 counts (`DS_ACC_RES_PER_G`) about a per-axis zero point
     * that is NOT zero. Both are the shape a nominal constant cannot express.
     */
    private fun realisticCal(): DsDevice.MotionCal = DsDevice.MotionCal.parse(
        calBlob(
            id = 0x05,
            gyroBias = intArrayOf(10, -6, 3),
            gyroPlus = intArrayOf(10 + 8192, -6 + 8192, 3 + 8192),
            gyroMinus = intArrayOf(10 - 8192, -6 - 8192, 3 - 8192),
            speed = 512, // speed_plus + speed_minus = 1024
            accelPlus = intArrayOf(8300, 8200, 8000),
            accelMinus = intArrayOf(-8100, -8192, -8384),
        ),
        0x05,
    )

    @Test
    fun calibrationRescalesRawCountsOntoTheWireUnits() {
        val cal = realisticCal()
        // 100 °/s at this pad's 16 LSB per °/s = 1600 raw → the wire's 20 LSB per °/s = 2000.
        for (axis in 0 until 3) {
            assertEquals(2000, cal.gyroToWire(axis, 1600))
            assertEquals(-2000, cal.gyroToWire(axis, -1600))
            assertEquals(0, cal.gyroToWire(axis, 0))
        }
        // 1 g = the axis's zero point plus half its declared 2 g range → 10000 wire units.
        val zero = intArrayOf(100, 4, -192) // plus − range/2, per axis
        val oneG = intArrayOf(8300, 8200, 8000) // = accelPlus
        for (axis in 0 until 3) {
            assertEquals(10000, cal.accelToWire(axis, oneG[axis]))
            assertEquals(0, cal.accelToWire(axis, zero[axis]))
            assertEquals(-10000, cal.accelToWire(axis, zero[axis] - (oneG[axis] - zero[axis])))
        }
        // Both rescales are >1 here, so full-scale raw must clamp rather than wrap the i16.
        assertEquals(32767, cal.gyroToWire(0, 30000))
        assertEquals(-32768, cal.gyroToWire(0, -30000))
        assertEquals(32767, cal.accelToWire(0, 30000))
        // The capture logs this, and it is the discriminator the owed on-glass check reads: a pad
        // whose blob was read declares its own resolution, the fallback declares the wire's.
        assertTrue(cal.toString().startsWith("gyro 16/16/16 LSB/°·s"))
        assertTrue(DsDevice.MotionCal.NOMINAL.toString().startsWith("gyro 20/20/20 LSB/°·s"))
    }

    /**
     * The host's own virtual pads declare `DS_FEATURE_CALIBRATION` (`dualsense_proto.rs`) — a blob
     * that states the wire's units exactly. Reading it back must therefore be a passthrough: if
     * this ever stops holding, the client and the host disagree about what a motion sample means.
     */
    @Test
    fun theHostsOwnBlobIsAPassthrough() {
        val cal = DsDevice.MotionCal.parse(
            calBlob(
                id = 0x05,
                gyroBias = intArrayOf(0, 0, 0),
                gyroPlus = intArrayOf(10000, 10000, 10000),
                gyroMinus = intArrayOf(-10000, -10000, -10000),
                speed = 500,
                accelPlus = intArrayOf(10000, 10000, 10000),
                accelMinus = intArrayOf(-10000, -10000, -10000),
            ),
            0x05,
        )
        for (axis in 0 until 3) {
            assertEquals(2000, cal.gyroToWire(axis, 2000)) // 100 °/s
            assertEquals(10000, cal.accelToWire(axis, 10000)) // 1 g
            assertEquals(-1234, cal.gyroToWire(axis, -1234))
        }
    }

    /**
     * Anything unusable keeps the pre-calibration behaviour — accel on the nominal 8192 LSB/g,
     * gyro straight through. A pad with no readable calibration is better off slightly mis-scaled
     * than silent, so nothing here may zero motion.
     */
    @Test
    fun unusableCalibrationFallsBackInsteadOfZeroing() {
        val degenerate = calBlob(
            id = 0x02,
            gyroBias = intArrayOf(0, 0, 0),
            gyroPlus = intArrayOf(0, 0, 0),
            gyroMinus = intArrayOf(0, 0, 0),
            speed = 0,
            accelPlus = intArrayOf(0, 0, 0),
            accelMinus = intArrayOf(0, 0, 0),
            len = 37,
        )
        val cals = listOf(
            DsDevice.MotionCal.NOMINAL,
            DsDevice.MotionCal.parse(null, 0x05), // the GET_REPORT failed
            DsDevice.MotionCal.parse(ByteArray(8) { if (it == 0) 0x05 else 0 }, 0x05), // short reply
            DsDevice.MotionCal.parse(degenerate, 0x02), // a clone pad's zeroes
            DsDevice.MotionCal.parse(degenerate, 0x05), // someone else's report id
        )
        for (cal in cals) {
            for (axis in 0 until 3) {
                assertEquals(1234, cal.gyroToWire(axis, 1234)) // passthrough
                assertEquals(10000, cal.accelToWire(axis, 8192)) // 8192 raw LSB = 1 g
                assertEquals(-10000, cal.accelToWire(axis, -8192))
            }
        }
    }

    /** The parse applies the calibration at the motion offsets, per model, and defaults to nominal. */
    @Test
    fun parseStateAppliesTheCalibration() {
        val cal = realisticCal()
        // DS5: gyro at [16..22), accel at [22..28). Pitch = 1600 raw (100 °/s), accel z = 8000 (1 g).
        val ds5 = ds5Report {
            it[16] = 0x40; it[17] = 0x06 // 1600
            it[26] = 0x40; it[27] = 0x1F // 8000
        }
        val five = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, ds5, 64, five, cal))
        assertEquals(2000, five.gyro[0])
        assertEquals(10000, five.accel[2])
        // DS4: gyro at [13..19), accel at [19..25). Same numbers, same answers.
        val ds4 = ds4Report {
            it[13] = 0x40; it[14] = 0x06
            it[23] = 0x40; it[24] = 0x1F
        }
        val four = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSHOCK4, ds4, 64, four, cal))
        assertEquals(2000, four.gyro[0])
        assertEquals(10000, four.accel[2])
        // No calibration argument = the nominal fallback: gyro through, accel ×10000/8192.
        val nominal = DsDevice.State()
        assertTrue(DsDevice.parseState(DsDevice.Model.DUALSENSE, ds5, 64, nominal))
        assertEquals(1600, nominal.gyro[0])
        assertEquals(8000L * 10000 / 8192, nominal.accel[2].toLong())
    }

    /** Each model asks for the feature report its firmware actually serves over USB. */
    @Test
    fun calibrationReportIdentityPerModel() {
        assertEquals(0x05, DsDevice.Model.DUALSENSE.calReportId)
        assertEquals(41, DsDevice.Model.DUALSENSE.calReportLen)
        assertEquals(0x05, DsDevice.Model.DUALSENSE_EDGE.calReportId)
        assertEquals(41, DsDevice.Model.DUALSENSE_EDGE.calReportLen)
        assertEquals(0x02, DsDevice.Model.DUALSHOCK4.calReportId)
        assertEquals(37, DsDevice.Model.DUALSHOCK4.calReportLen)
    }

    // ---- output builders (offsets = the host parser's: `parse_ds_output` / `parse_ds4_output`) ----

    @Test
    fun ds5RumbleReportFlagsAndMotors() {
        val r = DsDevice.ds5RumbleReport(DsDevice.Model.DUALSENSE, low = 0xFF00, high = 0x1200)
        assertEquals(48, r.size)
        assertEquals(0x02, r[0].toInt())
        assertEquals(0x03, r[1].toInt()) // compat vibration | haptics select
        assertEquals(0x04, r[39].toInt()) // VIBRATION2 (fw ≥ 2.24)
        assertEquals(0x12, r[3].toInt() and 0xFF) // high = right/small at [3]
        assertEquals(0xFF, r[4].toInt() and 0xFF) // low = left/big at [4]
        // A nonzero amplitude never collapses to motor 0.
        assertEquals(1, DsDevice.ds5RumbleReport(DsDevice.Model.DUALSENSE, 0x00FF, 0)[4].toInt())
        // The Edge's output report is the 64-byte variant.
        assertEquals(64, DsDevice.ds5RumbleReport(DsDevice.Model.DUALSENSE_EDGE, 0, 0).size)
    }

    @Test
    fun ds5TriggerReportPlacesTheBlockPerSide() {
        val effect = ByteArray(11) { (it + 1).toByte() }
        val r2 = DsDevice.ds5TriggerReport(DsDevice.Model.DUALSENSE, which = 1, effect = effect)
        assertEquals(0x04, r2[1].toInt()) // R2 valid flag
        assertEquals(1, r2[11].toInt()) // block at [11..22)
        assertEquals(11, r2[21].toInt())
        assertEquals(0, r2[22].toInt())
        val l2 = DsDevice.ds5TriggerReport(DsDevice.Model.DUALSENSE, which = 0, effect = effect)
        assertEquals(0x08, l2[1].toInt()) // L2 valid flag
        assertEquals(1, l2[22].toInt()) // block at [22..33)
        assertEquals(11, l2[32].toInt())
        // Oversized wire effects clamp to the 11-byte hardware block.
        val big = DsDevice.ds5TriggerReport(DsDevice.Model.DUALSENSE, 1, ByteArray(20) { 0x7F })
        assertEquals(0, big[22].toInt())
    }

    @Test
    fun ds5LightbarPlayerLedsAndInit() {
        val led = DsDevice.ds5LightbarReport(DsDevice.Model.DUALSENSE, 1, 2, 3)
        assertEquals(0x04, led[2].toInt()) // lightbar valid flag
        assertEquals(1, led[45].toInt()); assertEquals(2, led[46].toInt()); assertEquals(3, led[47].toInt())
        val pl = DsDevice.ds5PlayerLedsReport(DsDevice.Model.DUALSENSE, 0xFF)
        assertEquals(0x10, pl[2].toInt()) // player-LED valid flag
        assertEquals(0x1F, pl[44].toInt()) // masked to the 5 LEDs
        val init = DsDevice.ds5InitReport(DsDevice.Model.DUALSENSE)
        assertEquals(0x02, init[39].toInt()) // lightbar-setup enable
        assertEquals(0x02, init[42].toInt()) // LIGHT_OUT — releases the firmware animation
    }

    @Test
    fun ds5MicLedSetsOnlyItsFlagAndMode() {
        val mic = DsDevice.ds5MicLedReport(DsDevice.Model.DUALSENSE_EDGE, 2)
        assertEquals(0x02, mic[0].toInt())
        assertEquals(0x01, mic[2].toInt()) // mic-LED valid flag
        assertEquals(2, mic[9].toInt()) // pulse
        assertEquals(3, mic.count { it.toInt() != 0 })
    }

    @Test
    fun ds4ReportIsAFullStateWrite() {
        val r = DsDevice.ds4Report(low = 0xAB00, high = 0x0100, r = 9, g = 8, b = 7)
        assertEquals(32, r.size)
        assertEquals(0x05, r[0].toInt())
        assertEquals(0x03, r[1].toInt()) // motors | LED, both — composed state
        assertEquals(0x01, r[4].toInt()) // high = weak/right at [4]
        assertEquals(0xAB, r[5].toInt() and 0xFF) // low = strong/left at [5]
        assertEquals(9, r[6].toInt()); assertEquals(8, r[7].toInt()); assertEquals(7, r[8].toInt())
        assertEquals(0, r[9].toInt()) // blink untouched
    }

    @Test
    fun modelResolution() {
        assertEquals(DsDevice.Model.DUALSENSE, DsDevice.modelFor(0x0CE6))
        assertEquals(DsDevice.Model.DUALSENSE_EDGE, DsDevice.modelFor(0x0DF2))
        assertEquals(DsDevice.Model.DUALSHOCK4, DsDevice.modelFor(0x05C4))
        assertEquals(DsDevice.Model.DUALSHOCK4, DsDevice.modelFor(0x09CC))
        assertEquals(null, DsDevice.modelFor(0x1234))
        assertEquals(Gamepad.PREF_DUALSENSE, DsDevice.Model.DUALSENSE.pref)
        assertEquals(Gamepad.PREF_DUALSENSEEDGE, DsDevice.Model.DUALSENSE_EDGE.pref)
        assertEquals(Gamepad.PREF_DUALSHOCK4, DsDevice.Model.DUALSHOCK4.pref)
    }
}
