package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.hasScrollAction
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollToNode
import io.unom.punktfunk.kit.Gamepad
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * A full-control session on a two-screen device with no pad shown, whose actions land in [fired].
 * The screenshots use it too.
 */
internal fun fakeRingActions(fired: MutableList<String> = mutableListOf()) = RingActions(
    endStream = { fired += "end" },
    disconnectLinger = { fired += "linger" },
    touchMode = { TouchMode.TRACKPAD },
    cycleTouchMode = {},
    keyboardGranted = { true },
    keyboard = {},
    textSupported = true,
    sendText = {},
    stats = { StatsVerbosity.NORMAL },
    cycleStats = {},
    micAvailable = { true },
    micMuted = { false },
    toggleMic = {},
    hostActions = { emptyList() },
    invokeHost = {},
    sendShortcut = {},
    padAvailable = { true },
    padShown = { false },
    togglePad = { fired += "pad" },
    tapPadButton = { fired += "tap $it" },
    pointerGranted = { true },
    padMouseTarget = { 0 },
    padMouseMode = { 0 },
    cyclePadMouse = {},
    audioMute = { 0 },
    audioMuteLabel = { null },
    toggleStreamMute = {},
    currentMode = { intArrayOf(1920, 1080, 60) },
    requestMode = { _, _, _ -> },
    screensSwappable = { true },
    swapScreens = { fired += "swap" },
)

/**
 * The companion panel's contract: its tiles fire the ring's actions under the ring's rules, the
 * controller's tab connects the pad, and the pages follow the session's grants. `sdk = [36]`: see
 * [OsdScaleTest].
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class CompanionPanelTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val fired = mutableListOf<String>()

    private fun show(page: CompanionPage, onPage: (CompanionPage) -> Unit = {}) {
        compose.setContent {
            CompanionPanel(
                pages = CompanionPage.entries, page = page, onPage = onPage,
                header = PanelHeader("Living Room PC", "1920×1080 · 60 Hz"),
                stats = emptyList(), tier = StatsVerbosity.NORMAL, onTier = {},
                cfg = OverlayConfig.platformDefault(), actions = fakeRingActions(fired),
                haptics = ConsoleHaptics(null), trackpad = {}, pad = {},
            )
        }
    }

    @Test
    fun aTileFiresTheRingsAction() {
        show(CompanionPage.ACTIONS)
        compose.onNodeWithText("Guide button").performClick()
        assertEquals(listOf("tap ${Gamepad.BTN_GUIDE}"), fired)
    }

    @Test
    fun endingTheStreamTakesTwoPresses() {
        show(CompanionPage.ACTIONS)
        compose.onNode(hasScrollAction()).performScrollToNode(hasText("End stream"))
        compose.onNodeWithText("End stream").performClick()
        assertEquals(emptyList<String>(), fired)
        compose.onNodeWithText("End stream").performClick()
        assertEquals(listOf("end"), fired)
    }

    @Test
    fun theSwapTileTradesTheScreens() {
        show(CompanionPage.ACTIONS)
        compose.onNode(hasScrollAction()).performScrollToNode(hasText("Swap screens"))
        compose.onNodeWithText("Swap screens").performClick()
        assertEquals(listOf("swap"), fired)
    }

    @Test
    fun theControllerTabConnectsThePad() {
        var picked: CompanionPage? = null
        show(CompanionPage.STATS) { picked = it }
        compose.onNodeWithText("Controller").performClick()
        assertEquals(listOf("pad"), fired)
        assertEquals(CompanionPage.PAD, picked)
    }

    @Test
    fun pagesFollowTheGrants() {
        assertEquals(
            listOf(CompanionPage.STATS, CompanionPage.ACTIONS),
            companionPages(pointer = false, pad = false),
        )
        assertEquals(CompanionPage.entries, companionPages(pointer = true, pad = true))
    }

    @Test
    fun theControllerPageIsNeverRemembered() {
        val context = compose.activity
        CompanionMemory.keep(context, CompanionPage.ACTIONS)
        CompanionMemory.keep(context, CompanionPage.PAD)
        assertEquals(CompanionPage.ACTIONS, CompanionMemory.page(context))
    }

    @Test
    fun eachSecondScreenKeepsItsOwnSwap() {
        val context = compose.activity
        CompanionMemory.keepSwap(context, "Built-in Screen 2", true)
        assertEquals(true, CompanionMemory.swapped(context, "Built-in Screen 2"))
        assertEquals(false, CompanionMemory.swapped(context, "HDMI Screen"))
        CompanionMemory.keepSwap(context, "Built-in Screen 2", false)
        assertEquals(false, CompanionMemory.swapped(context, "Built-in Screen 2"))
    }
}
