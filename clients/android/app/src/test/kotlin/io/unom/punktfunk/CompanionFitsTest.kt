package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Test

/** The second-screen size rule (design/android-dual-screen.md §7), pure. */
class CompanionFitsTest {
    @Test
    fun aMeasuredPanelCountsByItsDiagonal() {
        assertEquals(true, companionFits(3.9f, sizeless = false, handheld = true))
        assertEquals(true, companionFits(5.5f, sizeless = false, handheld = false))
        assertEquals(false, companionFits(13.8f, sizeless = false, handheld = true))
    }

    @Test
    fun aSizelessPanelCountsOnAHandheldAlone() {
        assertEquals(true, companionFits(13.8f, sizeless = true, handheld = true))
        assertEquals(false, companionFits(13.8f, sizeless = true, handheld = false))
        assertEquals(false, companionFits(Float.POSITIVE_INFINITY, sizeless = true, handheld = false))
    }
}
