package io.unom.punktfunk

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.input.TextFieldState
import androidx.compose.foundation.text.input.clearText
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedSecureTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.ContentType
import androidx.compose.ui.semantics.contentType
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.unit.dp

/**
 * The password prompt for a profile on a person's own account: its name, a masked field the
 * system's password manager can fill, one switch (off), and the host's sentence after a refusal.
 * The field is plain `remember` state, never saved instance state. Unlock hands its characters
 * over as a CharArray and clears the field; leaving the composition counts as Cancel.
 */
@Composable
internal fun UnlockPromptDialog(ask: PasswordAsk) {
    val field = remember(ask) { TextFieldState() }
    var switchOn by remember(ask) { mutableStateOf(false) }
    fun finish(submit: Boolean) {
        val answer = if (submit) {
            val text = field.text
            PasswordAnswer(CharArray(text.length) { text[it] }, switchOn)
        } else {
            null
        }
        field.clearText()
        if (!ask.answer.complete(answer)) answer?.password?.fill('\u0000')
    }
    DisposableEffect(ask) {
        onDispose {
            field.clearText()
            ask.answer.complete(null)
        }
    }
    AlertDialog(
        onDismissRequest = { finish(submit = false) },
        title = { Text("Unlock ${ask.profileName}") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                OutlinedSecureTextField(
                    state = field,
                    label = { Text("Password") },
                    isError = ask.error != null,
                    supportingText = ask.error?.let { { Text(it) } },
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Done),
                    onKeyboardAction = { if (field.text.isNotEmpty()) finish(submit = true) },
                    modifier = Modifier
                        .fillMaxWidth()
                        .semantics { contentType = ContentType.Password },
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        if (ask.biometric) "Unlock with fingerprint on this device" else "Stay signed in on this device",
                        style = MaterialTheme.typography.bodyLarge,
                        modifier = Modifier.weight(1f).padding(end = 12.dp),
                    )
                    Switch(checked = switchOn, onCheckedChange = { switchOn = it })
                }
            }
        },
        confirmButton = {
            TextButton(enabled = field.text.isNotEmpty(), onClick = { finish(submit = true) }) { Text("Unlock") }
        },
        dismissButton = { TextButton(onClick = { finish(submit = false) }) { Text("Cancel") } },
    )
}
