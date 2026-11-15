package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** [displayHdr] and [hdrOn] are pure: the panel's HDR10 listing, the OS headroom signal, the player's yes. */
class DisplayHdrTest {

    /** A built-in panel the OS can light above SDR white (the Nothing Phone 3) streams HDR. */
    @Test
    fun aPanelWithHeadroomIsFull() {
        assertEquals(DisplayHdr.Full, displayHdr(hdr10 = true, headroom = true))
    }

    /** A panel that lists HDR10 with no headroom (the S20+ on LineageOS) shows PQ dark. */
    @Test
    fun aPanelWithoutHeadroomSaysSo() {
        assertEquals(DisplayHdr.NoHeadroom, displayHdr(hdr10 = true, headroom = false))
    }

    /** No signal (API < 34, a TV, an external screen) keeps the panel's own listing. */
    @Test
    fun noSignalKeepsTheListing() {
        assertEquals(DisplayHdr.Full, displayHdr(hdr10 = true, headroom = null))
        assertEquals(DisplayHdr.None, displayHdr(hdr10 = false, headroom = null))
    }

    /** Headroom never makes an SDR panel an HDR one. */
    @Test
    fun headroomAloneIsNotHdr() {
        assertEquals(DisplayHdr.None, displayHdr(hdr10 = false, headroom = true))
    }

    /** The default-on setting lights no HDR without headroom: that takes the player's yes. */
    @Test
    fun noHeadroomWaitsForTheYes() {
        assertFalse(Settings().hdrOn(DisplayHdr.NoHeadroom))
        assertTrue(Settings(hdrWithoutHeadroom = true).hdrOn(DisplayHdr.NoHeadroom))
        assertFalse(Settings(hdrEnabled = false, hdrWithoutHeadroom = true).hdrOn(DisplayHdr.NoHeadroom))
    }

    /** The yes moves no other display: full stays the setting's, an SDR panel stays SDR. */
    @Test
    fun theYesIsForThatDisplayAlone() {
        assertTrue(Settings().hdrOn(DisplayHdr.Full))
        assertFalse(Settings(hdrEnabled = false, hdrWithoutHeadroom = true).hdrOn(DisplayHdr.Full))
        assertFalse(Settings(hdrWithoutHeadroom = true).hdrOn(DisplayHdr.None))
    }

    /** A yes on the defaults turns HDR on there. */
    @Test
    fun aYesOnTheDefaultsTurnsHdrOn() {
        val after = Settings(hdrEnabled = false).allowingHdrWithoutHeadroom(inPreset = false)
        assertTrue(after.hdrOn(DisplayHdr.NoHeadroom))
    }

    /** A first yes inside a preset leaves the defaults off, though the stored default was on. */
    @Test
    fun aFirstYesInAPresetLeavesTheDefaultsOff() {
        val after = Settings().allowingHdrWithoutHeadroom(inPreset = true)
        assertTrue(after.hdrWithoutHeadroom)
        assertFalse(after.hdrOn(DisplayHdr.NoHeadroom))
    }

    /** A later yes inside a preset keeps defaults that already showed HDR on. */
    @Test
    fun aLaterYesInAPresetKeepsTheDefaultsOn() {
        val after = Settings(hdrWithoutHeadroom = true).allowingHdrWithoutHeadroom(inPreset = true)
        assertTrue(after.hdrOn(DisplayHdr.NoHeadroom))
    }
}
