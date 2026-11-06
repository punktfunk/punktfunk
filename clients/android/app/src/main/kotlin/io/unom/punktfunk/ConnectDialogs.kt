package io.unom.punktfunk

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.PendingTrust
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The "Add host" bottom sheet: optional name + address + port, then connect at [modeLabel]. Field
 * state stays hoisted in ConnectScreen so a dismissed sheet keeps its half-typed values.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun AddHostSheet(
    hostName: String,
    onHostNameChange: (String) -> Unit,
    host: String,
    onHostChange: (String) -> Unit,
    port: String,
    onPortChange: (String) -> Unit,
    connecting: Boolean,
    modeLabel: String,
    onDismiss: () -> Unit,
    onConnect: (host: String, port: Int, name: String) -> Unit,
) {
    val scope = rememberCoroutineScope()
    val sheetState = rememberModalBottomSheetState()
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 24.dp)
                .padding(bottom = 32.dp),
        ) {
            Text(stringResource(R.string.hosts_add_title), style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.height(4.dp))
            Text(
                stringResource(R.string.hosts_add_hint),
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(20.dp))
            OutlinedTextField(
                value = hostName,
                onValueChange = onHostNameChange,
                label = { Text(stringResource(R.string.hosts_field_name)) },
                placeholder = { Text(stringResource(R.string.hosts_field_name_placeholder)) },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(16.dp))
            OutlinedTextField(
                value = host,
                onValueChange = onHostChange,
                label = { Text(stringResource(R.string.hosts_field_address)) },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(16.dp))
            OutlinedTextField(
                value = port,
                onValueChange = { v -> onPortChange(v.filter { it.isDigit() }.take(5)) },
                label = { Text(stringResource(R.string.hosts_field_port)) },
                singleLine = true,
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(20.dp))
            Button(
                enabled = !connecting && host.isNotBlank() && port.isNotBlank(),
                onClick = {
                    val h = host.trim()
                    val p = port.toIntOrNull() ?: 9777
                    val n = hostName
                    scope.launch { sheetState.hide() }.invokeOnCompletion {
                        onDismiss()
                        onConnect(h, p, n)
                    }
                },
                modifier = Modifier.fillMaxWidth(),
            ) { Text(stringResource(R.string.android_dial_connect_at_mode, modeLabel)) }
        }
    }
}

/**
 * The SPAKE2 PIN ceremony dialog. Runs [HostServices.pair] off the UI thread itself (the
 * pin/name/error state is dialog-local); on success hands the host's verified fingerprint to
 * [onPaired], which saves it. Dismissal is blocked while a pair attempt is in flight.
 */
@Composable
internal fun PairPinDialog(
    pt: PendingTrust,
    services: HostServices,
    identity: ClientIdentity?,
    onPaired: (fpHex: String) -> Unit,
    onDismiss: () -> Unit,
) {
    val scope = rememberCoroutineScope()
    var pin by remember(pt) { mutableStateOf("") }
    val context = LocalContext.current
    var name by remember(pt) { mutableStateOf(deviceName(context)) }
    var pairing by remember(pt) { mutableStateOf(false) }
    var err by remember(pt) { mutableStateOf<String?>(null) }
    AlertDialog(
        onDismissRequest = { if (!pairing) onDismiss() },
        title = { Text(stringResource(R.string.pair_title, pt.name)) },
        text = {
            Column {
                Text(stringResource(R.string.pair_intro_pin_only))
                OutlinedTextField(
                    value = pin,
                    onValueChange = { v -> pin = v.filter { it.isDigit() }.take(4) },
                    label = { Text(stringResource(R.string.pair_pin)) },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                )
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text(stringResource(R.string.pair_device_name)) },
                    singleLine = true,
                )
                err?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            }
        },
        confirmButton = {
            TextButton(
                enabled = !pairing && pin.length == 4 && identity != null,
                onClick = {
                    val id = identity
                    if (id != null) {
                        pairing = true
                        err = null
                        scope.launch {
                            val paired = withContext(Dispatchers.IO) {
                                services.pair(pt.host, pt.port, id, pin, name)
                            }
                            pairing = false
                            when (paired) {
                                // Verified host fp — the caller saves it.
                                is HostServices.Paired.Ok -> onPaired(paired.fp)
                                // Cause-specific: wrong PIN vs not-armed vs unreachable.
                                is HostServices.Paired.Failed ->
                                    err = context.getString(ConnectErrors.pairMessage(paired.token))
                            }
                        }
                    }
                },
            ) { Text(stringResource(if (pairing) R.string.pair_pairing else R.string.pair_action)) }
        },
        dismissButton = {
            TextButton(enabled = !pairing, onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) }
        },
    )
}

