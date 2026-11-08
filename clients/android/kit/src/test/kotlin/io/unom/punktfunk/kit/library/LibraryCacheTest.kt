package io.unom.punktfunk.kit.library

import org.junit.Assert.assertEquals
import org.junit.Test
import java.nio.file.Files

// The cached catalog must come back as the host sent it: a restore that drops metadata leaves the
// launch hold with a bare title until the next fetch.
class LibraryCacheTest {
    @Test
    fun metadataSurvivesARoundTrip() {
        val cache = LibraryCache(Files.createTempDirectory("pf-lib").toFile())
        val game = GameEntry(
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
        cache.store("host", listOf(game))
        assertEquals(listOf(game), cache.load("host")?.games)
    }
}
