package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pins [codecCaps], the rule behind the Hello's codec bits and 10-bit asks: a codec counts only
 * with a hardware decoder that fits the mode and, with 10-bit asked, decodes Main10. Also the
 * [DecoderSnapshot] a connect reads them and its flags from.
 * Run: `./gradlew :kit:testDebugUnitTest -PskipRustBuild`.
 */
class DecoderProbeTest {
    private val hevc = "video/hevc"
    private val av1 = "video/av01"
    private val main10 = listOf(1, 2)

    @Test
    fun softwareAv1BelowApi29IsNotAdvertised() {
        // Below API 29 the name decides, whatever the flag says.
        val tv = listOf(
            DecoderFacts("OMX.MTK.VIDEO.DECODER.HEVC", hevc, hardware = false, profiles = main10),
            DecoderFacts("OMX.MTK.VIDEO.DECODER.SW.AV1", av1, hardware = true),
            DecoderFacts("libgav1", av1, hardware = true),
            DecoderFacts("c2.android.av1-decoder", av1, hardware = true),
        )
        assertEquals(DecodeCaps(1 or 2, false), codecCaps(tv, tenBit = false, api = 28))
    }

    @Test
    fun fromApi29TheHardwareFlagDecides() {
        val dav1d = listOf(DecoderFacts("c2.vendor.av1.decoder", av1, hardware = false))
        assertEquals(1, codecCaps(dav1d, tenBit = false, api = 29).codecBits)
        val hw = listOf(DecoderFacts("c2.vendor.av1.decoder", av1, hardware = true))
        assertEquals(1 or 4, codecCaps(hw, tenBit = false, api = 29).codecBits)
    }

    @Test
    fun aDecoderThatCannotTakeTheModeIsNotAdvertised() {
        val box = listOf(
            DecoderFacts("c2.mtk.avc.decoder", "video/avc", hardware = true),
            DecoderFacts("c2.mtk.av1.decoder", av1, hardware = true, fits = false),
        )
        assertEquals(1, codecCaps(box, tenBit = false, api = 33).codecBits)
        // A mode no decoder claims keeps the codecs: H.264 alone would fare worst there.
        assertEquals(1 or 4, codecCaps(box.drop(1), tenBit = false, api = 33).codecBits)
    }

    @Test
    fun tenBitKeepsOnlyTheCodecsThatDecodeMain10() {
        val box = listOf(
            DecoderFacts("c2.amlogic.hevc.decoder", hevc, hardware = true, profiles = main10),
            DecoderFacts("c2.amlogic.av1.decoder", av1, hardware = true, profiles = listOf(1)),
        )
        assertEquals(DecodeCaps(1 or 2, true), codecCaps(box, tenBit = true, api = 30))
        assertEquals(DecodeCaps(1 or 2 or 4, false), codecCaps(box, tenBit = false, api = 30))
    }

    @Test
    fun withoutAnyMain10DecoderTenBitIsDroppedAndTheCodecsStand() {
        val box = listOf(
            DecoderFacts("OMX.qcom.video.decoder.hevc", hevc, hardware = true, profiles = listOf(1)),
            DecoderFacts("OMX.qcom.video.decoder.av1", av1, hardware = true, profiles = listOf(1)),
        )
        assertEquals(DecodeCaps(1 or 2 or 4, false), codecCaps(box, tenBit = true, api = 28))
    }

    @Test
    fun blockedAndSecureDecodersNeverCount() {
        val box = listOf(
            DecoderFacts("OMX.qcom.video.decoder.hevcswvdec", hevc, hardware = true),
            DecoderFacts("c2.qti.hevc.decoder.secure", hevc, hardware = true),
            DecoderFacts("c2.qti.av1.decoder", av1, hardware = true, secureRequired = true),
        )
        assertEquals(1, codecCaps(box, tenBit = false, api = 33).codecBits)
    }

    @Test
    fun theStreamPickRanksHardwareThenVendorThenLowLatency() {
        val list = listOf(
            DecoderFacts("OMX.google.hevc.decoder", hevc, hardware = false),
            DecoderFacts("c2.other.hevc.decoder", hevc, hardware = true, lowLatency = true),
            DecoderFacts("c2.qti.hevc.decoder", hevc, hardware = true),
            DecoderFacts("c2.qti.hevc.decoder.low_latency", hevc, hardware = true, lowLatency = true),
        )
        assertEquals("c2.qti.hevc.decoder.low_latency", bestDecoder(list, api = 33)?.name)
        assertTrue(softwareByName("OMX.SEC.vp9.sw.dec") && !softwareByName("ARC.h264.decoder"))
    }

    /**
     * A connect's snapshot: the codec bits read the mode, the flags read the pick of each codec
     * advertised at any size. An AV1 decoder that misses the mode still has its pick read.
     */
    @Test
    fun aSnapshotReadsTheBitsAtTheModeAndTheFlagsAtAnySize() {
        val box = listOf(
            DecoderFacts("c2.android.avc.decoder", "video/avc", hardware = false),
            DecoderFacts("c2.amlogic.avc.decoder", "video/avc", hardware = true, partialFrame = true),
            DecoderFacts("c2.amlogic.hevc.decoder", hevc, hardware = true, profiles = main10, partialFrame = true),
            DecoderFacts("c2.amlogic.av1.decoder", av1, hardware = true, fits = false),
        )
        val tv = DecoderSnapshot(box, false, pyrowave = true, api = 33, vendorApi = 202404, listed = true, census = "")
        assertEquals(DecodeCaps(1 or 2 or 8, false), tv.decode)
        assertEquals(
            listOf("c2.amlogic.avc.decoder", "c2.amlogic.hevc.decoder", "c2.amlogic.av1.decoder"),
            tv.picks.map { it.second?.name },
        )
        assertFalse(tv.multiSliceTolerant || tv.partialFrameCapable || tv.showsFullRangeAsStudio)

        val qti = listOf(
            DecoderFacts("c2.qti.avc.decoder", "video/avc", hardware = true, partialFrame = true),
            DecoderFacts("c2.qti.hevc.decoder", hevc, hardware = true, partialFrame = true),
        )
        val phone = DecoderSnapshot(qti, false, pyrowave = false, api = 33, vendorApi = 32, listed = true, census = "")
        assertEquals(DecodeCaps(1 or 2, false), phone.decode)
        assertTrue(phone.multiSliceTolerant && phone.partialFrameCapable && phone.showsFullRangeAsStudio)

        // No codec list: H.264 alone, no flag, and no decoder opened for the caps line.
        val none = DecoderSnapshot(emptyList(), true, pyrowave = false, api = 33, vendorApi = 0, listed = false, census = "")
        assertEquals(DecodeCaps(1, false), none.decode)
        assertFalse(none.multiSliceTolerant || none.partialFrameCapable || none.showsFullRangeAsStudio)
        assertEquals("codec list unavailable", none.capsReport())
    }
}
