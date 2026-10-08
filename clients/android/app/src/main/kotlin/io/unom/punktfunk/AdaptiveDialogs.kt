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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.DialogProperties
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
        title = "Trust this host?",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Trust (TOFU)", primary = true, onClick = onTrust),
            DialogAction("Pair with PIN…", onClick = onPairInstead),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText("First connection to ${pt.host}:${pt.port}.")
        pt.advertisedFp?.let { PromptText("Fingerprint ${it.take(16)}…") }
        PromptText(
            "This host allows trust-on-first-use, but that can't tell an impostor from the real " +
                "host. Pairing with a PIN is stronger — it proves both sides.",
        )
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
        title = "A different host answered",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Pair", primary = true, onClick = onRepair),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText(
            "${pt.host} didn't present the identity saved for it. It may be another operating " +
                "system on the same PC, a reinstall, or an impostor. Pair with the host's PIN to " +
                "add it.",
        )
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
        title = "Pairing required",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Request access", primary = true, onClick = onRequestAccess),
            DialogAction("Use a PIN…", onClick = onUsePin),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText("${pt.host}:${pt.port} requires pairing before it will stream.")
        PromptText(
            "Request access and approve this device in the host's console (or web UI) — no PIN " +
                "needed. Or pair with the 4-digit PIN the host displays.",
        )
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
        title = "Open this link?",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Connect", primary = true, onClick = onConnect),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText("A link asks to connect to ${target.host.name} (${target.host.address}).")
        target.launch?.let { PromptText("It also asks the host to launch “$it”.") }
        PromptText(
            "It names the host by its label or address, which anything that can open a link " +
                "could guess. Shortcuts made in Punktfunk name the host's id and connect " +
                "without asking.",
        )
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
fun AwaitingApprovalPrompt(hostLabel: String, onCancel: () -> Unit) {
    PunktfunkDialog(
        title = "Waiting for approval",
        onDismiss = onCancel,
        actions = listOf(DialogAction("Cancel", primary = true, onClick = onCancel)),
        dismissOnOutsideTap = false,
    ) {
        // MUST be the name the connect actually knocked with (`HostConnect`), or this sends the
        // user looking for a row the console does not show.
        val label = deviceName(LocalContext.current)
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
                "Approve this device on $hostLabel.",
                color = MaterialTheme.colorScheme.onSurface,
            )
        }
        PromptText(
            "Open the host's console (or web UI) and approve “$label”. It connects " +
                "automatically once you approve — no PIN needed.",
        )
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
        title = "Allow local network access",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Allow", primary = true, onClick = onAllow),
            DialogAction("Open settings", onClick = onSettings),
            DialogAction("Not now", onClick = onDismiss),
        ),
    ) {
        PromptText(
            "Android blocks Punktfunk from talking to devices on your network, so it can't find " +
                "or reach any host until you allow it.",
        )
        PromptText(
            "If no prompt appears after you allow it, enable “Nearby devices” for Punktfunk in " +
                "system settings.",
        )
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
        title = "Network check",
        onDismiss = onDismiss,
        // Measuring runs probes for ten seconds and more; a tap outside must not abandon it midway.
        dismissOnOutsideTap = phase !is SpeedTestPhase.Measuring,
        actions = buildList {
            if (done != null) {
                add(
                    DialogAction(
                        when (target) {
                            SpeedTestTarget.Global -> "Apply"
                            is SpeedTestTarget.Preset -> "Apply to “${target.preset.name}”"
                            is SpeedTestTarget.Ask -> "Set in “${target.preset.name}”"
                        },
                        primary = true,
                    ) { onApply(true) },
                )
                if (target is SpeedTestTarget.Ask) {
                    add(DialogAction("Set as default") { onApply(false) })
                }
            }
            add(DialogAction("Close", primary = done == null, onClick = onDismiss))
        },
    ) {
        PromptText(hostName)
        when (phase) {
            SpeedTestPhase.Connecting -> PromptText("Connecting…")
            SpeedTestPhase.Measuring ->
                PromptText(
                    "Measuring — the host is sending test traffic. This takes a few seconds.",
                )
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
                phase.findings.forEach { f -> PromptText(findingText(f.id, f.numbers)) }
                PromptText(
                    "Recommended bitrate: %.0f Mbit/s".format(phase.recommendedMbps),
                )
                PromptText(speedTestTargetNote(target))
            }
        }
    }
}

/**
 * What the link carries, then the clean round's loss and jitter at its rate; without a round
 * (a host without a ramp) the figure stands alone — a blast's loss is not the link's.
 */
internal fun speedTestHeadline(done: SpeedTestPhase.Done): String {
    val carries = if (done.wall) "Link carries %.0f Mbit/s." else "Link carries at least %.0f Mbit/s."
    val clean = done.clean ?: return "%.0f Mbit/s measured".format(done.measuredMbps)
    return carries.format(done.measuredMbps) + " At %.0f Mbit/s: %.1f %% loss, %.1f ms jitter"
        .format(clean.rateKbps / 1000.0, clean.lossPct, clean.jitterUs / 1000.0)
}

/** One line saying which layer an Apply will write to, and why that one. */
private fun speedTestTargetNote(target: SpeedTestTarget): String = when (target) {
    SpeedTestTarget.Global ->
        "This host uses the default settings, so the bitrate goes there."
    is SpeedTestTarget.Preset ->
        "This host streams with “${target.preset.name}”, which sets its own bitrate — " +
            "that override is what it actually reads."
    is SpeedTestTarget.Ask ->
        "This host streams with “${target.preset.name}”, which currently inherits the default " +
            "bitrate. Setting it in the preset affects only this host's preset; setting it as " +
            "the default affects everything that inherits it."
}
