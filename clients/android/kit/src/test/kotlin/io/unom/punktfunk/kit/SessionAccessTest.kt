package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [SessionAccess] against `crates/core/punktfunk-core/testdata/grant-vectors.json`,
 * which core writes: the bit values are the wire, and the preset label is derived from the mask
 * by the rule every client shares. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class SessionAccessTest {

    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd; `../../../` is the repo root.
        val file = File("../../../crates/core/punktfunk-core/testdata/grant-vectors.json")
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
            "full" to R.string.grant_level_full,
            "controller" to R.string.grant_level_controller,
            "view" to R.string.grant_level_view,
            "custom" to R.string.grant_level_custom,
        )
        for (i in 0 until masks.length()) {
            val case = masks.getJSONObject(i)
            val mask = case.getInt("mask")
            assertEquals("normalize $mask", case.getInt("normalized"), SessionAccess.normalizeLegacyFull(mask))
            assertEquals("label $mask", labels.getValue(case.getString("level")), SessionAccess.label(mask))
        }
    }

    @Test
    fun `remaining names its message and numbers and is never empty`() {
        fun check(secs: Int, message: Int, vararg args: Int) = SessionAccess.remaining(secs).let {
            assertEquals("message $secs", message, it.message)
            assertEquals("args $secs", args.toList(), it.args)
        }
        check(7130, R.string.grant_remaining_hours_minutes, 1, 58)
        check(7200, R.string.grant_remaining_hours, 2)
        check(725, R.string.grant_remaining_minutes, 12)
        check(45, R.string.grant_remaining_seconds, 45)
        check(0, R.string.grant_remaining_seconds, 0)
    }
}
