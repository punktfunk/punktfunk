package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.R
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Spanish does not ship, so only the two accessibility-service strings carry it: the hand-written
 * `res/values-es` beside the generated English. On a Spanish device those two read Spanish and
 * every other message stays English.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36], qualifiers = "es")
class SpanishAccessibilityTest {
    private val res get() = ApplicationProvider.getApplicationContext<Context>().resources

    @Test
    fun theServiceNameAndDescriptionReadSpanish() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val es = JSONObject(File("../../../l10n/messages/android/es.json").readText())
        assertEquals(
            es.getString("android_keycapture_service_label"),
            res.getString(R.string.android_keycapture_service_label),
        )
        assertEquals(
            es.getString("android_keycapture_service_description"),
            res.getString(R.string.android_keycapture_service_description),
        )
        assertEquals("Atajos de teclado de Punktfunk", res.getString(R.string.android_keycapture_service_label))
    }

    @Test
    fun everyOtherMessageStaysEnglish() {
        assertEquals("Send every keyboard shortcut?", res.getString(R.string.android_keycapture_title))
        assertEquals("Statistics size", res.getString(R.string.player_stats_scale_pct))
        assertEquals("3 advanced settings changed", res.getQuantityString(R.plurals.prefs_advanced_changed, 3, 3))
    }
}
