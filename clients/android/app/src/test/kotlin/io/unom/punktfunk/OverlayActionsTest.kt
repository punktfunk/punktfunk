package io.unom.punktfunk

import io.unom.punktfunk.kit.Gamepad
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Replays `clients/shared/overlay-actions-vectors.json`, which the Rust and Swift parsers replay
 * too, so the three cannot drift. A new case belongs in that file. Run:
 * `./gradlew :app:testDebugUnitTest`.
 */
class OverlayActionsTest {
    private val vectors: JSONObject by lazy {
        // The module directory is the working directory, so clients/ is two levels up.
        val file = File("../../shared/overlay-actions-vectors.json")
        assertTrue("the vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    /** JSON equality with every number compared as a Float: each client prints the floats its own way. */
    private fun sameJson(a: Any?, b: Any?): Boolean = when {
        a is Number && b is Number -> a.toFloat() == b.toFloat()
        a is JSONArray && b is JSONArray ->
            a.length() == b.length() && (0 until a.length()).all { sameJson(a.get(it), b.get(it)) }
        a is JSONObject && b is JSONObject -> a.keys().asSequence().toSet().let { keys ->
            keys == b.keys().asSequence().toSet() && keys.all { sameJson(a.get(it), b.get(it)) }
        }
        else -> a == b
    }

    @Test
    fun sharedVectorsParseAndRoundTrip() {
        val ids = vectors.getJSONArray("slot_ids")
        for (i in 0 until ids.length()) {
            assertEquals(ids.getString(i), SlotId.parse(ids.getString(i))?.id)
        }
        val cases = vectors.getJSONArray("cases")
        assertTrue("the vector file is the contract", cases.length() >= 10)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val name = case.getString("name")
            val platform = when (val p = case.getString("platform")) {
                "touch" -> RingPlatform.TOUCH
                "desktop" -> RingPlatform.DESKTOP
                else -> error("$name: platform $p")
            }
            val cfg = OverlayConfig.parse(case.getString("blob"), platform)
            val ring = case.getJSONArray("ring")
            val want = (0 until ring.length()).map { if (ring.isNull(it)) null else ring.getString(it) }
            assertEquals("$name: ring", want, cfg.ring.map { it?.id })
            val stored = JSONObject(cfg.toJson())
            assertTrue("$name: stored $stored", sameJson(stored, case.getJSONObject("round_trip")))
            assertEquals("$name: reparse", cfg, OverlayConfig.parse(cfg.toJson(), platform))
        }
    }

    @Test
    fun sharedPadTypeCycle() {
        fun pref(name: String) = Gamepad.PREFS.first { it.name == name }.wire
        val rows = vectors.getJSONArray("pad_type_cycle")
        val cycle = (0 until rows.length()).map { i ->
            val row = rows.getJSONObject(i)
            pref(row.getString("name")).also { assertEquals(row.getString("label"), padTypeLabel(it)) }
        }
        assertEquals(cycle, PAD_TYPE_CYCLE.toList())
        cycle.forEachIndexed { i, p -> assertEquals(cycle[(i + 1) % cycle.size], nextPadType(p)) }
        val outside = vectors.getJSONArray("pad_type_outside_cycle")
        for (i in 0 until outside.length()) {
            assertEquals(outside.getString(i), Gamepad.PREF_AUTO, nextPadType(pref(outside.getString(i))))
        }
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
}
