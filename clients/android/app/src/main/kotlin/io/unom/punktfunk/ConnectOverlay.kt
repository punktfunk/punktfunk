package io.unom.punktfunk

import android.content.res.Configuration
import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Bedtime
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.DialogProperties
import io.unom.punktfunk.kit.wakingLine

/**
 * Which phase of the connect flow to draw — the pure view model [ConnectOverlay] resolves from the
 * live dial/wake state, so [ConnectModal] can render (and be screenshot-tested)
 * statelessly.
 */
internal sealed interface ConnectPhase {
    val hostName: String

    /** The dial is in flight (shown the instant a host is picked). */
    data class Connecting(override val hostName: String) : ConnectPhase

    /** A dial's unlock is starting [profile]'s desktop, which can take a minute. */
    data class Starting(override val hostName: String, val profile: String) : ConnectPhase

    /** A sleeping host is being Wake-on-LAN'd and we're waiting for it to advertise again. */
    data class Waking(override val hostName: String, val seconds: Int, val connectsAfter: Boolean) : ConnectPhase

    /** The wake wait ran out — offer retry / cancel. */
    data class WakeTimedOut(override val hostName: String) : ConnectPhase
}

/** Per-phase copy, shared by the console takeover and the touch modal so both read identically. */
private data class ConnectCopy(
    val title: String,
    val subtitle: String,
    /** Monospace the subtitle so a ticking seconds counter doesn't jitter its width. */
    val monoSubtitle: Boolean,
    val cancelLabel: String,
)

private fun connectCopy(phase: ConnectPhase): ConnectCopy = when (phase) {
    is ConnectPhase.Connecting -> ConnectCopy(
        "Connecting to ${phase.hostName}", "Establishing a secure connection…", false, "Cancel",
    )
    is ConnectPhase.Starting -> ConnectCopy(wakingLine(phase.profile), "This can take a minute.", false, "Cancel")
    is ConnectPhase.Waking -> ConnectCopy(
        "Waking ${phase.hostName}…",
        "Waiting for it to come online · ${phase.seconds}s",
        true,
        // A wake-only wait (no dial after) says "Stop Waiting"; a wake that will connect says "Cancel".
        if (!phase.connectsAfter) "Stop Waiting" else "Cancel",
    )
    is ConnectPhase.WakeTimedOut -> ConnectCopy(
        "${phase.hostName} didn't wake",
        "It may still be booting, or it's powered off / off this network.",
        false,
        "Cancel",
    )
}

/**
 * The unified "getting you connected" feedback — one flow for BOTH phases of reaching a host, so the
 * user gets feedback the instant they pick one and it flows seamlessly into a wake if the host turns
 * out to be asleep:
 *
 *  - **Connecting** ([connectingHostName] non-null): the dial is in flight. Shown immediately on tap,
 *    so a host that takes a beat to answer no longer looks like nothing happened. With
 *    [startingProfile], the dial's unlock is starting that profile's desktop.
 *  - **Waking** ([WakeController.waking] non-null): the dial failed on a sleeping host, so we're firing
 *    Wake-on-LAN and waiting for it to advertise again, escalating to a retry/cancel prompt on timeout.
 *
 * Drawn as a Material [ConnectModal] over the host grid, matching the app's other dialogs.
 *
 * The two phases hand off within a single Compose frame (see ConnectScreen's `doConnectDirect` →
 * `waker.start` → redial), so nothing blinks between them.
 */
@Composable
fun ConnectOverlay(
    connectingHostName: String?,
    waker: WakeController,
    onCancelConnect: () -> Unit,
    startingProfile: String? = null,
) {
    val waking = waker.waking
    // Waking takes precedence (it only exists after a dial has failed) so a stray overlap can't strand
    // the "Connecting…" phase over a wake in progress.
    val phase = when {
        waking != null && waking.timedOut -> ConnectPhase.WakeTimedOut(waking.hostName)
        waking != null -> ConnectPhase.Waking(waking.hostName, waking.seconds, waking.connectsAfter)
        connectingHostName != null && startingProfile != null ->
            ConnectPhase.Starting(connectingHostName, startingProfile)
        connectingHostName != null -> ConnectPhase.Connecting(connectingHostName)
        else -> return
    }

    // System Back / pad B (remapped) cancels whatever's in flight — a plain dial or the wake wait.
    val cancel = { if (waking != null) waker.cancel() else onCancelConnect() }

    // The AlertDialog owns its own scrim + system-Back handling (routed to cancel).
    ConnectModal(phase = phase, onCancel = cancel, onRetry = { waker.retry() })
}

/**
 * The default-UI presentation: a Material dialog over the host grid, matching the app's other touch
 * dialogs. A spinner (or the sleep glyph once timed out) sits above the title; the scrim is inert so a
 * stray tap can't drop a connect in flight — only the buttons or system Back cancel.
 */
@Composable
internal fun ConnectModal(
    phase: ConnectPhase,
    onCancel: () -> Unit,
    onRetry: () -> Unit,
) {
    val copy = connectCopy(phase)
    val timedOut = phase is ConnectPhase.WakeTimedOut
    AlertDialog(
        onDismissRequest = onCancel,
        properties = DialogProperties(dismissOnClickOutside = false),
        icon = {
            if (timedOut) {
                Icon(Icons.Filled.Bedtime, contentDescription = null)
            } else {
                CircularProgressIndicator(modifier = Modifier.size(28.dp), strokeWidth = 3.dp)
            }
        },
        title = { Text(copy.title, textAlign = TextAlign.Center) },
        text = {
            Text(
                copy.subtitle,
                textAlign = TextAlign.Center,
                fontFamily = if (copy.monoSubtitle) FontFamily.Monospace else FontFamily.Default,
            )
        },
        // No confirm action until the wake times out; then "Try Again" is the primary button.
        confirmButton = {
            if (timedOut) TextButton(onClick = onRetry) { Text("Try Again") }
        },
        dismissButton = {
            TextButton(onClick = onCancel) { Text(copy.cancelLabel) }
        },
    )
}
