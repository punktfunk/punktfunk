package io.unom.punktfunk

import androidx.compose.ui.unit.IntSize
import io.unom.punktfunk.kit.VideoFit
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [FrameMap] — how the whole-container gesture layer turns a finger into a frame
 * pixel. The placement itself is pinned by the kit's shared-vector test. Run:
 * `./gradlew :app:testDebugUnitTest`.
 */
class FrameMapTest {
    private val phone = IntSize(3216, 1440)

    @Test
    fun fitClampsABarContactOntoThePictureEdge() {
        // A 16:9 stream on a 20:9 panel: the picture spans x 328…2888.
        val m = VideoFrame(VideoFit.FIT, 1920, 1080).at(phone)
        assertEquals(0, m.x(100f))
        assertEquals(1919, m.x(3100f))
        assertEquals(960, m.x(1608f))
        assertEquals(1920 to 1080, m.width to m.height)
    }

    @Test
    fun cropMapsTheScreenEdgeOntoTheFirstVisibleRow() {
        val m = VideoFrame(VideoFit.CROP, 1920, 1080).at(phone)
        assertEquals(0, m.x(0f))
        assertEquals(110, m.y(0f)) // the top ~110 rows are cut off
        assertEquals(970, m.y(1440f))
    }

    @Test
    fun stretchReachesEveryFrameEdge() {
        val m = VideoFrame(VideoFit.STRETCH, 1920, 1080).at(phone)
        assertEquals(1919 to 1079, m.x(3216f) to m.y(1440f))
        assertEquals(0.5f, m.nx(1608f), 1e-3f)
    }

    @Test
    fun anUnknownFrameMapsTheContainerOntoItself() {
        val m = VideoFrame(VideoFit.CROP, 0, 0).at(IntSize(1280, 720))
        assertEquals(640 to 360, m.x(640f) to m.y(360f))
        assertEquals(1280 to 720, m.width to m.height)
    }

    @Test
    fun anUnmeasuredContainerIsEmpty() {
        assertTrue(VideoFrame(VideoFit.FIT, 1920, 1080).at(IntSize.Zero).isEmpty)
    }

    // A 1920×2160 picture spanning two screens: a Thor's lower 1240×1080 panel shows rows
    // 1080…2159, letterboxed, and the host still sees one 1920×2160 frame.
    @Test
    fun theLowerHalfLandsInTheLowerRows() {
        val m = VideoFrame(VideoFit.FIT, 1920, 2160, originY = 1080, span = 1080).at(IntSize(1240, 1080))
        assertEquals(1920 to 2160, m.width to m.height)
        assertEquals(1080, m.y(0f))
        assertEquals(2159, m.y(1080f))
        assertEquals(0.5f, m.ny(0f), 1e-3f)
        val crop = m.sourceCrop()
        assertEquals(0.5f, crop[1], 1e-3f)
        assertEquals(1f, crop[3], 1e-3f)
    }

    @Test
    fun theUpperHalfStopsAtTheSplit() {
        val m = VideoFrame(VideoFit.FIT, 1920, 2160, originY = 0, span = 1080).at(IntSize(1920, 1080))
        assertEquals(0 to 1079, m.y(0f) to m.y(1080f))
        assertEquals(0.5f, m.sourceCrop()[3], 1e-3f)
        assertEquals(floatArrayOf(0f, 0f, 1f, 1f).toList(), VideoFrame(VideoFit.FIT, 0, 0).at(IntSize.Zero).sourceCrop().toList())
    }
}
