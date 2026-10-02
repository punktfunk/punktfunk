package io.unom.punktfunk

import android.content.Context
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
    screenLayouts = { ScreenLayout.entries },
    cycleScreens = { fired += "screens" },
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
    fun theScreensTileCyclesTheLayout() {
        show(CompanionPage.ACTIONS)
        compose.onNode(hasScrollAction()).performScrollToNode(hasText("Screens"))
        compose.onNodeWithText("Screens").performClick()
        assertEquals(listOf("screens"), fired)
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
    fun eachSecondScreenKeepsItsOwnLayout() {
        val context = compose.activity
        CompanionMemory.keepLayout(context, "Built-in Screen 2", ScreenLayout.SPANNED)
        assertEquals(ScreenLayout.SPANNED, CompanionMemory.layout(context, "Built-in Screen 2"))
        assertEquals(ScreenLayout.PANEL, CompanionMemory.layout(context, "HDMI Screen"))
        CompanionMemory.keepLayout(context, "Built-in Screen 2", ScreenLayout.PANEL)
        assertEquals(ScreenLayout.PANEL, CompanionMemory.layout(context, "Built-in Screen 2"))
    }

    @Test
    fun aSwapKeptBeforeLayoutsReadsAsThePictureBelow() {
        val context = compose.activity
        context.getSharedPreferences("punktfunk_companion", Context.MODE_PRIVATE)
            .edit().putBoolean("swap:Old Screen", true).commit()
        assertEquals(ScreenLayout.SWAPPED, CompanionMemory.layout(context, "Old Screen"))
        CompanionMemory.keepLayout(context, "Old Screen", ScreenLayout.PANEL)
        assertEquals(ScreenLayout.PANEL, CompanionMemory.layout(context, "Old Screen"))
    }

    @Test
    fun aTwoScreenConsoleIsKnownByAnySpelling() {
        for (tag in listOf("nds", "Nintendo DS", "3DS", "Nintendo 3DS", "New Nintendo 3DS", "wiiu", "Wii U")) {
            assertEquals(tag, true, twoScreenPlatform(tag))
        }
        for (tag in listOf(null, "", "steam", "Nintendo Switch", "gba")) {
            assertEquals(tag, false, twoScreenPlatform(tag))
        }
    }

    @Test
    fun aSpannedPictureAsksForTwoHalves() {
        val context = compose.activity
        val (w, h, hz) = nativeDisplayMode(context, spanned = false)
        assertEquals(Triple(w, 2 * h, hz), nativeDisplayMode(context, spanned = true))
    }

    @Test
    fun theCycleSkipsWhatThePairCannotBuild() {
        val two = listOf(ScreenLayout.PANEL, ScreenLayout.SWAPPED)
        assertEquals(ScreenLayout.SWAPPED, ScreenLayout.PANEL.next(two))
        assertEquals(ScreenLayout.PANEL, ScreenLayout.SWAPPED.next(two))
        assertEquals(ScreenLayout.PANEL, ScreenLayout.SPANNED.next(two))
        assertEquals(ScreenLayout.PANEL, ScreenLayout.PANEL.next(emptyList()))
    }
}
