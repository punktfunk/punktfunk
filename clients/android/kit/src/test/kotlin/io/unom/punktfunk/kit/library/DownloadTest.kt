package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import java.util.Locale
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

// A launch's download as its hold reads it; the words match the Rust console shell's.
class DownloadTest {
    private val was = Locale.getDefault()

    // A figure is written for the default locale: pin one with a "." decimal point.
    @Before
    fun english() = Locale.setDefault(Locale.ENGLISH)

    @After
    fun restore() = Locale.setDefault(was)

    private fun gb(figure: String) = say(R.string.common_size_gb, figure)
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
        assertEquals(
            listOf(
                say(R.string.lib_download_progress, gb("12.3"), gb("26")),
                say(R.string.lib_download_rate, say(R.string.common_size_mb, "48")),
                say(R.string.lib_download_eta_minutes, 4L),
            ),
            d.line(),
        )
        assertEquals(47, Math.round((d.fraction ?: 0f) * 100))
        assertNull(d.stopped("Quail"))
        assertTrue(LibraryClient.parseStatus("""{"games":[]}""").downloads.isEmpty())
    }

    @Test
    fun aStoppedDownloadSaysWhyTheGameDidNotStart() {
        val failed = Download("a", "failed", error = "the server is down")
        assertFalse(failed.live)
        assertEquals(
            say(R.string.lib_download_failed_why, "Quail", "the server is down"),
            failed.stopped("Quail"),
        )
        assertEquals(say(R.string.lib_download_paused, "Quail"), Download("a", "paused").stopped("Quail"))
        assertEquals(
            listOf(Say.Text("Verifying")),
            Download("a", "installing", phase = "Verifying").line(),
        )
        assertEquals(
            listOf(say(R.string.lib_download_so_far, gb("3.1"))),
            Download("a", "downloading", doneBytes = 3_100_000_000).line(),
        )
        assertEquals(say(R.string.common_size_kb, "512"), humanBytes(512_000))
        // A comma-decimal language reads the figure its own way.
        assertEquals(say(R.string.common_size_gb, "12,3"), humanBytes(12_300_000_000, Locale.GERMANY))
    }

    /** The Rust client's rules: one row per title, only what the grants allow. */
    @Test
    fun aTitlesMenuOffersTheOneActionItsFilesAllow() {
        val missing = TitleInstall("missing", sizeBytes = 26_000_000_000, freeBytes = 212_000_000_000)
        val installed = missing.copy(state = "installed")
        val all = 0xFF
        val pick = InstallAction::forTitle
        assertNull(pick(null, null, all))
        assertEquals(InstallAction.Install, pick(missing, null, all))
        assertEquals(InstallAction.Pause, pick(missing, Download("a", "downloading"), all))
        assertNull(pick(missing, Download("a", "installing"), all))
        assertEquals(InstallAction.Resume, pick(missing, Download("a", "paused"), all))
        assertEquals(InstallAction.Remove, pick(installed, null, all))
        assertEquals(InstallAction.Install, pick(missing, null, SessionAccessLaunch))
        assertNull(pick(installed, null, SessionAccessLaunch))
        assertNull(pick(installed, null, null))
        assertEquals(
            say(R.string.lib_install_size_free, gb("26"), gb("212")),
            InstallAction.Install.label(missing),
        )
        assertEquals(say(R.string.lib_download_remove_size, gb("26")), InstallAction.Remove.label(installed))

        assertEquals(TileBadge("download", gb("26")), TileBadge.forTitle(missing, null))
        assertNull(TileBadge.forTitle(installed, null))
        assertEquals(
            TileBadge("pause", say(R.string.lib_badge_percent, 42)),
            TileBadge.forTitle(missing, Download("a", "paused", doneBytes = 429, totalBytes = 1000)),
        )
        assertEquals(
            Say.Text("Quit Quail first."),
            InstallOutcome.fromReply(409, "Quit Quail first.").notice(InstallAction.Remove, "Quail"),
        )
        assertEquals(
            say(R.string.lib_install_unsupported),
            InstallOutcome.fromReply(404, null).notice(InstallAction.Pause, "Quail"),
        )
        // A failure names its action: a resume that fails is not an install that failed.
        val failure = InstallOutcome.fromReply(500, null)
        assertEquals(
            say(R.string.lib_resume_failed, "Quail", say(R.string.error_mgmt_refused, 500)),
            failure.notice(InstallAction.Resume, "Quail"),
        )
        assertEquals(
            0xFF,
            LibraryClient.parseStatus("""{"games":[],"grants":255}""").grants,
        )
    }

    private companion object {
        const val SessionAccessLaunch = io.unom.punktfunk.kit.SessionAccess.LAUNCH
    }
}
