package io.unom.punktfunk

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import io.unom.punktfunk.kit.R
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** The touch Bitrate row's custom entry: an off-menu rate is custom and reads as its number. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class CustomBitrateTest {
    private val context get() = ApplicationProvider.getApplicationContext<Context>()

    @Test
    fun anOffMenuRateIsCustomAndNeverAutomatic() {
        assertFalse(Settings().isCustomBitrate())
        assertFalse(Settings(bitrateKbps = 20_000).isCustomBitrate())
        // The speed test stores 70 % of the measurement, which is rarely a menu entry.
        assertTrue(Settings(bitrateKbps = 14_000).isCustomBitrate())
        assertEquals("14 Mbit/s", context.getString(R.string.common_mbits, bitrateMbits(14_000)))
        assertEquals("14.9 Mbit/s", context.getString(R.string.common_mbits, bitrateMbits(14_900)))
        assertTrue(CUSTOM_BITRATE !in BITRATE_OPTIONS)
    }
}
