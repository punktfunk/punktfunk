package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import io.unom.punktfunk.console.SkiaConsole
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.Seat
import io.unom.punktfunk.kit.SeatState
import io.unom.punktfunk.kit.library.Download
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.json.JSONTokener
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * `clients/shared/console-bridge-vectors.json` against this shell: every command the console
 * sends has a handler or is dropped on purpose, and the [ConsoleJson] builders write each model
 * sample. Robolectric because [SkiaConsole] holds a main-thread `Handler`.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class ConsoleBridgeVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/console-bridge-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun sample(model: String, i: Int = 0): Any =
        vectors.getJSONObject("models").getJSONArray(model).get(i)

    /** One JSON value, whatever the key order or number spelling. */
    private fun same(a: Any?, b: Any?): Boolean = when {
        a is JSONObject && b is JSONObject -> a.length() == b.length() &&
            a.keys().asSequence().all { b.has(it) && same(a.get(it), b.get(it)) }
        a is JSONArray && b is JSONArray -> a.length() == b.length() &&
            (0 until a.length()).all { same(a.get(it), b.get(it)) }
        a is Number && b is Number -> a.toDouble() == b.toDouble()
        else -> a == b
    }

    private fun assertWrites(model: String, i: Int, json: String) {
        val want = sample(model, i)
        assertTrue("$model[$i] wrote $json, not $want", same(want, JSONTokener(json).nextValue()))
    }

    @Test
    fun everyCommandHasAHandlerOrIsDroppedOnPurpose() {
        val cmds = vectors.getJSONArray("commands")
        val names = (0 until cmds.length())
            .map { i -> cmds.get(i).let { it as? String ?: (it as JSONObject).keys().next() } }
            .toSet()
        assertEquals(names, SkiaConsole.commands.keys + SkiaConsole.ignoredCommands)
        assertTrue(SkiaConsole.ignoredCommands.none { it in SkiaConsole.commands })
    }

    @Test
    fun pairAndWakeWriteTheSamples() {
        assertWrites("PairPhase", 0, ConsoleJson.pairIdle())
        assertWrites("PairPhase", 1, ConsoleJson.pairBusy())
        assertWrites("PairPhase", 2, ConsoleJson.pairFailed("Wrong PIN."))
        assertWrites("PairPhase", 3, ConsoleJson.pairPaired("ab12"))
        assertWrites(
            "WakeStatus", 0,
            ConsoleJson.wakeStatus("ab12", "Desk", 12, timedOut = false, online = true, thenConnect = true),
        )
    }

    @Test
    fun speedPhasesWriteTheSamples() {
        assertWrites("SpeedPhase", 0, ConsoleJson.speedPhase(SpeedTestPhase.Connecting))
        assertWrites("SpeedPhase", 1, ConsoleJson.speedPhase(SpeedTestPhase.Measuring))
        assertWrites("SpeedPhase", 2, ConsoleJson.speedProgress(41_000))
        assertWrites("SpeedPhase", 3, ConsoleJson.speedPhase(SpeedTestPhase.Failed("Couldn't reach 10.0.0.2.")))
        val done = SpeedTestPhase.Done(
            throughputKbps = 120_000, lossPct = 0.5, recommendedKbps = 84_000, wall = true,
            clean = CleanRound(100_000, 0.5, 800), findings = listOf(Finding(3, 1, listOf(120_000, 0, 0))),
        )
        assertWrites("SpeedPhase", 4, ConsoleJson.speedPhase(done))
    }

    @Test
    fun profileAnswersWriteTheSamples() {
        val rows = listOf(
            ListedProfile(
                id = "p1", displayName = "Ada", accent = "#3366ff", owner = true,
                seat = Seat(SeatState.STARTING, detail = "Starting Steam", steamSignIn = true),
            ),
            ListedProfile(id = "p2", displayName = "Ben", legacySeat = true),
        )
        assertWrites("ProfilesAnswer", 0, ConsoleJson.profilesAnswer(ProfilesAnswer.Listed(rows)))
        assertWrites("ProfilesAnswer", 1, ConsoleJson.profilesAnswer(ProfilesAnswer.NoProfiles))
        assertWrites("ProfilesAnswer", 2, ConsoleJson.profilesAnswer(ProfilesAnswer.Failed("Couldn't load the profiles.")))
    }

    /** Only the error phase: `nativeConsoleLibraryBegin` stands in for a bare `Loading`. */
    @Test
    fun libraryPushesWriteTheSamples() {
        assertWrites("LibraryPhase", 3, ConsoleJson.libraryError("Not paired", "Pair this device first.", false))
        val downloads = listOf(
            Download(
                "steam:570", "downloading", doneBytes = 1_048_576, totalBytes = 4_194_304,
                rateBps = 524_288, etaS = 6, phase = "File 2 of 3",
            ),
            Download("steam:620", "failed", error = "The disk is full."),
        )
        assertWrites("DownloadsPush", 0, ConsoleJson.downloads(downloads, 255))
    }

    /** The second sample's battery reads an `InputDevice`, which a JVM test cannot build. */
    @Test
    fun padsWriteTheSample() {
        val sc2 = ConsoleJson.ExtraPad(
            "Steam Controller", "sc2:1", 0, "28DE:1302 · gamepad", forwarded = true, rumble = true,
        )
        assertWrites("PadsJson", 0, ConsoleJson.pads(emptyList(), null, listOf(sc2)))
    }

    /** Android's own keys are top-level, beside the shared ones. */
    @Test
    fun settingsCarryTheSampleKeysFlat() {
        val want = sample("Settings") as JSONObject
        val got = ConsoleJson.settings(
            Settings(bitrateKbps = 20_000, gamepadUiEnabled = false, lowLatencyMode = false), null,
        )
        for (key in want.keys()) assertTrue(key, same(want.get(key), got.opt(key)))
    }
}
