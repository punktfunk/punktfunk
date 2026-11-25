package io.unom.punktfunk

import io.unom.punktfunk.kit.VideoFit
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** [pictureSizeMm] is pure: the panel's DPI, the window the picture fills, and the mode it shows. */
class PictureSizeTest {
    private val panel = 2400 to 1080

    private fun mm(
        xdpi: Float = 400f,
        ydpi: Float = 400f,
        densityDpi: Int = 420,
        view: Pair<Int, Int> = panel,
        mode: Pair<Int, Int> = panel,
        fit: VideoFit = VideoFit.FIT,
        tvOrHeadset: Boolean = false,
    ) = pictureSizeMm(xdpi, ydpi, densityDpi, view, mode, fit, tvOrHeadset)

    /** A phone streaming its own mode fullscreen sends its panel: 2400×1080 at 400 PPI. */
    @Test
    fun aFullscreenPhoneSendsItsPanel() {
        assertEquals(152 to 69, mm())
    }

    /** Render scale asks other pixels for the same glass: the size holds at every mode. */
    @Test
    fun renderScaleKeepsTheSize() {
        assertEquals(152 to 69, mm(mode = 1200 to 540))
        assertEquals(152 to 69, mm(mode = 4800 to 2160))
    }

    /** Half the width of the screen shows the picture at half its size. */
    @Test
    fun aSplitScreenWindowIsOnlyTheGlassItCovers() {
        assertEquals(76 to 34, mm(view = 1200 to 1080))
    }

    /** Stretch fills the view whatever the mode's shape. */
    @Test
    fun stretchFillsTheView() {
        assertEquals(152 to 69, mm(mode = 2400 to 810, fit = VideoFit.STRETCH))
    }

    @Test
    fun aTvOrHeadsetSendsNone() {
        assertNull(mm(tvOrHeadset = true))
    }

    @Test
    fun anAbsurdDpiSendsNone() {
        assertNull(mm(xdpi = 2000f, ydpi = 2000f))
        assertNull(mm(xdpi = 20f, ydpi = 20f))
        assertNull(mm(xdpi = Float.NaN))
    }

    /** Android's stand-ins and a copied density are not a measured panel. */
    @Test
    fun aStandInDpiSendsNone() {
        assertNull(mm(xdpi = 320f, ydpi = 320f))
        assertNull(mm(xdpi = 160f, ydpi = 160f))
        assertNull(mm(xdpi = 213f, ydpi = 213f))
        assertNull(mm(xdpi = 420f, ydpi = 420f, densityDpi = 420))
    }

    /** Square pixels: axes far apart mean one of them is wrong. */
    @Test
    fun axesFarApartSendNone() {
        assertNull(mm(xdpi = 400f, ydpi = 160f))
        assertEquals(151 to 68, mm(xdpi = 401.5f, ydpi = 403.4f))
    }
}
