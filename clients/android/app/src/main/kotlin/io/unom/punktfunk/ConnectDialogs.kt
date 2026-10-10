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
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.NativeBridge
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
            Text("Add a host", style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.height(4.dp))
            Text(
                "Enter its address. You'll pair with the host's PIN on first connect.",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(20.dp))
            OutlinedTextField(
                value = hostName,
                onValueChange = onHostNameChange,
                label = { Text("Name (optional)") },
                placeholder = { Text("e.g. Living Room") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(16.dp))
            OutlinedTextField(
                value = host,
                onValueChange = onHostChange,
                label = { Text("Host") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(16.dp))
            OutlinedTextField(
                value = port,
                onValueChange = { v -> onPortChange(v.filter { it.isDigit() }.take(5)) },
                label = { Text("Port") },
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
            ) { Text("Connect  ($modeLabel)") }
        }
    }
}

/**
 * The SPAKE2 PIN ceremony dialog. Runs [NativeBridge.nativePair] off the UI thread itself (the
 * pin/name/error state is dialog-local); on success hands the host's verified fingerprint to
 * [onPaired], which saves it. Dismissal is blocked while a pair attempt is in flight.
 */
@Composable
internal fun PairPinDialog(
    pt: PendingTrust,
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
        title = { Text("Pair with PIN") },
        text = {
            Column {
                Text("Enter the 4-digit PIN shown on the host.")
                OutlinedTextField(
                    value = pin,
                    onValueChange = { v -> pin = v.filter { it.isDigit() }.take(4) },
                    label = { Text("PIN") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                )
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("This device") },
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
                            val fp = withContext(Dispatchers.IO) {
                                NativeBridge.nativePair(
                                    pt.host, pt.port, id.certPem, id.privateKeyPem, pin, name,
                                )
                            }
                            pairing = false
                            if (fp.isNotEmpty()) {
                                onPaired(fp) // verified host fp — caller saves it
                            } else {
                                // Cause-specific: wrong PIN vs not-armed vs unreachable.
                                err = ConnectErrors.pairMessage(NativeBridge.nativeTakeLastError())
                            }
                        }
                    }
                },
            ) { Text(if (pairing) "Pairing…" else "Pair") }
        },
        dismissButton = {
            TextButton(enabled = !pairing, onClick = onDismiss) { Text("Cancel") }
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
        title = { Text("Edit host") },
        text = {
            Column(
                modifier = Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                OutlinedTextField(
                    value = name,
                    onValueChange = { name = it },
                    label = { Text("Name") },
                    placeholder = { Text(target.address) },
                    singleLine = true,
                )
                OutlinedTextField(
                    value = address,
                    onValueChange = { address = it },
                    label = { Text("Address") },
                    singleLine = true,
                )
                OutlinedTextField(
                    value = port,
                    onValueChange = { v -> port = v.filter { it.isDigit() }.take(5) },
                    label = { Text("Port") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                )
                OutlinedTextField(
                    value = mac,
                    onValueChange = { mac = it },
                    label = { Text("Wake-on-LAN MAC") },
                    placeholder = { Text("auto-filled when the host is seen") },
                    singleLine = true,
                )
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text("Shared clipboard", style = MaterialTheme.typography.bodyLarge)
                        Text(
                            "Text copied here pastes on this host and vice versa",
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
            ) { Text("Save") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
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
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("${action.label}?") },
        text = {
            Text(
                "This ends every stream from $hostName and anything running on it. " +
                    "You'll need to wake or start it again.",
            )
        },
        confirmButton = {
            TextButton(onClick = onConfirm) { Text(action.label) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}
