package io.unom.punktfunk

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

/**
 * The field table is the one list every settings walk loops over, so the two things that can
 * still drift are a property without a row and a row that does not round-trip. Both are checked
 * here without naming a single field, which is what makes the next field's forgetting loud.
 */
class SettingsFieldsTest {
    /** The data class's instance properties (statics are the companion's constants). */
    private fun properties(c: Class<*>) = c.declaredFields
        .filter { !java.lang.reflect.Modifier.isStatic(it.modifiers) }
        .map { it.name }
        .filter { it != "extra" }
        .toSet()

    @Test
    fun everySettingsPropertyHasARow() {
        assertEquals(properties(Settings::class.java), SettingsFields.ALL.map { it.name }.toSet())
    }

    @Test
    fun everyOverlayPropertyHasAPresetRow() {
        assertEquals(properties(SettingsOverlay::class.java), SettingsFields.PRESET.map { it.name }.toSet())
    }

    /** A value that differs from the default in every row, so a dropped row shows as a mismatch. */
    private fun moved(): Settings = Settings(
        width = 3840, height = 2160, hz = 120,
        bitrateKbps = 40_000, renderScale = 0.5, videoFit = "crop", secondScreen = false,
        hdrEnabled = false, tenBitSdr = true, compositor = 2, gamepad = 3, gamepadForwarding = false,
        systemButtons = "host", guideGesture = "off", audioChannels = 6, audioFormat = AUDIO_FORMAT_LOSSLESS_96,
        codec = "av1", micEnabled = true, echoCancel = false, keepHostAudio = true,
        statsVerbosity = StatsVerbosity.DETAILED, advancedStats = true, hudPlacement = "bottomTrailing",
        statsScalePct = 150, exitHint = false, showAdvanced = true, touchMode = TouchMode.TOUCH, gamepadUiEnabled = false,
        reduceUiResolution = true, gamepadUiMode = GAMEPAD_UI_ALWAYS, uiPalette = "crimson",
        lowLatencyMode = false, presentPriority = "smooth", smoothBuffer = 2, autoWakeEnabled = false,
        backgroundKeepAlive = true, backgroundTimeoutMinutes = 30, padRumble = false,
        rumbleOnPhone = true, gyroOnPhone = true, dsCapture = false,
        padHaptics = false, padSpeaker = true, mouseMode = MouseMode.CAPTURE, invertScroll = true,
        overlayActions = "{\"ring\":[]}", backOpensRing = false, startIn = "library", defaultHost = "desk",
    )

    @Test
    fun everyRowMovesInTheProbe() {
        val a = Settings(); val b = moved()
        for (f in SettingsFields.ALL) assertNotEquals(f.name, f.get(a), f.get(b))
    }

    /**
     * A codec this device can't decode is dropped from the picker, and the one case where it
     * can't be dropped — it is the stored value — says why it is not in effect (#1138).
     */
    @Test
    fun anUndecodableStoredCodecSaysSo() {
        val capable = codecOptionsFor("av1", av1Capable = true, pyrowaveCapable = true)
        assertEquals(CODEC_OPTIONS, capable)

        val bare = codecOptionsFor("hevc", av1Capable = false, pyrowaveCapable = false)
        assertEquals(listOf("auto", "hevc", "h264"), bare.map { it.first })

        val kept = codecOptionsFor("av1", av1Capable = false, pyrowaveCapable = false)
        assertEquals(listOf("auto", "hevc", "h264", "av1"), kept.map { it.first })
        val label = kept.first { it.first == "av1" }.second
        assertNotEquals("AV1", label)
        assert(label.contains("no hardware decoder")) { label }
    }

    @Test
    fun thePresetOverlayRoundTripsEveryRow() {
        val want = moved()
        val overlay = SettingsFields.PRESET.fold(SettingsOverlay()) { o, f -> o.pin(f.key, want) }
        assertEquals(SettingsFields.PRESET_KEYS, overlay.overridden() - SettingsOverlay.FIELD_RESOLUTION + setOf("width", "height"))
        val back = SettingsOverlay.fromJson(JSONObject(overlay.toJson().toString()))
        assertEquals(overlay, back)
        for (f in SettingsFields.PRESET) assertEquals(f.name, f.get(want), f.get(back.apply(Settings())))
        // Clearing every override by key leaves nothing — including keys a stale KNOWN list once missed.
        val cleared = SettingsFields.PRESET.fold(back) { o, f -> o.clear(f.key) }
        assertEquals(emptySet<String>(), cleared.overridden())
        assertEquals(0, cleared.toJson().length())
    }
}
