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
import org.xmlpull.v1.XmlPullParser

/**
 * The resources :kit generates from `l10n/messages`, as the app resolves them: the seed messages
 * read as the literals they replaced, and the per-app language list holds the shipped languages.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class MessagesTest {
    private val res get() = ApplicationProvider.getApplicationContext<Context>().resources

    @Test
    fun seedMessagesReadInEnglish() {
        assertEquals("Statistics size", res.getString(R.string.player_stats_scale_pct))
        assertEquals("1 advanced setting changed", res.getQuantityString(R.plurals.prefs_advanced_changed, 1, 1))
        assertEquals("3 advanced settings changed", res.getQuantityString(R.plurals.prefs_advanced_changed, 3, 3))
    }

    @Test
    fun localeConfigListsTheShippedLanguages() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val shipped = JSONObject(File("../../../l10n/locales.json").readText()).getJSONArray("shipped")
        val listed = mutableSetOf<String>()
        res.getXml(R.xml.locales_config).use { xml ->
            while (xml.next() != XmlPullParser.END_DOCUMENT) {
                if (xml.eventType == XmlPullParser.START_TAG && xml.name == "locale") {
                    listed += xml.getAttributeValue("http://schemas.android.com/apk/res/android", "name")
                }
            }
        }
        assertEquals(List(shipped.length()) { shipped.getString(it) }.toSet(), listed)
    }
}
