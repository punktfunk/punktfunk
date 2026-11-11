package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Test

class TextTest {
    @Test
    fun asciiDigitsLeavesAsciiAlone() {
        assertEquals("0123456789", asciiDigits("0123456789"))
        assertEquals("", asciiDigits(""))
    }

    @Test
    fun asciiDigitsMapsFullWidthAndArabicIndic() {
        assertEquals("4821", asciiDigits("４８２１"))
        assertEquals("0123456789", asciiDigits("０１２３４５６７８９"))
        assertEquals("0123", asciiDigits("٠١٢٣"))
        assertEquals("48", asciiDigits("4８"))
    }

    @Test
    fun asciiDigitsDropsEverythingElse() {
        assertEquals("12", asciiDigits("a1 b2-"))
        assertEquals("", asciiDigits("ＡＢ　:"))
    }
}
