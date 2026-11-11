package io.unom.punktfunk

import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import io.unom.punktfunk.console.SystemEntry
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** A console field on the system keyboard: what opens, which keyboard, what goes back. */
class ConsoleSystemEntryTest {
    private fun field(json: String) = SystemEntry.from(JSONObject(json))

    @Test
    fun the_bridges_field_opens_with_its_label_and_text() {
        // The `edit_text` body `bridge.rs` sends.
        val pin = field("""{"label":"PIN","text":"12","digits":true,"secure":false}""")
        assertEquals(SystemEntry("PIN", "12", digits = true), pin)
        assertNull("a password stays on the console's tray", field("""{"label":"Password","text":"","digits":false,"secure":true}"""))
    }

    @Test
    fun digits_get_the_number_pad_and_the_rest_the_full_keyboard() {
        val digits = SystemEntry("Port", "", digits = true).keyboard
        assertEquals(KeyboardType.Number, digits.keyboardType)
        val text = SystemEntry("Name", "", digits = false).keyboard
        assertEquals(KeyboardType.Text, text.keyboardType)
        assertEquals(ImeAction.Done, digits.imeAction)
        assertEquals(ImeAction.Done, text.imeAction)
    }

    @Test
    fun the_old_text_is_erased_one_code_point_per_backspace() {
        // A surrogate pair and a decomposed accent: two UTF-16 units, one and two code points.
        assertEquals(1 to "서울", SystemEntry("Name", "🎮", digits = false).replacement("서울"))
        assertEquals(2 to "", SystemEntry("Name", "é", digits = false).replacement(""))
        assertEquals(0 to "デスク", SystemEntry("Name", "", digits = false).replacement("デスク"))
    }

    @Test
    fun full_width_digits_reach_a_digits_field_as_ascii() {
        assertEquals(2 to "47989", SystemEntry("Port", "12", digits = true).replacement("４７９８９"))
        // A text field keeps what was typed.
        assertEquals(0 to "１２", SystemEntry("Name", "", digits = false).replacement("１２"))
    }
}
