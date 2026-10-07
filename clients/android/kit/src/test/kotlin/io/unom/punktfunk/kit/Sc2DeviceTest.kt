package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM tests of [Sc2Device] — the SC2 protocol surface this client shares with the Apple
 * client (`Sc2Device.swift`) and the host (`triton_proto.rs` / `pf_driver_proto::triton`).
 * Run: `./gradlew :kit:testDebugUnitTest`.
 *
 * The tables the three share are pinned by `clients/shared/sc2-vectors.json`, so an edit on one
 * side alone goes red.
 */
class Sc2DeviceTest {

    /** One section of `clients/shared/sc2-vectors.json`. */
    private fun shared(key: String): org.json.JSONArray {
        val file = File("../../shared/sc2-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        return JSONObject(file.readText()).getJSONArray(key)
    }

    /** SC2 bit → `Gamepad.BTN_*`, the rows Swift's `wireMap` and the host's fallback replay. */
    private val expected: List<Pair<Int, Int>> by lazy {
        val rows = shared("buttons")
        (0 until rows.length()).map { rows.getJSONObject(it).run { getInt("sc2") to getInt("wire") } }
    }

    @Test
    fun `wire map matches the shared vectors`() {
        for ((sc2, wire) in expected) {
            assertEquals("sc2 bit 0x${Integer.toHexString(sc2)}", wire, Sc2Device.wireButtons(sc2))
        }
    }

    @Test
    fun `every mapped bit at once, and nothing else`() {
        val allSc2 = expected.fold(0) { acc, (sc2, _) -> acc or sc2 }
        val allWire = expected.fold(0) { acc, (_, wire) -> acc or wire }
        assertEquals(allWire, Sc2Device.wireButtons(allSc2))
        // Unmapped SC2 bits (trackpad touch, trigger clicks, the left-pad bits) translate to
        // nothing.
        assertEquals(0, Sc2Device.wireButtons(allSc2.inv()))
        assertEquals(0, Sc2Device.wireButtons(0))
    }

    /**
     * A state report of [id]: buttons LE u32 @2, triggers i16 @6/@8, sticks i16 @10..16 — the
     * 46-byte BLE shape (long enough for every offset the parser reads).
     */
    private fun stateReport(
        id: Int = Sc2Device.ID_STATE_BLE,
        buttons: Int = 0,
        lt: Int = 0,
        rt: Int = 0,
        lsX: Int = 0,
        lsY: Int = 0,
        rsX: Int = 0,
        rsY: Int = 0,
    ): ByteArray = ByteArray(46).also {
        it[0] = id.toByte()
        it[2] = buttons.toByte()
        it[3] = (buttons ushr 8).toByte()
        it[4] = (buttons ushr 16).toByte()
        it[5] = (buttons ushr 24).toByte()
        fun i16(o: Int, v: Int) {
            it[o] = v.toByte()
            it[o + 1] = (v shr 8).toByte()
        }
        i16(6, lt); i16(8, rt)
        i16(10, lsX); i16(12, lsY); i16(14, rsX); i16(16, rsY)
    }

    /** The parse contract, case for case with Swift's `testParseStateTruthTable`. */
    @Test
    fun `parse state truth table`() {
        val out = Sc2Device.State()
        val report = stateReport(
            buttons = Sc2Device.A or Sc2Device.STEAM or Sc2Device.RPAD_CLICK,
            lt = 32767, rt = -100, lsX = -32768, lsY = 32767, rsX = 1234, rsY = -1234,
        )
        assertTrue(Sc2Device.parseState(report, report.size, out))
        assertEquals(Sc2Device.A or Sc2Device.STEAM or Sc2Device.RPAD_CLICK, out.buttons)
        assertEquals(255, out.lt) // 32767 >> 7
        assertEquals(0, out.rt) // negative clamps to 0, it does not wrap
        assertEquals(-32768, out.lsX)
        assertEquals(32767, out.lsY)
        assertEquals(1234, out.rsX)
        assertEquals(-1234, out.rsY)

        // All three state shapes parse — identical offsets for everything read here.
        for (id in listOf(Sc2Device.ID_STATE, Sc2Device.ID_STATE_BLE, Sc2Device.ID_STATE_TIMESTAMP)) {
            val r = stateReport(id = id)
            assertTrue("id 0x${Integer.toHexString(id)}", Sc2Device.parseState(r, r.size, out))
        }
        // Non-state ids and short reports answer false (battery/status still ride the RAW plane;
        // they simply have no typed mirror).
        val battery = stateReport(id = Sc2Device.ID_BATTERY)
        assertFalse(Sc2Device.parseState(battery, battery.size, out))
        val short = stateReport()
        assertFalse(Sc2Device.parseState(short, 17, out))
    }

    /**
     * Every feature report the capture writes, byte for byte: the two it opens with and the
     * lizard restore it closes with. The Apple client sends the identical 64-byte zero-padded
     * frames (`Sc2Device.disableLizard` / its USB `normalizeJoysticks`), and the firmware
     * accepts the padded form.
     */
    @Test
    fun `feature command bytes verbatim`() {
        assertEquals(64, Sc2Device.DISABLE_LIZARD.size)
        // [id 1][ID_SET_SETTINGS_VALUES 0x87][len 3][SETTING_LIZARD_MODE 9][LIZARD_MODE_OFF u16]
        assertArrayEquals(
            byteArrayOf(0x01, 0x87.toByte(), 0x03, 0x09, 0x00, 0x00),
            Sc2Device.DISABLE_LIZARD.copyOf(6),
        )
        assertEquals(64, Sc2Device.NORMALIZE_JOYSTICKS.size)
        // …[SETTING_ENABLE_RAW_JOYSTICK 0x2E][0 u16] — without it a controller previously opened
        // in raw mode reports ADC coordinates (~0..3200), a few percent of full travel.
        assertArrayEquals(
            byteArrayOf(0x01, 0x87.toByte(), 0x03, 0x2E, 0x00, 0x00),
            Sc2Device.NORMALIZE_JOYSTICKS.copyOf(6),
        )
        // The release frame is the same setting with a non-zero value — the pad goes back to
        // driving the OS with its kb/mouse the moment the capture lets go.
        assertEquals(64, Sc2Device.ENABLE_LIZARD.size)
        assertArrayEquals(
            byteArrayOf(0x01, 0x87.toByte(), 0x03, 0x09, 0x01, 0x00),
            Sc2Device.ENABLE_LIZARD.copyOf(6),
        )
        // All three are pure padding past the command — a stray byte would reach the firmware.
        assertTrue(Sc2Device.DISABLE_LIZARD.drop(6).all { it == 0.toByte() })
        assertTrue(Sc2Device.NORMALIZE_JOYSTICKS.drop(6).all { it == 0.toByte() })
        assertTrue(Sc2Device.ENABLE_LIZARD.drop(6).all { it == 0.toByte() })
    }

    // ---- BLE framing: the rules Valve's vendor service actually applies, mirrored pair for
    // pair with the Apple client's `Sc2FramingTests.swift`. ----

    @Test
    fun `a state-sized payload gets the 0x45 prepend`() {
        // The live shape: a 45-byte characteristic value becomes a 46-byte id-first frame.
        val payload = ByteArray(45).also { it[0] = 0xE5.toByte() } // seq, not a report id
        val framed = Sc2Device.frameIncoming(payload)
        assertEquals(46, framed.size)
        assertEquals(Sc2Device.ID_STATE_BLE.toByte(), framed[0])
        assertArrayEquals(payload, framed.copyOfRange(1, framed.size))
        // The rule's floor: exactly 40 bytes still counts as state-sized.
        assertEquals(41, Sc2Device.frameIncoming(ByteArray(40)).size)
    }

    @Test
    fun `a short payload passes through unmodified`() {
        // Battery and status keep whatever framing the firmware gave them. No 0x45 to 0x42
        // rewrite and no zero-pad — those belong to a synthetic-USB queue contract, not ours.
        val battery = byteArrayOf(Sc2Device.ID_BATTERY.toByte(), 0x64, 0x01)
        assertArrayEquals(battery, Sc2Device.frameIncoming(battery))
        assertEquals(39, Sc2Device.frameIncoming(ByteArray(39)).size)
    }

    @Test
    fun `an output write strips the id and trims to its declared length`() {
        // A native-length 0x80 grip rumble (10 B wire = 1 id + 9 payload) rides B5 with 9 bytes.
        val rumble = byteArrayOf(0x80.toByte(), 0, 1, 2, 3, 4, 5, 6, 7, 8)
        val write = Sc2Device.outputWrite(rumble)!!
        assertEquals("100f6cb5-1735-4313-b402-38567131e5f3", write.charUuid)
        assertArrayEquals(rumble.copyOfRange(1, rumble.size), write.payload)
        // 0x82, Steam's ping/test buzz, rides B7. This is the mis-route that made every actuator
        // but one silent: the link used to send every id to the first writable characteristic.
        val buzz = byteArrayOf(0x82.toByte(), 0x03, 0x01, 0xFF.toByte())
        val buzzWrite = Sc2Device.outputWrite(buzz)!!
        assertEquals("100f6cb7-1735-4313-b402-38567131e5f3", buzzWrite.charUuid)
        assertArrayEquals(byteArrayOf(0x03, 0x01, 0xFF.toByte()), buzzWrite.payload)
    }

    @Test
    fun `a rumble frame lands on the offsets the host reads back`() {
        // `triton_proto::parse_triton_rumble` reads left.speed at 4 and right.speed at 7 of a
        // 10-byte frame — the gain byte between them is what makes the two offsets uneven.
        val frame = Sc2Device.rumbleFrame(0x1234, 0x5678)
        assertEquals(10, frame.size)
        assertEquals(0x80, frame[0].toInt() and 0xFF)
        assertEquals(0x34, frame[4].toInt() and 0xFF)
        assertEquals(0x12, frame[5].toInt() and 0xFF)
        assertEquals(0x78, frame[7].toInt() and 0xFF)
        assertEquals(0x56, frame[8].toInt() and 0xFF)
        // Zero speeds are the stop frame: same id and intensity, both motors at rest.
        val stop = Sc2Device.rumbleFrame(0, 0)
        assertEquals(0x80, stop[0].toInt() and 0xFF)
        assertArrayEquals(ByteArray(6), stop.copyOfRange(4, 10))
        // Full length declared, so the BLE leg writes all nine payload bytes to B5.
        val write = Sc2Device.outputWrite(frame)!!
        assertEquals("100f6cb5-1735-4313-b402-38567131e5f3", write.charUuid)
        assertEquals(9, write.payload.size)
        // And the USB leg coalesces it, because a level supersedes the level before it.
        assertEquals(OutReportQueue.KEY_RUMBLE, Sc2Device.outputCoalesceKey(frame))
    }

    @Test
    fun `an output write drops an old host's 64-byte padding`() {
        // Current hosts pre-trim each frame; an older one pads to 64 B. The write must carry
        // exactly the declared stripped length either way.
        val padded = ByteArray(64)
        padded[0] = 0x80.toByte()
        for (i in 1..9) padded[i] = i.toByte()
        padded[10] = 0xEE.toByte() // past the declared length — must not reach the firmware
        assertArrayEquals(
            byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8, 9),
            Sc2Device.outputWrite(padded)!!.payload,
        )
    }

