package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.HostText
import io.unom.punktfunk.kit.ListedProfile
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.SeatGate
import io.unom.punktfunk.kit.library.InstallAction
import io.unom.punktfunk.kit.library.InstallOutcome
import io.unom.punktfunk.kit.library.MgmtReply
import io.unom.punktfunk.kit.seatGate
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * The host's sentences worded by id, replayed from `clients/shared/host-text-vectors.json` (the
 * vectors `pf_client_core::host_text` and the Swift `HostTextTests` run too).
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class HostTextTest {
    private val res get() = ApplicationProvider.getApplicationContext<Context>().resources

    /**
     * Every case words as its `sentence`. A message the build has reads in the app's language,
     * not the host's English beside it; an id it does not know, or one that is not the host's to
     * ask for, reads as that English.
     */
    @Test
    fun everyVectorWordsAsItsSentence() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/host-text-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("cases")
        assertTrue(cases.length() > 50)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val id = case.getString("id")
            val say = Say.Host(id, HostText.paramsOf(case.getJSONObject("params")), case.getString("english"))
            assertEquals(id, case.getString("sentence"), say.words(res))
        }
    }

    /** A newer host may send more values than this build's message holds, or fewer. */
    @Test
    fun valuesThatDoNotFitTheMessageReadAsTheHostsEnglish() {
        fun said(id: String, params: Map<String, String>) = Say.Host(id, params, "English.").words(res)
        assertEquals("English.", said("apierr_credential_wrong", emptyMap()))
        assertEquals("English.", said("apierr_credential_wrong", mapOf("account" to "jdoe", "extra" to "x")))
        assertEquals("English.", said("launch_closed_at_once", mapOf("title" to "Quail")))
        assertEquals("English.", said("apierr_profile_limit", emptyMap()))
    }

    @Test
    fun anApiErrorIsWordedByItsCode() {
        val coded = """{"error":"The host's English.","code":"title_in_use","params":{"title":"Quail"}}"""
        assertEquals("Quit Quail first.", HostText.apiError(coded)!!.words(res))
        assertEquals(
            "Quit Quail first.",
            HostText.apiError("""{"error":"Quit Quail first.","code":"from_a_newer_host"}""")!!.words(res),
        )
        assertEquals(Say.Text("unknown setting id"), HostText.apiError("""{"error":"unknown setting id"}"""))
        assertNull(HostText.apiError("""{"error":""}"""))
        assertNull(HostText.apiError("<html>"))
        // The refusal of an install reads as the message, and a host without a code as its English.
        val refused = MgmtReply.Answer.of(409, coded)
        assertEquals("Quit Quail first.", InstallOutcome.fromReply(409, refused.apiError).notice(InstallAction.Remove, "Quail").words(res))
    }

    /** A seat line the host names reads as its message; the supervisor's own text stays as sent. */
    @Test
    fun aSeatLineIsWordedByItsId() {
        fun gate(extra: JSONObject.() -> Unit) = seatGate(
            ListedProfile.parse(
                JSONObject().put("id", "kid").put(
                    "seat",
                    JSONObject().put("state", "starting").put("detail", "The host's English.").apply(extra),
                ),
            ),
        ) as SeatGate.Wait
        val worded = gate {
            put("detail_id", "seatline_stage_sign_in_try")
            put("detail_params", JSONObject().put("attempt", "2").put("total", "3"))
        }
        assertEquals("Signing in to the seat's Windows account (try 2 of 3)…", worded.detail!!.words(res))
        assertEquals("The host's English.", gate { put("detail_id", "seatline_from_a_newer_host") }.detail!!.words(res))
        assertEquals(Say.Text("The host's English."), gate {}.detail)
    }

    /** A launch the host did not start, as `nativeLaunchRefusal` hands it over, reads as its message. */
    @Test
    fun aLaunchRefusalIsWordedByItsId() {
        fun launch(id: String, params: String) =
            HostText.launch("""{"id":"$id","message":"The host's English.","params":$params}""")!!.words(res)
        assertEquals(
            "Couldn't start that game — it isn't in this host's library anymore.",
            launch("launch_title_not_in_library", "{}"),
        )
        assertEquals(
            "Couldn't start Quail — it closed again after 4 s.",
            launch("launch_closed_at_once", """{"seconds":"4","title":"Quail"}"""),
        )
        assertEquals("The host's English.", launch("launch_from_a_newer_host", "{}"))
        assertNull(HostText.launch(""))
    }

    /** An action's reason is worded by its code; the system's own words arrive without one. */
    @Test
    fun anActionReasonIsWordedByItsCode() {
        fun action(code: String, id: String = "power.sleep") = HostActions.Action(
            id, "x", danger = false, available = false,
            unavailableReason = "The host's English.", unavailableCode = code,
        )
        assertEquals("This host doesn't support it", action("host_power_not_supported_here").reason(res))
        assertEquals("The host's English.", action("from_a_newer_host").reason(res))
        assertEquals("The host's English.", action("").reason(res))
        assertEquals("Restart Punktfunk", action("", "host.restart").text(res))
        assertEquals("Next monitor", action("", "display.next").text(res))
    }

    /** Every close token the core names reads as `reject_<token>`. */
    @Test
    fun everyCloseTokenHasItsMessage() {
        val tokens = listOf(
            "not-armed", "bound-other", "rate-limited", "identity-required", "denied", "approval-timeout",
            "superseded", "wire-version", "busy", "setup-failed", "access-expired", "launch-not-permitted",
            "host-power", "profile-unknown", "no-seat", "seat-occupied", "seat-unavailable", "profile-locked",
        )
        for (token in tokens) {
            val id = ConnectErrors.connectMessage(token, requestAccess = false)
            assertEquals(token, "reject_" + token.replace('-', '_'), res.getResourceEntryName(id))
        }
        assertEquals(R.string.reject_busy, ConnectErrors.connectMessage("busy", requestAccess = false))
    }
}
