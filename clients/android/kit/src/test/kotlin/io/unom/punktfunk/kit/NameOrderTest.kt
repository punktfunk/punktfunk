package io.unom.punktfunk.kit

import java.util.Locale
import org.junit.Assert.assertEquals
import org.junit.Test

class NameOrderTest {
    private fun sorted(locale: Locale, vararg names: String) =
        names.toList().sortedWith(nameOrder(locale) { it })

    @Test
    fun plainAsciiNamesKeepTheOrderTheLowercaseSortGave() {
        val names = listOf(
            "Zed", "alpha", "Bravo", "Steam Deck", "desk", "Desk 2", "laptop", "Living Room", "game-pc", "9000",
        )
        for (locale in listOf(Locale.ENGLISH, Locale.GERMAN, Locale.JAPANESE)) {
            assertEquals(locale.toString(), names.sortedBy { it.lowercase() }, names.sortedWith(nameOrder(locale) { it }))
        }
    }

    @Test
    fun aCaseDifferenceIsATieAndKeepsInputOrder() {
        assertEquals(listOf("anna", "Anna"), sorted(Locale.ENGLISH, "anna", "Anna"))
        assertEquals(listOf("Anna", "anna"), sorted(Locale.ENGLISH, "Anna", "anna"))
    }

    @Test
    fun anAccentedNameSortsWhereItsLanguageExpectsIt() {
        assertEquals(listOf("apfel", "Äpfel", "Bravo", "Zed"), sorted(Locale.GERMAN, "Zed", "Äpfel", "Bravo", "apfel"))
        assertEquals(listOf("Bravo", "Zed", "Äpfel"), sorted(Locale.forLanguageTag("sv"), "Zed", "Äpfel", "Bravo"))
    }
}