    @Test
    fun `an output write clamps to what arrived`() {
        val short = byteArrayOf(0x80.toByte(), 1, 2)
        assertArrayEquals(byteArrayOf(1, 2), Sc2Device.outputWrite(short)!!.payload)
        assertNull(Sc2Device.outputWrite(byteArrayOf(0x80.toByte()))) // no payload to write
        assertNull(Sc2Device.outputWrite(ByteArray(0)))
    }

    @Test
    fun `an unknown output id keeps its whole payload`() {
        // Strip the id, keep the rest, never guess-trim. The characteristic still follows the
        // firmware's id-plus-0x35 scheme, whatever the id.
        val unknown = byteArrayOf(0x90.toByte(), 1, 2, 3, 4, 5)
        val write = Sc2Device.outputWrite(unknown)!!
        assertEquals("100f6cc5-1735-4313-b402-38567131e5f3", write.charUuid)
        assertArrayEquals(byteArrayOf(1, 2, 3, 4, 5), write.payload)
    }

    @Test
    fun `a feature write strips the channel id and keeps the padding`() {
        // Feature frames arrive whole from the host: drop the 0x01, keep everything else. The
        // firmware accepts the zero-padded form.
        val payload = Sc2Device.featurePayload(Sc2Device.DISABLE_LIZARD)!!
        assertEquals(63, payload.size)
        assertArrayEquals(byteArrayOf(0x87.toByte(), 0x03, 0x09, 0x00, 0x00), payload.copyOf(5))
        assertNull(Sc2Device.featurePayload(byteArrayOf(0x01))) // no command to write
        assertNull(Sc2Device.featurePayload(ByteArray(0)))
    }

