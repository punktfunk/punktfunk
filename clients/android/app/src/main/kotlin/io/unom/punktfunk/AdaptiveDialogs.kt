package io.unom.punktfunk

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.size
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.DialogProperties
import io.unom.punktfunk.kit.R
import io.unom.punktfunk.models.PendingLinkConnect
import io.unom.punktfunk.models.PendingTrust

// The touch UI's prompts, each described once — a title, a list of [DialogAction]s (primary
// first) and a body — and drawn as a Material `AlertDialog`.

/** One button of a prompt. [primary] lifts it into the confirm slot; the rest lay out beside. */
class DialogAction(
    val label: String,
    val primary: Boolean = false,
    val enabled: Boolean = true,
    val onClick: () -> Unit,
)

/**
 * One prompt. [actions] is ordered PRIMARY FIRST — the first (or the one flagged primary) becomes
 * `confirmButton`, the rest lay out beside it.
 */
@Composable
fun PunktfunkDialog(
    title: String,
    onDismiss: () -> Unit,
    actions: List<DialogAction>,
    /**
     * False pins the prompt open against a stray tap outside it — for a dialog sitting over work
     * in flight, where a mis-tap would abandon it.
     */
    dismissOnOutsideTap: Boolean = true,
    body: @Composable ColumnScope.() -> Unit,
) {
    val primary = actions.firstOrNull { it.primary } ?: actions.firstOrNull()
    val rest = actions.filter { it !== primary }
    AlertDialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(dismissOnClickOutside = dismissOnOutsideTap),
        title = { Text(title) },
        text = { Column(verticalArrangement = Arrangement.spacedBy(10.dp)) { body() } },
        confirmButton = {
            primary?.let { a ->
                TextButton(onClick = a.onClick, enabled = a.enabled) { Text(a.label) }
            }
        },
        dismissButton = {
            if (rest.isNotEmpty()) {
                Row {
                    rest.forEach { a ->
                        TextButton(onClick = a.onClick, enabled = a.enabled) { Text(a.label) }
                    }
                }
            }
        },
    )
}

/** A prompt's body paragraph, dimmed to sit under the title. */
@Composable
internal fun PromptText(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

/** First connection to a host that advertised pair=optional: offer TOFU, but pitch PIN pairing. */
@Composable
fun TrustNewHostPrompt(
    pt: PendingTrust,
    onTrust: () -> Unit,
    onPairInstead: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        title = stringResource(R.string.trust_title),
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction(stringResource(R.string.trust_accept), primary = true, onClick = onTrust),
            DialogAction(stringResource(R.string.pair_use_pin), onClick = onPairInstead),
            DialogAction(stringResource(R.string.common_cancel), onClick = onDismiss),
        ),
    ) {
        PromptText(stringResource(R.string.trust_first_connection, "${pt.host}:${pt.port}"))
        pt.advertisedFp?.let {
            PromptText(stringResource(R.string.android_trust_fingerprint, it.take(16)))
        }
        PromptText(stringResource(R.string.trust_explain))
    }
}

/** A typed address refused the pin saved for it — pair by PIN, never a silent re-trust. */
@Composable
fun FingerprintChangedPrompt(
    pt: PendingTrust,
    onRepair: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        title = stringResource(R.string.trust_mismatch_title),
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction(stringResource(R.string.pair_action), primary = true, onClick = onRepair),
            DialogAction(stringResource(R.string.common_cancel), onClick = onDismiss),
        ),
    ) {
        PromptText(stringResource(R.string.android_trust_mismatch_body, pt.host))
    }
}

/**
 * A fresh pair=required (or manual/unknown-policy) host: offer the two ways in. "Request access" is
 * the no-PIN path — connect and wait for the operator to click Approve in the host's console;
 * "Use a PIN…" switches to the SPAKE2 ceremony.
 */
@Composable
fun RequestAccessPrompt(
    pt: PendingTrust,
    onRequestAccess: () -> Unit,
    onUsePin: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        title = stringResource(R.string.pair_required_title),
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction(stringResource(R.string.pair_request_access), primary = true, onClick = onRequestAccess),
            DialogAction(stringResource(R.string.pair_use_pin), onClick = onUsePin),
            DialogAction(stringResource(R.string.common_cancel), onClick = onDismiss),
        ),
    ) {
        PromptText(stringResource(R.string.pair_required_body, pt.host))
        PromptText(stringResource(R.string.pair_intro))
    }
}

/**
 * A `punktfunk://` link that named a saved host by its label or its address rather than by its
 * stable id: both are guessable, and the activity is exported, so the dial happens on the user's
 * tap instead of on the link's say-so. A link that names the id — every shortcut Punktfunk itself
 * emits — never reaches this prompt.
 */
@Composable
fun LinkConnectPrompt(
    target: PendingLinkConnect,
    onConnect: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        title = stringResource(R.string.link_confirm_title),
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction(stringResource(R.string.common_connect), primary = true, onClick = onConnect),
            DialogAction(stringResource(R.string.common_cancel), onClick = onDismiss),
        ),
    ) {
        PromptText(stringResource(R.string.link_confirm_asks, target.host.name, target.host.address))
        target.launch?.let { PromptText(stringResource(R.string.link_confirm_launch, it)) }
        PromptText(stringResource(R.string.link_confirm_guessable))
    }
}

/**
 * The no-PIN "request access" wait: the connect is parked on the host until the operator approves
 * this device. Cancel returns the UI immediately — the caller trips the per-attempt flag so a late
 * approval is torn down silently (see ConnectScreen.requestAccess) and resumes discovery.
 *
 * Outside taps are ignored: a connect is parked on the host, and a stray tap beside the card is not
 * a decision to abandon it.
 */
