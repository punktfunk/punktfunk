package io.unom.punktfunk.kit

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pins [limitedOnlyBt709Hal], the rule behind [DecoderSnapshot.showsFullRangeAsStudio]: only a
 * Qualcomm decoder on a vendor older than Android 13 asks for studio range.
 * Run: `./gradlew :kit:testDebugUnitTest`.
 */
class StudioRangeHalTest {
    @Test
    fun anOldQualcommVendorAsks() {
        assertTrue(limitedOnlyBt709Hal("c2.qti.avc.decoder.low_latency", 30))
        assertTrue(limitedOnlyBt709Hal("OMX.qcom.video.decoder.avc", 32))
    }

    @Test
    fun anUnreadableVendorLevelAsks() = assertTrue(limitedOnlyBt709Hal("c2.qti.hevc.decoder", 0))

    @Test
    fun anAndroid13OrLaterQualcommVendorKeepsFullRange() {
        assertFalse(limitedOnlyBt709Hal("c2.qti.hevc.decoder", 33))
        assertFalse(limitedOnlyBt709Hal("c2.qti.av1.decoder.low_latency", 202404))
    }

    @Test
    fun otherVendorsNeverAsk() = assertFalse(limitedOnlyBt709Hal("c2.exynos.hevc.decoder", 30))
}
