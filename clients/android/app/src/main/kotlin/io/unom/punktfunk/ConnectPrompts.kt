package io.unom.punktfunk

import androidx.compose.runtime.Composable
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.models.PendingLinkConnect
import io.unom.punktfunk.models.PendingTrust

/**
 * Everything `ConnectScreen` puts ON TOP of whichever home it drew — the trust and pairing
 * ceremony, a link's connect confirmation, the parked "Waiting for approval…", the console's host
 * options, the speed test, the edit form, the local-network rationale, and finally the connect
 * takeover.
 *
 * They live together because their ORDER is the contract: this is a stack of siblings in one tree,
 * so the last one drawn is the one on top, and [ConnectOverlay] is last on purpose — a dial can
 * start from any of the prompts above it, and its takeover has to cover the prompt that started it.
 *
 * Only the state each prompt reads comes in; every action goes back out as a callback, because they
 * all end in the connect/pair engine or in the host store, which the screen owns. Nothing in here
 * decides anything — it decides only what is visible.
 */
@Composable
internal fun ConnectPrompts(
    /** The client identity — the PIN ceremony needs it to run SPAKE2; null while it is still minting. */
    identity: ClientIdentity?,
    presets: List<StreamPreset>,
    isOnline: (KnownHost) -> Boolean,
    // ---- trust / pairing --------------------------------------------------------------------
    pendingTrust: PendingTrust?,
    /** Dismiss (null) or re-aim the SAME decision at another kind — "Pair with PIN…" does that. */
    onPendingTrustChange: (PendingTrust?) -> Unit,
    /** Trust-on-first-use accepted: dial with no pin. Offered only for a `pair=optional` host. */
    onTrustNew: (PendingTrust) -> Unit,
    /** The PIN ceremony completed with this host fingerprint — save as paired, then dial. */
    onPaired: (PendingTrust, String) -> Unit,
    onRequestAccess: (PendingTrust) -> Unit,
    // ---- a link that named a saved host by a guessable reference ----------------------------
    /** Non-null while such a link waits for the OK that turns it into a plain dial. */
    pendingLinkConnect: PendingLinkConnect?,
    onConfirmLinkConnect: (PendingLinkConnect) -> Unit,
    onDismissLinkConnect: () -> Unit,
    // ---- the parked no-PIN request ----------------------------------------------------------
    /** Non-null while a "request access" connect sits parked on the host awaiting approval. */
    awaitingHostName: String?,
    onCancelApproval: () -> Unit,
    // ---- speed test --------------------------------------------------------------------------
    speedTest: HostCardEntry?,
    /** Which layer Apply writes to. Resolved by the caller (it holds the store); set with [speedTest]. */
    speedTestTarget: SpeedTestTarget?,
    speedTestPhase: SpeedTestPhase,
    /** true = write the measured bitrate to the preset, false = to the global default. */
    onApplySpeedTest: (Boolean) -> Unit,
    onDismissSpeedTest: () -> Unit,
    // ---- edit host ---------------------------------------------------------------------------
    editTarget: KnownHost?,
    /** A MAC from the live advert, for a host whose own is not learned yet. */
    editSuggestedMacs: List<String>,
    onSaveHost: (KnownHost) -> Unit,
    onDismissEdit: () -> Unit,
    // ---- local network permission ------------------------------------------------------------
    lnpPrompt: Boolean,
    onAllowLocalNetwork: () -> Unit,
    onOpenSystemSettings: () -> Unit,
    onDismissLnpPrompt: () -> Unit,
    // ---- the connect takeover ----------------------------------------------------------------
    connectingHostName: String?,
    waker: WakeController,
    onCancelConnect: () -> Unit,
) {
    pendingTrust?.let { pt ->
        val onPair = { onPendingTrustChange(pt.copy(kind = PendingTrust.Kind.PAIR)) }
        when (pt.kind) {
            PendingTrust.Kind.TRUST_NEW -> TrustNewHostPrompt(
                pt,
                onTrust = { onTrustNew(pt) },
                onPairInstead = onPair,
                onDismiss = { onPendingTrustChange(null) },
            )
            PendingTrust.Kind.FP_CHANGED ->
                FingerprintChangedPrompt(pt, onPair) { onPendingTrustChange(null) }
            PendingTrust.Kind.REQUEST_ACCESS -> RequestAccessPrompt(
                pt,
                onRequestAccess = { onRequestAccess(pt) },
                onUsePin = onPair,
                onDismiss = { onPendingTrustChange(null) },
            )
            PendingTrust.Kind.PAIR -> {
                val onSavePaired = { fp: String -> onPaired(pt, fp) }
                PairPinDialog(pt, identity, onSavePaired) { onPendingTrustChange(null) }
            }
        }
    }

    pendingLinkConnect?.let { plc ->
        LinkConnectPrompt(
            target = plc,
            onConnect = { onConfirmLinkConnect(plc) },
            onDismiss = onDismissLinkConnect,
        )
    }

    awaitingHostName?.let { hostLabel ->
        AwaitingApprovalPrompt(hostLabel = hostLabel, onCancel = onCancelApproval)
    }

    if (speedTest != null && speedTestTarget != null) {
        SpeedTestPrompt(
            speedTest.host.name, speedTestTarget, speedTestPhase,
            onApplySpeedTest, onDismissSpeedTest,
        )
    }

    editTarget?.let { kh ->
        EditHostDialog(
            target = kh,
            suggestedMacs = editSuggestedMacs,
            presets = presets,
            onSave = onSaveHost,
            onDismiss = onDismissEdit,
        )
    }

    if (lnpPrompt) {
        // Android 17+ local-network-permission rationale: re-request (a permanently-denied request
        // returns instantly without a system prompt — hence the settings deep link alongside).
        LocalNetworkPrompt(
            onAllow = onAllowLocalNetwork,
            onSettings = onOpenSystemSettings,
            onDismiss = onDismissLnpPrompt,
        )
    }

    // Topmost: the full-screen connect takeover — instant "Connecting…" feedback on any dial, flowing
    // seamlessly into the "Waking…" wait if the host turns out to be asleep.
    ConnectOverlay(
        connectingHostName = connectingHostName,
        waker = waker,
        onCancelConnect = onCancelConnect,
    )
}