@Composable
fun AwaitingApprovalPrompt(hostLabel: String, deviceCode: String?, onCancel: () -> Unit) {
    PunktfunkDialog(
        title = stringResource(R.string.pair_waiting_for_approval),
        onDismiss = onCancel,
        actions = listOf(DialogAction(stringResource(R.string.common_cancel), primary = true, onClick = onCancel)),
        dismissOnOutsideTap = false,
    ) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            CircularProgressIndicator(
                modifier = Modifier.size(20.dp),
                strokeWidth = 2.dp,
                color = MaterialTheme.colorScheme.primary,
            )
            Text(
                stringResource(R.string.dial_approve_no_pin, hostLabel),
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
        // The host's web console shows the same code beside this request.
        if (deviceCode != null) {
            Text(
                stringResource(R.string.pair_device_code, deviceCode),
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Medium,
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
    }
}

/**
 * Android 17+ Local Network Protection rationale: ACCESS_LOCAL_NETWORK was denied, so discovery and
 * every connect are dead — offer the system prompt again and a settings deep link (a permanently-
 * denied request returns instantly without ever showing the prompt, so "Allow" alone isn't enough).
 */
@Composable
fun LocalNetworkPrompt(
    onAllow: () -> Unit,
    onSettings: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        title = stringResource(R.string.android_local_network_title),
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction(stringResource(R.string.android_local_network_allow), primary = true, onClick = onAllow),
            DialogAction(stringResource(R.string.android_local_network_settings), onClick = onSettings),
            DialogAction(stringResource(R.string.android_local_network_later), onClick = onDismiss),
        ),
    ) {
        PromptText(stringResource(R.string.android_local_network_body))
        PromptText(stringResource(R.string.android_local_network_hint))
    }
}

/**
 * The link measurement and what to do with the result. A TV box on a powerline adapter is exactly
 * the machine whose link is worth measuring, so this belongs on the couch surface too — and so
 * does [speedTestTargetNote], which the console used to omit, leaving a console user to guess
 * which layer Apply would write to.
 */
@Composable
fun SpeedTestPrompt(
    hostName: String,
    target: SpeedTestTarget,
    phase: SpeedTestPhase,
    onApply: (toPreset: Boolean) -> Unit,
    onDismiss: () -> Unit,
) {
    val done = phase as? SpeedTestPhase.Done
    PunktfunkDialog(
        title = stringResource(R.string.speedtest_title),
        onDismiss = onDismiss,
        // Measuring runs probes for ten seconds and more; a tap outside must not abandon it midway.
        dismissOnOutsideTap = phase !is SpeedTestPhase.Measuring,
        actions = buildList {
            if (done != null) {
                add(
                    DialogAction(
                        when (target) {
                            SpeedTestTarget.Global ->
                                stringResource(R.string.speedtest_use_rate, mbitRate(done.recommendedMbps))
                            is SpeedTestTarget.Preset ->
                                stringResource(R.string.speedtest_apply_to_preset, target.preset.name)
                            is SpeedTestTarget.Ask ->
                                stringResource(R.string.speedtest_set_in_preset, target.preset.name)
                        },
                        primary = true,
                    ) { onApply(true) },
                )
                if (target is SpeedTestTarget.Ask) {
                    add(DialogAction(stringResource(R.string.speedtest_set_default)) { onApply(false) })
                }
            }
            add(DialogAction(stringResource(R.string.common_close), primary = done == null, onClick = onDismiss))
        },
    ) {
        PromptText(hostName)
        when (phase) {
            SpeedTestPhase.Connecting -> PromptText(stringResource(R.string.speedtest_connecting))
            SpeedTestPhase.Measuring -> PromptText(stringResource(R.string.speedtest_measuring_body))
            is SpeedTestPhase.Failed -> Text(
                phase.message,
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.error,
            )
            is SpeedTestPhase.Done -> {
                Text(
                    speedTestHeadline(phase),
                    style = MaterialTheme.typography.bodyLarge,
                    fontWeight = FontWeight.SemiBold,
                    color = MaterialTheme.colorScheme.onSurface,
                )
                phase.findings.forEach { f -> PromptText(findingMessage(f.id, f.numbers).asString()) }
                PromptText(stringResource(R.string.speedtest_recommended_body, mbitRate(phase.recommendedMbps)))
                PromptText(speedTestTargetNote(target))
            }
        }
    }
}

/**
 * What the link carries, then the clean round's loss and jitter at its rate; without a round
 * (a host without a ramp) the figure stands alone — a blast's loss is not the link's.
 */
@Composable
internal fun speedTestHeadline(done: SpeedTestPhase.Done): String {
    val rate = mbitRate(done.measuredMbps)
    val clean = done.clean ?: return stringResource(R.string.speedtest_measured, rate)
    val carries = stringResource(
        if (done.wall) R.string.speedtest_link_carries_body else R.string.speedtest_link_carries_at_least_body,
        rate,
    )
    val round = stringResource(
        R.string.speedtest_clean_round_body,
        mbitRate(clean.rateKbps / 1000.0),
        fixed(clean.lossPct, 1),
        fixed(clean.jitterUs / 1000.0, 1),
    )
    return "$carries $round"
}

/** One line saying which layer an Apply will write to, and why that one. */
@Composable
private fun speedTestTargetNote(target: SpeedTestTarget): String = when (target) {
    SpeedTestTarget.Global -> stringResource(R.string.android_speedtest_target_default_body)
    is SpeedTestTarget.Preset ->
        stringResource(R.string.android_speedtest_target_preset_body, target.preset.name)
    is SpeedTestTarget.Ask ->
        stringResource(R.string.android_speedtest_target_ask_body, target.preset.name)
}
