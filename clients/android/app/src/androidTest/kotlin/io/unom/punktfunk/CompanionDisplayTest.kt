package io.unom.punktfunk

import android.hardware.display.DisplayManager
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.view.Display
import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.ViewRootForTest
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.models.ActiveSession
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Shape (b) of design/android-dual-screen.md on a simulated second screen the size of an Ayn Thor's
 * lower one: the companion panel comes up there as a Presentation with the stream, its tiles reach
 * the session, it trades screens with the picture, and it leaves with the stream. Over a ZERO
 * handle, as [StreamScreenTest].
 */
@RunWith(AndroidJUnit4::class)
class CompanionDisplayTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val ended = mutableListOf<SessionEndReason>()

    private fun shell(cmd: String) {
        val out = InstrumentationRegistry.getInstrumentation().uiAutomation.executeShellCommand(cmd)
        ParcelFileDescriptor.AutoCloseInputStream(out).use { it.readBytes() }
    }

    @Before
    fun attachASecondScreen() {
        shell("settings put global overlay_display_devices 1240x1080/420")
        val dm = compose.activity.getSystemService(DisplayManager::class.java)
        val deadline = SystemClock.uptimeMillis() + 5_000
        while (dm.displays.size < 2 && SystemClock.uptimeMillis() < deadline) SystemClock.sleep(100)
        forgetSwap()
    }

    @After
    fun detachIt() {
        forgetSwap()
        shell("settings delete global overlay_display_devices")
    }

    private fun second(): Display {
        val dm = compose.activity.getSystemService(DisplayManager::class.java)
        return requireNotNull(companionDisplay(compose.activity, dm)) { "no second screen came up" }
    }

    private fun forgetSwap() {
        val dm = compose.activity.getSystemService(DisplayManager::class.java)
        companionDisplay(compose.activity, dm)?.let { CompanionMemory.keepLayout(compose.activity, it.name, ScreenLayout.PANEL) }
    }

    /** The display whose window draws the panel's Actions tab. */
    private fun panelDisplay(): Int? = compose.onAllNodesWithText("Actions").fetchSemanticsNodes()
        .singleOrNull()?.let { (it.root as? ViewRootForTest)?.view?.display?.displayId }

    @Test
    fun thePanelComesUpOnTheSecondScreenAndLeavesWithTheStream() {
        var streaming by mutableStateOf(true)
        compose.setContent {
            if (streaming) {
                StreamScreen(ActiveSession(handle = 0L, settings = Settings(), clipboardSync = false)) { ended += it }
            }
        }
        compose.waitUntil(5_000) { compose.onAllNodesWithText("Actions").fetchSemanticsNodes().isNotEmpty() }
        compose.onNodeWithText("Actions").performClick()
        compose.onNodeWithText("End stream").performClick()
        compose.waitForIdle()
        assertEquals(emptyList<SessionEndReason>(), ended)
        compose.onNodeWithText("End stream").performClick()
        compose.waitForIdle()
        assertEquals(listOf(SessionEndReason.LOCAL), ended)

        streaming = false
        compose.waitUntil(5_000) { compose.onAllNodesWithText("Actions").fetchSemanticsNodes().isEmpty() }
    }

    @Test
    fun swappingTradesTheScreensAndThePairKeepsIt() {
        val second = second()
        val own = compose.activity.display!!.displayId
        compose.setContent {
            StreamScreen(ActiveSession(handle = 0L, settings = Settings(), clipboardSync = false)) { ended += it }
        }
        compose.waitUntil(5_000) { panelDisplay() == second.displayId }
        compose.onNodeWithText("Actions").performClick()

        compose.onNodeWithText("Screens").performClick()
        compose.waitUntil(5_000) { panelDisplay() == own }
        assertEquals(ScreenLayout.SWAPPED, CompanionMemory.layout(compose.activity, second.name))
        assertEquals(second.displayId, pictureDisplay(compose.activity, compose.activity.display!!).displayId)

        compose.onNodeWithText("Screens").performClick()
        compose.waitUntil(5_000) { panelDisplay() == second.displayId }
        assertEquals(ScreenLayout.PANEL, CompanionMemory.layout(compose.activity, second.name))
        assertEquals(emptyList<SessionEndReason>(), ended)
    }
}
