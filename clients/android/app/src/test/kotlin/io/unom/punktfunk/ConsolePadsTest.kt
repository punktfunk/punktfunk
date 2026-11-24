package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.Sc2Device
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * A captured or ungranted Steam Controller 2 has no `InputDevice`, so `Gamepad.pads()` cannot
 * see it and the console's chip falls back to "TV remote". The extras list is its only way in.
 */
class ConsolePadsTest {
    @Test
    fun aCapturedSc2NamesTheChipWithNoInputDevice() {
        val json = ConsoleJson.pads(
            emptyList(),
            null,
            listOf(
                ConsoleJson.ExtraPad(
                    name = "Steam Controller 2 Puck",
                    key = "sc2:10462:4868",
                    pref = Gamepad.PREF_STEAMCONTROLLER2_PUCK,
                    detail = "28DE:1304 · usb",
                    forwarded = true,
                    rumble = true,
                ),
            ),
        )
        val j = JSONObject(json)
        assertEquals("Steam Controller 2 Puck", j.getString("label"))
        assertEquals(Gamepad.PREF_STEAMCONTROLLER2_PUCK, j.getInt("pref"))
        val pads = j.getJSONArray("pads")
        assertEquals(1, pads.length())
        assertEquals("Steam Controller 2 Puck", pads.getJSONObject(0).getString("name"))
        assertTrue(pads.getJSONObject(0).getBoolean("forwarded"))
        // The console refuses to send `PadAction::Rumble` on a row that says it has no motor,
        // so the capture's buzz is only reachable while this rides through.
        assertTrue(pads.getJSONObject(0).getBoolean("rumble"))
    }

    @Test
    fun `an uncaptured sc2 is listed without a rumble test`() {
        // Detected but not claimed (no USB grant, or passthrough off): there is no link to write
        // the grip-rumble report on, and a Test that does nothing is worse than a dimmed row.
        val j = JSONObject(
            ConsoleJson.pads(
                emptyList(),
                null,
                listOf(
                    ConsoleJson.ExtraPad(
                        name = "Steam Controller 2",
                        key = "sc2:10462:4866",
                        pref = Gamepad.PREF_STEAMCONTROLLER2,
                        detail = "28DE:1302 · usb",
                        forwarded = false,
                        rumble = false,
                    ),
                ),
            ),
        )
        val pad = j.getJSONArray("pads").getJSONObject(0)
        assertEquals("Steam Controller 2", j.getString("label"))
        assertFalse(pad.getBoolean("rumble"))
        assertFalse(pad.getBoolean("forwarded"))
    }

    @Test
    fun `a pad's speaker volume is read from the saved document and defaults to full`() {
        val doc = JSONObject("""{"pads": {"54C:CE6:Wireless Controller": {"speaker_volume": 60}}}""")
        assertEquals(60, ConsoleJson.padSpeakerVolume(doc, "54C:CE6:Wireless Controller"))
        assertEquals(100, ConsoleJson.padSpeakerVolume(doc, "54C:DF2:Other"))
        assertEquals(100, ConsoleJson.padSpeakerVolume(JSONObject(), "54C:CE6:Wireless Controller"))
        assertEquals(100, ConsoleJson.padSpeakerVolume(null, "54C:CE6:Wireless Controller"))
        val wild = JSONObject("""{"pads": {"k": {"speaker_volume": 400}}}""")
        assertEquals(100, ConsoleJson.padSpeakerVolume(wild, "k"))
    }

    @Test
    fun noExtrasLeavesTheChipEmpty() {
        val j = JSONObject(ConsoleJson.pads(emptyList(), null))
        assertTrue(j.isNull("label"))
        assertTrue(j.isNull("pref"))
        assertEquals(0, j.getJSONArray("pads").length())
    }

    /** The input test reads a captured SC2 whole: grips, ⋯, both pads, a finger only where one
     *  touches, and the pad it is drawn as. */
    @Test
    fun aCapturedSc2ReadsWholeInTheInputTest() {
        val s = Sc2Device.State(
            buttons = Sc2Device.L4 or Sc2Device.R5 or Sc2Device.QAM or Sc2Device.LPAD_CLICK or
                Sc2Device.LPAD_TOUCH,
            lsY = 32767, lpadX = -16384, lpadY = 16384, rpadX = 9000,
        )
        val j = JSONObject(ConsoleJson.sc2PadTest(s, classic = false).json())
        val held = j.getJSONArray("held")
        assertEquals(listOf("Misc", "L4", "R5", "LPad"), (0 until held.length()).map { held.getString(it) })
        val axes = j.getJSONArray("axes")
        assertEquals(8, axes.length())
        assertEquals(-1.0, axes.getJSONArray(1).getDouble(1), 1e-6) // LY: device +y up, test down
        assertEquals("LPadX", axes.getJSONArray(6).getString(0))
        assertEquals(-0.5, axes.getJSONArray(6).getDouble(1), 1e-6)
        assertEquals(-0.5, axes.getJSONArray(7).getDouble(1), 1e-6)
        assertEquals(Gamepad.PREF_STEAMCONTROLLER2, j.getInt("pref"))
        assertEquals(Gamepad.PREF_STEAMCONTROLLER, ConsoleJson.sc2PadTest(s, classic = true).pref)
    }
}
