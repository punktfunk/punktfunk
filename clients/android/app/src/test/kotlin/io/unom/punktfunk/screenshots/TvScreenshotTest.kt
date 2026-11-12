package io.unom.punktfunk.screenshots

import android.content.pm.PackageManager
import androidx.activity.ComponentActivity
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The same Roborazzi harness as ScreenshotTest, at Android TV geometry: 960×540dp in the
 * `television` UI mode at xhdpi (2.0×) = 1920×1080 px — the Play Store's 16:9 TV screenshot size,
 * captured 1:1 with no resampling. Only the screens that exist on a TV are shot here: the
 * gamepad-console shell (what LEANBACK_LAUNCHER opens into) and the in-stream view. Files are
 * prefixed `tv-` so the artifact separates the form factors.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [36], qualifiers = "w960dp-h540dp-television-xhdpi")
class TvScreenshotTest {
    // Before the activity starts, so it comes up in the language.
    @get:Rule(order = 0)
    val language = ShotLanguageRule()

    @get:Rule(order = 1)
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val out = "build/outputs/roborazzi"

    private fun shootRoot(name: String, content: @androidx.compose.runtime.Composable () -> Unit) {
        compose.mainClock.autoAdvance = false
        // The television qualifier leaves leanback off, so isTvDevice would answer phone.
        shadowOf(compose.activity.packageManager).setSystemFeature(PackageManager.FEATURE_LEANBACK, true)
        compose.setContent { ShotTheme(content) }
        compose.mainClock.advanceTimeBy(800)
        compose.onRoot().captureRoboImage("$out/tv-$name.png")
    }

    @Test
    fun stream() =
        shootRoot("stream") { StreamScene(io.unom.punktfunk.StatsVerbosity.COMPACT, loss = false) }

    @Test
    fun streamDetailed() =
        shootRoot("stream-detailed") { StreamScene(io.unom.punktfunk.StatsVerbosity.DETAILED, loss = false) }
}
