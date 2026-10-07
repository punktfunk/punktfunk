package io.unom.punktfunk

import io.unom.punktfunk.kit.Gamepad
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The Kotlin twin of pf-client-core's `overlay_actions` tests — the same blobs, the same
 * outcomes, so the two parsers cannot drift. Run: `./gradlew :app:testDebugUnitTest`.
 */
class OverlayActionsTest {
    private val full = """{"v":2,
        "ring":["end_stream","shortcut:s1","host:power.sleep","stats",null,"pad"],
        "shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]}],
        "pad":{"layout":"sticks","opacity":0.3,"scale":1.2}}"""

    @Test
    fun roundTripsThroughJson() {
        val cfg = OverlayConfig.parse(full)
        assertEquals(SlotId.Shortcut("s1"), cfg.ring[1])
        assertEquals(SlotId.Host("power.sleep"), cfg.ring[2])
        assertNull(cfg.ring[4])
        assertEquals("sticks", cfg.pad.layout)
        assertEquals(listOf("ctrl", "shift", "escape"), cfg.shortcut("s1")!!.keys)
        assertEquals(cfg, OverlayConfig.parse(cfg.toJson()))
    }

    @Test
    fun shortRingsPadAndLongRingsTruncate() {
        val short = OverlayConfig.parse("""{"ring":["mic"]}""", RingPlatform.DESKTOP)
        assertEquals(SlotId.Mic, short.ring[0])
        assertTrue(short.ring.drop(1).all { it == null })
        assertEquals(6, short.ring.size)
        val long = OverlayConfig.parse(
            """{"ring":["mic","mic","mic","mic","mic","mic","stats","stats"]}""",
            RingPlatform.DESKTOP,
        )
        assertEquals(6, long.ring.size)
        assertTrue(long.ring.all { it == SlotId.Mic })
    }

    @Test
    fun unknownIdsAndDanglingShortcutsAreEmptySlots() {
        val cfg = OverlayConfig.parse("""{"ring":["teleport","shortcut:nope","host:","stats"]}""")
        assertNull("a newer client's id degrades to empty", cfg.ring[0])
        assertNull("no such shortcut", cfg.ring[1])
        assertNull("a host id needs a name", cfg.ring[2])
        assertEquals(SlotId.Stats, cfg.ring[3])
    }

    @Test
    fun emptyOrBrokenBlobsAreThePlatformDefault() {
        val touch = OverlayConfig.platformDefault(RingPlatform.TOUCH)
        val desktop = OverlayConfig.platformDefault(RingPlatform.DESKTOP)
        assertEquals(touch, OverlayConfig.parse(""))
        assertEquals(touch, OverlayConfig.parse(null))
        assertEquals(desktop, OverlayConfig.parse("{not json", RingPlatform.DESKTOP))
        assertEquals(SlotId.Pad, touch.ring[5])
        assertEquals(SlotId.SendText, desktop.ring[5])
        val cfg = OverlayConfig.parse("""{"v":2,"ring":[]}""")
        assertEquals(PadConfig(), cfg.pad)
        assertTrue(cfg.ring.all { it == null })
    }

    /** The Kotlin port of `pad_control_tweaks_round_trip_and_carry_unknown_ids`. */
    @Test
    fun padControlTweaksRoundTripAndCarryUnknownIds() {
        val blob = """{"v":2,"pad":{"layout":"full","opacity":0.45,"scale":1.0,
            "controls":{"ls":{"x":0.1,"y":0.8,"scale":1.5},"weird":{"hidden":true}},
            "controls_narrow":{"face":{"scale":0.75}}}}"""
        val cfg = OverlayConfig.parse(blob)
        assertEquals(PadTweak(x = 0.1f, y = 0.8f, scale = 1.5f), cfg.pad.controls["ls"])
        assertTrue("an unknown id is data, not an error", cfg.pad.controls["weird"]!!.hidden)
        assertEquals(0.75f, cfg.pad.controlsNarrow["face"]!!.scale)
        val json = cfg.toJson()
        assertTrue("a rewrite keeps what it does not know", "weird" in json)
        assertEquals(cfg, OverlayConfig.parse(json))
        val plain = OverlayConfig.platformDefault(RingPlatform.TOUCH).toJson()
        assertTrue("an untouched pad keeps its blob clean", "controls" !in plain)
        val sparse = OverlayConfig.parse("""{"pad":{"controls":{"rs":{"x":0.5}}}}""").toJson()
        assertTrue("absent fields stay absent: $sparse", """"rs":{"x":0.5}""" in sparse)
    }

    /** Every case Rust's `key_vk` wrote; `../../../` from the module directory is the repo root. */
    @Test
    fun keyVkMatchesTheRustVectors() {
        val file = File("../../../crates/punktfunk-core/testdata/key-vk-vectors.json")
        assertTrue("the vector file must be reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("cases")
        assertTrue("the vector file has cases", cases.length() > 0)
        val wrong = (0 until cases.length()).map { cases.getJSONObject(it) }.mapNotNull { c ->
            val name = c.getString("name")
            val want = if (c.isNull("vk")) null else c.getInt("vk")
            "${JSONObject.quote(name)}: Rust $want, Kotlin ${keyVk(name)}".takeIf { keyVk(name) != want }
        }
        assertTrue(wrong.joinToString("\n"), wrong.isEmpty())
    }

    @Test
    fun chordLegends() {
        assertEquals("Ctrl+Shift+Esc", chordChip(listOf("ctrl", "shift", "escape")))
        assertEquals("Win", keyLegend("win"))
        assertEquals("PgUp", keyLegend("pageup"))
        assertEquals("F4", keyLegend("f4"))
        assertEquals("←", keyLegend("left"))
    }

    @Test
    fun slotIdsAreStableStrings() {
        for (id in listOf(
            "end_stream", "end_game", "disconnect_linger", "touch_mode", "keyboard", "stats", "mic", "pad",
            "send_text", "guide", "qam", "pad_mouse", "pad_type", "stream_mute", "swap_screens",
            "host:power.reboot", "shortcut:s2",
        )) {
            assertEquals(id, SlotId.parse(id)!!.id)
        }
    }

    @Test
    fun padTypeCycleWrapsAndASettingsOnlyTypeStepsToAutomatic() {
        val seen = mutableListOf(Gamepad.PREF_AUTO)
        var p = nextPadType(Gamepad.PREF_AUTO)
        while (p != Gamepad.PREF_AUTO) {
            seen += p
            p = nextPadType(p)
        }
        assertEquals(PAD_TYPE_CYCLE.toList(), seen)
        assertEquals(Gamepad.PREF_AUTO, nextPadType(Gamepad.PREF_STEAMCONTROLLER2))
        assertEquals("DualShock 4", padTypeLabel(Gamepad.PREF_DUALSHOCK4))
    }
}
