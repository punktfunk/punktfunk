package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [SessionAccess] against `crates/punktfunk-core/testdata/grant-vectors.json`,
 * which core writes: the bit values are the wire, and the preset label is derived from the mask
 * by the rule every client shares. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class SessionAccessTest {

    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd; `../../../` is the repo root.
        val file = File("../../../crates/punktfunk-core/testdata/grant-vectors.json")
        assertTrue("the vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    /** Bit-for-bit the core vocabulary — a reorder here would mislabel every session. */
    @Test
    fun `bits match the core vectors`() {
        val bits = vectors.getJSONObject("bits")
        val mine = mapOf(
            "GAMEPAD" to SessionAccess.GAMEPAD,
            "POINTER" to SessionAccess.POINTER,
            "KEYBOARD" to SessionAccess.KEYBOARD,
            "CLIPBOARD" to SessionAccess.CLIPBOARD,
            "MIC" to SessionAccess.MIC,
            "LAUNCH" to SessionAccess.LAUNCH,
            "POWER" to SessionAccess.POWER,
            "MANAGE_GAMES" to SessionAccess.MANAGE_GAMES,
        )
        assertEquals(bits.keys().asSequence().toSet(), mine.keys)
        mine.forEach { (name, bit) -> assertEquals(name, bits.getInt(name), bit) }
        assertEquals(vectors.getInt("all"), SessionAccess.ALL)
    }

    /** The legacy-full read and the derived preset, per mask, as core computes them. */
    @Test
    fun `labels match the core vectors`() {
        val masks = vectors.getJSONArray("masks")
        val labels = mapOf(
            "full" to "Full control",
            "controller" to "Controller only",
            "view" to "View only",
            "custom" to "Custom",
        )
        for (i in 0 until masks.length()) {
            val case = masks.getJSONObject(i)
            val mask = case.getInt("mask")
            assertEquals("normalize $mask", case.getInt("normalized"), SessionAccess.normalizeLegacyFull(mask))
            assertEquals("label $mask", labels.getValue(case.getString("level")), SessionAccess.label(mask))
        }
    }

    @Test
    fun `remaining label is compact and never empty`() {
        assertEquals("1 h 58 m", SessionAccess.remainingLabel(7130))
        assertEquals("2 h", SessionAccess.remainingLabel(7200))
        assertEquals("12 m", SessionAccess.remainingLabel(725))
        assertEquals("45 s", SessionAccess.remainingLabel(45))
        assertEquals("0 s", SessionAccess.remainingLabel(0))
    }
}
