package io.unom.punktfunk.kit.library

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

// A launch's download as its hold reads it; the words match the Rust console shell's.
class DownloadTest {
    @Test
    fun statusCarriesDownloadsBesideTheGames() {
        val s = LibraryClient.parseStatus(
            """{"games":[{"app_id":"custom:a","title":"Quail","state":"launching"}],
              "downloads":[{"app_id":"custom:a","title":"Quail","state":"downloading",
              "done_bytes":12300000000,"total_bytes":26000000000,"rate_bps":48000000,"eta_s":240,
              "started_at":"x","updated_at":"y"}]}""",
        )
        assertEquals("launching", s.games[0].state)
        val d = s.downloads[0]
        assertTrue(d.live)
        assertEquals("12.3 GB of 26 GB · 48 MB/s · about 4 min left", d.line())
        assertEquals(47, Math.round((d.fraction ?: 0f) * 100))
        assertNull(d.stopped("Quail"))
        assertTrue(LibraryClient.parseStatus("""{"games":[]}""").downloads.isEmpty())
    }

    @Test
    fun aStoppedDownloadSaysWhyTheGameDidNotStart() {
        val failed = Download("a", "failed", error = "the server is down")
        assertFalse(failed.live)
        assertEquals("Quail didn't download — the server is down", failed.stopped("Quail"))
        assertEquals(
            "Quail's download was paused. Start it again to resume.",
            Download("a", "paused").stopped("Quail"),
        )
        assertEquals("Verifying", Download("a", "installing", phase = "Verifying").line())
        assertEquals("3.1 GB so far", Download("a", "downloading", doneBytes = 3_100_000_000).line())
        assertEquals("512 kB", humanBytes(512_000))
    }
}
