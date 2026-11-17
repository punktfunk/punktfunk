package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.models.ActiveSession
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/**
 * The stream screen's effect wiring, on a device where the JNI core loads: composed over a ZERO
 * session handle (every native call is a no-op or a null answer on it), so what is under test is
 * the screen's own plumbing — the banner, the ring, the back handler, the end-session path and a
 * clean teardown — not the pipeline. Run: `./gradlew :app:connectedDebugAndroidTest`.
 */
@RunWith(AndroidJUnit4::class)
class StreamScreenTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val ended = mutableListOf<SessionEndReason>()

    private fun compose(settings: Settings = Settings()) {
        compose.setContent {
            StreamScreen(ActiveSession(handle = 0L, settings = settings, clipboardSync = false)) {
                ended += it
            }
        }
    }

    @Test
    fun startBannerNamesTheGesturesOnAPadlessTouchDevice() {
        compose()
        compose.onNodeWithText(
            "Back or a two-finger twist opens quick actions · three-finger tap for stats",
        ).assertIsDisplayed()
    }

    @Test
    fun backOpensTheRingAndEndStreamTakesTwoTaps() {
        compose()
        compose.waitForIdle()
        pressBack()
        compose.waitUntil(5_000) {
            compose.onAllNodesWithContentDescriptionExact("More").fetchSemanticsNodes().isNotEmpty()
        }
        compose.onNodeWithContentDescription("More").assertIsDisplayed()
        // A destructive slot arms on the first tap and fires on the second.
        compose.onNodeWithContentDescription("End stream").performClick()
        compose.waitForIdle()
        assertEquals(emptyList<SessionEndReason>(), ended)
        compose.onNodeWithText("End stream? Tap again").assertIsDisplayed()
        compose.onNodeWithContentDescription("End stream").performClick()
        compose.waitForIdle()
        assertEquals(listOf(SessionEndReason.LOCAL), ended)
    }

    @Test
    fun backClosesACommittedRing() {
        compose()
        compose.waitForIdle()
        pressBack()
        compose.waitUntil(5_000) {
            compose.onAllNodesWithContentDescriptionExact("More").fetchSemanticsNodes().isNotEmpty()
        }
        pressBack()
        compose.waitUntil(5_000) {
            compose.onAllNodesWithContentDescriptionExact("More").fetchSemanticsNodes().isEmpty()
        }
        assertEquals(emptyList<SessionEndReason>(), ended)
    }

    @Test
    fun leavingTheScreenTearsDownWithoutTouchingTheHandleTwice() {
        val shown = androidx.compose.runtime.mutableStateOf(true)
        compose.setContent {
            if (shown.value) {
                StreamScreen(ActiveSession(handle = 0L, settings = Settings(), clipboardSync = false)) {
                    ended += it
                }
            }
        }
        compose.waitForIdle()
        compose.runOnUiThread { shown.value = false }
        compose.waitForIdle()
        // Composing the screen again afterwards proves the activity survived the teardown.
        compose.runOnUiThread { shown.value = true }
        compose.waitForIdle()
        compose.onNodeWithText(
            "Back or a two-finger twist opens quick actions · three-finger tap for stats",
        ).assertIsDisplayed()
    }

    /** The system Back, through the activity's dispatcher — what the gesture and a TV remote land on. */
    private fun pressBack() {
        compose.activityRule.scenario.onActivity { it.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
    }

    private fun androidx.compose.ui.test.junit4.ComposeTestRule.onAllNodesWithContentDescriptionExact(label: String) =
        onAllNodes(androidx.compose.ui.test.hasContentDescription(label))
}
