package io.unom.punktfunk

import android.content.pm.PackageManager
import android.text.InputType
import android.view.inputmethod.EditorInfo
import androidx.activity.ComponentActivity
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.text.TextLayoutResult
import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performKeyInput
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.pressKey
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import org.robolectric.shadows.ShadowDialog

/**
 * The prompt driven by a remote or a pad alone: focus in the field, Enter signs in, B cancels, and
 * neither the field nor the keyboard shows a digit of the PIN.
 */
@OptIn(ExperimentalTestApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36], qualifiers = "w960dp-h540dp-television-xhdpi")
class UnlockPromptTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private fun show(ask: PasswordAsk) {
        shadowOf(compose.activity.packageManager).setSystemFeature(PackageManager.FEATURE_LEANBACK, true)
        InstrumentationRegistry.getInstrumentation().setInTouchMode(false)
        compose.setContent { UnlockPromptDialog(ask) }
        compose.waitForIdle()
    }

    @Test
    fun a_remote_types_the_pin_where_focus_starts_and_enter_signs_in() {
        val ask = PasswordAsk("Kid", biometric = false, error = null, pin = true)
        show(ask)
        compose.onNodeWithText("PIN").assertIsFocused()
        val field = compose.onNode(hasSetTextAction())
        field.performTextInput("2468")
        field.performKeyInput { pressKey(Key.Enter) }
        val answer = ask.answer.getCompleted()!!
        assertEquals("2468", String(answer.password))
    }

    @Test
    fun a_pads_b_cancels() {
        val ask = PasswordAsk("Ada", biometric = false, error = null)
        show(ask)
        compose.onNode(hasSetTextAction()).performKeyInput { pressKey(Key.ButtonB) }
        assertTrue(ask.answer.isCompleted)
        assertNull(ask.answer.getCompleted())
    }

    @Test
    fun a_pin_shows_no_digit_and_asks_the_keyboard_for_a_number_password() {
        show(PasswordAsk("Kid", biometric = false, error = null, pin = true))
        val field = compose.onNode(hasSetTextAction())
        compose.mainClock.autoAdvance = false
        // One digit at a time, as a keyboard types: the last one is never shown, not even briefly.
        for (digit in "2468") field.performTextInput(digit.toString())
        compose.mainClock.advanceTimeByFrame()
        val layouts = mutableListOf<TextLayoutResult>()
        field.performSemanticsAction(SemanticsActions.GetTextLayoutResult) { it(layouts) }
        assertEquals("\u2022".repeat(4), layouts.single().layoutInput.text.text)
        val info = EditorInfo()
        ShadowDialog.getLatestDialog().window!!.decorView.findFocus().onCreateInputConnection(info)
        assertEquals(InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD, info.inputType and 0xfff)
        assertTrue("no fullscreen line", info.imeOptions and EditorInfo.IME_FLAG_NO_FULLSCREEN != 0)
    }

    @Test
    fun a_password_on_a_tv_shows_no_character_and_is_a_password_for_the_keyboard() {
        show(PasswordAsk("Ada", biometric = false, error = null))
        val field = compose.onNode(hasSetTextAction())
        compose.mainClock.autoAdvance = false
        for (c in "pw") field.performTextInput(c.toString())
        compose.mainClock.advanceTimeByFrame()
        val layouts = mutableListOf<TextLayoutResult>()
        field.performSemanticsAction(SemanticsActions.GetTextLayoutResult) { it(layouts) }
        assertEquals("\u2022".repeat(2), layouts.single().layoutInput.text.text)
        val info = EditorInfo()
        ShadowDialog.getLatestDialog().window!!.decorView.findFocus().onCreateInputConnection(info)
        assertEquals(InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD, info.inputType and 0xfff)
    }
}
