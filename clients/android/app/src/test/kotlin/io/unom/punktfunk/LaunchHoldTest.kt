package io.unom.punktfunk

import io.unom.punktfunk.kit.HostText
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.Say
import io.unom.punktfunk.kit.say
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [launchGaveUp] — which of the hold's exits gets a message (#1072).
 * Run: `./gradlew -PexcludeScreenshots :app:testDebugUnitTest`.
 */
class LaunchHoldTest {
    @Test
    fun aLaunchStillComingUpIsNeverGivenUpOn() {
        // The host lists nothing yet: a launch resolving is worth 15 seconds.
        assertNull(launchGaveUp("Quail", null, 0.0))
        assertNull(launchGaveUp("Quail", null, 14.9))
        // A cold Steam boot with shader work is `launching` for minutes.
        assertNull(launchGaveUp("Quail", "launching", 119.0))
        // Launches that worked: the caller shows the stream, with nothing to say.
        assertNull(launchGaveUp("Quail", "running", 300.0))
        assertNull(launchGaveUp("Quail", "untracked", 300.0))
        assertNull(launchGaveUp("Quail", "grace", 300.0))
        // A word the host adds later is not a failure either.
        assertNull(launchGaveUp("Quail", "window", 300.0))
    }

    @Test
    fun aRunningGameHoldsOnlyWhileItsWindowIsStillToCome() {
        assertTrue(launchStillHeld("running", awaitingWindow = true, elapsed = 60.0))
        assertFalse(launchStillHeld("running", awaitingWindow = false, elapsed = 1.0))
        assertFalse(launchStillHeld("window", awaitingWindow = false, elapsed = 1.0))
        // No window by the cap: show the stream rather than hold forever.
        assertFalse(launchStillHeld("running", awaitingWindow = true, elapsed = 120.0))
    }

    @Test
    fun eachGivingUpExitSaysWhatDidNotHappen() {
        assertEquals(say(R.string.lib_hold_gave_up_not_started, "Quail"), launchGaveUp("Quail", null, 15.0))
        assertEquals(say(R.string.lib_hold_gave_up_slow, "Quail"), launchGaveUp("Quail", "launching", 120.0))
        // Straight away: the host un-launches the record the moment it sees this.
        assertEquals(say(R.string.lib_hold_gave_up_exited, "Quail"), launchGaveUp("Quail", "exited", 0.4))
    }

    @Test
    fun theHostsRefusalEndsTheHoldAtOnce() {
        val english = "Couldn't start that game — it isn't in this host's library anymore."
        val refused = HostText.launch("""{"id":"launch_title_not_in_library","message":"$english","params":{}}""")
        assertEquals(Say.Host("launch_title_not_in_library", emptyMap(), english), refused)
        // The first poll says it, whatever /status shows: the hold stops waiting there.
        assertEquals(refused, launchGaveUp("Quail", null, 0.0, refused))
        assertEquals(refused, launchGaveUp("Quail", "launching", 0.0, refused))
        // A host that says nothing still gets the hold's own line, at 15 seconds.
        assertNull(HostText.launch(""))
        assertNull(launchGaveUp("Quail", null, 14.9, null))
        assertEquals(say(R.string.lib_hold_gave_up_not_started, "Quail"), launchGaveUp("Quail", null, 15.0, null))
    }
}
