package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * One client setting is spelled out in fifteen places — the data class, its prefs key, `load`,
 * `save`, the preset overlay's eight members, both halves of the console bridge, and its settings
 * row. Two of those fifteen fail to compile when a field is missed. The other thirteen are named
 * arguments with defaults, `put` statements and `when` arms, all of which are perfectly legal one
 * field short — so the miss ships, and surfaces as a setting that saves but never loads, or an
 * override that cannot be reset (`ten_bit_sdr` was exactly that).
 *
 * These tests are FIELD-BLIND on purpose. They set every field away from its default and compare
 * whole objects, so a new setting is covered the day it is added and nobody has to remember to
 * extend a list here — which is the failure this file exists to stop, and which the console test
 * next door still has as a hand-written seven-line assertion.
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
        rumbleOnPhone = true,
        gyroOnPhone = true,
        sc2Capture = false,
        dsCapture = false,
        padHaptics = false,
        padSpeaker = true,
        mouseMode = MouseMode.CAPTURE,
        invertScroll = true,
        overlayActions = "zz-ring-blob",
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
     * back unchanged. Anything the console genuinely does not own belongs in the copy below with
     * its reason — the point is that adding a setting forces that decision rather than silently
     * dropping it on the floor the first time a user opens the console.
     */
    @Test
    fun everySettingSurvivesTheConsoleBridge() {
        val got = ConsoleJson.applySettings(Settings(), ConsoleJson.settings(every, null))
        assertEquals(every, got)
    }
}
