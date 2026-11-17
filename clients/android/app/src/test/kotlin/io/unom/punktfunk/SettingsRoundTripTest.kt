package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * [SettingsFields] is the one list of [Settings] fields: the prefs store, the preset overlay and
 * both halves of the console bridge loop over it. A property missing from one of those paths
 * still compiles (named arguments carry defaults), so the miss ships — as a setting that saves
 * but never loads, or an override that cannot be reset.
 *
 * These tests are FIELD-BLIND on purpose. They set every field away from its default and compare
 * whole objects, so a new setting is covered the day it is added and nobody has to remember to
 * extend a list here.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class SettingsRoundTripTest {

    /** Every field away from its default, in values each consumer accepts as real. */
    private val every = Settings(
        width = 3840,
        height = 2160,
        hz = 120,
        bitrateKbps = 80_000,
        renderScale = 0.75,
        hdrEnabled = false,
        hdrWithoutHeadroom = true,
        tenBitSdr = true,
        compositor = 2,
        gamepad = 3,
        gamepadForwarding = false,
        systemButtons = "local",
        guideGesture = "on",
        audioChannels = 6,
        audioFormat = AUDIO_FORMAT_LOSSLESS_96,
        codec = "av1",
        micEnabled = true,
        echoCancel = false,
        keepHostAudio = true,
        statsVerbosity = StatsVerbosity.DETAILED,
        touchMode = TouchMode.POINTER,
        gamepadUiEnabled = false,
        reduceUiResolution = true,
        gamepadUiMode = GAMEPAD_UI_ALWAYS,
        uiPalette = "crimson",
        lowLatencyMode = false,
        presentPriority = "smooth",
        smoothBuffer = 2,
        autoWakeEnabled = false,
        padRumble = false,
        keepAwake = false,
        rumbleOnPhone = true,
        gyroOnPhone = true,
        dsCapture = false,
        padHaptics = false,
        padSpeaker = true,
        mouseMode = MouseMode.CAPTURE,
        invertScroll = true,
        overlayActions = "zz-ring-blob",
        shortcuts = "zz-shortcuts-blob",
        backOpensRing = false,
        startIn = "library",
        defaultHost = "zz-host-id",
    )

    /**
     * `save` then `load` must return what went in — every field, not a chosen few. A field missing
     * from either half is silent both ways: the setting appears to take, and comes back as the
     * default on the next launch.
     */
    @Test
    fun everySettingSurvivesSaveAndLoad() {
        val store = SettingsStore(RuntimeEnvironment.getApplication())
        store.save(every)
        assertEquals(every, store.load())
    }

    /**
     * The console shell and the touch UI edit ONE store, so what one writes the other must read
     * back unchanged. Anything the console genuinely does not own needs a test of its own with its
     * reason (see [theHostScreenIsNeverAGlobal]) — adding a setting forces that decision rather
     * than silently dropping it the first time a user opens the console.
     */
    @Test
    fun everySettingSurvivesTheConsoleBridge() {
        val got = ConsoleJson.applySettings(Settings(), ConsoleJson.settings(every, null))
        assertEquals(every, got)
    }

    /** A preset's host screen has no global value: neither the prefs nor the console's document hold one. */
    @Test
    fun theHostScreenIsNeverAGlobal() {
        val store = SettingsStore(RuntimeEnvironment.getApplication())
        store.save(every.copy(hostScreen = "second"))
        assertEquals(every, store.load())
        assertFalse(ConsoleJson.settings(every.copy(hostScreen = "second"), null).has("host_screen"))
        val doc = ConsoleJson.settings(every, null).put("host_screen", "main")
        assertEquals(every, ConsoleJson.applySettings(every, doc))
    }
}
