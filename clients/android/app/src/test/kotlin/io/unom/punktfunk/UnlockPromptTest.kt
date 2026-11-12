package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performKeyInput
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
import org.robolectric.annotation.Config

/** The prompt driven by a remote or a pad alone: focus in the field, Enter signs in, B cancels. */
@OptIn(ExperimentalTestApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36], qualifiers = "w960dp-h540dp-television-xhdpi")
class UnlockPromptTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private fun show(ask: PasswordAsk) {
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
}
