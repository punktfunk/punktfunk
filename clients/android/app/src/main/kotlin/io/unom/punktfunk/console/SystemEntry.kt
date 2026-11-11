package io.unom.punktfunk.console

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.input.key.Key
import androidx.compose.ui.input.key.KeyEventType
import androidx.compose.ui.input.key.key
import androidx.compose.ui.input.key.onPreviewKeyEvent
import androidx.compose.ui.input.key.type
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.TextFieldValue
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.asciiDigits
import org.json.JSONObject

/** A console field the system keyboard types into: the bridge's `edit_text`. */
internal data class SystemEntry(val label: String, val text: String, val digits: Boolean) {
    /** Digits get the number pad; the rest the full keyboard, with its candidates. */
    val keyboard: KeyboardOptions
        get() = KeyboardOptions(
            keyboardType = if (digits) KeyboardType.Number else KeyboardType.Text,
            imeAction = ImeAction.Done,
        )

    /**
     * How many Backspaces clear the console's copy, and the text that replaces it. The console's
     * Backspace drops one code point. A CJK keyboard types full-width digits.
     */
    fun replacement(typed: String): Pair<Int, String> =
        text.codePointCount(0, text.length) to if (digits) asciiDigits(typed) else typed

    companion object {
        /** `null` for a password, which the console's own tray types. */
        fun from(field: JSONObject): SystemEntry? =
            if (field.optBoolean("secure")) null
            else SystemEntry(field.optString("label"), field.optString("text"), field.optBoolean("digits"))
    }
}

/**
 * [entry] in a real editable, so the system IME composes, offers candidates and dictation, and a
 * TV remote drives its keyboard. Its own window: the console's pad probes see none of its keys.
 */
@Composable
internal fun SystemEntryDialog(entry: SystemEntry, onDone: (String) -> Unit, onCancel: () -> Unit) {
    var value by remember(entry) { mutableStateOf(TextFieldValue(entry.text, TextRange(entry.text.length))) }
    AlertDialog(
        onDismissRequest = onCancel,
        confirmButton = { TextButton(onClick = { onDone(value.text) }) { Text(stringResource(R.string.common_done)) } },
        dismissButton = { TextButton(onClick = onCancel) { Text(stringResource(R.string.common_cancel)) } },
        text = {
            // The dialog's own window: its focus and keyboard, not the activity's.
            val focus = remember { FocusRequester() }
            val keyboard = LocalSoftwareKeyboardController.current
            LaunchedEffect(entry) {
                focus.requestFocus()
                keyboard?.show()
            }
            OutlinedTextField(
                value = value,
                onValueChange = { value = it },
                label = { Text(entry.label) },
                singleLine = true,
                keyboardOptions = entry.keyboard,
                keyboardActions = KeyboardActions(onDone = { onDone(value.text) }),
                modifier = Modifier
                    .fillMaxWidth()
                    .focusRequester(focus)
                    // The IME sees each key first; what it passes on is a hardware Enter or a pad's B.
                    .onPreviewKeyEvent { ev ->
                        if (ev.type != KeyEventType.KeyDown) return@onPreviewKeyEvent false
                        when (ev.key) {
                            Key.Enter, Key.NumPadEnter -> onDone(value.text)
                            Key.ButtonB, Key.Escape -> onCancel()
                            else -> return@onPreviewKeyEvent false
                        }
                        true
                    },
            )
        },
    )
}
