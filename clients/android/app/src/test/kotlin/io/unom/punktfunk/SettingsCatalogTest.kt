package io.unom.punktfunk

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/settings-catalog.json` against [SettingsFields]: every shared setting is a row
 * under its catalogue key (or its `android.` twin) or one this client does not have. A new
 * catalogue key fails here until one of the two is true.
 */
class SettingsCatalogTest {
    /** Catalogue keys with no Android store. The console gates their rows off on this platform. */
    private val absent = setOf(
        "fullscreen_on_stream", "follow_os_theme", "reduce_motion", "library_view", "library_sections",
        "host_sort", "host_grouping", "enable_444", "vsync", "allow_vrr", "decoder", "audio_route",
        "inhibit_shortcuts", "cursor_gestures", "forward_pad", "sc2_capture",
    )

    /** Catalogue keys stored under another name. */
    private val renamed = mapOf("resolution" to "width", "low_latency" to "low_latency_mode")

    @Test
    fun everyCatalogueKeyIsStoredOrAbsent() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/settings-catalog.json")
        assertTrue("the catalogue must be reachable at ${file.absolutePath}", file.isFile)
        val settings = JSONObject(file.readText()).getJSONArray("settings")
        val keys = (0 until settings.length()).map { settings.getJSONObject(it).getString("key") }
        assertTrue(keys.isNotEmpty())
        val stored = SettingsFields.ALL.map { it.key }.toSet()
        for (key in keys.filter { it !in absent }) {
            val name = renamed[key] ?: key
            assertTrue("$key is in the catalogue but not stored", name in stored || "android.$name" in stored)
        }
        for (key in absent) assertTrue("$key is listed absent but not in the catalogue", key in keys)
    }
}
