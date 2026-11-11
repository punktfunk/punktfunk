package io.unom.punktfunk.kit

/**
 * [s]'s decimal digits as ASCII, the rest dropped. A CJK keyboard types full-width digits
 * (`０`..`９`) by default, and the host only matches a PIN or port written in ASCII.
 */
fun asciiDigits(s: String): String = buildString {
    for (c in s) {
        val d = Character.digit(c, 10)
        if (d >= 0) append('0' + d)
    }
}
