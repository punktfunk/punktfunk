package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.Button
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.components.HostLine
import io.unom.punktfunk.components.HostStatusLine
import io.unom.punktfunk.components.HostTile
import io.unom.punktfunk.components.SCREEN_MARGIN
import io.unom.punktfunk.components.ScreenTitle
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.kit.security.KnownHost

/**
 * What the host page can do. A null act is one the host can't take yet: the library and the logs
 * need a pairing, a wake needs an offline host with a known MAC.
 */
internal class HostPageActs(
    val back: () -> Unit,
    val connect: () -> Unit,
    val browse: (() -> Unit)?,
    /** Opens the profile picker; null until this device has picked a profile. */
    val switchProfile: (() -> Unit)?,
    /** Sets the preset a plain tap connects with; null is the default settings. */
    val bind: (String?) -> Unit,
    val togglePin: (String) -> Unit,
    val pickScreen: (DisplayTargets.ScreenRow) -> Unit,
    val edit: () -> Unit,
    val setClipboard: (Boolean) -> Unit,
    val speedTest: () -> Unit,
    val copyLink: () -> Unit,
    val pair: () -> Unit,
    val unpair: () -> Unit,
    val setDefault: (Boolean) -> Unit,
    val wake: (() -> Unit)?,
    /** Runs a host action; the caller confirms a dangerous one and explains an unavailable one. */
    val power: (HostActions.Action) -> Unit,
    val sendLogs: (() -> Unit)?,
    val remove: () -> Unit,
)

/**
 * The host page: everything about one saved host that is not a plain connect, with the acts its
 * card's menu once held. Opened by the card's info button. It draws the live record, so an edit
 * shows at once.
 */
