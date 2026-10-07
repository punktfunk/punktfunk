package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * The preset model — the part of this feature that is wrong-or-right rather than pretty-or-ugly.
 * A preset is a named bundle of OVERRIDES, not a snapshot: an untouched field keeps following the
 * global live, a touched one is recorded even when it equals today's global (a pin), and the only
 * way back to inheriting is an explicit reset. These tests are the Kotlin twin of the Rust
 * `presets.rs` suite, so the two can't drift.
 *
 * `sdk = [36]` for the same reason the screenshot tests pin it: Robolectric ships android-all jars
 * only up to API 36 while the app compiles against 37.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class PresetsTest {
    private val base = Settings(
        width = 1920,
        height = 1080,
        bitrateKbps = 20_000,
        codec = "hevc",
        touchMode = TouchMode.TRACKPAD,
        mouseMode = MouseMode.DESKTOP,
    )

    @Test
    fun overlayAppliesOnlyWhatItOverrides() {
        val empty = SettingsOverlay()
        assertTrue(empty.isEmpty())
        assertEquals(base, empty.apply(base))

        val overlay = SettingsOverlay(
            width = 3840,
            height = 2160,
            hz = 120,
            bitrateKbps = 80_000,
            renderScale = 1.5,
            codec = "av1",
            hdrEnabled = false,
            compositor = 4,
            audioChannels = 6,
            audioFormat = AUDIO_FORMAT_LOSSLESS_96,
            micEnabled = true,
            touchMode = TouchMode.POINTER,
            mouseMode = MouseMode.CAPTURE,
            invertScroll = true,
            gamepad = 6,
            statsVerbosity = StatsVerbosity.DETAILED,
            lowLatencyMode = false,
        )
        assertFalse(overlay.isEmpty())
        val out = overlay.apply(base)
        assertEquals(Triple(3840, 2160, 120), Triple(out.width, out.height, out.hz))
        assertEquals(80_000, out.bitrateKbps)
        assertEquals(1.5, out.renderScale, 0.0)
        assertEquals("av1", out.codec)
        assertFalse(out.hdrEnabled)
        assertEquals(4, out.compositor)
        assertEquals(6, out.audioChannels)
        assertEquals(AUDIO_FORMAT_LOSSLESS_96, out.audioFormat)
        assertTrue(out.micEnabled)
        assertEquals(TouchMode.POINTER, out.touchMode)
        assertEquals(MouseMode.CAPTURE, out.mouseMode)
        assertTrue(out.invertScroll)
        assertEquals(6, out.gamepad)
        assertEquals(StatsVerbosity.DETAILED, out.statsVerbosity)
        assertFalse(out.lowLatencyMode)

        // Device-scope settings are not in the overlay at all, so no preset can move them.
        assertEquals(base.gamepadUiEnabled, out.gamepadUiEnabled)
        assertEquals(base.gamepadUiMode, out.gamepadUiMode)
        assertEquals(base.autoWakeEnabled, out.autoWakeEnabled)
    }

    @Test
    fun anOverrideEqualToTheGlobalIsAPinThatSurvivesTheGlobalMoving() {
        val pin = SettingsOverlay(bitrateKbps = 20_000) // exactly what `base` says today
        assertFalse(pin.isEmpty())
        assertEquals(20_000, pin.apply(base.copy(bitrateKbps = 50_000)).bitrateKbps)
    }

    @Test
    fun absorbRecordsTheTouchedFieldOnly() {
        var o = SettingsOverlay()

        // One control fires: before = what it was showing, after = what the user picked.
        var before = o.apply(base)
        o = o.absorb(before, before.copy(codec = "av1"))
        assertEquals("av1", o.codec)
        assertNull("nothing else may be recorded", o.bitrateKbps)

        // Setting it BACK to the global's value is still an override — the pin case, and the whole
        // difference between this and diffing against the globals at save time.
        before = o.apply(base)
        o = o.absorb(before, before.copy(codec = "hevc"))
        assertEquals("hevc", o.codec)
        assertEquals("hevc", o.apply(base.copy(codec = "h264")).codec)

        // Identical snapshots record nothing.
        before = o.apply(base)
        assertEquals(o, o.absorb(before, before))
    }

    @Test
    fun clearDropsOneOverride() {
        val o = SettingsOverlay(width = 3840, height = 2160, codec = "av1")
        assertEquals(setOf(SettingsOverlay.FIELD_RESOLUTION, "codec"), o.overridden())
        assertNull(o.clear("codec").codec)
        // Width and height are one control, so they reset together.
        val reset = o.clear(SettingsOverlay.FIELD_RESOLUTION)
        assertNull(reset.width)
        assertNull(reset.height)
        assertEquals(o, o.clear("no_such_field")) // unknown names are a no-op, never a crash
    }

    @Test
    fun catalogRoundTripsAndPreservesWhatItCannotRepresent() {
        val store = PresetStore(RuntimeEnvironment.getApplication())
        val game = newPreset("Game").copy(
            accent = "#ff8800",
            overrides = SettingsOverlay(
                width = 3840,
                height = 2160,
                hz = 120,
                // A codec string this build's picker can't show is still stored and still applied:
                // the host is the component that decides what it can encode.
                codec = "vvc-from-the-future",
                extra = mapOf("some_new_axis" to 7),
            ),
            extra = mapOf("future_preset_key" to "kept"),
        )
        store.save(game)
        store.save(newPreset("Work"))

        val loaded = store.byId(game.id)!!
        assertEquals("Game", loaded.name)
        assertEquals("#ff8800", loaded.accent)
        assertEquals("vvc-from-the-future", loaded.overrides.codec)
        assertEquals(3840, loaded.overrides.width)
        // The don't-clobber rule: an older build must not erase a newer one's keys by opening it.
        assertEquals(mapOf<String, Any>("some_new_axis" to 7), loaded.overrides.extra)
        assertEquals(mapOf<String, Any>("future_preset_key" to "kept"), loaded.extra)
        assertEquals("vvc-from-the-future", loaded.overrides.apply(base).codec)

        // A preset that overrides nothing is the "inherits everything" one a create starts at.
        assertTrue(store.all().first { it.name == "Work" }.overrides.isEmpty())
        assertEquals(listOf("Game", "Work"), store.all().map { it.name })
    }

    /**
     * A catalog from before the rename moves over once. Deleting every preset afterwards does not
     * bring the old ones back, and the old file stays as it was for an older build of this app.
     */
    @Test
    fun aPreRenameCatalogMovesOverOnce() {
        val app = RuntimeEnvironment.getApplication()
        val legacy = app.getSharedPreferences("punktfunk_profiles", Context.MODE_PRIVATE)
        val game = """{"id":"a1b2c3d4e5f6","name":"Game"}"""
        legacy.edit().putString("a1b2c3d4e5f6", game).commit()
        val store = PresetStore(app)
        assertEquals(listOf("Game"), store.all().map { it.name })
        store.delete("a1b2c3d4e5f6")
        assertTrue(PresetStore(app).all().isEmpty())
        assertEquals(setOf("a1b2c3d4e5f6"), legacy.all.keys)
    }

    /**
     * Every field this build models must be in the store's KNOWN key set, or the load files it
     * under "keys a newer build wrote" as WELL as into its own field — and `clear` (which only
     * nulls the field) then writes it straight back out of that carry-through map, so resetting
     * the row to inherited silently never sticks. `ten_bit_sdr` shipped exactly that way.
     *
     * Stated as behaviour rather than by reading KNOWN: set every field, round-trip, and nothing
     * may have been mistaken for a stranger.
     */
    @Test
    fun everyModelledOverrideSurvivesAResetToInherited() {
        val store = PresetStore(RuntimeEnvironment.getApplication())
        val all = SettingsOverlay(
            width = 3840, height = 2160, hz = 120, bitrateKbps = 80_000, renderScale = 1.5,
            codec = "av1", hdrEnabled = false, tenBitSdr = true, compositor = 4,
            audioChannels = 6, audioFormat = AUDIO_FORMAT_LOSSLESS_96, micEnabled = true,
            echoCancel = false, keepHostAudio = true, touchMode = TouchMode.POINTER,
            mouseMode = MouseMode.CAPTURE, invertScroll = true, overlayActions = "ring-blob",
            gamepad = 6, gamepadForwarding = false, systemButtons = "local", guideGesture = "on",
            statsVerbosity = StatsVerbosity.DETAILED, lowLatencyMode = false,
            presentPriority = "smooth", smoothBuffer = 2,
        )
        val p = newPreset("Everything").copy(overrides = all)
        store.save(p)
        val loaded = store.byId(p.id)!!.overrides
        assertEquals("a modelled key was carried through as an unknown one", emptyMap<String, Any>(), loaded.extra)
        assertEquals(all.overridden(), loaded.overridden())

        // …and a reset must still be gone after the store has been through JSON again.
        for (field in all.overridden()) {
            store.save(p.copy(overrides = loaded.clear(field)))
            assertFalse("$field came back after a reset", field in store.byId(p.id)!!.overrides.overridden())
        }
    }

    @Test
    fun resolvePrefersIdsAndRefusesAmbiguity() {
        val store = PresetStore(RuntimeEnvironment.getApplication())
        val work = newPreset("Work")
        val work2 = newPreset("work") // saved directly: the UI's name guard is what prevents this
        val game = newPreset("Game")
        listOf(work, work2, game).forEach(store::save)

        assertEquals(PresetResolution.FOUND, store.resolve(work.id).second)
        assertEquals(work.id, store.resolve(work.id).first!!.id)
        // Two presets carry this name — refuse rather than pick whichever came first.
        assertEquals(PresetResolution.AMBIGUOUS, store.resolve("Work").second)
        assertNull(store.resolve("Work").first)
        assertEquals(game.id, store.resolve("GAME").first!!.id) // names match case-insensitively
        assertEquals(PresetResolution.NOT_FOUND, store.resolve("nope").second)
        assertEquals(PresetResolution.NOT_FOUND, store.resolve("").second)

        assertTrue(store.nameTaken("GAME"))
        assertFalse(store.nameTaken("GAME", except = game.id)) // renaming in place is allowed
        assertFalse(store.nameTaken("Travel"))
    }

    @Test
    fun presetPrecedenceIsOneOffThenBindingThenNone() {
        val store = PresetStore(RuntimeEnvironment.getApplication())
        val work = newPreset("Work")
        val game = newPreset("Game")
        listOf(work, game).forEach(store::save)
        val bound = host().copy(presetId = work.id)

        // A plain tap follows the binding…
        assertEquals(work.id, store.resolveFor(bound, oneOff = null)!!.id)
        // …a one-off wins over it, by id or by unique name, and never rebinds anything…
        assertEquals(game.id, store.resolveFor(bound, oneOff = game.id)!!.id)
        assertEquals(game.id, store.resolveFor(bound, oneOff = "game")!!.id)
        assertEquals(work.id, store.resolveFor(bound, oneOff = null)!!.id)
        // …and the empty reference is a real choice — "force the global defaults" — not "unset".
        assertNull(store.resolveFor(bound, oneOff = ""))
        // An unbound host is today's behaviour: the globals.
        assertNull(store.resolveFor(host(), oneOff = null))
        assertNull(store.resolveFor(null, oneOff = null))
    }

    /** A title's own binding sits between the one-off and the host's default. */
    @Test
    fun aTitleBindingOutranksTheHostAndYieldsToAOneOff() {
        val store = PresetStore(RuntimeEnvironment.getApplication())
        val work = newPreset("Work")
        val game = newPreset("Game")
        listOf(work, game).forEach(store::save)
        val h = host().copy(presetId = work.id, gamePresets = mapOf("halo" to game.id))

        assertEquals(game.id, store.resolveFor(h, oneOff = null, launch = "halo")!!.id)
        // A title with no entry inherits the host's default; the desktop does too.
        assertEquals(work.id, store.resolveFor(h, oneOff = null, launch = "doom")!!.id)
        assertEquals(work.id, store.resolveFor(h, oneOff = null)!!.id)
        // A one-off still wins, and "" still forces the globals.
        assertEquals(work.id, store.resolveFor(h, oneOff = work.id, launch = "halo")!!.id)
        assertNull(store.resolveFor(h, oneOff = "", launch = "halo"))
        // Deleted title preset: the host's default stands, not the globals.
        store.delete(game.id)
        assertEquals(work.id, store.resolveFor(h, oneOff = null, launch = "halo")!!.id)
    }

    @Test
    fun aDeletedPresetLeavesNoErrorBehind() {
        val store = PresetStore(RuntimeEnvironment.getApplication())
        val work = newPreset("Work")
        store.save(work)
        val h = host().copy(presetId = work.id, pinnedPresetIds = listOf(work.id, work.id))
        assertEquals(1, store.pinsFor(h).size) // a duplicate pin is one card, not two

        store.delete(work.id)
        // A dangling binding resolves as "no preset" — never an error, never a blocked connect —
        // and its pinned card simply stops rendering.
        assertNull(store.resolveFor(h, oneOff = null))
        assertTrue(store.pinsFor(h).isEmpty())
        assertEquals(base, base.effectiveFor(store.resolveFor(h, oneOff = null)))
    }

    /**
     * A preset created from the UI gets a colour, and a distinct one — the accent is the WHOLE
     * signal on a bound host card's chip and a pinned card's tint, so two presets sharing it (or
     * having none) makes those surfaces say less than they look like they're saying.
     */
    @Test
    fun creationHandsOutADistinctColour() {
        val made = mutableListOf<StreamPreset>()
        repeat(PRESET_ACCENTS.size) { made += newPreset("p$it", nextAccent(made)) }
        assertEquals(PRESET_ACCENTS, made.map { it.accent })
        // Past the palette it wraps rather than handing out nothing — a duplicate colour beats an
        // invisible chip, and the picker is right there.
        assertEquals(PRESET_ACCENTS.first(), nextAccent(made))
        // A gap is reused before wrapping.
        assertEquals(PRESET_ACCENTS[2], nextAccent(made.filter { it.accent != PRESET_ACCENTS[2] }))
        // The colour is presentation, so it never reaches the resolved settings.
        assertEquals(base, made.first().overrides.apply(base))
    }

    /**
     * The audio-format setting is a STRING, and the two numbers it turns into are what the `Hello`
     * carries — get the mapping wrong and the session either spends 8.5 Mbps it was not asked for
     * or silently declines to ask for what it was. The Opus row is the load-bearing one: it must
     * be the `0`/`0` "did not ask" sentinel, because core sets `CLIENT_CAP_AUDIO_HIRES` on ANY
     * non-zero field — see [theOpusSettingDoesNotAdvertiseTheLosslessCapability].
     */
    @Test
    fun theAudioFormatSettingMapsToTheWireFieldsItClaims() {
        assertEquals(
            AUDIO_FORMAT_WIRE_UNSPECIFIED,
            base.copy(audioFormat = AUDIO_FORMAT_OPUS).audioFormatWire(),
        )
        // Both rate families. The 44.1 one was deferred only for as long as the shared jitter
        // policy divided by 1 000 before it multiplied (44 100 → 44 samples/ms, every buffer
        // figure 2.3 % out); core multiplies first now, so these are simply rates.
        assertEquals(
            44_100 to 24,
            base.copy(audioFormat = AUDIO_FORMAT_LOSSLESS_441).audioFormatWire(),
        )
        assertEquals(
            48_000 to 24,
            base.copy(audioFormat = AUDIO_FORMAT_LOSSLESS_48).audioFormatWire(),
        )
        assertEquals(
            88_200 to 24,
            base.copy(audioFormat = AUDIO_FORMAT_LOSSLESS_882).audioFormatWire(),
        )
        assertEquals(
            96_000 to 24,
            base.copy(audioFormat = AUDIO_FORMAT_LOSSLESS_96).audioFormatWire(),
        )
        assertEquals(
            176_400 to 24,
            base.copy(audioFormat = AUDIO_FORMAT_LOSSLESS_1764).audioFormatWire(),
        )
        // The default is the legacy request — a fresh install asks for exactly what it always did.
        assertEquals(AUDIO_FORMAT_WIRE_UNSPECIFIED, Settings().audioFormatWire())
        // A newer build's value (or a corrupted pref) falls back to Opus rather than reaching the
        // host as an unrepresentable rate: a settings string must never be able to block a connect.
        assertEquals(
            AUDIO_FORMAT_WIRE_UNSPECIFIED,
            base.copy(audioFormat = "lossless192").audioFormatWire(),
        )
    }

    /**
     * ⚠⚠ **A user who chose Standard (Opus) must not advertise `CLIENT_CAP_AUDIO_HIRES`**, and the
     * only thing standing between them and 1.5 Mbps of PCM they did not ask for is that this pair
     * is `0`/`0`.
     *
     * Core's `advertised_client_caps` sets the bit when EITHER field is non-zero — it keys on "a
     * format was specified", not on "the format differs from the default", because 48 kHz/16-bit is
     * both the legacy pair AND the cheapest lossless rung and the other rule would make that rung
     * unrequestable. The host's gate then accepts 48 kHz/16-bit as a perfectly supported format. So
     * a client that sends the legacy-looking numbers as its stand-in for "default" opts every one of
     * its users in, on every host that has not deliberately opted out — which since 2026-08-17 is
     * every host, `PUNKTFUNK_AUDIO_HIRES` having gone default-ON — with no surface anywhere saying
     * so: a declined session and a silently granted one look identical from the settings screen.
     *
     * This client did exactly that until the four clients were compared. The rule is restated here
     * rather than reached through core because Kotlin cannot call it; core's own tests pin the other
     * half.
     */
    @Test
    fun theOpusSettingDoesNotAdvertiseTheLosslessCapability() {
        // Core's rule, verbatim: `audio_rate_hz != 0 || audio_bits != 0`.
        fun asksForHiRes(wire: Pair<Int, Int>) = wire.first != 0 || wire.second != 0

        assertFalse(asksForHiRes(base.copy(audioFormat = AUDIO_FORMAT_OPUS).audioFormatWire()))
        assertFalse(asksForHiRes(Settings().audioFormatWire()))
        assertFalse(asksForHiRes(base.copy(audioFormat = "lossless192").audioFormatWire()))
        // …and every row that IS a lossless choice must ask, or the setting does nothing at all.
        // That asymmetry is the whole contract.
        for ((value, _) in AUDIO_FORMAT_OPTIONS.drop(1)) {
            assertTrue(value, asksForHiRes(base.copy(audioFormat = value).audioFormatWire()))
        }
    }

    /**
     * The stored values are a CROSS-CLIENT contract, shared verbatim with the Apple client's
     * `AudioFormatChoice` raw values and the desktop `AUDIO_FORMATS`. A preset carries the key
     * through untouched, so a rename here does not break a round trip loudly — it breaks it
     * silently, by leaving the other client to fall back to its own global default on a preset
     * that looks like it applied. Spelled out as literals rather than referenced through the
     * constants, because a test that reads the constant cannot detect the constant changing.
     *
     * The naming rule for anything added later is the kHz figure with the decimal point dropped.
     */
    @Test
    fun theStoredAudioFormatValuesAreTheOnesEveryOtherClientStores() {
        assertEquals("opus", AUDIO_FORMAT_OPUS)
        assertEquals("lossless441", AUDIO_FORMAT_LOSSLESS_441)
        assertEquals("lossless48", AUDIO_FORMAT_LOSSLESS_48)
        assertEquals("lossless882", AUDIO_FORMAT_LOSSLESS_882)
        assertEquals("lossless96", AUDIO_FORMAT_LOSSLESS_96)
        assertEquals("lossless1764", AUDIO_FORMAT_LOSSLESS_1764)
        // Opus first (the default), then the lossless rows by ascending rate.
        assertEquals(
            listOf(
                AUDIO_FORMAT_OPUS,
                AUDIO_FORMAT_LOSSLESS_441,
                AUDIO_FORMAT_LOSSLESS_48,
                AUDIO_FORMAT_LOSSLESS_882,
                AUDIO_FORMAT_LOSSLESS_96,
                AUDIO_FORMAT_LOSSLESS_1764,
            ),
            AUDIO_FORMAT_OPTIONS.map { it.first },
        )
        // Every offered row resolves to a DISTINCT request — a duplicate would be a menu entry the
        // wire cannot tell from its neighbour — every lossless one is 24-bit, and exactly one row
        // (Opus, the first) is the "did not ask" sentinel.
        val wire = AUDIO_FORMAT_OPTIONS.map { base.copy(audioFormat = it.first).audioFormatWire() }
        assertEquals(wire.size, wire.toSet().size)
        assertTrue(wire.drop(1).all { it.second == 24 })
        assertEquals(1, wire.count { it == AUDIO_FORMAT_WIRE_UNSPECIFIED })
        assertEquals(AUDIO_FORMAT_WIRE_UNSPECIFIED, wire.first())
    }

    @Test
    fun mintedIdsAreWellFormed() {
        val id = newPresetId()
        assertEquals(12, id.length)
        assertTrue(id.all { it.isDigit() || it in 'a'..'f' })
        assertNotEquals(id, newPresetId())
    }

    private fun host() = KnownHost("192.168.1.42", 9777, "Desk", "a".repeat(64), paired = true)
}
