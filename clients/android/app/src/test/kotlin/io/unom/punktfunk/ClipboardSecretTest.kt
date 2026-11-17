package io.unom.punktfunk

import android.content.ClipDescription
import android.os.PersistableBundle
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** A clip a password manager marks secret stays on the device. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36]) // Robolectric has no SDK 37 image yet; the app targets 37
class ClipboardSecretTest {
    private fun description(sensitive: Boolean?) =
        ClipDescription("copy", arrayOf(ClipDescription.MIMETYPE_TEXT_PLAIN)).apply {
            if (sensitive != null) {
                extras = PersistableBundle().apply {
                    putBoolean("android.content.extra.IS_SENSITIVE", sensitive)
                }
            }
        }

    @Test
    fun a_clip_marked_sensitive_is_secret() {
        assertTrue(isSecretClip(description(true)))
    }

    @Test
    fun an_unmarked_or_cleared_clip_is_not() {
        assertFalse(isSecretClip(description(null)))
        assertFalse(isSecretClip(description(false)))
        assertFalse(isSecretClip(null))
    }
}
