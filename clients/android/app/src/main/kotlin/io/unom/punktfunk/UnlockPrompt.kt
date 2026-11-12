package io.unom.punktfunk

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.input.TextFieldState
import androidx.compose.foundation.text.input.clearText
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedSecureTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.ContentType
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentType
import androidx.compose.ui.semantics.error
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R

/**
 * The prompt for a profile on a person's own account or a locked one: its name, a masked field
 * for the account password (the password manager can fill it) or the profile's PIN (the number
 * pad), one switch (off), and the host's sentence after a refusal, spoken when it shows. The
 * field is plain `remember` state, never saved instance state. Sign In hands its characters over
 * as a CharArray and clears the field; leaving the composition counts as Cancel.
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
    val refusal = ask.error?.asString()
    AlertDialog(
        onDismissRequest = { finish(submit = false) },
        title = { Text(stringResource(R.string.unlock_title, ask.profileName)) },
        text = {
            val view = LocalView.current
            LaunchedEffect(ask) {
                // A live region needs a node that was there before; the dialog opens with the line.
                @Suppress("DEPRECATION")
                refusal?.let { view.announceForAccessibility(it) }
            }
            Column(
                verticalArrangement = Arrangement.spacedBy(12.dp),
                modifier = Modifier.verticalScroll(rememberScrollState()),
            ) {
                OutlinedSecureTextField(
                    state = field,
                    label = { Text(stringResource(if (ask.pin) R.string.unlock_pin else R.string.unlock_password)) },
                    placeholder = if (ask.pin) ({ Text(stringResource(R.string.unlock_pin_placeholder)) }) else null,
                    isError = refusal != null,
                    supportingText = refusal?.let { { Text(it) } },
                    keyboardOptions = if (ask.pin) {
                        KeyboardOptions(keyboardType = KeyboardType.NumberPassword, imeAction = ImeAction.Done)
                    } else {
                        KeyboardOptions(imeAction = ImeAction.Done)
                    },
                    onKeyboardAction = { if (field.text.isNotEmpty()) finish(submit = true) },
                    modifier = Modifier
                        .fillMaxWidth()
                        .semantics {
                            if (!ask.pin) contentType = ContentType.Password
                            if (refusal != null) error(refusal)
                        },
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        stringResource(
                            if (ask.biometric) R.string.android_unlock_with_fingerprint else R.string.unlock_stay_signed_in,
                        ),
                        style = MaterialTheme.typography.bodyLarge,
                        modifier = Modifier.weight(1f).padding(end = 12.dp),
                    )
                    Switch(checked = switchOn, onCheckedChange = { switchOn = it })
                }
            }
        },
        confirmButton = {
            TextButton(enabled = field.text.isNotEmpty(), onClick = { finish(submit = true) }) {
                Text(stringResource(R.string.unlock_sign_in))
            }
        },
        dismissButton = {
            TextButton(onClick = { finish(submit = false) }) { Text(stringResource(R.string.common_cancel)) }
        },
    )
}