@Composable
internal fun HostDetailScreen(
    host: KnownHost,
    /** The OS-identity chain for the tile: the live advert's, else the stored one. */
    os: String,
    status: HostLine,
    /** The last confirmation and the last failure, as on the grid. */
    notice: String?,
    failure: String?,
    isDefault: Boolean,
    presets: List<StreamPreset>,
    /** What the host lets this device do to it. */
    power: List<HostActions.Action>,
    screens: DisplayTargets.Targets?,
    /** A dial is in flight: Connect waits. */
    busy: Boolean,
    acts: HostPageActs,
) {
    BackHandler(onBack = acts.back)
    val res = LocalContext.current.resources
    var confirmRemove by remember { mutableStateOf(false) }
    var confirmUnpair by remember { mutableStateOf(false) }
    Column(Modifier.fillMaxSize()) {
        ScreenTitle(stringResource(R.string.hostcard_details_hint), onBack = acts.back)
        // Under the title's margin; a wide window does not stretch the rows.
        Box(Modifier.fillMaxSize(), contentAlignment = Alignment.TopStart) {
            Column(
                verticalArrangement = Arrangement.spacedBy(20.dp),
                modifier = Modifier
                    .widthIn(max = 680.dp)
                    .verticalScroll(rememberScrollState())
                    .padding(start = SCREEN_MARGIN, end = SCREEN_MARGIN, top = 4.dp, bottom = 16.dp),
            ) {
                if (notice != null || failure != null) NoticeBanners(notice, failure)

                SettingsGroup {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        HostTile(host.name, os, filled = host.fpHex.isNotEmpty(), size = 64.dp)
                        Spacer(Modifier.width(16.dp))
                        Column {
                            Text(
                                host.name,
                                style = MaterialTheme.typography.titleLarge,
                                fontWeight = FontWeight.SemiBold,
                                maxLines = 2,
                                overflow = TextOverflow.Ellipsis,
                            )
                            HostStatusLine(status)
                        }
                    }
                    Button(onClick = acts.connect, enabled = !busy, modifier = Modifier.fillMaxWidth()) {
                        Icon(Icons.Filled.PlayArrow, contentDescription = null, modifier = Modifier.size(18.dp))
                        Spacer(Modifier.width(8.dp))
                        Text(
                            // With a game up a connect resumes it.
                            if (status is HostLine.Playing) {
                                stringResource(R.string.lib_resume_title, status.game)
                            } else {
                                stringResource(R.string.common_connect)
                            },
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    acts.browse?.let {
                        OutlinedButton(onClick = it, modifier = Modifier.fillMaxWidth()) {
                            Text(stringResource(R.string.hostcard_browse_library))
                        }
                    }
                    host.asProfile?.let { pick ->
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            ProfileFace(
                                pick.displayName, 28, picture = pick.avatar,
                                art = if (pick.avatar != null) rememberProfileArt(host) else null,
                            )
                            Text(
                                stringResource(R.string.hostcard_playing_as, pick.displayName),
                                style = MaterialTheme.typography.bodyLarge,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier.weight(1f).padding(start = 12.dp),
                            )
                            acts.switchProfile?.let {
                                TextButton(onClick = it) { Text(stringResource(R.string.common_change)) }
                            }
                        }
                    }
                }

                if (presets.isNotEmpty()) {
                    SettingsGroup(header = stringResource(R.string.hostcard_section_presets)) {
                        HostPresetBinding(
                            presets = presets,
                            // A binding to a deleted preset reads as the default settings, as a connect does.
                            boundId = host.presetId?.takeIf { id -> presets.any { it.id == id } },
                            onBind = acts.bind,
                            pins = host.pinnedPresetIds,
                            onTogglePin = acts.togglePin,
                        )
                    }
                }

                // Which host screen a connect streams. The host's bound preset decides when it pins one.
                val under = host.presetId?.let { id -> presets.firstOrNull { it.id == id } }
                val offered = screens?.takeIf { host.paired }
                val rows = offered?.rowsUnder(under, res).orEmpty()
                val note = offered?.noteUnder(under, res)
                if (rows.isNotEmpty() || note != null) {
                    SettingsGroup(header = stringResource(R.string.hostcard_section_screen), footer = note) {
                        rows.forEach { row ->
                            row.header?.let {
                                Text(
                                    it,
                                    style = MaterialTheme.typography.labelMedium,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                            ScreenPickRow(row) { acts.pickScreen(row) }
                        }
                    }
                }

                SettingsGroup(header = stringResource(R.string.hostcard_section_connection)) {
                    ValueRow(stringResource(R.string.hosts_field_address), "${host.address}:${host.port}")
                    ValueRow(stringResource(R.string.hostcard_management_port), host.effectiveMgmtPort.toString())
                    if (host.mac.isNotEmpty()) {
                        ValueRow(stringResource(R.string.hosts_field_mac), host.mac.joinToString(", "))
                    }
                    ToggleRow(
                        title = stringResource(R.string.hosts_clipboard),
                        subtitle = stringResource(R.string.hosts_clipboard_hint),
                        checked = host.clipboardSync,
                        onCheckedChange = acts.setClipboard,
                    )
                    ActRow(stringResource(R.string.hostcard_edit), onClick = acts.edit)
                    ActRow(stringResource(R.string.hostcard_speed_test), onClick = acts.speedTest)
                    ActRow(stringResource(R.string.hostcard_copy_link), onClick = acts.copyLink)
                }

                SettingsGroup(header = stringResource(R.string.hostcard_section_pairing)) {
                    ValueRow(
                        stringResource(
                            when {
                                host.paired -> R.string.hostcard_paired
                                host.fpHex.isNotEmpty() -> R.string.hostcard_tofu
                                else -> R.string.hostcard_status_not_paired
                            },
                        ),
                        // Enough of the fingerprint to compare with the host's own.
                        host.fpHex.takeIf { it.length > 12 }?.let { "${it.take(8)}…${it.takeLast(4)}" }.orEmpty(),
                    )
                    ActRow(stringResource(R.string.hostcard_pair_with_pin), onClick = acts.pair)
                    if (host.paired) {
                        ToggleRow(
                            title = stringResource(R.string.hostcard_default_host),
                            subtitle = stringResource(R.string.player_start_in_host_hint, host.name),
                            checked = isDefault,
                            onCheckedChange = acts.setDefault,
                        )
                    }
                    if (host.fpHex.isNotEmpty()) {
                        ActRow(stringResource(R.string.hostcard_forget_identity), danger = true) { confirmUnpair = true }
                    }
                }

                if (acts.wake != null || power.isNotEmpty()) {
                    SettingsGroup(header = stringResource(R.string.hostcard_section_power)) {
                        acts.wake?.let { ActRow(stringResource(R.string.hostcard_wake), onClick = it) }
                        power.forEach { a ->
                            val name = a.text(res)
                            ActRow(
                                if (a.available) name else stringResource(R.string.hostcard_action_unavailable, name),
                                subtitle = a.reason(res).takeIf { !a.available && it.isNotEmpty() },
                                danger = a.danger,
                            ) { acts.power(a) }
                        }
                    }
                }

                acts.sendLogs?.let {
                    SettingsGroup(
                        header = stringResource(R.string.hostcard_section_logs),
                        footer = stringResource(R.string.hostcard_send_logs_hint),
                    ) {
                        ActRow(stringResource(R.string.hostcard_send_logs), onClick = it)
                    }
                }

                SettingsGroup {
                    ActRow(stringResource(R.string.hostcard_remove), danger = true) { confirmRemove = true }
                }
            }
        }
    }

    if (confirmUnpair) {
        PunktfunkDialog(
            title = stringResource(R.string.hostcard_forget_identity_title, host.name),
            onDismiss = { confirmUnpair = false },
            actions = listOf(
                DialogAction(stringResource(R.string.hostcard_forget_identity_confirm), primary = true) {
                    confirmUnpair = false
                    acts.unpair()
                },
                DialogAction(stringResource(R.string.common_cancel)) { confirmUnpair = false },
            ),
        ) { PromptText(stringResource(R.string.hostcard_forget_identity_body)) }
    }
    if (confirmRemove) {
        PunktfunkDialog(
            title = stringResource(R.string.hostcard_remove_title, host.name),
            onDismiss = { confirmRemove = false },
            actions = listOf(
                DialogAction(stringResource(R.string.common_remove), primary = true) {
                    confirmRemove = false
                    acts.remove()
                },
                DialogAction(stringResource(R.string.common_cancel)) { confirmRemove = false },
            ),
        ) { PromptText(stringResource(R.string.hostcard_remove_body)) }
    }
}

/** A fact about the host: its label, and its value trailing. */
@Composable
private fun ValueRow(label: String, value: String) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Text(label, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
        Text(
            value,
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(start = 12.dp),
        )
    }
}

/** One act as a row; [danger] draws it in the error colour, [subtitle] says why it is unavailable. */
@Composable
private fun ActRow(title: String, subtitle: String? = null, danger: Boolean = false, onClick: () -> Unit) {
    Column(Modifier.fillMaxWidth().clickable(role = Role.Button, onClick = onClick)) {
        Text(
            title,
            style = MaterialTheme.typography.bodyLarge,
            color = if (danger) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.primary,
        )
        if (subtitle != null) {
            Text(
                subtitle,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
    }
}

/** One Screen row: the checked one does nothing, a greyed one says why under it. */
@Composable
private fun ScreenPickRow(row: DisplayTargets.ScreenRow, onPick: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .selectable(
                selected = row.checked,
                enabled = row.pickable && !row.checked,
                role = Role.RadioButton,
                onClick = onPick,
            ),
    ) {
        RadioButton(selected = row.checked, onClick = null, enabled = row.pickable)
        Column(Modifier.padding(start = 12.dp)) {
            Text(
                row.label,
                style = MaterialTheme.typography.bodyLarge,
                color = MaterialTheme.colorScheme.onSurface.copy(alpha = if (row.pickable) 1f else 0.38f),
            )
            (row.reason ?: row.hint ?: row.detail.ifEmpty { null })?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}
