package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/game-override-vectors.json` against [GameOverrides.fit] — the cases core and the
 * Swift twin run, so every client asks the host for the same mode under one game's overrides.
 */
class GameOverridesTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/game-override-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun JSONArray.mode() = Triple(getInt(0), getInt(1), getInt(2))

    @Test
    fun everySharedFitVectorAgrees() {
        val rows = vectors.getJSONArray("fit")
        assertTrue("the vector file is the contract; keep it rich", rows.length() >= 11)
        for (i in 0 until rows.length()) {
            val row = rows.getJSONObject(i)
            // A row with nothing set parses to null: the same as no overrides at all.
            val o = GameOverrides.from(row) ?: GameOverrides()
            val got = o.fit(row.getJSONArray("mode").mode(), row.getJSONArray("native").mode(), row.getInt("max"))
            assertEquals(row.getString("why"), row.getJSONArray("expect").mode(), got)
        }
    }

    @Test
    fun theHostLineIsTheSetsNameElseTheValues() {
        assertEquals(listOf(Say.Text("4K")), GameOverrides(name = "4K", lines = 2160).summary)
        assertEquals(
            listOf(Say.Text("1080p60"), say(R.string.lib_host_override_hdr_off), Say.Text("DualSense")),
            GameOverrides(lines = 1080, hz = 60, hdr = false, gamepad = "DualSense ").summary,
        )
        assertEquals(listOf(say(R.string.common_hz, 120)), GameOverrides(hz = 120, gamepad = "joycon").summary)
    }
}
