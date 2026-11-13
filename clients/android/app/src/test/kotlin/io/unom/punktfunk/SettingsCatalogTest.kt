package io.unom.punktfunk

import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.link.StartIn
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/settings-catalog.json` against this client: every entry naming `android` is a
 * stored field and a row on the settings screen (or one the console keeps), in its category and
 * tier, worded by its `player_<key>` message, with its options, default and preset tier. A row
 * for a setting the catalogue does not give to Android fails here too.
 */
class SettingsCatalogTest {
    /** Entries naming `android` that the settings screen has no row for: the console draws them. */
    private val consoleOnly = mapOf(
        "ui_palette" to "the console's own look",
        "reduce_motion" to "the console's own look",
        "library_view" to "the console's library",
        "library_sections" to "Customize on the Library header",
        "host_sort" to "Sort on the Hosts header",
        "host_grouping" to "Group on the Hosts header",
        "reduce_ui_resolution" to "a weak GPU's console",
    )

    /** Entries naming `android` that no Android field stores: the console's document keeps them. */
    private val keptByConsole = setOf(
        "reduce_motion", "library_view", "library_sections", "host_sort", "host_grouping",
    )

    /** Helpers the category functions call, with the category their rows sit in. */
    private val helperPage = mapOf("PyroWaveQualityRow" to "display")

    /** Pickers that list the catalogue's options in another order, or fewer: open rows. */
    private val differs = mapOf(
        "gamepad" to listOf("auto", "xbox360", "dualsense", "xboxone", "dualshock4", "steamdeck"),
    )

    private val entries: List<JSONObject> by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/settings-catalog.json")
        assertTrue("the catalogue must be reachable at ${file.absolutePath}", file.isFile)
        val settings = JSONObject(file.readText()).getJSONArray("settings")
        (0 until settings.length()).map { settings.getJSONObject(it) }
    }

    private fun JSONObject.strings(name: String): List<String> =
        getJSONArray(name).let { a -> (0 until a.length()).map { scalar(a.get(it)) } }

    private fun JSONObject.names(platform: String) = platform in strings("on")

    private fun scalar(v: Any): String = when (v) {
        is Double -> if (v % 1.0 == 0.0) v.toLong().toString() else v.toString()
        else -> v.toString()
    }

    /** The field a catalogue key is stored under: its own key, or the `android.` twin. */
    private fun fieldFor(key: String): SettingsFields.Field<*>? {
        val wanted = if (key == "resolution") setOf("width") else setOf(key, "android.$key")
        return SettingsFields.ALL.firstOrNull { (it.console?.key ?: it.key) in wanted }
    }

    @Test
    fun everyCatalogueEntryIsStoredOrKeptByTheConsole() {
        assertTrue(entries.isNotEmpty())
        for (e in entries) {
            val key = e.getString("key")
            val stored = fieldFor(key) != null
            if (e.names("android")) {
                assertEquals("$key: a field, or the console keeps it", key !in keptByConsole, stored)
            } else {
                assertFalse("$key is stored, and the catalogue leaves android out", stored)
            }
        }
        for (key in keptByConsole + consoleOnly.keys) {
            val e = entries.firstOrNull { it.getString("key") == key }
            assertTrue("$key is listed here but not in the catalogue for android", e != null && e.names("android"))
        }
    }

    @Test
    fun presetTierMatchesTheOverlay() {
        for (e in entries.filter { it.names("android") }) {
            val key = e.getString("key")
            val field = fieldFor(key) ?: continue
            assertEquals("$key: preset", e.getBoolean("preset"), field.overlay != null)
        }
    }

    @Test
    fun defaultsMatchAFreshInstall() {
        for (e in entries.filter { it.names("android") && it.has("default") }) {
            val key = e.getString("key")
            val field = fieldFor(key) ?: continue
            val json = JSONObject()
            field.consoleWrite(json, Settings())
            val raw = scalar(json.get(field.console?.key ?: field.key))
            // An empty start screen reads as the hosts list.
            val stored = if (key == "start_in") StartIn.parse(raw).stored else raw
            assertEquals("$key: default", scalar(e.get("default")), stored)
        }
    }

    @Test
    fun optionsMatchTheCatalogue() {
        for (e in entries.filter { it.names("android") && it.getString("key") !in consoleOnly }) {
            val key = e.getString("key")
            val got = options(key)
            if (!e.has("options")) {
                assertEquals("$key lists options the catalogue does not", null, got)
                continue
            }
            val want = e.strings("options")
            val listed = differs[key]
            if (listed == null) {
                assertEquals("$key: options", want, got)
            } else {
                assertEquals("$key", listed, got)
                assertTrue("$key: not the catalogue's options", listed != want && want.containsAll(listed))
            }
        }
    }

    /** The stored value of each entry of a picker's list, in the order the screen offers them. */
    private fun options(key: String): List<String>? = when (key) {
        "start_in" -> START_IN_OPTIONS.map { it.first }
        "stats_verbosity" -> STATS_VERBOSITY_OPTIONS.map { it.first.name.lowercase() }
        "gamepad_ui_mode" -> GAMEPAD_UI_MODE_OPTIONS.map { it.first }
        "hud_placement" -> HUD_PLACEMENT_OPTIONS.map { it.first }
        "stats_scale_pct" -> STATS_SCALE_OPTIONS.map { it.toString() }
        "background_timeout_minutes" -> BACKGROUND_TIMEOUT_OPTIONS.map { it.toString() }
        "refresh_hz" -> REFRESH_OPTIONS.map { it.toString() }
        "video_fit" -> VIDEO_FIT_OPTIONS.map { it.first }
        "present_priority" -> PRESENT_PRIORITY_OPTIONS.map { it.first }
        "smooth_buffer" -> (listOf(0) + smoothBufferCosts(60).map { it.first }).map { it.toString() }
        "render_scale" -> RenderScale.PRESETS.map { scalar(it) }
        "codec" -> CODEC_OPTIONS.map { it.first }
        "compositor" -> COMPOSITOR_OPTIONS.map { SettingsFields.COMPOSITOR_NAMES[it.first] }
        "audio_channels" -> AUDIO_CHANNEL_OPTIONS.map { it.first.toString() }
        "audio_format" -> AUDIO_FORMAT_OPTIONS.map { it.first }
        "touch_mode" -> TOUCH_MODE_OPTIONS.map { it.first.name.lowercase() }
        "mouse_mode" -> MOUSE_MODE_OPTIONS.map { it.first.storedName }
        "gamepad" -> GAMEPAD_OPTIONS.map { o -> Gamepad.PREFS.first { it.wire == o.first }.name }
        "system_buttons" -> SYSTEM_BUTTON_OPTIONS.map { it.first }
        else -> null
    }

    // ---- The settings screen's source ---------------------------------------------------------
    // The screen is Compose code, not a table, so the rows are read from its source: which
    // `R.string.player_<key>` it words a row with, in which category function, inside which
    // advanced group.

    private val source: String by lazy {
        val file = File("src/main/kotlin/io/unom/punktfunk/SettingsScreen.kt")
        assertTrue("the settings screen must be reachable at ${file.absolutePath}", file.isFile)
        file.readText()
            .replace(Regex("(?s)/\\*.*?\\*/"), { " ".repeat(it.value.length) })
            .replace(Regex("(?m)^\\s*//.*$"), { " ".repeat(it.value.length) })
    }

    private fun matching(s: String, open: Int, a: Char, b: Char): Int {
        var depth = 0
        for (i in open until s.length) {
            when (s[i]) {
                a -> depth++
                b -> if (--depth == 0) return i
            }
        }
        error("unbalanced $a at $open")
    }

    /** Top-level functions: name, then the offsets of its body. */
    private val functions: List<Triple<String, Int, Int>> by lazy {
        Regex("(?m)^(?:private |internal )?fun (?:<[^>]*> )?(?:[A-Za-z<>?.]+\\.)?([A-Za-z0-9_]+)\\(")
            .findAll(source).map { m ->
                val open = source.indexOf('{', matching(source, m.range.last, '(', ')'))
                Triple(m.groupValues[1], open, matching(source, open, '{', '}'))
            }.toList()
    }

    /** The category function names, by the catalogue category they draw. */
    private val categories = mapOf(
        "GeneralSettings" to "general",
        "DisplaySettings" to "display",
        "InputSettings" to "input",
        "AudioSettings" to "audio",
        "ControllerSettings" to "controllers",
    )

    /** The advanced groups: the offsets of each trailing lambda. */
    private val advancedGroups: List<IntRange> by lazy {
        Regex("AdvancedGroup\\(").findAll(source).map { m ->
            val args = matching(source, m.range.last, '(', ')')
            val open = source.indexOf('{', args)
            open..matching(source, open, '{', '}')
        }.toList()
    }

    /** Where `R.string.player_<key>` words a row: (category, advanced) for each use. */
    private fun uses(key: String): List<Pair<String, Boolean>> =
        Regex("R\\.string\\.player_$key(?![A-Za-z0-9_])").findAll(source).map { m ->
            val at = m.range.first
            val fn = functions.first { at in it.second..it.third }.first
            val page = categories[fn] ?: helperPage[fn] ?: error("$key is worded in $fn, a function this test does not know")
            page to advancedGroups.any { at in it }
        }.toList()

    @Test
    fun rowsSitInTheirCategoryAndTier() {
        for (fn in categories.keys) assertTrue("no $fn in the screen", functions.any { it.first == fn })
        for (e in entries) {
            val key = e.getString("key")
            val used = uses(key).distinct()
            if (!e.names("android")) {
                assertTrue("$key has a row, and the catalogue leaves android out", used.isEmpty())
                continue
            }
            if (key in consoleOnly) {
                assertTrue("$key has a row, and is listed as the console's", used.isEmpty())
                continue
            }
            val want = e.getString("category") to e.getBoolean("advanced")
            assertEquals("$key: its category and tier on the settings screen", listOf(want), used)
        }
    }
}