/**
 * Edit a saved host: name, address, port, the Wake-on-LAN MAC, and the per-host settings the record
 * owns — shared clipboard (a trust decision about THIS machine, so it was never really a global).
 * The MAC is auto-learned from the host's mDNS advert while it's online, but this is where you can
 * enter or correct it (e.g. to wake a host you've only ever reached by address). [suggestedMacs]
 * prefills the field from the live advert when nothing's been learned yet. Keyed by the host so
 * reopening resets the fields. Mirrors the Apple client's edit form.
 */
@Composable
internal fun EditHostDialog(
    target: KnownHost,
    suggestedMacs: List<String>,
    presets: List<StreamPreset>,
    onSave: (KnownHost) -> Unit,
    onDismiss: () -> Unit,
) {
    var name by remember(target) { mutableStateOf(target.name) }
    var address by remember(target) { mutableStateOf(target.address) }
    var port by remember(target) { mutableStateOf(target.port.toString()) }
    var mac by remember(target) {
        mutableStateOf(target.mac.ifEmpty { suggestedMacs }.joinToString(", "))
    }
    var clipboard by remember(target) { mutableStateOf(target.clipboardSync) }
    // A binding whose preset was deleted reads as "Default settings" (which is what it already
    // resolves to) and is cleaned off the record on the next save — never an error state.
    var boundId by remember(target, presets) {
        mutableStateOf(target.presetId?.takeIf { id -> presets.any { it.id == id } })
    }
    var pins by remember(target) { mutableStateOf(target.pinnedPresetIds) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.hosts_edit_title)) },
        text = {
            Column(
                modifier = Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text(stringResource(R.string.hosts_field_name)) },
                    placeholder = { Text(target.address) },
                    singleLine = true,
                )
                OutlinedTextField(
                    value = address,
                    onValueChange = { address = it },
                    label = { Text(stringResource(R.string.hosts_field_address)) },
                    singleLine = true,
                )
                OutlinedTextField(
                    value = port,
                    onValueChange = { v -> port = v.filter { it.isDigit() }.take(5) },
                    label = { Text(stringResource(R.string.hosts_field_port)) },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                )
                OutlinedTextField(
                    value = mac,
                    onValueChange = { mac = it },
                    label = { Text(stringResource(R.string.hosts_field_mac)) },
                    placeholder = { Text(stringResource(R.string.hosts_field_mac_placeholder)) },
                    singleLine = true,
                )
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(stringResource(R.string.hosts_clipboard), style = MaterialTheme.typography.bodyLarge)
                        Text(
                            stringResource(R.string.hosts_clipboard_hint),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    Switch(checked = clipboard, onCheckedChange = { clipboard = it })
                }
                if (presets.isNotEmpty()) {
                    HostPresetBinding(
                        presets = presets,
                        boundId = boundId,
                        onBind = { boundId = it },
                        pins = pins,
                        onTogglePin = { id ->
                            pins = if (id in pins) pins - id else pins + id
                        },
                    )
                }
            }
        },
        confirmButton = {
            TextButton(
                enabled = address.isNotBlank(),
                onClick = {
                    onSave(
                        target.copy(
                            name = name.trim().ifEmpty { target.address },
                            address = address.trim(),
                            port = port.toIntOrNull() ?: target.port,
                            mac = KnownHostStore.parseMacs(mac),
                            clipboardSync = clipboard,
                            presetId = boundId,
                            pinnedPresetIds = pins,
                        ),
                    )
                },
            ) { Text(stringResource(R.string.common_save)) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) }
        },
    )
}

/**
 * "Restart host?" / "Shut down host?" — the confirmation a destructive host action takes before
 * it runs (`design/host-actions.md` §7). Sleep is reversible from the same menu ("Wake host"),
 * so it never reaches here; restart and shut down lose whatever is on that machine, so they do.
 */
@Composable
internal fun HostActionConfirmDialog(
    hostName: String,
    action: HostActions.Action,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
) {
    val label = action.text(LocalContext.current.resources)
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(stringResource(R.string.host_power_confirm_title, label)) },
        text = { Text(stringResource(R.string.hostcard_action_confirm_body, hostName)) },
        confirmButton = {
            TextButton(onClick = onConfirm) { Text(label) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text(stringResource(R.string.common_cancel)) }
        },
    )
}
