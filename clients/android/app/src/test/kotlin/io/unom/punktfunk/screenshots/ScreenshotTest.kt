package io.unom.punktfunk.screenshots

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * App-store / marketing screenshots of the native Android client, rendered on the JVM by Roborazzi
 * (Robolectric Native Graphics) — no emulator, GPU, host, or JNI core. The scenes (ShotScenes.kt)
 * render the REAL Compose UI with mock state.
 *
 * `sdk = [36]` is mandatory: Robolectric ships android-all jars only up to API 36 (Android 16), and
 * the app's compileSdk is 37. PNGs land in build/outputs/roborazzi/.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class ScreenshotTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val out = "build/outputs/roborazzi"

    // Pausing the animation clock before composing (then advancing once past the entrance animation
    // and freezing) is what makes a text-field-bearing scene capturable: a focused field blinks its
    // cursor via an infinite animation that otherwise keeps Compose perpetually "busy", so
    // setContent's wait-for-idle never returns. Frozen, the capture is also deterministic.

    /**
     * Full-screen content scenes: the compose root fills the device, so a root capture is the
     * shot. [statusBar] draws the fake system bar and pushes content below it (see
     * [ShotStatusFrame]) — off for the immersive surfaces (stream, console shell), which hide
     * the real bar too.
     */
    private fun shootRoot(
        name: String,
        statusBar: Boolean = true,
        device: String = "phone",
        content: @androidx.compose.runtime.Composable () -> Unit,
    ) {
        compose.mainClock.autoAdvance = false
        compose.setContent { ShotTheme { if (statusBar) ShotStatusFrame(content) else content() } }
        compose.mainClock.advanceTimeBy(800)
        compose.onRoot().captureRoboImage("$out/$device-$name.png")
    }

    /** Dialog scenes: the AlertDialog is a separate window, so capture the whole screen (all windows). */
    private fun shootScreen(
        name: String,
        statusBar: Boolean = true,
        content: @androidx.compose.runtime.Composable () -> Unit,
    ) {
        compose.mainClock.autoAdvance = false
        compose.setContent { ShotTheme { if (statusBar) ShotStatusFrame(content) else content() } }
        // 1.6 s, not 0.8: a ModalBottomSheet's entrance spring is still mid-rise at 0.8 s and the
        // add-host sheet's Connect button was captured half below the frame.
        compose.mainClock.advanceTimeBy(1600)
        captureScreenRoboImage("$out/phone-$name.png")
    }

    @Test
    fun hosts() = shootRoot("hosts") { HostsScene() }

    @Test
    fun settings() = shootRoot("settings") { SettingsScene() }

    // One category page per shot: the sub-section headers, the caption-under-control fields and
    // the "applies from the next session" footers live inside a category, not on the root list.
    @Test
    fun settingsDisplay() = shootRoot("settings-display") {
        SettingsCategoryScene(io.unom.punktfunk.SettingsCategory.Display)
    }

    @Test
    fun settingsInput() = shootRoot("settings-input") {
        SettingsCategoryScene(io.unom.punktfunk.SettingsCategory.Input)
    }

    @Test
    fun settingsPreset() = shootRoot("settings-preset") { SettingsPresetScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi") // landscape — the stream is immersive
    fun stream() = shootRoot("stream", statusBar = false) { StreamScene(io.unom.punktfunk.StatsVerbosity.DETAILED) }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamCompact() = shootRoot("stream-compact", statusBar = false) { StreamScene(io.unom.punktfunk.StatsVerbosity.COMPACT) }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamNormal() = shootRoot("stream-normal", statusBar = false) { StreamScene(io.unom.punktfunk.StatsVerbosity.NORMAL) }

    // Both banner texts, in the stream's own landscape geometry — it is bottom-centre, so the
    // aspect is load-bearing.
    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamBannerPad() = shootRoot("stream-banner-pad", statusBar = false) { StreamBannerScene(pad = true) }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamBannerTouch() = shootRoot("stream-banner-touch", statusBar = false) { StreamBannerScene(pad = false) }

    // The companion panel at an Ayn Thor lower screen's size (1240×1080 at about 420 dpi).
    @Test
    @Config(sdk = [36], qualifiers = "w472dp-h411dp-xxhdpi")
    fun companionStats() = shootRoot("companion-stats", statusBar = false) {
        CompanionScene(io.unom.punktfunk.CompanionPage.STATS)
    }

    @Test
    @Config(sdk = [36], qualifiers = "w472dp-h411dp-xxhdpi")
    fun companionActions() = shootRoot("companion-actions", statusBar = false) {
        CompanionScene(io.unom.punktfunk.CompanionPage.ACTIONS)
    }

    // The touch flow is a Material dialog over the host grid (a separate window → shootScreen).
    @Test
    fun connecting() = shootScreen("connecting") {
        HostsScene()
        ConnectingScene()
    }

    @Test
    fun waking() = shootScreen("waking") {
        HostsScene()
        WakingScene()
    }

    @Test
    fun wakeTimedOut() = shootScreen("wake-timed-out") {
        HostsScene()
        WakeTimedOutScene()
    }

    /**
     * The touch presentation, pads connected — landscape, like every store frame: the app is
     * built for horizontal use, and a portrait capture shows a layout nobody streams in.
     */
    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun controllers() = shootRoot("controllers") { ControllersScene() }

    /**
     * The same shelf as the TOUCH grid — the presentation a finger gets from a host card's
     * "Browse library…". Portrait (the default qualifiers), because that is the orientation a
     * phone browses a poster wall in, and the one whose column count the layout has to get right.
     */
    @Test
    fun libraryTouch() = shootRoot("library-touch") { TouchLibraryScene() }

    @Test
    fun trust() = shootScreen("trust") {
        HostsScene()
        TrustDialog()
    }

    @Test
    fun newPreset() = shootRoot("new-preset") { NewPresetScene() }

    @Test
    fun speedTest() = shootScreen("speed-test") {
        HostsScene()
        SpeedTestScene()
    }

    @Test
    fun pair() = shootScreen("pair") {
        HostsScene()
        PairDialog()
    }

    /**
     * The add-host sheet (separate window → whole-screen capture). Pixel-like geometry, not the
     * default 360×800dp: same 1080×2400 px, but at 420 dpi the extra dp headroom is what lets the
     * sheet's Connect button — the row that carries the resolution promise — fit in frame.
     */
    @Test
    @Config(sdk = [36], qualifiers = "w411dp-h915dp-420dpi")
    fun addHost() = shootScreen("add-host") { AddHostScene() }

    // Play's 7" and 10" tablet slots at their exact sizes: xhdpi is 2 px per dp, so 600×960 dp
    // is 1200×1920 px and 800×1280 dp is 1600×2560 px. Grids portrait, stream and pads landscape.
    @Test
    @Config(sdk = [36], qualifiers = "w600dp-h960dp-xhdpi")
    fun tablet7Hosts() = shootRoot("hosts", device = "tablet7") { HostsScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w600dp-h960dp-xhdpi")
    fun tablet7Library() = shootRoot("library", device = "tablet7") { TouchLibraryScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w960dp-h600dp-xhdpi")
    fun tablet7Controllers() = shootRoot("controllers", device = "tablet7") { ControllersScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w960dp-h600dp-xhdpi")
    fun tablet7Stream() = shootRoot("stream", statusBar = false, device = "tablet7") {
        StreamScene(io.unom.punktfunk.StatsVerbosity.DETAILED)
    }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h1280dp-xhdpi")
    fun tablet10Hosts() = shootRoot("hosts", device = "tablet10") { HostsScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h1280dp-xhdpi")
    fun tablet10Library() = shootRoot("library", device = "tablet10") { TouchLibraryScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w1280dp-h800dp-xhdpi")
    fun tablet10Controllers() = shootRoot("controllers", device = "tablet10") { ControllersScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w1280dp-h800dp-xhdpi")
    fun tablet10Stream() = shootRoot("stream", statusBar = false, device = "tablet10") {
        StreamScene(io.unom.punktfunk.StatsVerbosity.DETAILED)
    }
}
