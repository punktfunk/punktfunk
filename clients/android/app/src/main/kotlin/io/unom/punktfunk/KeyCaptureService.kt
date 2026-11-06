package io.unom.punktfunk

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.AccessibilityServiceInfo
import android.content.Context
import android.content.Intent
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import io.unom.punktfunk.kit.R

/**
 * Hardware keys ahead of Android's own shortcuts. The window policy takes Alt+Tab, every Meta
 * chord and the Language key before an app sees them; an accessibility service that filters keys
 * runs in the input dispatcher before that policy. The user turns it on under Accessibility.
 *
 * It hands every key to the focused stream ([stream], set by [MainActivity]) and consumes what the
 * stream owns. It asks for keys only while a stream holds the window: every filtered key, in any
 * app, waits up to 500 ms for this process to answer, and a late one lands as a long press. It
 * never reads window content.
 */
class KeyCaptureService : AccessibilityService() {
    override fun onServiceConnected() {
        instance = this
        running = true
        filterKeys(stream != null)
    }

    override fun onUnbind(intent: Intent?): Boolean {
        instance = null
        running = false
        return super.onUnbind(intent)
    }

    private fun filterKeys(on: Boolean) {
        val info = serviceInfo ?: return
        val flag = AccessibilityServiceInfo.FLAG_REQUEST_FILTER_KEY_EVENTS
        info.flags = if (on) info.flags or flag else info.flags and flag.inv()
        serviceInfo = info
    }

    /** A key consumed here never reaches the dispatcher, so it never repeats there either;
     *  the stream's handler re-creates the repeat (`MainActivity.armKeyRepeat`). */
    override fun onKeyEvent(event: KeyEvent): Boolean = stream?.invoke(event) == true

    override fun onAccessibilityEvent(event: AccessibilityEvent?) {}

    override fun onInterrupt() {}

    companion object {
        /** Main thread only; cleared on unbind. */
        private var instance: KeyCaptureService? = null

        /** The focused stream's key handler; null whenever no stream holds the window. Setting it
         *  turns the service's key filter on or off; main thread only. */
        @Volatile
        var stream: ((KeyEvent) -> Boolean)? = null
            set(value) {
                val changed = (field == null) != (value == null)
                field = value
                if (changed) instance?.filterKeys(value != null)
            }

        /** The user has the service on, so Alt+Tab needs no stand-in and the banner says nothing. */
        var running by mutableStateOf(false)
            private set

        private const val PREFS = "punktfunk_key_capture"
        private const val K_ANSWERED = "disclosure_answered"

        /** The launch prompt has been answered once; after that only Settings offers it. */
        fun disclosureAnswered(context: Context): Boolean =
            prefs(context).getBoolean(K_ANSWERED, false)

        fun markDisclosureAnswered(context: Context) {
            prefs(context).edit().putBoolean(K_ANSWERED, true).apply()
        }

        private fun prefs(context: Context) =
            context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
    }
}

/**
 * The prominent disclosure Play requires before the service is turned on, because Punktfunk is not
 * an accessibility tool: what the service receives, where it goes, and an explicit Agree. It stands
 * alone; Play rejects one that shares a prompt with other disclosures. Agree opens Android's
 * Accessibility page; either answer closes the prompt. The `android_keycapture_*` messages match
 * the Play listing paragraph in `clients/android/store/play-listing.md`.
 */
@Composable
fun KeyCaptureDisclosure(onDismiss: () -> Unit) {
    val context = LocalContext.current
    PunktfunkDialog(
        title = stringResource(R.string.android_keycapture_title),
        onDismiss = onDismiss,
        dismissOnOutsideTap = false,
        actions = listOf(
            DialogAction(stringResource(R.string.android_keycapture_agree), primary = true) {
                onDismiss()
                openAccessibilitySettings(context)
            },
            DialogAction(stringResource(R.string.android_keycapture_decline), onClick = onDismiss),
        ),
    ) {
        PromptText(stringResource(R.string.android_keycapture_body_why))
        PromptText(stringResource(R.string.android_keycapture_body_what))
        PromptText(stringResource(R.string.android_keycapture_body_how))
    }
}

/** Android's Accessibility page, where the key service is switched on; a TV without one is a no-op. */
internal fun openAccessibilitySettings(context: Context) {
    runCatching {
        context.startActivity(Intent(android.provider.Settings.ACTION_ACCESSIBILITY_SETTINGS))
    }
}
