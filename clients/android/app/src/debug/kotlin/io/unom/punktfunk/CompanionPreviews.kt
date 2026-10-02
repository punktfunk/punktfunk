package io.unom.punktfunk

import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.tooling.preview.Preview

/*
 * Android Studio previews of the companion panel (design/android-dual-screen.md §6): open this
 * file, switch the editor to Split or Design, and every page renders at both lower screens —
 * the Thor's 3.9″ panel and the Dual Screen add-on's 5.5″ one. "Start Interactive Mode" on
 * the first preview walks the rail; "Run Preview" puts a page on a plugged-in device. Debug
 * source set only: nothing here ships.
 */

/** The two lower screens, by their real pixel size and density. */
@Preview(name = "Ayn Thor 3.9in", device = "spec:width=1240px,height=1080px,dpi=420", showBackground = true)
@Preview(name = "Dual Screen add-on 5.5in", device = "spec:width=1920px,height=1080px,dpi=400", showBackground = true)
private annotation class LowerScreens

/** A full-control session with a controller forwarded and nothing shown yet, as the panel sees it. */
private val previewActions = RingActions(
    endStream = {}, disconnectLinger = {},
    touchMode = { TouchMode.TRACKPAD }, cycleTouchMode = {},
    keyboardGranted = { true }, keyboard = {},
    textSupported = true, sendText = {},
    stats = { StatsVerbosity.NORMAL }, cycleStats = {},
    micAvailable = { true }, micMuted = { false }, toggleMic = {},
    hostActions = {
        listOf(HostActions.Action("power.sleep", "Sleep host", danger = false, available = true, unavailableReason = ""))
    },
    invokeHost = {}, sendShortcut = {},
    padAvailable = { true }, padShown = { false }, togglePad = {}, tapPadButton = {},
    pointerGranted = { true }, padMouseTarget = { 1 }, padMouseMode = { 0 }, cyclePadMouse = {},
    audioMute = { 0 }, audioMuteLabel = { null }, toggleStreamMute = {},
    currentMode = { intArrayOf(1920, 1080, 120) }, requestMode = { _, _, _ -> },
    screenLayouts = { ScreenLayout.entries },
)

private val previewStats = listOf(
    HudLine(0, "1920×1080@120 · HEVC 10-bit · c2.qti.hevc.decoder · low-latency · HDR"),
    HudLine(1, "received 119 fps · decoded 119 · presented 118 · 92.1 Mb/s"),
    HudLine(1, "host 0.6 ms · decode 0.4 ms · display 0.2 ms (avg)"),
    HudLine(1, "lost 0.0% · skipped 0.0% · rtt 1.2 ms"),
)

@Composable
private fun PreviewPanel(page: CompanionPage, onPage: (CompanionPage) -> Unit = {}) {
    MaterialTheme(colorScheme = BrandDark, typography = PunktfunkTypography) {
        CompanionPanel(
            pages = CompanionPage.entries,
            page = page,
            onPage = onPage,
            header = PanelHeader("Living Room PC · Starfall Vale", "1920×1080 · 120 Hz"),
            stats = previewStats,
            tier = StatsVerbosity.NORMAL,
            onTier = {},
            cfg = OverlayConfig.platformDefault(),
            actions = previewActions,
            haptics = ConsoleHaptics(null),
            keys = { _, _ -> },
            trackpad = {},
            pad = {},
        )
    }
}

/** The whole panel; the rail works in interactive mode. */
@LowerScreens
@Composable
private fun PanelInteractivePreview() {
    var page by remember { mutableStateOf(CompanionPage.STATS) }
    PreviewPanel(page) { page = it }
}

@LowerScreens
@Composable
private fun StatsPreview() = PreviewPanel(CompanionPage.STATS)

@LowerScreens
@Composable
private fun ActionsPreview() = PreviewPanel(CompanionPage.ACTIONS)

@LowerScreens
@Composable
private fun KeyboardPreview() = PreviewPanel(CompanionPage.KEYBOARD)

@LowerScreens
@Composable
private fun TrackpadPreview() = PreviewPanel(CompanionPage.TRACKPAD)

@LowerScreens
@Composable
private fun ControllerPreview() = PreviewPanel(CompanionPage.PAD)
