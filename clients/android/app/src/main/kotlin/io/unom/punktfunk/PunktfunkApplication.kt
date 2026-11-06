package io.unom.punktfunk

import android.app.Application
import android.content.Context
import io.unom.punktfunk.kit.NativeBridge
import org.json.JSONArray

/** The app's languages as BCP-47 tags, most preferred first. */
fun languageTags(context: Context): List<String> {
    val locales = context.resources.configuration.locales
    return List(locales.size()) { locales[it].toLanguageTag() }
}

/**
 * Process start. The Rust side words the palette names, the PyroWave warning and the stats
 * overlay's lines in the process-wide language, which is English until told: set it here, before
 * any of them is read with no console built.
 */
class PunktfunkApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        // A JVM unit test has no native library, so the call may not link.
        runCatching { NativeBridge.nativeSetLanguages(JSONArray(languageTags(this)).toString()) }
    }
}
