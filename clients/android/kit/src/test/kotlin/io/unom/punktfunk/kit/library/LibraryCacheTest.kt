package io.unom.punktfunk.kit.library

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.nio.file.Files

// The cached catalog must come back as the host sent it: a restore that drops metadata leaves the
// launch hold with a bare title until the next fetch.
class LibraryCacheTest {
    private val game = GameEntry(
        id = "rom-manager:1",
        store = "rom-manager",
        title = "Mario Kart",
        art = Artwork(portrait = "https://x/p.png", header = null, hero = null),
        platform = "SNES",
        developer = "Nintendo",
        releaseYear = 1992,
        genres = listOf("Racing"),
        stats = GameStats(lastPlayedUnixMs = 1_757_160_000_000, playTimeMs = 5_400_000, launchCount = 12),
        overrides = GameOverrides(name = "1080p60 DualSense", lines = 1080, hz = 60, hdr = false, gamepad = "dualsense"),
    )

    @Test
    fun metadataSurvivesARoundTrip() {
        val cache = LibraryCache(Files.createTempDirectory("pf-lib").toFile())
        cache.store("host", null, listOf(game))
        assertEquals(listOf(game), cache.load("host", null)?.games)
    }

    // A profile's shelf never paints the box's titles, and the box's file keeps the name it had
    // before profiles, so an old file loads as the box's.
    @Test
    fun eachProfileReadsOnlyItsOwnCatalog() {
        val dir = Files.createTempDirectory("pf-lib").toFile()
        val cache = LibraryCache(dir)
        cache.store("host", null, listOf(game))
        assertNull(cache.load("host", "kid"))
        cache.store("host", "kid", listOf(game.copy(id = "custom:kid1")))
        assertEquals(listOf("rom-manager:1"), cache.load("host", null)?.games?.map { it.id })
        assertEquals(listOf("custom:kid1"), cache.load("host", "kid")?.games?.map { it.id })
        assertEquals(dir, cache.pathFor("host", null).parentFile)

        cache.forget("host")
        assertTrue(dir.list().orEmpty().isEmpty())
    }
}
