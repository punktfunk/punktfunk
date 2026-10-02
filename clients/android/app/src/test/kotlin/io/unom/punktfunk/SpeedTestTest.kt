package io.unom.punktfunk

import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * Where a measured bitrate lands. The measurement itself is the host's job; the decision this code
 * makes is which layer to write — and the long-standing wrong answer (always the global) is exactly
 * what made measuring the slow box downstairs re-tune the desktop too
 * (design/client-settings-profiles.md §5.3).
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class SpeedTestTest {
    private val store get() = PresetStore(RuntimeEnvironment.getApplication())
    private fun host() = KnownHost("192.168.1.42", 9777, "Desk", "a".repeat(64), paired = true)

    @Test
    fun anUnboundHostTargetsTheGlobalDefault() {
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(host(), null, store))
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(null, null, store))
    }

    @Test
    fun aPresetThatSetsBitrateIsTheLayerThatHostReads() {
        val s = store
        val game = newPreset("Game").copy(overrides = SettingsOverlay(bitrateKbps = 50_000))
        s.save(game)
        val target = SpeedTestTarget.resolve(host().copy(presetId = game.id), null, s)
        assertEquals(game.id, (target as SpeedTestTarget.Preset).preset.id)
    }

    @Test
    fun aPresetThatInheritsBitrateAsksWhichLayer() {
        val s = store
        val work = newPreset("Work") // overrides nothing
        s.save(work)
        val target = SpeedTestTarget.resolve(host().copy(presetId = work.id), null, s)
        // Both layers are defensible here, so the user picks — we don't guess.
        assertEquals(work.id, (target as SpeedTestTarget.Ask).preset.id)
    }

    @Test
    fun theOneOffPickWinsAndTheEmptyOneForcesTheDefaults() {
        val s = store
        val game = newPreset("Game").copy(overrides = SettingsOverlay(bitrateKbps = 50_000))
        val work = newPreset("Work")
        listOf(game, work).forEach(s::save)
        val bound = host().copy(presetId = work.id)

        // Testing from a pinned card measures — and writes — that card's preset.
        assertEquals(game.id, (SpeedTestTarget.resolve(bound, game.id, s) as SpeedTestTarget.Preset).preset.id)
        // "Connect with: Default settings" is a real choice, so its speed test targets the global.
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(bound, "", s))
        // A dangling binding resolves as no preset everywhere else; here too.
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(host().copy(presetId = "gone"), null, s))
    }

    @Test
    fun applyingWritesOnlyTheBitrate_andOnlyToTheChosenLayer() {
        val s = store
        val game = newPreset("Game").copy(
            overrides = SettingsOverlay(bitrateKbps = 50_000, width = 3840, height = 2160),
        )
        s.save(game)
        val globals = Settings(bitrateKbps = 20_000, codec = "hevc")
        var savedGlobals: Settings? = null

        val where = applySpeedTestResult(
            kbps = 84_000,
            target = SpeedTestTarget.Preset(game),
            toPreset = true,
            presets = s,
            settings = globals,
            onGlobalChange = { savedGlobals = it },
        )
        assertEquals("“Game”", where)
        assertNull("the global must not move when a preset was the target", savedGlobals)
        val after = s.byId(game.id)!!.overrides
        assertEquals(84_000, after.bitrateKbps)
        // Nothing else in the overlay is a speed test's business.
        assertEquals(3840, after.width)
        assertEquals(2160, after.height)
    }

    @Test
    fun theAskCaseHonoursWhichButtonWasPressed() {
        val s = store
        val work = newPreset("Work")
        s.save(work)
        val globals = Settings(bitrateKbps = 20_000)
        var savedGlobals: Settings? = null

        // "Set as default" writes the global and leaves the preset inheriting.
        val whereGlobal = applySpeedTestResult(
            42_000, SpeedTestTarget.Ask(work), toPreset = false, presets = s,
            settings = globals, onGlobalChange = { savedGlobals = it },
        )
        assertEquals("the default bitrate", whereGlobal)
        assertEquals(42_000, savedGlobals!!.bitrateKbps)
        assertNull(s.byId(work.id)!!.overrides.bitrateKbps)

        // "Set in Work" records the override instead — and now that preset stops inheriting.
        savedGlobals = null
        val wherePreset = applySpeedTestResult(
            42_000, SpeedTestTarget.Ask(work), toPreset = true, presets = s,
            settings = globals, onGlobalChange = { savedGlobals = it },
        )
        assertEquals("“Work”", wherePreset)
        assertNull(savedGlobals)
        assertEquals(42_000, s.byId(work.id)!!.overrides.bitrateKbps)
    }

    @Test
    fun theFlatReportParsesIntoFindings() {
        // Header of 17, then one finding of six: a faster host port offering the capped profile.
        val v = doubleArrayOf(
            940_000.0, 1.0, 1.0, 470_000.0, 0.0, 300.0, 1.0, 1000.0, 32_768.0, 1.0, 1.0, 2500.0,
            32_768.0, 2.0, 2.0, 0.1, 1.0,
            1.0, 2.0, 1.0, 2500.0, 1000.0, 200.0,
        )
        val done = parseNetworkCheck(v)!!
        assertEquals(940_000, done.throughputKbps)
        assertTrue(done.wall)
        assertEquals(470_000, done.clean!!.rateKbps)
        assertEquals(658_000, done.recommendedKbps)
        assertEquals(1, done.findings.size)
        assertEquals(1, done.offeredProfile)
        assertEquals(listOf(2500, 1000, 200), done.findings[0].numbers)
        assertTrue(findingText(1, done.findings[0].numbers).contains("2500 vs 1000"))
        assertTrue(findingText(7, listOf(0, 0, 0)).endsWith("Wi-Fi."))
        // A report short of its header, or of its findings, is no report.
        assertEquals(null, parseNetworkCheck(doubleArrayOf(1.0, 2.0)))
        assertEquals(null, parseNetworkCheck(v.copyOf(20)))
        assertEquals("capped", profileName(1))
    }

    @Test
    fun theRecommendationLeavesHeadroom() {
        // 70 % of measured, in the desktop clients' integer order — a stream needs room for the
        // FEC overhead and for the loss a burst measurement doesn't see.
        val done = SpeedTestPhase.Done(throughputKbps = 100_000, lossPct = 0.4, recommendedKbps = 100_000 / 10 * 7)
        assertEquals(70_000, done.recommendedKbps)
        assertEquals(100.0, done.measuredMbps, 0.001)
        assertEquals(70.0, done.recommendedMbps, 0.001)
        assertTrue(done.recommendedKbps < done.throughputKbps)
    }
}
