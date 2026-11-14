package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.components.HostLine
import io.unom.punktfunk.console.SkiaConsole
import io.unom.punktfunk.kit.library.GameStats
import io.unom.punktfunk.kit.library.LibraryGroupBy
import io.unom.punktfunk.kit.library.LibrarySection
import io.unom.punktfunk.kit.library.LibrarySortKey
import io.unom.punktfunk.kit.security.KnownHostStore
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The touch library's layout lives in the console's settings document, in the console's keys. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric 4.16 has no SDK 37 image yet; the app targets 37
class LibraryLayoutTest {
    private val context: Context get() = ApplicationProvider.getApplicationContext()
    private val prefs get() = context.getSharedPreferences(SkiaConsole.PREFS, Context.MODE_PRIVATE)
    private fun doc() = JSONObject(prefs.getString("json", null)!!)

    @Test
    fun favorites_are_filed_per_host_and_an_empty_list_leaves_no_key() {
        val s = Settings()
        assertEquals(setOf("steam:1"), LibraryLayout.toggleFavorite(context, s, "aa", "steam:1"))
        LibraryLayout.toggleFavorite(context, s, "aa", "steam:2")
        assertTrue(LibraryLayout.favorites(context, "bb").isEmpty())
        assertEquals("[\"steam:1\",\"steam:2\"]", doc().getJSONArray("favorites.aa").toString())
        LibraryLayout.toggleFavorite(context, s, "aa", "steam:1")
        LibraryLayout.toggleFavorite(context, s, "aa", "steam:2")
        assertFalse(doc().has("favorites.aa"))
    }

    @Test
    fun a_saved_layout_keeps_the_console_keys_it_does_not_own() {
        prefs.edit().putString("json", """{"library_view":"grid","library_sections":"games,-recent"}""").apply()
        val loaded = LibraryLayout.load(context)
        assertEquals(LibrarySection.GAMES to true, loaded.sections[0])
        assertEquals(LibrarySection.RECENT to false, loaded.sections[1])
        LibraryLayout.save(context, Settings(), loaded.copy(sort = LibrarySortKey.RECENT, groupBy = LibraryGroupBy.STORE))
        assertEquals("grid", doc().getString("library_view"))
        assertEquals("recent", doc().getString("library_sort"))
        assertEquals(LibraryGroupBy.STORE, LibraryLayout.load(context).groupBy)
    }

    @Test
    fun a_forgotten_host_leaves_no_favorites() {
        val store = KnownHostStore(context)
        val kh = store.trust("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true)
        LibraryLayout.toggleFavorite(context, Settings(), kh.fpHex, "steam:1")
        HostRecords.forget(context, store, kh, Settings())
        assertTrue(LibraryLayout.favorites(context, kh.fpHex).isEmpty())
    }

    @Test
    fun the_play_line_reads_like_a_person_says_it() {
        val res = context.resources
        val hour = 3_600_000L
        val stats = GameStats(lastPlayedUnixMs = 1_000, playTimeMs = 14 * hour, lastRunMs = 0, launchCount = 12)
        assertEquals(
            "Last played 2 h ago · 14 h total · 12 launches",
            PlayStatsText.line(res, stats, nowMs = 1_000 + 2 * hour),
        )
        assertEquals("3 d ago", PlayStatsText.lastPlayed(res, stats, nowMs = 1_000 + 72 * hour))
        assertEquals("14 h", PlayStatsText.playTime(res, stats))
        assertNull(PlayStatsText.line(res, stats.copy(lastPlayedUnixMs = 0), nowMs = 5))
    }

    @Test
    fun a_host_reads_as_one_sentence() {
        fun line(pinned: Boolean = true, online: Boolean = true, playing: String? = null, wakes: Boolean = false) =
            HostLine.of(pinned, online, connecting = false, playing, wakes)
        assertEquals(HostLine.Playing("Halo"), line(playing = "Halo"))
        assertEquals(HostLine.Online, line())
        // An unpaired host that answers asks for a pairing before anything else.
        assertEquals(HostLine.NotPaired, line(pinned = false, playing = "Halo"))
        assertEquals(HostLine.OfflineWakes, line(online = false, wakes = true))
        assertEquals(HostLine.Offline, line(online = false, playing = "Halo"))
    }
}