    @Test
    fun `the lizard-off keepalive leads with the settings command`() {
        // The characteristic value carries no 0x01 channel byte, so the firmware reads byte 0 as
        // the command id: unstripped, the frame parses as command 0x01 and lizard mode never
        // goes off. That was the Bluetooth defect — the pad kept emulating a keyboard and mouse
        // on the phone, and Steam's forwarded gyro-enable was swallowed the same way.
        val payload = Sc2Device.featurePayload(Sc2Device.DISABLE_LIZARD)!!
        assertEquals(0x87.toByte(), payload[0]) // ID_SET_SETTINGS_VALUES, never the channel id
        assertArrayEquals(Sc2Device.DISABLE_LIZARD.copyOfRange(1, 64), payload)
    }

    @Test
    fun `the feature characteristic is outside the per-report block`() {
        // The regression that made every feature write land on a rumble actuator: resolving
        // writes by scanning the output block alone can never reach the feature characteristic.
        val outputs = (0x80..0x89).map { Sc2Device.bleOutputChar(it) }
        assertFalse(Sc2Device.BLE_FEATURE_CHAR in outputs)
        assertEquals("100f6c34-1735-4313-b402-38567131e5f3", Sc2Device.BLE_FEATURE_CHAR)
    }

    /** `clients/shared/sc2-vectors.json`: the host's id-included lengths, one byte longer. */
    @Test
    fun `output lengths match the shared vectors`() {
        val rows = shared("out_report_len")
        for (i in 0 until rows.length()) {
            val row = rows.getJSONObject(i)
            val id = row.getInt("id")
            // Undeclared ids stay whole on both sides: the host does not trim, we do not guess.
            val want = if (row.isNull("len")) null else row.getInt("len") - 1
            assertEquals("id 0x%02x".format(id), want, Sc2Device.strippedOutputLen(id))
        }
        assertNull(Sc2Device.strippedOutputLen(0x00))
    }

    @Test
    fun `a rumble stream collapses while a queued pulse survives`() {
        // More rumbles than the queue holds: uncoalesced, the overflow evicts the pulse.
        val q = OutReportQueue<ByteArray>()
        val pulse = byteArrayOf(0x81.toByte(), 1)
        q.offer(pulse, Sc2Device.outputCoalesceKey(pulse))
        repeat(OutReportQueue.CAP + 8) { n ->
            val rumble = byteArrayOf(0x80.toByte(), n.toByte())
            q.offer(rumble, Sc2Device.outputCoalesceKey(rumble))
        }
        assertEquals(2, q.size)
        assertArrayEquals(pulse, q.poll())
        assertArrayEquals(byteArrayOf(0x80.toByte(), (OutReportQueue.CAP + 7).toByte()), q.poll())
    }
}
