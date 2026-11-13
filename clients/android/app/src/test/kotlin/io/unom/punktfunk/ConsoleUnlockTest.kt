package io.unom.punktfunk

import io.unom.punktfunk.console.failedPhase
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/** How a console dial's refusal reaches the console, in the codes `clients/shared` names. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class ConsoleUnlockTest {
    private val res get() = RuntimeEnvironment.getApplication().resources

    private val phases = JSONObject(File("../../shared/console-bridge-vectors.json").readText())
        .getJSONObject("codes").getJSONArray("phase")

    private fun code(name: String) = (0 until phases.length()).first { phases.getString(it) == name }

    @Test
    fun a_locked_profile_reopens_the_prompt_with_the_hosts_own_sentence() {
        val said = "Wrong password."
        assertEquals(code("profile_locked") to said, failedPhase(PROFILE_LOCKED, said, false, res))
        // Nothing said: the prompt opens bare, never under a stand-in line.
        assertEquals(code("profile_locked") to "", failedPhase(PROFILE_LOCKED, "", false, res))
    }

    @Test
    fun other_refusals_keep_their_phases() {
        assertEquals(code("profile_gone"), failedPhase("profile-unknown", "", false, res).first)
        val (phase, message) = failedPhase("timeout", "", false, res)
        assertEquals(code("failed"), phase)
        assertEquals(res.getString(ConnectErrors.connectMessage("timeout", false)), message)
        assertEquals(code("failed") to "Not now.", failedPhase("denied", "Not now.", false, res))
    }
}
